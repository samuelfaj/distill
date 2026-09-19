//! Mock inference server: logs every request, answers by the tiers in `inference_override`, shuts down on drop.

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::conversation::ReadConversation;
use crate::conversation_script::{Conversation, ScriptViolation};
use crate::feedback_endpoint::FeedbackEndpointState;
pub use crate::feedback_endpoint::FeedbackPost;
pub use crate::gated_upload_proxy::GatedUploadProxy;
use crate::inference_override::InferenceOverrides;
pub use crate::inference_override::{InferenceExpectation, InferenceRequestMatcher};
use crate::inference_request::DEFAULT_MODEL;
pub use crate::inference_request::InferenceEndpoint;
use crate::inference_route::InferenceRoute;
use crate::mock_server_tls::ThrowawayCa;
pub use crate::request_log::LogEntry;
use crate::request_log::RequestLog;
pub use crate::scripted::{ScriptedBody, ScriptedResponse, SseEvent};
use crate::storage_endpoint::StorageEndpointState;
pub use crate::storage_endpoint::StorageUpload;
use crate::telemetry_events::TelemetryEventsState;

/// A model served by `/v1/models`.
/// Each field is emitted under its camelCase name when set, at the top level except for `agent_type`, which goes in `_meta`.
#[derive(Debug, Clone)]
pub struct MockModelEntry {
    id: String,
    agent_type: Option<String>,
    api_backend: Option<String>,
    supports_backend_search: bool,
    supports_reasoning_effort: bool,
    reasoning_effort: Option<String>,
    /// Each entry is a table carrying a `value` key, or a bare value string.
    /// `parse_remote_model_value` defines the full shape.
    reasoning_efforts: Vec<Value>,
    /// Sets `You are <label>` in the primary system prompt, so a model switch is visible on the wire.
    system_prompt_label: Option<String>,
}

impl MockModelEntry {
    pub fn new(id: impl Into<String>) -> Self {
        MockModelEntry {
            id: id.into(),
            agent_type: None,
            api_backend: None,
            supports_backend_search: false,
            supports_reasoning_effort: false,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            system_prompt_label: None,
        }
    }

    pub fn with_agent_type(id: impl Into<String>, agent_type: impl Into<String>) -> Self {
        MockModelEntry {
            agent_type: Some(agent_type.into()),
            ..MockModelEntry::new(id)
        }
    }

    pub fn with_api_backend(mut self, api_backend: impl Into<String>) -> Self {
        self.api_backend = Some(api_backend.into());
        self
    }

    pub fn with_supports_backend_search(mut self, supports: bool) -> Self {
        self.supports_backend_search = supports;
        self
    }

