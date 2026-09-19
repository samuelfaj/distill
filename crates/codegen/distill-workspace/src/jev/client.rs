// Modified for Distill by Samuel Fajreldines, 2026.
//! Minimal Jev HTTP client: one attempt, caller-owned deadline, no body or
//! credential logging.
//!
//! The client is deliberately thin (plan §12.5): it builds the request body
//! from typed questions, sends it with a bearer token resolved *at call time*
//! from a configured environment variable, reads the response body **inside**
//! the deadline (a slow body is a timeout, not a transport error), maps
//! non-2xx onto [`JevError`], and validates that every question came back with
//! an answer of the right type. It never retries and never logs bodies.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::error::{JevError, JevErrorKind};
use super::provider::{ChatReply, JevProvider, ReasoningShape, chat_request_body, parse_chat_reply};
use super::types::{
    JevAnswerSet, Json, Question, QuestionId, SystemOneRequest, SystemOneResponse, Usage,
};

/// Default endpoint root; overridable per config (never from project config).
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// Default model alias; pin a version when thresholds are calibrated (item 89).
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Per-operation deadline (plan §12.5: 10 s, and it must cover the body read).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Environment variable consulted when no resolver is injected.
pub const DEFAULT_API_KEY_ENV: &str = "JEV_API_KEY";
/// State ceiling for one request (Jev's own budget is 32k tokens for state +
/// longest question; we stop far below it so a request is cheap and focused).
pub const DEFAULT_MAX_STATE_BYTES: usize = 32 * 1024;
/// Completion ceiling for a chat-completions backend: a typed answer is a small
/// JSON object, and a lower ceiling is what keeps a decision call cheap.
pub const DEFAULT_MAX_COMPLETION_TOKENS: u32 = 2_048;
/// Deadline for one catalogue item call, before the caller's own resolution.
pub const DEFAULT_ITEM_BUDGET: Duration = Duration::from_millis(4_000);
/// Thinking level a decision call asks for when the configuration does not say.
/// Decision calls are the cheap lane: they must answer fast and cost little, and
/// a lever's own threshold decides what to do with a low-confidence answer.
pub const DEFAULT_REASONING_EFFORT: &str = "low";

/// Resolves the bearer token by environment variable name at call time.
pub type ApiKeyResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Client configuration. `enabled`/endpoint are resolved by the caller from
/// managed/env/user layers only; a project config must not be able to turn this
/// on or repoint it (plan §1.6/I-6).
#[derive(Debug, Clone)]
pub struct JevClientConfig {
    pub base_url: String,
    pub model: String,
    pub timeout: Duration,
    pub api_key_env: String,
    pub max_state_bytes: usize,
    /// Which wire protocol the decision layer speaks (TypeSafe System One, or an
    /// OpenAI-compatible chat endpoint such as OpenRouter).
    pub provider: JevProvider,
    /// How the configured model wants to be told how much to think.
    pub reasoning_shape: ReasoningShape,
    /// Thinking level asked for on the decision call itself (`none` disables).
    pub reasoning_effort: String,
    /// Completion ceiling sent to a chat-completions backend.
    pub max_completion_tokens: u32,
    /// Deadline for one catalogue item call (a lever's own batch), never longer
    /// than `timeout`. The permission actor uses `timeout`; catalogue items sit
    /// on the tool-result path, where the answer is worth less than the wait.
    pub item_budget: Duration,
}

impl Default for JevClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
            api_key_env: DEFAULT_API_KEY_ENV.to_owned(),
            max_state_bytes: DEFAULT_MAX_STATE_BYTES,
            provider: JevProvider::default(),
            reasoning_shape: ReasoningShape::default(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_owned(),
            max_completion_tokens: DEFAULT_MAX_COMPLETION_TOKENS,
            item_budget: DEFAULT_ITEM_BUDGET,
        }
    }
}

impl JevClientConfig {
    /// The endpoint used for one call, with any trailing slash removed.
    pub fn endpoint(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.provider.path()
        )
    }
}

/// One configured client. Construction performs no network I/O.
pub struct JevClient {
    http: reqwest::Client,
    config: JevClientConfig,
    key_resolver: ApiKeyResolver,
}