    pub fn with_supports_reasoning_effort(mut self, supports: bool) -> Self {
        self.supports_reasoning_effort = supports;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_reasoning_efforts(mut self, efforts: Vec<Value>) -> Self {
        self.reasoning_efforts = efforts;
        self
    }

    pub fn with_system_prompt_label(mut self, label: impl Into<String>) -> Self {
        self.system_prompt_label = Some(label.into());
        self
    }

    fn to_json(&self) -> Value {
        // `context_length` is the OpenRouter field a Responses-route daemon sizes its context by.
        let mut obj = json!({
            "id": self.id,
            "object": "model",
            "created": 1234567890,
            "owned_by": "test",
            "context_length": 131_072
        });
        if let Some(map) = obj.as_object_mut() {
            if let Some(agent_type) = &self.agent_type {
                map.insert("_meta".into(), json!({ "agentType": agent_type }));
            }
            if let Some(backend) = &self.api_backend {
                map.insert("apiBackend".into(), json!(backend));
            }
            if self.supports_backend_search {
                map.insert("supportsBackendSearch".into(), json!(true));
            }
            if self.supports_reasoning_effort {
                map.insert("supportsReasoningEffort".into(), json!(true));
            }
            if let Some(effort) = &self.reasoning_effort {
                map.insert("reasoningEffort".into(), json!(effort));
            }
            if !self.reasoning_efforts.is_empty() {
                map.insert("reasoningEfforts".into(), json!(self.reasoning_efforts));
            }
            if let Some(label) = &self.system_prompt_label {
                map.insert("systemPromptLabel".into(), json!(label));
            }
        }
        obj
    }
}

#[derive(Clone, Copy, Default)]
enum StartupFetchStall {
    #[default]
    None,
    Delay(Duration),
    Hang,
}

#[derive(Clone)]
struct RouterState {
    log: Arc<RequestLog>,
    models: Arc<std::sync::RwLock<Vec<Value>>>,
    settings: Arc<std::sync::RwLock<Option<Value>>>,
    overrides: InferenceOverrides,
    inference: InferenceRoute,
    storage: Arc<StorageEndpointState>,
    feedback: Arc<FeedbackEndpointState>,
    telemetry: Arc<TelemetryEventsState>,
    startup_fetch_stall: Arc<std::sync::RwLock<StartupFetchStall>>,
    startup_stalls_served: Arc<AtomicU32>,
    user_tier: Arc<std::sync::RwLock<Option<String>>>,
}

impl RouterState {
    /// Counted once it is let through.
    async fn stall_startup_fetch(&self) {
        let stall = *self.startup_fetch_stall.read().unwrap();
        match stall {
            StartupFetchStall::None => return,
            StartupFetchStall::Delay(delay) => tokio::time::sleep(delay).await,
            StartupFetchStall::Hang => std::future::pending().await,
        }
        self.startup_stalls_served.fetch_add(1, Ordering::Relaxed);
    }
}

enum Transport {
    Plain,
    Tls,
}

pub struct MockInferenceServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    state: RouterState,
    tls_ca: Option<ThrowawayCa>,
}

impl MockInferenceServer {
    pub async fn start() -> anyhow::Result<Self> {
        Self::start_with_models(vec![MockModelEntry::new(DEFAULT_MODEL)]).await
    }

    pub async fn start_with_models(models: Vec<MockModelEntry>) -> anyhow::Result<Self> {
        Self::start_inner(models, None, Transport::Plain).await
    }

    /// Start a mock that returns 401 on inference requests missing `Authorization: Bearer <required_token>`.
    pub async fn start_with_required_auth(
        models: Vec<MockModelEntry>,
        required_token: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Self::start_inner(models, Some(required_token.into()), Transport::Plain).await
    }

    /// Serve the same router over HTTPS with a throwaway CA; there is no plaintext listener,
    /// so a logged request implies a completed TLS handshake.
    /// [`Self::url`] is `https://127.0.0.1:PORT/v1` and [`Self::ca_pem_path`] is the path to the
    /// CA PEM the client must trust. Only [`crate::headless::run_headless`] and
    /// [`crate::headless::run_headless_with_env`] inject it; any other runner must set
    /// `GROK_EXTRA_CA_BUNDLE` to [`Self::ca_pem_path`] on its own `TestSandbox` via `set_env`.
    pub async fn start_tls() -> anyhow::Result<Self> {
        Self::start_inner(
            vec![MockModelEntry::new(DEFAULT_MODEL)],
            None,
            Transport::Tls,
        )
        .await
    }

    async fn start_inner(
        models: Vec<MockModelEntry>,
        required_token: Option<String>,
        transport: Transport,
    ) -> anyhow::Result<Self> {
        let log = Arc::new(RequestLog::new());
        let overrides = InferenceOverrides::new(required_token);
        let state = RouterState {
            inference: InferenceRoute::new(log.clone(), overrides.clone()),
            log,
            models: Arc::new(std::sync::RwLock::new(
                models.iter().map(MockModelEntry::to_json).collect(),
            )),
            settings: Arc::new(std::sync::RwLock::new(None)),
            overrides,
            storage: Arc::new(StorageEndpointState::default()),
            feedback: Arc::new(FeedbackEndpointState::default()),
            telemetry: Arc::new(TelemetryEventsState::default()),
            startup_fetch_stall: Arc::new(std::sync::RwLock::new(StartupFetchStall::None)),
            startup_stalls_served: Arc::new(AtomicU32::new(0)),
            user_tier: Arc::new(std::sync::RwLock::new(None)),
        };
        let app = Self::build_router(state.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let (addr, tls_ca) = match transport {
            Transport::Plain => {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .context("bind mock inference server")?;
                let addr = listener.local_addr().context("local_addr")?;
                tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await
                        .unwrap();
                });
                (addr, None)
            }
            Transport::Tls => {
                let (addr, ca) = crate::mock_server_tls::serve_tls(app, shutdown_rx).await?;
                (addr, Some(ca))
            }
        };

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::TcpStream::connect(addr).await.is_err() {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("mock server not ready within 5s");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        Ok(MockInferenceServer {
            addr,
            shutdown_tx: Some(shutdown_tx),
            state,
            tls_ca,
        })
    }

    pub fn set_models(&self, models: Vec<MockModelEntry>) {
        let mut guard = self.state.models.write().unwrap();
        *guard = models.iter().map(MockModelEntry::to_json).collect();
    }

    /// Stream this text instead of echoing. Deltas reconstruct it byte for byte.
    pub fn set_response(&self, text: impl Into<String>) {
        self.state.inference.set_response(text.into());
    }

    /// Consumed FIFO per `path`, e.g. `"/v1/chat/completions"`.
    /// An empty queue falls back to the response mode.
    pub fn enqueue_response(&self, path: impl Into<String>, response: ScriptedResponse) {
        self.state.overrides.enqueue_response(path, response);
    }

    /// Default-responder concurrency cap: over `cap` in-flight (each held for `hold`), extra requests get 429 with `Retry-After`.
    /// Scripted responses and expectations bypass the cap.
    pub fn set_inference_concurrency_cap(&self, cap: usize, hold: Duration, retry_after_secs: u64) {
        self.state
            .overrides
            .set_concurrency_cap(cap, hold, retry_after_secs);
    }

    /// Register one named response matched atomically by endpoint and request kind.
    #[must_use = "keep the handle to synchronize and assert expectation satisfaction"]
    pub fn expect_response(
        &self,
        name: impl Into<String>,
        matcher: InferenceRequestMatcher,
        response: ScriptedResponse,
    ) -> InferenceExpectation {
        self.state.overrides.register_expectation(
            name, matcher, response, /*block_before_terminal*/ false,
        )
    }

    /// Register a named response that pauses immediately before completion.
    #[must_use = "keep the handle to release and assert expectation satisfaction"]
    pub fn expect_response_blocked(
        &self,
        name: impl Into<String>,
        matcher: InferenceRequestMatcher,
        response: ScriptedResponse,
    ) -> InferenceExpectation {
        self.state
            .overrides
            .register_expectation(name, matcher, response, /*block_before_terminal*/ true)
    }

    /// Queue one byte-exact response per foreground turn.
    pub fn set_agent_turns(&self, turns: impl IntoIterator<Item = String>) {
        self.state
            .inference
            .set_agent_turns(turns.into_iter().collect());
    }

    /// Replaces earlier conversations; one not served through stays reported.
    pub fn set_conversations(&self, conversations: impl IntoIterator<Item = Conversation>) {
        self.state
            .overrides
            .conversation_scripts()
            .set(conversations);
    }

    /// Each violation once: those recorded, then every script not served through or never opened.
    #[must_use]
    pub fn script_violations(&self) -> Vec<ScriptViolation> {
        self.state.overrides.conversation_scripts().violations()
    }

    /// Later requests fall through to the fallback modes; the drop check has nothing left to report.
    #[must_use = "assert on the violations; finishing disarms the drop check"]
    pub fn finish_scripts(&self) -> Vec<ScriptViolation> {
        self.state.overrides.conversation_scripts().finish()
    }

    /// A violation serves a reply instead of failing the request, so this also runs on drop.
    #[track_caller]
    pub fn assert_no_script_violations(&self) {
        let violations = self.script_violations();
        assert!(
            violations.is_empty(),
            "conversation scripts departed from:\n{}\nconversations:\n{}\nrequests:\n{}\n\
             a test that returned early through `?` has its error replaced by this panic; \
             call finish_scripts() before returning",
            lines(&violations),
            lines(&self.conversations()),
            self.request_log_summary()
        );
    }