fn env_key_resolver() -> ApiKeyResolver {
    Arc::new(|name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

impl JevClient {
    /// Builds a client through the workspace-sanctioned TLS policy builder.
    pub fn new(config: JevClientConfig) -> Result<Self, JevError> {
        Self::with_key_resolver(config, env_key_resolver())
    }

    /// Test seam: injects the credential resolver instead of reading the process
    /// environment (keeps tests hermetic and free of global env mutation).
    pub fn with_key_resolver(
        config: JevClientConfig,
        key_resolver: ApiKeyResolver,
    ) -> Result<Self, JevError> {
        let http = distill_extra_ca::build_reqwest_client(|builder| {
            builder
                .connect_timeout(Duration::from_secs(5))
                .pool_idle_timeout(Duration::from_secs(30))
                .user_agent("grok-jev-client")
        })
        .map_err(|e| JevError::transport(format!("http client build failed: {e}")))?;
        Ok(Self {
            http,
            config,
            key_resolver,
        })
    }

    pub fn config(&self) -> &JevClientConfig {
        &self.config
    }

    /// Whether the configured credential is currently resolvable (never leaks it).
    pub fn credential_present(&self) -> bool {
        (self.key_resolver)(&self.config.api_key_env).is_some()
    }

    /// Sends one speculative battery: all questions travel in a single request
    /// (plan item 96 — batching is ~12x cheaper than one call per question).
    pub async fn ask(
        &self,
        state: &Json,
        questions: &BTreeMap<QuestionId, Question>,
    ) -> Result<JevAnswerSet, JevError> {
        if questions.is_empty() {
            return Err(JevError::invalid("questions must not be empty"));
        }
        for (id, question) in questions {
            question
                .validate()
                .map_err(|e| JevError::invalid(format!("question `{id}`: {}", e.detail())))?;
        }

        let state_bytes = serde_json::to_vec(state)
            .map_err(|e| JevError::invalid(format!("state serialization failed: {e}")))?
            .len();
        if state_bytes > self.config.max_state_bytes {
            return Err(JevError::invalid(format!(
                "state is {state_bytes} bytes, over the {} byte ceiling for one request",
                self.config.max_state_bytes
            )));
        }

        let secret = (self.key_resolver)(&self.config.api_key_env).ok_or_else(|| {
            JevError::invalid(format!(
                "credential env `{}` is unset or empty",
                self.config.api_key_env
            ))
        })?;

        let body = self.request_body(state, questions)?;

        let started = Instant::now();
        let deadline = self.config.timeout;
        let http = self.http.clone();
        let url = self.config.endpoint();

        // One attempt; the deadline covers connection, response *and body read*.
        let attempt = async {
            let response = http
                .post(&url)
                .bearer_auth(&secret)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| JevError::transport(format!("request failed: {e}")))?;
            let status = response.status();
            let retry_after_ms = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_ms);
            let request_id = response
                .headers()
                .get("x-typesafe-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = response
                .bytes()
                .await
                .map_err(|e| JevError::transport(format!("response body failed: {e}")))?;
            Ok::<_, JevError>((status, request_id, retry_after_ms, bytes))
        };

        let (status, request_id, retry_after_ms, bytes) =
            match tokio::time::timeout(deadline, attempt).await {
                Err(_) => {
                    return Err(JevError::timeout(format!(
                        "no complete response within {} ms",
                        deadline.as_millis()
                    ))
                    .redact(&secret));
                }
                Ok(Err(err)) => return Err(err.redact(&secret)),
                Ok(Ok(parts)) => parts,
            };
        let latency_ms = started.elapsed().as_millis() as u64;

        if !status.is_success() {
            let parsed: Option<Json> = serde_json::from_slice(&bytes).ok();
            return Err(JevError::from_status(
                status.as_u16(),
                parsed.as_ref(),
                request_id,
                retry_after_ms,
            )
            .redact(&secret));
        }

        self.parse_reply(&bytes, questions, request_id, latency_ms)
            .map_err(|error| error.redact(&secret))
    }

    /// One request body for the configured provider: the System One envelope, or
    /// the chat-completions rendering of the same typed battery.
    fn request_body(
        &self,
        state: &Json,
        questions: &BTreeMap<QuestionId, Question>,
    ) -> Result<Vec<u8>, JevError> {
        // Both TypeSafe hosts take the typed envelope as it is; only a chat
        // backend needs the questions rendered into a prompt.
        if self.config.provider.speaks_typed_envelope() {
            return serde_json::to_vec(&SystemOneRequest {
                state: state.clone(),
                model: self.config.model.clone(),
                questions: questions.clone(),
            })
            .map_err(|e| JevError::invalid(format!("request serialization failed: {e}")));
        }
        let body = chat_request_body(
            state,
            questions,
            &self.config.model,
            self.config.reasoning_shape,
            &self.config.reasoning_effort,
            self.config.max_completion_tokens,
        )
        .and_then(|body| {
            serde_json::to_vec(&body)
                .map_err(|e| JevError::invalid(format!("request serialization failed: {e}")))
        })?;
        Ok(body)
    }

    /// Turns a 200 body into typed answers, per provider.
    ///
    /// Both halves end on the same contract: a question that came back without a
    /// usable answer is `Invalid`, which the caller treats as fail-defer.
    fn parse_reply(
        &self,
        bytes: &[u8],
        questions: &BTreeMap<QuestionId, Question>,
        request_id: Option<String>,
        latency_ms: u64,
    ) -> Result<JevAnswerSet, JevError> {
        if !self.config.provider.speaks_typed_envelope() {
            let ChatReply {
                model,
                id,
                content,
                usage,
                truncated,
            } = parse_chat_reply(bytes)?;
            if truncated {
                return Err(JevError::invalid(
                    "answer was cut at the completion ceiling, so it cannot be read",
                ));
            }
            let answers = super::provider::parse_decision_answers(&content, questions)?;
            return Ok(JevAnswerSet {
                model: model.unwrap_or_else(|| self.config.model.clone()),
                answers,
                usage,
                request_id: id.or(request_id),
                latency_ms,
            });
        }

        let parsed: SystemOneResponse = serde_json::from_slice(bytes)
            .map_err(|e| JevError::invalid(format!("malformed 200 response: {e}")))?;

        for (id, question) in questions {
            match (parsed.answers.get(id), question) {
                (None, _) => {
                    return Err(JevError::invalid(format!("missing answer for `{id}`")));
                }
                (Some(answer), Question::Noul { .. }) if answer.kind() != "noul" => {
                    return Err(JevError::invalid(format!(
                        "answer `{id}` is a {} but a noul question was asked",
                        answer.kind()
                    )));
                }
                (Some(answer), Question::Choice { .. }) if answer.kind() != "choice" => {
                    return Err(JevError::invalid(format!(
                        "answer `{id}` is a {} but a choice question was asked",
                        answer.kind()
                    )));
                }
                (Some(answer), Question::Score { .. }) if answer.kind() != "score" => {
                    return Err(JevError::invalid(format!(
                        "answer `{id}` is a {} but a score question was asked",
                        answer.kind()
                    )));
                }
                (Some(_), _) => {}
            }
        }

        Ok(JevAnswerSet {
            model: parsed.model,
            answers: parsed.answers,
            usage: parsed.usage.unwrap_or(Usage::default()),
            // The decisions endpoint reports its id in the body, the direct
            // service in a header: whichever arrived names the call.
            request_id: request_id.or(parsed.id),
            latency_ms,
        })
    }
}

/// Whether the named environment variable currently holds a non-empty value.
/// Used by the TUI badge; it never returns or logs the value itself.
pub fn credential_in_env(env_name: &str) -> bool {
    std::env::var(env_name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Parses `Retry-After` as either delta-seconds or milliseconds spellings used
/// by the vendor (`retry-after-ms` values arrive pre-labeled by the caller).
fn parse_retry_after_ms(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    None
}

/// True when the transport-level error kind is worth a log line at debug level.
pub fn is_transport_failure(err: &JevError) -> bool {
    matches!(err.kind(), JevErrorKind::Transport | JevErrorKind::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;

    const TEST_KEY: &str = "apikey_test_sentinel_do_not_leak";

    #[derive(Clone)]
    enum StubReply {
        Json(u16, String),
        SleepThenJson(u64, String),
    }

    #[derive(Clone)]
    struct Stub {
        reply: Arc<Mutex<StubReply>>,
        hits: Arc<Mutex<Vec<()>>>,
        seen_auth: Arc<Mutex<Vec<String>>>,
        seen_body: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        fn new(reply: StubReply) -> Self {
            Self {
                reply: Arc::new(Mutex::new(reply)),
                hits: Arc::new(Mutex::new(Vec::new())),
                seen_auth: Arc::new(Mutex::new(Vec::new())),
                seen_body: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn hits(&self) -> usize {
            self.hits.lock().expect("stub lock").len()
        }

        fn last_body(&self) -> String {
            self.seen_body
                .lock()
                .expect("stub lock")
                .last()
                .cloned()
                .unwrap_or_default()
        }

        fn last_auth(&self) -> String {
            self.seen_auth
                .lock()
                .expect("stub lock")
                .last()
                .cloned()
                .unwrap_or_default()
        }

        fn set(&self, reply: StubReply) {
            *self.reply.lock().expect("stub lock") = reply;
        }
    }

    async fn handler(
        State(stub): State<Stub>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> impl IntoResponse {
        stub.hits.lock().expect("stub lock").push(());
        stub.seen_auth.lock().expect("stub lock").push(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        );
        stub.seen_body
            .lock()
            .expect("stub lock")
            .push(String::from_utf8_lossy(&body).to_string());
        let reply = stub.reply.lock().expect("stub lock").clone();
        match reply {
            StubReply::Json(status, body) => (
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                [
                    ("content-type", "application/json"),
                    ("x-typesafe-request-id", "req-stub-1"),
                ],
                body,
            ),
            StubReply::SleepThenJson(ms, body) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("x-typesafe-request-id", "req-stub-slow"),
                    ],
                    body,
                )
            }
        }
    }

    async fn spawn_stub(stub: Stub) -> String {
        let app = Router::new()
            .route("/v1/systemone", post(handler))
            .route("/alpha/decisions", post(handler))
            .route("/chat/completions", post(handler))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn test_client(base_url: String, timeout: Duration) -> JevClient {
        let config = JevClientConfig {
            base_url,
            timeout,
            ..JevClientConfig::default()
        };
        let resolver: ApiKeyResolver = Arc::new(|_| Some(TEST_KEY.to_owned()));
        JevClient::with_key_resolver(config, resolver).expect("client builds")
    }

    fn ok_body() -> String {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "escapes": {"type": "noul", "noul": 0.15},
                "risk": {"type": "choice", "choice": "routine_build", "confidence": 0.9,
                          "probabilities": {"routine_build": 0.9, "destructive": 0.05, "mutating_local": 0.05}},
                "severity": {"type": "score", "score": 0.4, "confidence": 0.8,
                              "legend": {"0": "none", "1": "minor", "2": "serious"},
                              "probabilities": {"0": 0.6, "1": 0.4, "2": 0.0}}
            },
            "usage": {"input_tokens": 321, "output_tokens": 44}
        })
        .to_string()
    }

    fn sample_questions() -> BTreeMap<QuestionId, Question> {
        let mut questions = BTreeMap::new();
        questions.insert(
            "risk".to_owned(),
            Question::choice(
                "What is the risk class of this command?",
                [
                    ("routine_build".to_owned(), Json::Null),
                    ("mutating_local".to_owned(), Json::Null),
                    ("destructive".to_owned(), Json::Null),
                ]
                .into_iter()
                .collect(),
            )
            .expect("valid choice"),
        );
        questions.insert(
            "escapes".to_owned(),
            Question::noul("Does this action write or delete outside the workspace root?"),
        );
        questions.insert(
            "severity".to_owned(),
            Question::score(
                "How severe would the damage be if this decision were wrong?",
                vec![
                    Json::from("No damage"),
                    Json::from("Minor, recoverable local damage"),
                    Json::from("Serious: data loss or unrecoverable state"),
                ],
            )
            .expect("valid score"),
        );
        questions
    }

    #[tokio::test]
    async fn golden_request_body_matches_the_contract() {
        let stub = Stub::new(StubReply::Json(200, ok_body()));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_secs(5));

        let state = serde_json::json!({
            "proposed_action": {"tool": "bash", "command": "cargo check -p distill-workspace"},
        });
        let answers = client
            .ask(&state, &sample_questions())
            .await
            .expect("call succeeds");

        // Typed answers came back and were validated.
        assert_eq!(answers.choice("risk"), Some("routine_build"));
        assert_eq!(answers.noul("escapes"), Some(0.15));
        assert!((answers.score_normalized("severity").expect("score") - 0.2).abs() < 1e-9);
        assert_eq!(answers.usage.input(), 321);
        assert_eq!(answers.usage.output(), 44);
        assert_eq!(answers.request_id.as_deref(), Some("req-stub-1"));
        assert_eq!(answers.model, "jev-1.13.0");

        // Wire shape (§12.5): state/model/questions with type tags and criteria.
        let body: Json = serde_json::from_str(&stub.last_body()).expect("stub saw JSON");
        assert!(body["state"]["proposed_action"]["command"].is_string());
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["questions"]["risk"]["type"], "choice");
        assert!(body["questions"]["risk"]["criteria"].is_object());
        assert_eq!(body["questions"]["escapes"]["type"], "noul");
        assert!(body["questions"]["escapes"].get("criteria").is_none());
        assert_eq!(body["questions"]["severity"]["type"], "score");
        assert_eq!(
            body["questions"]["severity"]["criteria"]
                .as_array()
                .expect("array rubric")
                .len(),
            3
        );
        // The bearer token travelled in the header, never in the body.
        assert!(stub.last_auth().starts_with("Bearer "));
        assert!(!stub.last_body().contains(TEST_KEY));
    }

    /// The Jev model on OpenRouter is the same contract behind another URL: the
    /// shipped client posts the typed envelope to `/alpha/decisions` and reads
    /// the provider's own completion id out of the body.
    #[tokio::test]
    async fn the_decisions_provider_posts_the_envelope_and_reads_the_body_id() {
        let body = serde_json::json!({
            "model": "typesafe/jev-1.13-20260917",
            "answers": {
                "escapes": {"type": "noul", "noul": 0.17},
                "risk": {"type": "choice", "choice": "routine_build", "confidence": 0.99,
                          "probabilities": {"routine_build": 1.0, "mutating_local": 0.0,
                                            "destructive": 0.0}},
                "severity": {"type": "score", "score": 0.23, "confidence": 0.65,
                              "legend": {"0": "No damage", "1": "Minor",
                                          "2": "Data loss"},
                              "probabilities": {"0": 0.77, "1": 0.23, "2": 0.0}}
            },
            "usage": {"input_tokens": 393, "output_tokens": 77},
            "id": "gen-dec-1789771460-aYLoYIO7TRHU0lewVP1H",
            "provider": "TypeSafe"
        })
        .to_string();
        let stub = Stub::new(StubReply::Json(200, body));
        let base = spawn_stub(stub.clone()).await;
        let config = JevClientConfig {
            base_url: format!("{base}/"),
            model: "~typesafe/jev-latest".to_owned(),
            provider: JevProvider::OpenRouterDecisions,
            ..JevClientConfig::default()
        };
        assert_eq!(
            config.endpoint(),
            format!("{base}/alpha/decisions"),
            "the base's trailing slash does not double up"
        );
        let resolver: ApiKeyResolver = Arc::new(|_| Some(TEST_KEY.to_owned()));
        let client = JevClient::with_key_resolver(config, resolver).expect("client builds");
        let answers = client
            .ask(
                &serde_json::json!({"proposed_action": {"tool": "bash"}}),
                &sample_questions(),
            )
            .await
            .expect("the decisions endpoint answers the typed contract");

        assert_eq!(answers.choice("risk"), Some("routine_build"));
        assert_eq!(answers.noul("escapes"), Some(0.17));
        assert_eq!(answers.model, "typesafe/jev-1.13-20260917");
        // A host that sends the request-id header wins; the body id is the
        // fallback for the host that does not (the live decisions run asserts
        // that side, and `SystemOneResponse` is tested for the field itself).
        assert_eq!(answers.request_id.as_deref(), Some("req-stub-1"));
        assert_eq!(answers.usage.input(), 393);

        // The envelope travelled as the contract spells it, not as a prompt.
        let sent: Json = serde_json::from_str(&stub.last_body()).expect("JSON body");
        assert!(sent["state"]["proposed_action"]["tool"].is_string());
        assert_eq!(sent["model"], "~typesafe/jev-latest");
        assert_eq!(sent["questions"]["risk"]["type"], "choice");
        assert!(
            sent.get("messages").is_none(),
            "a typed-envelope host is never sent a chat prompt"
        );
    }

    #[tokio::test]
    async fn status_codes_map_onto_the_taxonomy() {
        let stub = Stub::new(StubReply::Json(200, ok_body()));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_secs(5));
        let state = serde_json::json!({"a": 1});

        for (status, kind) in [
            (400u16, JevErrorKind::Invalid),
            (401, JevErrorKind::Invalid),
            (403, JevErrorKind::Invalid),
            (404, JevErrorKind::Invalid),
            (422, JevErrorKind::Invalid),
            (429, JevErrorKind::RateLimited),
            (500, JevErrorKind::Unavailable),
            (529, JevErrorKind::Unavailable),
        ] {
            stub.set(StubReply::Json(
                status,
                serde_json::json!({"error": {"field_path": "questions.risk.type"}}).to_string(),
            ));
            let err = client
                .ask(&state, &sample_questions())
                .await
                .expect_err("non-2xx must fail");
            assert_eq!(err.kind(), kind, "status {status}");
            assert!(!err.is_retryable(), "hot path never retries");
            if status == 422 {
                assert_eq!(err.field_path(), Some("questions.risk.type"));
            }
        }
    }

    #[tokio::test]
    async fn a_slow_body_is_a_timeout_not_a_transport_error() {
        let stub = Stub::new(StubReply::SleepThenJson(600, ok_body()));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_millis(150));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("deadline must fire");
        assert_eq!(err.kind(), JevErrorKind::Timeout);
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn malformed_200_and_missing_answers_are_invalid() {
        let stub = Stub::new(StubReply::Json(200, "{\"model\":\"m\"}".to_owned()));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_secs(5));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("missing answers must fail");
        assert_eq!(err.kind(), JevErrorKind::Invalid);

        stub.set(StubReply::Json(200, ok_body()));
        let mut questions = sample_questions();
        questions.insert("extra".to_owned(), Question::noul("extra question"));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &questions)
            .await
            .expect_err("unanswered question must fail");
        assert_eq!(err.kind(), JevErrorKind::Invalid);
        assert!(err.detail().contains("extra"));
    }

    #[tokio::test]
    async fn answers_of_the_wrong_type_are_rejected() {
        let swapped = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "escapes": {"type": "noul", "noul": 0.1},
                "risk": {"type": "noul", "noul": 0.9},
                "severity": {"type": "score", "score": 0.0, "legend": {}, "probabilities": {}},
            },
        })
        .to_string();
        let stub = Stub::new(StubReply::Json(200, swapped));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_secs(5));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("type mismatch must fail");
        assert_eq!(err.kind(), JevErrorKind::Invalid);
        assert!(err.detail().contains("risk"));
    }

    #[tokio::test]
    async fn transport_failure_when_nothing_listens() {
        // Port 1 is reserved and never serves; a refused connection is transport.
        let client = test_client("http://127.0.0.1:1".to_owned(), Duration::from_secs(2));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("connection must fail");
        assert_eq!(err.kind(), JevErrorKind::Transport);
    }

    #[tokio::test]
    async fn a_leaky_server_cannot_put_the_key_in_our_error() {
        let stub = Stub::new(StubReply::Json(
            401,
            serde_json::json!({"error": {"message": format!("bad token {TEST_KEY}")}}).to_string(),
        ));
        let base = spawn_stub(stub.clone()).await;
        let client = test_client(base, Duration::from_secs(5));
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("401 must fail");
        let rendered = format!("{err} {err:?}");
        assert!(
            !rendered.contains(TEST_KEY),
            "credential leaked: {rendered}"
        );
    }

    #[tokio::test]
    async fn missing_credential_fails_before_any_io() {
        let stub = Stub::new(StubReply::Json(200, ok_body()));
        let base = spawn_stub(stub.clone()).await;
        let config = JevClientConfig {
            base_url: base,
            ..JevClientConfig::default()
        };
        let resolver: ApiKeyResolver = Arc::new(|_| None);
        let client = JevClient::with_key_resolver(config, resolver).expect("client builds");
        assert!(!client.credential_present());
        let err = client
            .ask(&serde_json::json!({"a": 1}), &sample_questions())
            .await
            .expect_err("no credential must fail");
        assert_eq!(err.kind(), JevErrorKind::Invalid);
        assert_eq!(stub.hits(), 0, "no request may leave without a credential");
    }

    #[tokio::test]
    async fn oversized_state_is_rejected_before_any_io() {
        let stub = Stub::new(StubReply::Json(200, ok_body()));
        let base = spawn_stub(stub.clone()).await;
        let mut client = test_client(base, Duration::from_secs(5));
        client.config.max_state_bytes = 32;
        let err = client
            .ask(
                &serde_json::json!({"blob": "x".repeat(200)}),
                &sample_questions(),
            )
            .await
            .expect_err("state ceiling must fail");
        assert_eq!(err.kind(), JevErrorKind::Invalid);
        assert_eq!(stub.hits(), 0, "oversized state must not be sent");
    }
}