    #[must_use]
    pub fn conversations(&self) -> Vec<ReadConversation> {
        self.state.inference.conversations()
    }

    /// `None` until the mock has opened that many conversations.
    #[must_use]
    pub fn conversation(&self, number: usize) -> Option<ReadConversation> {
        self.conversations()
            .into_iter()
            .find(|conversation| conversation.number() == number)
    }

    #[must_use]
    pub fn conversation_for_system_prompt(&self, contains: &str) -> Option<ReadConversation> {
        self.conversations().into_iter().find(|conversation| {
            conversation
                .first_system_prompt()
                .is_some_and(|prompt| prompt.contains(contains))
        })
    }

    /// Until this is called, `GET /v1/settings` returns 404.
    pub fn set_settings(&self, settings: impl serde::Serialize) {
        let value = serde_json::to_value(settings).expect("serialize settings");
        let mut guard = self.state.settings.write().unwrap();
        *guard = Some(value);
    }

    /// The smallest settings payload that opens the subscription gate.
    /// Without it a client sits on the upsell screen.
    pub fn preset_allow_access(&self) {
        self.set_settings(json!({ "allow_access": true }));
    }

    /// Stand in for a black-holed backend.
    pub fn set_hang(&self, hang: bool) {
        *self.state.startup_fetch_stall.write().unwrap() = if hang {
            StartupFetchStall::Hang
        } else {
            StartupFetchStall::None
        };
    }

    pub fn set_startup_fetch_delay(&self, delay: Duration) {
        *self.state.startup_fetch_stall.write().unwrap() = StartupFetchStall::Delay(delay);
    }

    pub fn clear_startup_fetch_stall(&self) {
        *self.state.startup_fetch_stall.write().unwrap() = StartupFetchStall::None;
    }

    pub fn startup_stalls_served(&self) -> u32 {
        self.state.startup_stalls_served.load(Ordering::Relaxed)
    }

    /// The `subscriptionTier` on `GET /v1/user`.
    /// `None`, the default, omits the field, which the shell reads as the free tier.
    pub fn set_user_subscription_tier(&self, tier: Option<&str>) {
        *self.state.user_tier.write().unwrap() = tier.map(str::to_owned);
    }

    /// Defaults to `"end_turn"`.
    pub fn set_messages_stop_reason(&self, stop_reason: impl Into<String>) {
        self.state
            .inference
            .set_messages_stop_reason(stop_reason.into());
    }

    /// Emit each SSE event after `delay`, so a test can hold a turn visibly streaming.
    /// `None` restores instant streaming.
    /// Applies to requests started after the call.
    pub fn set_chunk_delay(&self, delay: Option<Duration>) {
        self.state.inference.set_chunk_delay(delay);
    }

    /// Hold foreground terminal SSE events until [`Self::release_agent_completions`].
    /// Prefer per-expectation blocking.
    pub fn hold_agent_completions(&self) {
        self.state.overrides.hold_completions();
    }

    /// Let held and future agent turns emit their terminal event.
    pub fn release_agent_completions(&self) {
        self.state.overrides.release_completions();
    }

    /// e.g. `http://127.0.0.1:12345/v1` (`https://` for [`Self::start_tls`])
    pub fn url(&self) -> String {
        format!("{}://{}/v1", self.scheme(), self.addr)
    }

    /// Scheme and host without the `/v1` inference prefix (`http://127.0.0.1:PORT`, or `https://`
    /// for [`Self::start_tls`]).
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme(), self.addr)
    }

    fn scheme(&self) -> &'static str {
        if self.tls_ca.is_some() {
            "https"
        } else {
            "http"
        }
    }

    /// Path to the throwaway CA PEM a client must trust; `None` for a plain-HTTP server.
    pub fn ca_pem_path(&self) -> Option<&Path> {
        self.tls_ca.as_ref().map(ThrowawayCa::pem_path)
    }

    pub fn request_count(&self) -> u32 {
        self.state.log.count()
    }

    pub fn request_count_for(&self, path: &str) -> usize {
        self.state.log.count_for(path)
    }

    /// Stop retaining entries. [`Self::request_count`] stays exact.
    pub fn set_keep_requests(&self, enabled: bool) {
        self.state.log.set_keep_entries(enabled);
    }

    pub fn requests(&self) -> Vec<LogEntry> {
        self.state.log.entries()
    }

    /// Bodies of all received requests, in arrival order (body-less requests such as `GET /v1/models` are skipped).
    pub fn request_bodies(&self) -> Vec<Value> {
        self.state.log.bodies()
    }

    pub fn has_chat_completion_request(&self) -> bool {
        self.state.log.has_path_containing("chat/completions")
    }

    pub fn has_responses_request(&self) -> bool {
        self.state.log.has_path_containing("responses")
    }

    /// Number of `POST /v1/messages` requests received so far.
    pub fn messages_request_count(&self) -> usize {
        self.state.log.count_for("/v1/messages")
    }

    /// Format the request log for diagnostic output on test failures.
    pub fn request_log_summary(&self) -> String {
        self.state.log.summary()
    }

    /// Get the system prompt from the most recent inference request.
    pub fn last_system_prompt(&self) -> Option<String> {
        self.state.log.last_system_prompt()
    }

    /// While closed, every `/v1/storage` upload is rejected with 401.
    pub fn set_storage_unauthorized(&self, unauthorized: bool) {
        self.state.storage.set_unauthorized(unauthorized);
    }

    /// Total `/v1/storage` upload attempts seen, including 401-rejected ones.
    pub fn storage_request_count(&self) -> u32 {
        self.state.storage.request_count()
    }

    /// Only the uploads that were accepted.
    pub fn storage_uploads(&self) -> Vec<StorageUpload> {
        self.state.storage.uploads()
    }

    /// While set, every `POST /v1/feedback` answers 500 (the body is still recorded).
    pub fn set_feedback_failure(&self, fail: bool) {
        self.state.feedback.set_failure(fail);
    }

    /// Every `POST /v1/feedback` seen so far, accepted or scripted to fail, in arrival order.
    pub fn feedback_posts(&self) -> Vec<FeedbackPost> {
        self.state.feedback.posts()
    }

    /// Every product-telemetry event posted to `/v1/events` so far, flattened out of its batch, in arrival order.
    pub fn telemetry_events(&self) -> Vec<Value> {
        self.state.telemetry.events()
    }

    fn build_router(state: RouterState) -> Router {
        Router::new()
            .route(
                InferenceEndpoint::ChatCompletions.path(),
                state.inference.handler(InferenceEndpoint::ChatCompletions),
            )
            .route(
                InferenceEndpoint::Responses.path(),
                state.inference.handler(InferenceEndpoint::Responses),
            )
            .route(
                InferenceEndpoint::Messages.path(),
                state.inference.handler(InferenceEndpoint::Messages),
            )
            .route(
                "/v1/models",
                get({
                    let state = state.clone();
                    move || {
                        let state = state.clone();
                        async move {
                            state.log.record_get("/v1/models");
                            state.stall_startup_fetch().await;
                            let models_json = state.models.read().unwrap().clone();
                            Json(json!({
                                "object": "list",
                                "data": models_json,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/v1/settings",
                get({
                    let state = state.clone();
                    move || {
                        let state = state.clone();
                        async move {
                            state.log.record_get("/v1/settings");
                            state.stall_startup_fetch().await;
                            // Scripts take precedence, so a test can serve a transient payload before the steady-state value
                            if let Some(s) = state.overrides.pop_scripted("/v1/settings") {
                                return s.into_response_paced(None, None).await;
                            }
                            let maybe = state.settings.read().unwrap().clone();
                            match maybe {
                                Some(s) => Json(s).into_response(),
                                None => StatusCode::NOT_FOUND.into_response(),
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1/privacy/coding-data-retention",
                put({
                    let state = state.clone();
                    move |headers: HeaderMap, Json(body): Json<Value>| {
                        let state = state.clone();
                        async move {
                            let path = "/v1/privacy/coding-data-retention";
                            state.log.record("PUT", path, &body, &headers);
                            // Lets a test refuse the write
                            if let Some(s) = state.overrides.pop_scripted(path) {
                                return s.into_response_paced(None, None).await;
                            }
                            // Echo the received flag back like the real cli-chat-proxy does on success
                            let opt_out = body
                                .get("codingDataRetentionOptOut")
                                .cloned()
                                .unwrap_or(Value::Bool(false));
                            Json(json!({ "codingDataRetentionOptOut": opt_out })).into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/user",
                get({
                    let state = state.clone();
                    move |axum::extract::RawQuery(query): axum::extract::RawQuery| {
                        let state = state.clone();
                        async move {
                            // Log the query string so a test can count `?include=subscription` on its own
                            let path = match query {
                                Some(q) if !q.is_empty() => format!("/v1/user?{q}"),
                                _ => "/v1/user".to_owned(),
                            };
                            state.log.record_get(&path);
                            let tier = state.user_tier.read().unwrap().clone();
                            let mut body = json!({
                                "userId": "mock-user",
                                "email": "mock-user@test.invalid",
                            });
                            if let Some(t) = tier
                                && let Some(obj) = body.as_object_mut()
                            {
                                obj.insert("subscriptionTier".into(), json!(t));
                            }
                            Json(body).into_response()
                        }
                    }
                }),
            )
            .route(
                "/sessions/{id}/data",
                post({
                    let state = state.clone();
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let state = state.clone();
                        async move {
                            if let Some(reject) = state.overrides.auth_rejection(&headers) {
                                return reject;
                            }
                            state.log.record(
                                "POST",
                                &format!("/sessions/{id}/data"),
                                &body,
                                &headers,
                            );
                            StatusCode::OK.into_response()
                        }
                    }
                }),
            )
            .route(
                "/sessions/{id}",
                put({
                    let state = state.clone();
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let state = state.clone();
                        async move {
                            if let Some(reject) = state.overrides.auth_rejection(&headers) {
                                return reject;
                            }
                            state
                                .log
                                .record("PUT", &format!("/sessions/{id}"), &body, &headers);
                            StatusCode::OK.into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/storage",
                post({
                    let storage = state.storage.clone();
                    move |headers: HeaderMap, body: axum::body::Bytes| {
                        let storage = storage.clone();
                        async move { storage.handle(&headers, &body) }
                    }
                }),
            )
            // The shell POSTs `{GROK_FEEDBACK_BASE_URL}/feedback`, and the sandbox points that base at `url()` (which ends in `/v1`)
            // `/v1/feedback/{config,requests}` are deliberately unrouted: the shell treats their 404 as an old proxy
            .route(
                "/v1/feedback",
                post({
                    let feedback = state.feedback.clone();
                    move |headers: HeaderMap, body: axum::body::Bytes| {
                        let feedback = feedback.clone();
                        async move { feedback.handle(&headers, &body) }
                    }
                }),
            )
            // Product telemetry POSTs `GROK_TELEMETRY_EVENTS_URL` verbatim; tests point it at `{url()}/events`
            .route(
                "/v1/events",
                post({
                    let telemetry = state.telemetry.clone();
                    move |body: axum::body::Bytes| {
                        let telemetry = telemetry.clone();
                        async move { telemetry.handle(&body) }
                    }
                }),
            )
            // 404 reads as an old proxy, so the shell falls back to a plain `POST /v1/storage`
            .route(
                "/v1/storage/exists",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_exists",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_upload_json",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_upload",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/limits",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            // Body limit: repo-context archives can exceed axum's 2 MB default.
            .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
    }
}

fn lines<T: fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Drop for MockInferenceServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if !std::thread::panicking() {
            self.assert_no_script_violations();
        }
    }
}

#[cfg(test)]
#[path = "mock_server_tests.rs"]
mod tests;
