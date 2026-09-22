// Modified for Distill by Samuel Fajreldines, 2026.
//! HTTP client for the Grok sampling APIs.
//!
//! Owns the `reqwest::Client`, default request headers, and per-method defaults.
//! Talks to three backend shapes:
//!
//! * Chat Completions (`/chat/completions`)
//! * Responses API (`/responses`)
//! * Anthropic Messages API (`/messages`)
//!
//! All trace-upload and URL-based header injection is intentionally *not* here.
//! The session puts per-request headers (proxy auth, OTel context, etc.) into [`SamplerConfig::extra_headers`] before constructing the client.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use indexmap::IndexMap;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    USER_AGENT,
};
use serde::Serialize;
use tracing::Instrument;

use distill_sampling_types::error::{
    parse_error_code, try_parse_stream_error, user_facing_api_error_message,
};
use distill_sampling_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ConversationRequest,
    ConversationResponse, CreateResponseWrapper, DEFAULT_EXACT_REPETITION_MIN_TOKENS,
    DOOM_LOOP_CHECK_HEADER, EXACT_REPETITION_CHECK_HEADER, MessagesRequestWrapper, ReasoningEffort,
    ReasoningShape, ResponseModelMetadata, Result, SamplingError, SentCredential,
    build_messages_request, is_check_event, messages, rs,
};

use crate::config::{AuthScheme, OriginClientInfo, RequestCompression, SamplerConfig};
use crate::events::SamplingErrorInfo;
use crate::request_compression::{compress_body, should_compress};
use crate::span_timing::{ERROR, STATUS_CODE, SUCCESS, StreamSpanTiming};
use crate::stream_classify::{chat_chunk_class, message_event_class, responses_event_class};
use distill_auth::bearer_suffix;

pub use distill_sampling_types::ApiBackend;

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Product identifier baked into User-Agent strings.
const AGENT_PRODUCT: &str = "grok-shell";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 128_000;

/// Return the output reservation applied by the conversation adapters before
/// dispatch. Chat/Responses leave an unset provider default unknown; Messages
/// has the concrete Anthropic fallback. An explicit request pin always wins.
pub fn effective_conversation_output_tokens(
    config: &SamplerConfig,
    request: &ConversationRequest,
) -> Option<u32> {
    request
        .max_output_tokens
        .or(config.max_completion_tokens)
        .or_else(|| match config.api_backend {
            ApiBackend::Messages => Some(ANTHROPIC_DEFAULT_MAX_TOKENS),
            ApiBackend::ChatCompletions | ApiBackend::Responses => None,
        })
}

fn is_codex_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && matches!(
            url.path().trim_end_matches('/'),
            "/backend-api/codex" | "/backend-api"
        )
}

/// The ChatGPT Codex endpoint accepts instruction messages as `developer`,
/// not `system`, and rejects API-only sampling controls. Apply the same dialect
/// to normal turns and auxiliary requests such as session titles.
fn patch_codex_response_request(base_url: &str, body: &mut serde_json::Value) {
    if !is_codex_base_url(base_url) {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        for field in ["max_output_tokens", "temperature", "top_p"] {
            object.remove(field);
        }
    }
    if let Some(input) = body
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    {
        for item in input {
            if item.get("role").and_then(serde_json::Value::as_str) == Some("system") {
                item["role"] = serde_json::json!("developer");
            }
        }
    }
}

/// Per-request `x-grok-*` headers. Optional fields are skipped when empty/`None`.
struct GrokRequestHeaders<'a> {
    conv_id: &'a str,
    req_id: &'a str,
    model_id: &'a str,
    session_id: &'a str,
    turn_idx: Option<&'a str>,
    /// Turn-level resubmit attempt; the proxy counts retry traffic by it.
    transient_retry: Option<&'a str>,
    agent_id: &'a str,
    deployment_id: Option<&'a str>,
    user_id: Option<&'a str>,
}

impl GrokRequestHeaders<'_> {
    fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut b = builder
            .header("x-grok-conv-id", self.conv_id)
            .header("x-grok-req-id", self.req_id)
            .header("x-grok-model-override", self.model_id)
            .header("x-grok-session-id", self.session_id)
            .header("x-grok-agent-id", self.agent_id);
        if let Some(idx) = self.turn_idx {
            b = b.header("x-grok-turn-idx", idx);
        }
        if let Some(attempt) = self.transient_retry {
            b = b.header("x-grok-transient-retry", attempt);
        }
        if let Some(id) = self.deployment_id.filter(|s| !s.is_empty()) {
            b = b.header("x-grok-deployment-id", id);
        }
        if let Some(id) = self.user_id.filter(|s| !s.is_empty()) {
            b = b.header("x-grok-user-id", id);
        }
        b
    }
}

/// Deserialize a Responses SSE event, stripping unknown tools and rewriting terminal `total_tokens` from `context_details`.
pub(crate) fn deserialize_response_event(data: &str) -> Result<rs::ResponseStreamEvent> {
    let mut event = match serde_json::from_str::<rs::ResponseStreamEvent>(data) {
        Ok(event) => event,
        Err(first_err) => {
            // Try sanitizing: parse as Value, strip unknown tools, retry.
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data) {
                normalize_response_effort(&mut value);
                // Strip tools that async_openai's rs::Tool can't deserialize (e.g., Grok-specific "x_search")
                // Instead of maintaining a hardcoded allowlist, try deserializing each tool entry; if it fails, drop it
                if let Some(tools) = value
                    .pointer_mut("/response/tools")
                    .and_then(|v| v.as_array_mut())
                {
                    tools.retain(|t| serde_json::from_value::<rs::Tool>(t.clone()).is_ok());
                }
                if let Ok(mut event) = serde_json::from_value::<rs::ResponseStreamEvent>(value) {
                    apply_terminal_event_overrides(&mut event, data);
                    return Ok(event);
                }
            }
            return Err(SamplingError::Serialization(first_err));
        }
    };
    apply_terminal_event_overrides(&mut event, data);
    Ok(event)
}

/// Codex also sends control frames such as `keepalive`. Ignore only unknown
/// top-level event kinds; malformed JSON and invalid known events must still fail.
fn is_unknown_response_event(error: &SamplingError, data: &str) -> bool {
    let SamplingError::Serialization(error) = error else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let unknown = format!("unknown variant `{event_type}`");
    // Check the tag independently so a bad nested variant can never hide a
    // malformed known event, even if its name happens to match the outer type.
    error.to_string().contains(&unknown)
        && serde_json::from_value::<rs::ResponseStreamEvent>(serde_json::json!({
            "type": event_type
        }))
        .is_err_and(|error| error.to_string().contains(&unknown))
}

/// Rewrite a reasoning effort the typed SDK enum does not carry.
///
/// A Responses backend can echo `reasoning.effort: "disabled"` — measurably, even
/// when the request asked for `xhigh`. `async-openai`'s `ReasoningEffort` has no
/// `disabled`, so the event fails to deserialize and the turn aborts on the very
/// first SSE frame, before any model output. `none` is the value the SDK does
/// carry and the one the field means. Ported from open-grok's
/// `normalize_codex_response_event`.
///
/// This runs only on the sanitizing path (the fast typed parse is tried first), so
/// it costs nothing for the events that already parse.
fn normalize_response_effort(value: &mut serde_json::Value) {
    if let Some(effort) = value.pointer_mut("/response/reasoning/effort")
        && effort.as_str() == Some("disabled")
    {
        *effort = serde_json::Value::String("none".to_owned());
    }
}

/// On `response.completed` / `response.incomplete`, rewrite `usage.total_tokens` to the live context length from `context_details`.
/// Billing fields stay on the cumulative wire values, so telemetry is unaffected.
fn apply_terminal_event_overrides(event: &mut rs::ResponseStreamEvent, data: &str) {
    let response = match event {
        rs::ResponseStreamEvent::ResponseCompleted(e) => &mut e.response,
        rs::ResponseStreamEvent::ResponseIncomplete(e) => &mut e.response,
        _ => return,
    };
    // Re-parse for fields async_openai's types omit (context total, cost ticks).
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    // Stash normalized cost ticks in metadata for stream_responses. OpenRouter's
    // authoritative USD field takes precedence over the legacy Grok backfill.
    let cost_ticks = value.pointer("/response/usage").and_then(|usage| {
        let cost_usd = usage.get("cost").and_then(|cost| match cost {
            serde_json::Value::Number(number) => number.as_f64(),
            serde_json::Value::String(cost) => cost.parse::<f64>().ok(),
            _ => None,
        });
        cost_usd
            .and_then(distill_sampling_types::usd_cost_to_ticks)
            .or_else(|| {
                distill_sampling_types::reported_cost_ticks(
                    usage
                        .get("cost_in_usd_ticks")
                        .and_then(|value| value.as_i64()),
                )
            })
    });
    if let Some(ticks) = cost_ticks {
        response
            .metadata
            .get_or_insert_with(Default::default)
            .insert(COST_USD_TICKS_METADATA_KEY.to_owned(), ticks.to_string());
    }
    let Some(usage) = response.usage.as_mut() else {
        return;
    };
    let Some(total) = extract_context_total(&value) else {
        return;
    };
    usage.total_tokens = total;
}

/// Metadata key that carries cost ticks through the typed Response events, which have no field for them.
pub(crate) const COST_USD_TICKS_METADATA_KEY: &str = "xai.cost_usd_ticks";

/// Read `response.usage.context_details.{input_tokens, output_tokens}` from the parsed terminal-event JSON and return their sum.
/// Returns `None` if either field is missing or out of `u32` range.
fn extract_context_total(value: &serde_json::Value) -> Option<u32> {
    let cd = value.pointer("/response/usage/context_details")?;
    let i = u32::try_from(cd.get("input_tokens")?.as_u64()?).ok()?;
    let o = u32::try_from(cd.get("output_tokens")?.as_u64()?).ok()?;
    Some(i.saturating_add(o))
}

/// Splice the raw-JSON hosted-tool entries for `web_search` and `x_search` into a serialized Responses request body's `tools` array.
/// `x_search` has no `rs::Tool` variant, and `web_search`'s typed filters cannot carry `excluded_domains`, so both travel as raw JSON.
/// Neither may also be emitted as a typed `rs::Tool`; the API rejects the duplicate.
fn splice_extra_tool_entries(
    request_body: &mut serde_json::Value,
    entries: Vec<serde_json::Value>,
) {
    if entries.is_empty() {
        return;
    }
    if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        tools.extend(entries);
    } else {
        if let Some(obj) = request_body.as_object_mut() {
            obj.insert("tools".to_owned(), serde_json::Value::Array(entries));
        }
    }
}

/// Parse `Retry-After` as integer seconds, capped at 120; HTTP-dates yield `None`.
fn extract_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.min(120))
}

fn extract_should_retry(headers: &reqwest::header::HeaderMap) -> Option<bool> {
    headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            if s.eq_ignore_ascii_case("true") {
                Some(true)
            } else if s.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None // unknown value, treat as absent
            }
        })
}

fn extract_model_metadata(headers: &reqwest::header::HeaderMap) -> Option<ResponseModelMetadata> {
    let context_window = headers
        .get("x-grok-context-window")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let max_completion_tokens = headers
        .get("x-grok-max-completion-tokens")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());

    let models_etag = headers
        .get("x-models-etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if context_window.is_some() || max_completion_tokens.is_some() || models_etag.is_some() {
        Some(ResponseModelMetadata {
            context_window,
            max_completion_tokens,
            models_etag,
        })
    } else {
        None
    }
}

/// Wrapper for streaming chat completion requests that adds `stream` and `stream_options` without modifying the original `ChatCompletionRequest`.
#[derive(Serialize)]
struct StreamingChatRequest<'a> {
    #[serde(flatten)]
    inner: &'a ChatCompletionRequest,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

fn append_response_includes(body: &mut serde_json::Value, extra_includes: &[String]) {
    if extra_includes.is_empty() {
        return;
    }
    let Some(body) = body.as_object_mut() else {
        return;
    };
    let include = body.entry("include").or_insert(serde_json::Value::Null);
    if include.is_null() {
        *include = serde_json::Value::Array(Vec::new());
    }
    let Some(include) = include.as_array_mut() else {
        return;
    };
    for value in extra_includes {
        if !include
            .iter()
            .any(|existing| existing.as_str() == Some(value.as_str()))
        {
            include.push(serde_json::Value::String(value.clone()));
        }
    }
}

/// Resolve `env_http_headers` (`header -> env var`) into `headers` via `getenv`, skipping unset/blank/invalid entries and trimming values.
fn apply_env_http_headers(
    env_http_headers: &IndexMap<String, String>,
    getenv: impl Fn(&str) -> Option<String>,
    headers: &mut HeaderMap,
) {
    for (key, env_var) in env_http_headers {
        let Some(value) = getenv(env_var) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (Ok(name), Ok(header_value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::from_str(value),
        ) else {
            tracing::warn!(
                header = %key,
                env_var = %env_var,
                "skipping env_http_header with an invalid header name or value"
            );
            continue;
        };
        headers.insert(name, header_value);
    }
}

/// HTTP client for sampling. Cheap to clone.
/// Carries an `Arc`-backed `reqwest::Client` and the default headers/request-defaults computed from a [`SamplerConfig`] at construction time.
#[derive(Clone)]
pub struct SamplingClient {
    http: reqwest::Client,
    default_headers: HeaderMap,
    base_url: String,
    defaults: ClientDefaults,
    /// Optional 401-attribution hook.
    /// The shell wires this to emit a structured event at every UNAUTHORIZED arm so 401s can be bucketed by stale-snapshot vs. live-token-rejected.
    /// `None` for sampler-only callers and tests.
    attribution_callback: Option<crate::attribution::SharedAttributionCallback>,
    /// Per-request bearer override. See `SamplerConfig::bearer_resolver`.
    bearer_resolver: Option<crate::config::SharedBearerResolver>,
    /// Per-request header injection (OTel traceparent).
    header_injector: Option<crate::config::SharedHeaderInjector>,
    /// Endpoint URL builder, resolved once from `base_url` and `query_params`.
    endpoint: EndpointTemplate,
    first_use_noted: Arc<AtomicBool>,
}

impl std::fmt::Debug for SamplingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingClient")
            .field("base_url", &self.base_url)
            .field("defaults", &self.defaults)
            .field(
                "has_attribution_callback",
                &self.attribution_callback.is_some(),
            )
            .field("has_bearer_resolver", &self.bearer_resolver.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
struct ClientDefaults {
    model: String,
    max_completion_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    api_backend: ApiBackend,
    auth_scheme: AuthScheme,
    request_compression: RequestCompression,
    stream_tool_calls: bool,
    /// This model's own effort, applied when the request does not carry one
    /// (a round the decision layer moved onto another model, for instance).
    reasoning_effort: Option<ReasoningEffort>,
    /// How this model wants the thinking setting expressed (see [`SamplerConfig`]).
    reasoning_shape: ReasoningShape,
    reasoning_summary: Option<distill_sampling_types::ReasoningSummary>,
    extra_response_includes: Vec<String>,
    doom_loop_recovery: Option<distill_sampling_types::DoomLoopRecoveryPolicy>,
}

/// Endpoint URL builder, resolved once at client construction so each request only appends its path.
#[derive(Clone, Debug)]
enum EndpointTemplate {
    /// No query params and no query on the base URL (or an unparseable base): append the path to the base verbatim.
    Plain(String),
    /// Query params configured: `{prefix}/{path}{suffix}`.
    /// `suffix` starts with `?` and folds any base-URL params; a configured key wins over the same key in `base_url`.
    /// Pairs are percent-encoded with no duplicates.
    WithQuery { prefix: String, suffix: String },
}

impl EndpointTemplate {
    fn new(base_url: &str, query_params: &IndexMap<String, String>) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        // The fast path is safe only when there is nothing to fold: no configured params and no query already on the base
        // A base query would otherwise land before the appended path
        if query_params.is_empty() && !base.contains('?') {
            return Self::Plain(base);
        }
        let mut url = match reqwest::Url::parse(&base) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(
                    url = %base,
                    %error,
                    "failed to parse base URL for endpoint; sending without folded query"
                );
                return Self::Plain(base);
            }
        };
        let overridden: std::collections::HashSet<&str> =
            query_params.keys().map(String::as_str).collect();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !overridden.contains(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let prefix = {
            let mut prefix_url = url.clone();
            prefix_url.set_query(None);
            prefix_url.as_str().trim_end_matches('/').to_string()
        };
        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in &kept {
                pairs.append_pair(key, value);
            }
            for (key, value) in query_params {
                pairs.append_pair(key, value);
            }
        }
        let suffix = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        Self::WithQuery { prefix, suffix }
    }

    fn url_for_path(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        match self {
            Self::Plain(base) => format!("{base}/{path}"),
            Self::WithQuery { prefix, suffix } => format!("{prefix}/{path}{suffix}"),
        }
    }
}

// =============================================================================
// User-Agent helpers
// =============================================================================

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

fn agent_version() -> String {
    distill_version::VERSION.to_string()
}

/// Render a User-Agent string for the given origin client.
/// Mirrors the shell's `user_agent_string_for` but uses sampler-local constants.
/// The session typically owns the canonical User-Agent rendering for process-wide HTTP clients.
pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    let agent_version = agent_version();
    let platform = PlatformInfo::current();

    if origin.product == AGENT_PRODUCT && origin.version.as_deref() == Some(agent_version.as_str())
    {
        return format!(
            "{}/{} ({}; {})",
            AGENT_PRODUCT, agent_version, platform.os, platform.arch
        );
    }

    match origin.version.as_deref() {
        Some(origin_version) => format!(
            "{}/{} {}/{} ({}; {})",
            origin.product,
            origin_version,
            AGENT_PRODUCT,
            agent_version,
            platform.os,
            platform.arch
        ),
        None => format!(
            "{} {}/{} ({}; {})",
            origin.product, AGENT_PRODUCT, agent_version, platform.os, platform.arch
        ),
    }
}

/// A request builder coupled to the credential state it was built with, so a 401 arm cannot classify from anything but the build-time capture.
/// The wire default (`SentCredential::Unknown`, which charges the retry budget) stays the fail-closed one.
/// Only an explicit `sent_bearer: None` (a send the builder provably stamped no credential onto) reaches the uncharged lane via [`auth_rejected`].
struct SentRequest {
    builder: reqwest::RequestBuilder,
    /// Tail fragment of the credential in the built headers (`None` means no credential header).
    sent_bearer: Option<String>,
}

/// The one way a 401 becomes a `SamplingError::Auth` with a wire-derived credential classification: from the fragment its [`SentRequest`] captured.
fn auth_rejected(message: String, sent_bearer: Option<&str>) -> SamplingError {
    SamplingError::Auth {
        message,
        credential: SentCredential::from_sent_fragment(sent_bearer),
    }
}

// =============================================================================
// SamplingClient
// =============================================================================

impl SamplingClient {
    /// Uses an identity-specific client for configured mTLS; otherwise grabs the process-wide shared client.
    /// This does not perform any network I/O.
    pub fn new(config: SamplerConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ref api_key) = config.api_key {
            match config.auth_scheme {
                AuthScheme::XApiKey => {
                    let header_value = HeaderValue::from_str(api_key).map_err(|_| {
                        tracing::debug!(
                            api_key = %api_key,
                            "Invalid api_key: cannot be converted to a valid HTTP header"
                        );
                        SamplingError::auth_unknown(
                            "Invalid api_key: cannot be converted to a valid HTTP header",
                        )
                    })?;
                    headers.insert(HeaderName::from_static("x-api-key"), header_value);
                }
                AuthScheme::Bearer => {
                    let bearer = format!("Bearer {}", api_key);
                    let header_value = HeaderValue::from_str(&bearer).map_err(|_| {
                        tracing::debug!(
                            api_key = %api_key,
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header"
                        );
                        SamplingError::auth_unknown(
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header",
                        )
                    })?;
                    headers.insert(AUTHORIZATION, header_value);
                }
            }
        }

        // Apply all extra headers verbatim
        // This is the single injection point for proxy-auth headers and any other URL- or environment-specific headers the session decides to set
        for (key, value) in &config.extra_headers {
            let header_name = HeaderName::try_from(key.as_str())
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header name"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header value"))?;
            headers.insert(header_name, header_value);
        }

        // Resolve here, not into `extra_headers`, so an env-sourced secret stays out of persisted state
        apply_env_http_headers(
            &config.env_http_headers,
            |var| std::env::var(var).ok(),
            &mut headers,
        );

        // Add x-grok-client-version header for version gating at the proxy.
        if let Some(client_version) = config.client_version.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(client_version)
        {
            headers.insert(
                HeaderName::from_static("x-grok-client-version"),
                header_value,
            );
        }

        if let Some(deployment_id) = config.deployment_id.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(deployment_id)
        {
            headers.insert(
                HeaderName::from_static("x-grok-deployment-id"),
                header_value,
            );
        }

        if let Some(user_id) = config.user_id.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(user_id)
        {
            headers.insert(HeaderName::from_static("x-grok-user-id"), header_value);
        }

        if let Some(conversation_group_id) = config.conversation_group_id.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(conversation_group_id.as_ref())
        {
            headers.insert(
                HeaderName::from_static("x-grok-conv-group-id"),
                header_value,
            );
        }

        {
            let client_id = config
                .client_identifier
                .clone()
                .unwrap_or_else(|| DEFAULT_CLIENT_IDENTIFIER.to_string());
            if let Ok(header_value) = HeaderValue::from_str(&client_id) {
                headers.insert(
                    HeaderName::from_static("x-grok-client-identifier"),
                    header_value,
                );
            }
        }

        // Always set User-Agent: per-session origin if available, else fallback.
        {
            let ua_string = match config.origin_client.as_ref() {
                Some(origin) => user_agent_string_for(origin),
                None => user_agent_string_for(&OriginClientInfo {
                    product: AGENT_PRODUCT.to_string(),
                    version: Some(agent_version()),
                }),
            };
            if let Ok(v) = HeaderValue::from_str(&ua_string) {
                headers.insert(USER_AGENT, v);
            }
        }

        if config.force_http1 {
            tracing::info!("Using HTTP/1.1 for sampling client (force_http1=true)");
        }
        let http = if let Some(cert_dir) = config.mtls_cert_dir.as_deref() {
            crate::shared_http::mtls_client(cert_dir, config.force_http1)?
        } else if config.force_http1 {
            crate::shared_http::client_http1().map_err(SamplingError::Http)?
        } else {
            crate::shared_http::client().map_err(SamplingError::Http)?
        };

        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_new",
            base_url = %config.base_url,
            model = %config.model,
            api_backend = ?config.api_backend,
            auth_scheme = ?config.auth_scheme,
            request_compression = ?config.request_compression,
            // "unset" (not "none"): `ReasoningEffort::None` is a real wire value; logging the absent Option as "none" looked like we were sending it
            reasoning_effort = config.reasoning_effort.map_or("unset", |e| e.into()),
            has_api_key = config.api_key.is_some(),
            has_bearer_resolver = config.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
        );

        let defaults = ClientDefaults {
            model: config.model,
            max_completion_tokens: config.max_completion_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            api_backend: config.api_backend,
            auth_scheme: config.auth_scheme,
            request_compression: config.request_compression,
            stream_tool_calls: config.stream_tool_calls,
            reasoning_effort: config.reasoning_effort,
            reasoning_shape: config.reasoning_shape,
            reasoning_summary: config.reasoning_summary,
            extra_response_includes: config.extra_response_includes,
            doom_loop_recovery: config.doom_loop_recovery,
        };

        let endpoint = EndpointTemplate::new(&config.base_url, &config.query_params);

        Ok(Self {
            http,
            default_headers: headers,
            base_url: config.base_url,
            defaults,
            attribution_callback: config.attribution_callback,
            bearer_resolver: config.bearer_resolver,
            header_injector: config.header_injector,
            endpoint,
            first_use_noted: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn api_backend(&self) -> ApiBackend {
        self.defaults.api_backend.clone()
    }

    /// The exact endpoint template used by the corresponding conversation
    /// request, including configured query parameters.
    pub fn attribution_endpoint(&self) -> String {
        let path = match self.defaults.api_backend {
            ApiBackend::ChatCompletions => "chat/completions",
            ApiBackend::Responses => "responses",
            ApiBackend::Messages => "messages",
        };
        self.endpoint(path)
    }

    /// The effort mapping after the client fills its model defaults and applies
    /// the configured wire shape.
    pub fn attribution_applied_effort(
        &self,
        requested: Option<distill_sampling_types::ReasoningEffort>,
        max_tokens: Option<u32>,
    ) -> Option<String> {
        let requested = requested.or(self.defaults.reasoning_effort);
        distill_sampling_types::transmitted_reasoning_effort(
            self.defaults.api_backend.clone(),
            self.defaults.reasoning_shape,
            requested,
            max_tokens.or(self.defaults.max_completion_tokens),
        )
    }

    /// Give the bearer resolver its pre-send hook before [`Self::post`] reads it.
    /// Awaited separately because `post` is sync (its callers hand the builder straight to `send()`).
    async fn prepare_bearer(&self) {
        if let Some(resolver) = &self.bearer_resolver {
            resolver.prepare_for_send().await;
        }
    }

    /// The credential tail is captured at build time — see [`SentRequest`] for
    /// why a record-time re-read would race the recovery a 401 triggers.
    fn post(&self, url: impl reqwest::IntoUrl) -> SentRequest {
        if !self.first_use_noted.load(Ordering::Relaxed)
            && !self.first_use_noted.swap(true, Ordering::Relaxed)
        {
            crate::prewarm::note_first_sampling_use(&self.base_url);
        }
        let mut headers = self.default_headers.clone();
        if let Some(resolver) = &self.bearer_resolver {
            // Identity headers belong to the provider, not to the model entry: drop
            // any the config carried before the resolver has its say.
            for name in resolver.reserved_headers() {
                headers.remove(*name);
            }
            let resolved = resolver.current_auth();
            // Sole auth source: without a live bearer, send no credential rather
            // than a stale seed key. A resolver that opts out of failing closed
            // keeps the legacy fallback.
            if resolved.is_some() || resolver.fail_closed_on_missing() {
                headers.remove(AUTHORIZATION);
                headers.remove(HeaderName::from_static("x-api-key"));
            }
            if let Some(fresh) = resolved {
                match self.defaults.auth_scheme {
                    AuthScheme::XApiKey => {
                        if let Ok(v) = HeaderValue::from_str(&fresh.bearer) {
                            headers.insert(HeaderName::from_static("x-api-key"), v);
                        }
                    }
                    AuthScheme::Bearer => {
                        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", fresh.bearer)) {
                            headers.insert(AUTHORIZATION, v);
                        }
                    }
                }
                for (name, value) in fresh.extra_headers {
                    match (
                        HeaderName::try_from(name.as_str()),
                        HeaderValue::from_str(&value),
                    ) {
                        (Ok(name), Ok(value)) => {
                            headers.insert(name, value);
                        }
                        _ => tracing::warn!("dropped an unusable auth header from the resolver"),
                    }
                }
            }
        }
        {
            let auth_prefix = headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(20).collect::<String>());
            let x_api_key_prefix = headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(12).collect::<String>());
            tracing::info!(
                target: crate::sampling_log::TARGET,
                event = "client_post",
                base_url = %self.base_url,
                model = %self.defaults.model,
                api_backend = ?self.defaults.api_backend,
                auth_scheme = ?self.defaults.auth_scheme,
                has_bearer_resolver = self.bearer_resolver.is_some(),
                has_authorization_header = headers.get(AUTHORIZATION).is_some(),
                has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
                auth_header_prefix = auth_prefix.as_deref().unwrap_or("none"),
                x_api_key_prefix = x_api_key_prefix.as_deref().unwrap_or("none"),
            );
        }
        let sent_bearer = Self::sent_fragment_from_headers(&headers, &self.defaults.auth_scheme);
        if let Some(injector) = &self.header_injector {
            injector.inject(&mut headers);
        }
        SentRequest {
            builder: self.http.post(url).headers(headers),
            sent_bearer,
        }
    }

    /// Must run before the span gets its first child, which starts it and freezes its parent.
    fn adopt_traceparent(&self, span: &tracing::Span, traceparent: Option<&str>) {
        if let Some(injector) = &self.header_injector
            && let Some(traceparent) = traceparent
            && !span.is_disabled()
        {
            injector.set_span_parent(span, traceparent);
        }
    }

    /// Tail fragment of the credential in `headers`: `x-api-key` (Messages-API scheme) or `Authorization`.
    /// The fragment length is [`crate::attribution::BEARER_SUFFIX_LEN`].
    fn sent_fragment_from_headers(headers: &HeaderMap, scheme: &AuthScheme) -> Option<String> {
        let raw = match scheme {
            AuthScheme::XApiKey => headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::Bearer => headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer ")),
        };
        raw.map(|s| bearer_suffix(s).to_string())
    }

    /// Best-effort *build-time* view of what the next request would carry (resolver-authoritative).
    /// For request-start diagnostics ([`Self::auth_info`]) only.
    /// 401 attribution must use the fragment captured by [`Self::post`], which cannot race a recovery.
    fn current_sent_bearer_suffix(&self) -> Option<String> {
        if self.bearer_resolver.is_some() {
            return self
                .bearer_resolver
                .as_ref()
                .and_then(|r| r.current_bearer())
                .map(|s| bearer_suffix(&s).to_string());
        }
        Self::sent_fragment_from_headers(&self.default_headers, &self.defaults.auth_scheme)
    }

    /// Invoke the optional 401 attribution callback for one logical 401 response.
    /// The emit happens at the lowest layer that saw the status, so higher layers that react to a 401 must not emit a duplicate event.
    /// `sent_suffix` is the fragment [`Self::post`] captured for the rejected request.
    fn record_401_attribution(
        &self,
        consumer: crate::attribution::SamplingConsumer,
        sent_suffix: Option<&str>,
    ) {
        if let Some(cb) = self.attribution_callback.as_ref() {
            cb.record_401(consumer, sent_suffix);
        }
    }

    pub fn auth_info(&self) -> crate::sampling_log::AuthInfo {
        let auth_prefix = self.current_sent_bearer_suffix();
        let auth_type = match (&self.defaults.auth_scheme, &auth_prefix) {
            (AuthScheme::XApiKey, Some(_)) => "x-api-key",
            (AuthScheme::Bearer, Some(_)) => "bearer",
            (_, None) => "none",
        };
        crate::sampling_log::AuthInfo {
            auth_type,
            auth_prefix,
        }
    }

    fn is_sensitive_header(name: &str) -> bool {
        let lower = name.to_lowercase();
        lower.contains("authorization")
            || lower.contains("api-key")
            || lower.contains("apikey")
            || lower.contains("token")
            || lower.contains("secret")
    }

    /// Short lossy body snippet for error logs (never user-facing).
    fn body_preview(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).chars().take(500).collect()
    }

    /// Log all headers from a request at debug level (redacting sensitive values).
    fn log_request_headers(request: &reqwest::Request, endpoint_name: &str) {
        for (name, value) in request.headers().iter() {
            let value_str = if Self::is_sensitive_header(name.as_str()) {
                "[REDACTED]"
            } else {
                value.to_str().unwrap_or("[non-utf8]")
            };
            tracing::debug!(
                header_name = %name,
                header_value = %value_str,
                "Request header ({})",
                endpoint_name
            );
        }
    }

    fn endpoint(&self, path: &str) -> String {
        self.endpoint.url_for_path(path)
    }

    fn apply_defaults(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionRequest> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.max_tokens.is_none() {
            request.max_tokens = self.defaults.max_completion_tokens;
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        if request.reasoning_effort.is_none() {
            request.reasoning_effort = self.defaults.reasoning_effort;
        }
        // A model that takes a token budget instead of an effort name gets the
        // same chosen effort expressed in its own dialect.
        request.apply_reasoning_shape(self.defaults.reasoning_shape);
        request.apply_deepseek_thinking_toggle();

        Ok(request)
    }

    /// `sent_bearer` is the fragment [`Self::post`] captured for the request that produced `response` (401 attribution).
    async fn handle_response(
        &self,
        response: reqwest::Response,
        sent_bearer: Option<&str>,
    ) -> Result<ChatCompletionResponse> {
        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletions,
                    sent_bearer,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    sent_bearer,
                ));
            }
            let message = user_facing_api_error_message(status, bytes.as_ref());
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let completion = serde_json::from_slice::<ChatCompletionResponse>(&bytes).map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize ChatCompletionResponse"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(completion)
    }

    // =========================================================================
    // Chat Completions API
    // =========================================================================

    pub async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        let request_region = crate::span_timing::Region::from_span(tracing::info_span!(
            "sampling.nonstream_request",
            model = %model_id,
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
        ));

        tracing::debug!(
            base_url = %self.base_url,
            model_id = %model_id,
            "Sending chat completion request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            transient_retry: payload.x_grok_transient_retry.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("chat/completions"));
        let built_request = self
            .build_json_request(grok_headers.apply(builder), &payload)
            .await?;
        let response = self.send(built_request).await?;

        let status = response.status();
        request_region
            .span()
            .record("status_code", status.as_u16() as i64);
        request_region.span().record("success", status.is_success());

        self.handle_response(response, sent_bearer.as_deref()).await
    }

    /// Serialize `payload` onto `builder` the way `RequestBuilder::json` does
    /// (a caller-set `Content-Type` wins), zstd-compressing large bodies when
    /// the shell marked this endpoint as accepting it.
    async fn build_json_request<T: Serialize + ?Sized>(
        &self,
        builder: reqwest::RequestBuilder,
        payload: &T,
    ) -> Result<reqwest::Request> {
        let json = serde_json::to_vec(payload).map_err(|e| {
            tracing::error!("Failed to serialize request body: {}", e);
            SamplingError::Serialization(e)
        })?;
        let mut request = builder.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;
        if !request.headers().contains_key(CONTENT_TYPE) {
            request
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        let body = if should_compress(self.defaults.request_compression, json.len()) {
            match compress_body(&json).await {
                Some(compressed) => {
                    request
                        .headers_mut()
                        .insert(CONTENT_ENCODING, HeaderValue::from_static("zstd"));
                    compressed
                }
                None => json,
            }
        } else {
            json
        };
        *request.body_mut() = Some(reqwest::Body::from(body));
        Ok(request)
    }

    async fn send(&self, request: reqwest::Request) -> Result<reqwest::Response> {
        self.http
            .execute(request)
            .await
            .inspect_err(|e| tracing::debug!("HTTP request failed: {}", e))
            .map_err(Into::into)
    }

    async fn execute_stream_request(
        &self,
        built_request: reqwest::Request,
        span_timing: &mut StreamSpanTiming,
    ) -> Result<reqwest::Response> {
        span_timing.record_request_build();
        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            span_timing.record_transport_failure(&e.to_string());
            e
        })?;
        span_timing.record_response_headers();
        Ok(response)
    }

    /// Start a streaming chat completion request. Returns a stream of typed chunks.
    pub async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.chat_completion_stream",
            endpoint = %self.endpoint("chat/completions"),
            model_id = request.model.as_deref().unwrap_or(""),
        );
        self.adopt_traceparent(region.span(), request.traceparent.as_deref());
        if region.span().is_disabled() {
            self.chat_completion_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.chat_completion_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    async fn chat_completion_stream_inner(
        &self,
        request: ChatCompletionRequest,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        // Wrap the request with streaming fields and serialize once.
        let streaming_request = StreamingChatRequest {
            inner: &payload,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            transient_retry: payload.x_grok_transient_retry.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("chat/completions"));
        let http_request = grok_headers
            .apply(builder)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let built_request = self
            .build_json_request(http_request, &streaming_request)
            .await?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending chat/completions request"
        );
        Self::log_request_headers(&built_request, "chat/completions");
        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletionsStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("chat/completions");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }

            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "chat/completions API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        // Strip UTF-8 BOM if present: eventsource-stream 0.2.3 incorrectly slices BOM at byte 1 instead of 3.
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        let event_stream = byte_stream.eventsource();

        // Map SSE events into ChatCompletionChunk.
        // Uses `scan` so that `[DONE]` and transport errors both terminate the stream (`None`)
        // The first transport error is emitted to the consumer, then subsequent polls return `None`
        let chunks = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "chat_completions",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<ChatCompletionChunk>(data).map_err(|e| {
                                    tracing::error!(
                                        error = %e,
                                        raw_data = %data,
                                        "Failed to deserialize ChatCompletionChunk from stream"
                                    );
                                    SamplingError::Serialization(e)
                                }),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((
            span_timing.hold_until_first_content(chunks, chat_chunk_class),
            model_metadata,
        ))
    }

    // =========================================================================
    // Responses API
    // =========================================================================

    fn apply_response_defaults(&self, request: &mut CreateResponseWrapper) -> Result<()> {
        if request.inner.model.is_none() {
            request.inner.model = Some(self.defaults.model.clone());
        }

        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        if request.inner.max_output_tokens.is_none() {
            request.inner.max_output_tokens = self.defaults.max_completion_tokens;
        }

        // The API defaults `store` to true, which breaks ZDR compliance
        if request.inner.store.is_none() {
            request.inner.store = Some(false);
        }

        if let Some(summary) = self.defaults.reasoning_summary {
            let summary = summary.to_responses_api();
            match request.inner.reasoning.as_mut() {
                Some(reasoning) => reasoning.summary = summary,
                None if summary.is_some() => {
                    request.inner.reasoning = Some(rs::Reasoning {
                        effort: None,
                        summary,
                    });
                }
                None => {}
            }
        }

        // Include encrypted reasoning content if not specified
        let includes = request.inner.include.get_or_insert_with(Vec::new);
        if !includes.contains(&rs::IncludeEnum::ReasoningEncryptedContent) {
            includes.push(rs::IncludeEnum::ReasoningEncryptedContent);
        }

        Ok(())
    }

    /// Create a response using the Responses API (non-streaming).
    pub async fn create_response(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<rs::Response> {
        self.apply_response_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        let request_region = crate::span_timing::Region::from_span(tracing::info_span!(
            "sampling.nonstream_request",
            model = %model_id,
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
        ));

        // The trace field is process-local: upstream session code consumes it (and may upload a payload artifact); the sampler never forwards it
        // Drop it before we send
        request.trace.take();

        tracing::debug!("create_response: {:?}", &request);
        tracing::debug!("endpoint: {:?}", self.endpoint("responses"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            transient_retry: request.x_grok_transient_retry.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = request.body().map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        splice_extra_tool_entries(&mut request_body, extra_tool_entries);
        append_response_includes(&mut request_body, &self.defaults.extra_response_includes);
        // async-openai's ReasoningTextContent struct omits the `type` discriminator that the Responses API requires on input
        // Patch it in after serializing
        distill_sampling_types::patch_reasoning_text_types(&mut request_body);
        // A resumed session can carry ids the Responses API refuses (empty, over
        // 64 characters, off-charset): repair them at the same boundary, before
        // the body leaves.
        distill_sampling_types::patch_input_item_ids(&mut request_body);
        patch_codex_response_request(&self.base_url, &mut request_body);
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("responses"));
        let built_request = self
            .build_json_request(grok_headers.apply(builder), &request_body)
            .await?;
        let response = self.send(built_request).await?;

        let status = response.status();
        request_region
            .span()
            .record("status_code", status.as_u16() as i64);
        request_region.span().record("success", status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Responses,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let response_obj = serde_json::from_slice::<rs::Response>(&bytes).map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize rs::Response"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(response_obj)
    }

    /// Create a streaming response using the Responses API.
    ///
    /// Third element is the doom-loop collector, `Some` only when `doom_loop_recovery` is set.
    #[allow(clippy::type_complexity)]
    pub async fn create_response_stream(
        &self,
        request: CreateResponseWrapper,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.create_response_stream",
            endpoint = %self.endpoint("responses"),
            model_id = request.inner.model.as_deref().unwrap_or(""),
        );
        self.adopt_traceparent(region.span(), request.traceparent.as_deref());
        if region.span().is_disabled() {
            self.create_response_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.create_response_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    #[allow(clippy::type_complexity)]
    async fn create_response_stream_inner(
        &self,
        mut request: CreateResponseWrapper,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
        self.apply_response_defaults(&mut request)?;

        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // Drop process-local trace data (see note in `create_response`).
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending responses API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            transient_retry: request.x_grok_transient_retry.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = request.body().map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        // Inject Grok-specific fields not in async-openai's CreateResponse type.
        if self.defaults.stream_tool_calls
            && let Some(obj) = request_body.as_object_mut()
        {
            obj.insert("stream_tool_calls".to_owned(), serde_json::json!(true));
        }
        splice_extra_tool_entries(&mut request_body, extra_tool_entries);
        append_response_includes(&mut request_body, &self.defaults.extra_response_includes);
        distill_sampling_types::patch_reasoning_text_types(&mut request_body);
        // A resumed session can carry ids the Responses API refuses (empty, over
        // 64 characters, off-charset): repair them at the same boundary, before
        // the body leaves.
        distill_sampling_types::patch_input_item_ids(&mut request_body);
        patch_codex_response_request(&self.base_url, &mut request_body);
        // Fresh per attempt so signals never leak across retries; `None` (check disabled) sends no header and does no peek work per event
        let doom_loop = self
            .defaults
            .doom_loop_recovery
            .map(crate::doom_loop::DoomLoopSignalCollector::new);
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("responses"));
        let mut http_request = grok_headers
            .apply(builder)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if let Some(policy) = self.defaults.doom_loop_recovery {
            http_request = http_request
                .header(DOOM_LOOP_CHECK_HEADER, policy.window_tokens.to_string())
                .header(
                    EXACT_REPETITION_CHECK_HEADER,
                    DEFAULT_EXACT_REPETITION_MIN_TOKENS.to_string(),
                );
        }
        let built_request = self.build_json_request(http_request, &request_body).await?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending responses API stream request"
        );
        Self::log_request_headers(&built_request, "responses");
        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ResponsesStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        let event_stream = byte_stream.eventsource();

        let doom_loop_for_stream = doom_loop.clone();
        let ignore_unknown_events = is_codex_base_url(&self.base_url);

        // The scan item is an `Option`: `Some(None)` skips an absorbed doom-loop event without terminating the stream (`filter_map` below)
        // An outer `None` still ends the stream
        let events = event_stream
            .scan(false, move |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "responses",
                            data = %data,
                        );

                        // Intercept the non-standard doom-loop event before typed deserialization
                        // async-openai's event enum does not know it and would fail to parse it
                        // With the check disabled, `is_check_event` still guards against a server emitting it without opt-in (rollout skew)
                        let swallow = match &doom_loop_for_stream {
                            Some(collector) => collector.absorb(&event.event, data),
                            None => is_check_event(&event.event, data),
                        };
                        if swallow {
                            Some(None)
                        } else if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Some(Err(stream_error)))
                        } else {
                            let decoded = deserialize_response_event(data);
                            if ignore_unknown_events
                                && decoded
                                    .as_ref()
                                    .is_err_and(|error| is_unknown_response_event(error, data))
                            {
                                tracing::debug!("ignoring an unmodeled Codex Responses event");
                                Some(None)
                            } else {
                                Some(Some(decoded))
                            }
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Some(Err(SamplingError::EventStreamError(e.to_string()))))
                    }
                };
                std::future::ready(item)
            })
            .filter_map(std::future::ready)
            .boxed();

        Ok((
            span_timing.hold_until_first_content(events, responses_event_class),
            model_metadata,
            doom_loop,
        ))
    }

    // =========================================================================
    // Anthropic Messages API
    // =========================================================================

    fn apply_message_defaults(&self, request: &mut MessagesRequestWrapper) -> Result<()> {
        if request.inner.model.is_empty() {
            request.inner.model = self.defaults.model.clone();
        }

        if request.inner.max_tokens == 0 {
            request.inner.max_tokens = self
                .defaults
                .max_completion_tokens
                .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        }

        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        Ok(())
    }

    /// Create a message using the Anthropic Messages API (non-streaming).
    pub async fn create_message(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<messages::MessagesResponse> {
        self.apply_message_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        let request_region = crate::span_timing::Region::from_span(tracing::info_span!(
            "sampling.nonstream_request",
            model = %model_id,
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
        ));

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!("create_message: {:?}", &request.inner);
        tracing::debug!("endpoint: {:?}", self.endpoint("messages"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            transient_retry: request.x_grok_transient_retry.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("messages"));
        let built_request = self
            .build_json_request(grok_headers.apply(builder), &request.inner)
            .await?;
        let response = self.send(built_request).await?;

        let status = response.status();
        request_region
            .span()
            .record("status_code", status.as_u16() as i64);
        request_region.span().record("success", status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Messages,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let response_obj =
            serde_json::from_slice::<messages::MessagesResponse>(&bytes).map_err(|e| {
                let raw_body = String::from_utf8_lossy(&bytes);
                tracing::error!(
                    error = %e,
                    raw_body = %raw_body,
                    "Failed to deserialize MessagesResponse"
                );
                SamplingError::Serialization(e)
            })?;
        Ok(response_obj)
    }

    /// Create a streaming message using the Anthropic Messages API.
    pub async fn create_message_stream(
        &self,
        request: MessagesRequestWrapper,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.create_message_stream",
            endpoint = %self.endpoint("messages"),
            model_id = request.inner.model.as_str(),
        );
        self.adopt_traceparent(region.span(), request.traceparent.as_deref());
        if region.span().is_disabled() {
            self.create_message_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.create_message_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    async fn create_message_stream_inner(
        &self,
        mut request: MessagesRequestWrapper,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
        self.apply_message_defaults(&mut request)?;

        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending Messages API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            transient_retry: request.x_grok_transient_retry.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        self.prepare_bearer().await;
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("messages"));
        let http_request = grok_headers
            .apply(builder)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let built_request = self
            .build_json_request(http_request, &request.inner)
            .await?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending messages API stream request"
        );
        Self::log_request_headers(&built_request, "messages");
        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::MessagesStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        let event_stream = byte_stream.eventsource();

        // Map SSE events into MessageStreamEvent.
        // Uses `scan` so transport errors terminate the stream after the first error (same pattern as `chat_completion_stream`)
        let events = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "messages",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<messages::MessageStreamEvent>(data).map_err(
                                    |e| {
                                        tracing::error!(
                                            error = %e,
                                            raw_data = %data,
                                            "Failed to deserialize MessageStreamEvent from stream"
                                        );
                                        SamplingError::Serialization(e)
                                    },
                                ),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((
            span_timing.hold_until_first_content(events, message_event_class),
            model_metadata,
        ))
    }

    // =========================================================================
    // Unified Conversation API
    // =========================================================================

    fn apply_conversation_defaults(&self, request: &mut ConversationRequest) -> Result<()> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        if request.max_output_tokens.is_none() {
            request.max_output_tokens = self.defaults.max_completion_tokens;
        }

        Ok(())
    }

    /// Send a conversation request using the Chat Completions API (streaming).
    pub async fn conversation_stream(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion_stream(chat_request).await
    }

    /// Send a conversation request using the Chat Completions API (non-streaming).
    pub async fn conversation(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ChatCompletionResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion(chat_request).await
    }

    /// Send a conversation request using the Responses API (streaming).
    /// The third tuple element is the per-request doom-loop signal collector (see [`Self::create_response_stream`]).
    /// Callers that don't consume the signals can ignore it.
    #[allow(clippy::type_complexity)]
    pub async fn conversation_stream_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_transient_retry = request.x_grok_transient_retry.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        // The hosted tools travel as raw JSON, spliced in after serialization by `splice_extra_tool_entries`, whose doc explains why each one does
        let extra_tools = distill_sampling_types::extra_tool_entries(&request.hosted_tools);

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.reasoning_effort = request.reasoning_effort;
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_transient_retry = x_grok_transient_retry;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;
        wrapper.traceparent = request.traceparent;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response_stream(wrapper).await
    }

    /// Send a conversation request using the Responses API (non-streaming).
    pub async fn conversation_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<rs::Response> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_transient_retry = request.x_grok_transient_retry.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        // The hosted tools travel as raw JSON, spliced in by `create_response` via `splice_extra_tool_entries`, whose doc explains why
        let extra_tools = distill_sampling_types::extra_tool_entries(&request.hosted_tools);

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.reasoning_effort = request.reasoning_effort;
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_transient_retry = x_grok_transient_retry;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (streaming).
    pub async fn conversation_stream_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_transient_retry = request.x_grok_transient_retry.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_transient_retry = x_grok_transient_retry;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.traceparent = request.traceparent;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message_stream(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (non-streaming).
    pub async fn conversation_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<messages::MessagesResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_transient_retry = request.x_grok_transient_retry.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_transient_retry = x_grok_transient_retry;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message(wrapper).await
    }

    /// Backend-aware streaming call that collects the full response.
    /// Honors the request's [`LengthPolicy`](distill_sampling_types::LengthPolicy) like the actor path.
    /// The default still fails a text-only or empty `Length` stop, so side callers never persist a silently truncated result.
    pub async fn conversation_collect(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse> {
        self.conversation_collect_with_idle_timeout(request, std::time::Duration::from_secs(300))
            .await
    }

    /// [`Self::conversation_collect`] with a caller-chosen idle timeout, for short side calls (autocomplete, memory notes) that must give up fast.
    pub async fn conversation_collect_with_idle_timeout(
        &self,
        request: ConversationRequest,
        idle_timeout: std::time::Duration,
    ) -> Result<ConversationResponse> {
        self.conversation_collect_with_idle_timeout_and_rejection(request, idle_timeout)
            .await
            .0
    }

    /// Collect a response and retain it when the existing length gate rejects
    /// it. The normal error stays unchanged; the side channel lets billing
    /// consumers keep provider usage, cost, and response identity for a paid
    /// truncation instead of turning it into an unknown failed call.
    pub async fn conversation_collect_with_idle_timeout_and_rejection(
        &self,
        request: ConversationRequest,
        idle_timeout: std::time::Duration,
    ) -> (Result<ConversationResponse>, Option<ConversationResponse>) {
        let request_id = crate::types::RequestId::random();
        let length_policy = request.length_policy;
        let result = match self.api_backend() {
            ApiBackend::ChatCompletions => {
                let (raw, meta) = match self.conversation_stream(request).await {
                    Ok(value) => value,
                    Err(error) => return (Err(error), None),
                };
                let events =
                    crate::stream::stream_chat_completions(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Responses => {
                let (raw, meta, doom_loop) = match self.conversation_stream_responses(request).await
                {
                    Ok(value) => value,
                    Err(error) => return (Err(error), None),
                };
                let events =
                    crate::stream::stream_responses(raw, meta, request_id, idle_timeout, doom_loop);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Messages => {
                let (raw, meta) = match self.conversation_stream_messages(request).await {
                    Ok(value) => value,
                    Err(error) => return (Err(error), None),
                };
                let events = crate::stream::stream_messages(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
        };
        let response = match result {
            Ok((response, _metrics)) => response,
            Err(error) => return (Err(stream_collect_error(error)), None),
        };
        apply_length_policy_with_rejection(length_policy, response)
    }
}

/// Applies the request's [`distill_sampling_types::LengthPolicy`] to a collected response.
/// Fails a `Length` stop the policy rejects, logs the salvage breadcrumb otherwise.
/// The single gate shared by `drive_l2` and the direct-collect path so the two cannot drift.
pub(crate) fn apply_length_policy(
    policy: distill_sampling_types::LengthPolicy,
    response: distill_sampling_types::ConversationResponse,
) -> Result<distill_sampling_types::ConversationResponse> {
    apply_length_policy_with_rejection(policy, response).0
}

fn apply_length_policy_with_rejection(
    policy: distill_sampling_types::LengthPolicy,
    response: distill_sampling_types::ConversationResponse,
) -> (
    Result<distill_sampling_types::ConversationResponse>,
    Option<distill_sampling_types::ConversationResponse>,
) {
    use distill_sampling_types::LengthVerdict;
    match policy.verdict(&response) {
        LengthVerdict::Pass => (Ok(response), None),
        LengthVerdict::Fail => (Err(SamplingError::MaxTokensTruncation), Some(response)),
        LengthVerdict::Salvage => {
            // Breadcrumb for "why did the user get half an answer".
            tracing::info!(
                content_len = response.assistant().map_or(0, |a| a.content.len()),
                completion_tokens = response.usage.as_ref().map(|u| u.completion_tokens),
                "salvaging Length-truncated response per LengthPolicy::CompletePartial"
            );
            (Ok(response), None)
        }
        LengthVerdict::SalvageToolCalls => {
            // Breadcrumb for counting turns rescued from max_tokens_truncation.
            tracing::info!(
                tool_calls = response.tool_calls().len(),
                content_len = response.assistant().map_or(0, |a| a.content.len()),
                completion_tokens = response.usage.as_ref().map(|u| u.completion_tokens),
                "completing Length-truncated response with completed tool calls"
            );
            (Ok(response), None)
        }
    }
}

/// Rebuild `Api` from stream-collected info, preserving status, `Retry-After`, and `x-should-retry` (kind is lost on this path).
fn stream_collect_error(info: SamplingErrorInfo) -> SamplingError {
    SamplingError::Api {
        status: info
            .status_code
            .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
            .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        message: info.message,
        model_metadata: info.model_metadata,
        retry_after_secs: info.retry_after_secs,
        should_retry: info.should_retry,
        error_code: info.error_code,
    }
}

#[cfg(test)]
mod tests {

    /// A production-shaped `response.created` whose effort the SDK enum does not
    /// carry: without the rewrite the first SSE frame kills the turn.
    #[test]
    fn a_disabled_response_effort_parses_as_none() {
        let event = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "background": false,
                "created_at": 0,
                "id": "resp_1",
                "model": "gpt-6-astra",
                "object": "response",
                "output": [],
                "reasoning": {"context": "all_turns", "effort": "disabled",
                              "mode": "standard", "summary": "detailed"},
                "status": "in_progress",
                "tools": []
            }
        })
        .to_string();

        let typed = deserialize_response_event(&event).expect("disabled effort must parse");
        let rs::ResponseStreamEvent::ResponseCreated(created) = typed else {
            panic!("expected response.created, got something else");
        };
        assert_eq!(
            created
                .response
                .reasoning
                .and_then(|reasoning| reasoning.effort),
            Some(rs::ReasoningEffort::None),
            "the effort the API reported as `disabled` reads as `none`"
        );
    }
    use super::*;
    use crate::events::SamplingEvent;

    fn nth<T>(xs: &[T], i: usize) -> &T {
        let Some(x) = xs.get(i) else {
            panic!("expected item {i}, got {} items", xs.len());
        };
        x
    }
    use axum::{Router, body::Bytes, routing::post};
    use distill_sampling_types::ApiErrorCode;
    use distill_sampling_types::types::ChatRequestMessage;
    use indexmap::IndexMap;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[test]
    fn splice_extra_tool_entries_extends_existing_tools_array() {
        let mut body = serde_json::json!({ "tools": [{ "type": "function" }] });
        splice_extra_tool_entries(&mut body, vec![serde_json::json!({ "type": "web_search" })]);
        assert_eq!(
            body.get("tools"),
            Some(&serde_json::json!([{ "type": "function" }, { "type": "web_search" }]))
        );
    }

    #[test]
    fn splice_extra_tool_entries_creates_tools_array_when_absent() {
        let mut body = serde_json::json!({});
        splice_extra_tool_entries(&mut body, vec![serde_json::json!({ "type": "web_search" })]);
        assert_eq!(
            body.get("tools"),
            Some(&serde_json::json!([{ "type": "web_search" }]))
        );
    }

    #[test]
    fn splice_extra_tool_entries_noop_when_empty() {
        let mut body = serde_json::json!({ "tools": [{ "type": "function" }] });
        splice_extra_tool_entries(&mut body, vec![]);
        assert_eq!(
            body.get("tools"),
            Some(&serde_json::json!([{ "type": "function" }]))
        );
    }

    #[test]
    fn stream_collect_error_preserves_should_retry() {
        let info = SamplingErrorInfo {
            kind: crate::events::SamplingErrorKind::Api,
            status_code: Some(529),
            message: "Overloaded".into(),
            is_retryable: true,
            retry_after_secs: Some(3),
            should_retry: Some(false),
            error_code: Some(ApiErrorCode::InvalidImage),
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: distill_sampling_types::SentCredential::Unknown,
        };
        // SamplingError is not PartialEq (it carries reqwest/serde errors), so destructure once and compare all fields in a single assert
        let SamplingError::Api {
            status,
            message,
            model_metadata,
            retry_after_secs,
            should_retry,
            error_code,
        } = stream_collect_error(info)
        else {
            panic!("expected Api");
        };
        assert_eq!(
            (
                status.as_u16(),
                message.as_str(),
                model_metadata.is_none(),
                retry_after_secs,
                should_retry,
                error_code,
            ),
            (
                529,
                "Overloaded",
                true,
                Some(3),
                Some(false),
                Some(ApiErrorCode::InvalidImage)
            ),
        );
    }

    #[test]
    fn rejected_length_response_retains_paid_metadata_for_billing() {
        let response = ConversationResponse {
            items: vec![distill_sampling_types::ConversationItem::assistant("partial")],
            stop_reason: Some(distill_sampling_types::StopReason::Length),
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                total_tokens: 18,
                ..Default::default()
            }),
            cost_usd_ticks: Some(1234),
            message_chunks_emitted: 1,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: Some("msg-paid".to_string()),
            raw_stop_reason: None,
            stop_sequence: None,
        };

        let (result, rejected) =
            apply_length_policy_with_rejection(distill_sampling_types::LengthPolicy::Fail, response);

        assert!(matches!(
            result,
            Err(SamplingError::MaxTokensTruncation)
        ));
        let rejected = rejected.expect("the rejected response remains available to billing");
        assert_eq!(
            rejected.usage.map(|usage| usage.total_tokens),
            Some(18)
        );
        assert_eq!(rejected.cost_usd_ticks, Some(1234));
        assert_eq!(rejected.message_id.as_deref(), Some("msg-paid"));
    }

    fn minimal_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: "https://example.test".to_string(),
            model: "test-model".to_string(),
            context_window: 8192,
            ..Default::default()
        }
    }

    /// The shipped path from a model's configured shape to the wire body: the
    /// effort the decision layer chose reaches a budget-dialect model as the
    /// budget it stands for, and reaches every other model unchanged.
    #[test]
    fn apply_defaults_expresses_the_effort_in_the_models_own_shape() {
        let client = SamplingClient::new(SamplerConfig {
            reasoning_shape: ReasoningShape::MaxTokens,
            max_completion_tokens: Some(2_048),
            ..minimal_config()
        })
        .expect("client constructs without I/O");

        let mut request = ChatCompletionRequest::new("qwen/qwen3.7-flash", vec![]);
        request.reasoning_effort = Some(ReasoningEffort::High);
        let payload = client.apply_defaults(request).expect("defaults apply");
        assert_eq!(payload.reasoning_effort, None);
        // `high` is 2048 thinking tokens, clamped just under the 2048 ceiling so
        // the answer keeps room — the endpoint rejects a budget that does not fit.
        assert_eq!(
            payload.reasoning,
            Some(serde_json::json!({ "max_tokens": 1_984 }))
        );
        assert_eq!(payload.max_tokens, Some(2_048));

        // A model that takes the effort name is untouched: today's behaviour.
        let client = SamplingClient::new(minimal_config()).expect("client constructs without I/O");
        let mut request = ChatCompletionRequest::new("grok-4.5", vec![]);
        request.reasoning_effort = Some(ReasoningEffort::High);
        let payload = client.apply_defaults(request).expect("defaults apply");
        assert_eq!(payload.reasoning_effort, Some(ReasoningEffort::High));
        assert!(payload.reasoning.is_none());
    }

    /// The serialized StreamingChatRequest flattens all ChatCompletionRequest fields at top level.
    /// The wrapper adds `stream: true` and `stream_options.include_usage: true`.
    #[test]
    fn streaming_chat_request_serializes_correctly() {
        let request = ChatCompletionRequest {
            model: Some("test-model".into()),
            messages: vec![ChatRequestMessage::user("hello")],
            temperature: Some(0.7),
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            tools: None,
            tool_choice: None,
            search_parameters: None,
            response_format: None,
            reasoning: None,
            thinking: None,
            reasoning_effort: None,
            x_grok_conv_id: None,
            x_grok_req_id: None,
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_transient_retry: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
            traceparent: None,
        };

        let wrapper = StreamingChatRequest {
            inner: &request,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let json: serde_json::Value = serde_json::to_value(&wrapper).unwrap();
        let obj = json.as_object().unwrap();

        assert_eq!(obj.get("stream").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            obj.get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            !obj.keys().any(|k| k.starts_with("x_grok_")),
            "x_grok_* are header fields and must never serialize into the body: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            obj.get("traceparent").is_none(),
            "traceparent rides the span, never the body"
        );

        assert!(
            obj.get("inner").is_none(),
            "inner field should be flattened"
        );
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert!(obj.get("messages").is_some());
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!((temp - 0.7).abs() < 0.001, "temperature should be ~0.7");

        assert!(obj.get("max_tokens").is_none());
        assert!(obj.get("tools").is_none());
    }

    const EMPTY_RESPONSE_JSON: &str = r#"{"id":"resp","object":"response","created_at":0,"model":"test-model","status":"completed","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}"#;

    #[test]
    fn codex_unknown_events_do_not_hide_malformed_known_events() {
        for data in [
            r#"{"type":"keepalive"}"#,
            r#"{"type":"response.future_control"}"#,
        ] {
            let error = deserialize_response_event(data).unwrap_err();
            assert!(is_unknown_response_event(&error, data));
        }
        for data in [
            r#"{"type":"keepalive""#,
            r#"{"type":"response.output_text.delta"}"#,
            r#"{"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":{"type":"future_output"}}"#,
            r#"{"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":{"type":"response.output_item.done"}}"#,
        ] {
            let error = deserialize_response_event(data).unwrap_err();
            assert!(
                !is_unknown_response_event(&error, data),
                "must reject {data}"
            );
        }
    }

    #[tokio::test]
    async fn codex_control_frames_preserve_tool_calls_and_completion() {
        let terminal: serde_json::Value = serde_json::from_str(EMPTY_RESPONSE_JSON).unwrap();
        let frames = [
            serde_json::json!({"type":"keepalive"}),
            serde_json::json!({"type":"response.output_text.delta","sequence_number":1,
                "item_id":"msg_1","output_index":0,"content_index":0,"delta":"Working"}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,
                "item":{"type":"message","id":"msg_1","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Working","annotations":[]}]}}),
            serde_json::json!({"type":"response.future_control","payload":{}}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":3,"output_index":1,
                "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file",
                    "arguments":"{\"path\":\"index.html\"}","status":"completed"}}),
            serde_json::json!({"type":"keepalive"}),
            serde_json::json!({"type":"response.completed","sequence_number":4,"response":terminal}),
        ];
        let wire = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let wire = wire.clone();
                async move {
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(wire))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut client = SamplingClient::new(SamplerConfig {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        })
        .unwrap();
        // Route the Codex client to the fixture server without a real account.
        client.endpoint = EndpointTemplate::new(&format!("http://{addr}/v1"), &IndexMap::new());
        let request = || {
            CreateResponseWrapper::new(rs::CreateResponse {
                input: rs::InputParam::Text("read the file".into()),
                ..Default::default()
            })
        };
        let (raw, metadata, _) = client.create_response_stream(request()).await.unwrap();
        let events: Vec<_> = crate::stream::responses::stream_responses(
            raw,
            metadata,
            crate::types::RequestId::from("codex-keepalive"),
            std::time::Duration::from_secs(5),
            None,
        )
        .collect()
        .await;
        let Some(SamplingEvent::Completed { response, .. }) = events.last() else {
            panic!("stream did not complete: {events:?}");
        };
        assert_eq!(response.assistant_text(), "Working");
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments.as_ref(), "{\"path\":\"index.html\"}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SamplingEvent::ChannelToken { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );

        // Other providers retain their existing strict decoding behavior.
        client.base_url = "https://api.x.ai/v1".into();
        let (mut raw, _, _) = client.create_response_stream(request()).await.unwrap();
        assert!(raw.next().await.unwrap().is_err());
        server.abort();
    }

    #[tokio::test]
    async fn responses_stream_preserves_openrouter_cost_and_model_identity() {
        let terminal = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_openrouter",
                "object": "response",
                "created_at": 0,
                "model": "openrouter/provider-model",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_openrouter",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": "ok",
                        "annotations": []
                    }]
                }],
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 5,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 15,
                    "cost": 0.00012345
                }
            }
        });
        // No context_details: cost must still survive the existing total-token override path.
        let wire = format!("data: {terminal}\n\n");
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let wire = wire.clone();
                async move {
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(wire))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{addr}/v1"),
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        })
        .unwrap();
        let request = CreateResponseWrapper::new(rs::CreateResponse {
            input: rs::InputParam::Text("hello".into()),
            ..Default::default()
        });
        let (raw, metadata, _) = client.create_response_stream(request).await.unwrap();
        let events: Vec<_> = crate::stream::responses::stream_responses(
            raw,
            metadata,
            crate::types::RequestId::from("openrouter-cost"),
            std::time::Duration::from_secs(5),
            None,
        )
        .collect()
        .await;

        let Some(SamplingEvent::Completed { response, .. }) = events.last() else {
            panic!("stream did not complete: {events:?}");
        };
        assert_eq!(response.cost_usd_ticks, Some(1_234_500));
        assert_eq!(response.assistant_text(), "ok");
        assert_eq!(
            response
                .assistant()
                .and_then(|assistant| assistant.model_id.as_deref()),
            Some("openrouter/provider-model")
        );
        server.abort();
    }

    #[test]
    fn codex_request_preserves_instructions_and_other_providers() {
        let original = serde_json::json!({
            "input": [
                {"role": "system", "content": [{"type": "input_text", "text": "Base instructions"}]},
                {"role": "user", "content": "Hello"},
                {"role": "system", "content": "Later instructions"}
            ],
            "reasoning": {"effort": "low"},
            "max_output_tokens": 100,
            "temperature": 1.0,
            "top_p": 0.9
        });
        let mut codex = original.clone();
        patch_codex_response_request("https://chatgpt.com/backend-api/codex/", &mut codex);
        let mut expected = original.clone();
        expected["input"][0]["role"] = serde_json::json!("developer");
        expected["input"][2]["role"] = serde_json::json!("developer");
        for field in ["max_output_tokens", "temperature", "top_p"] {
            expected.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(codex, expected);

        let mut grok = original.clone();
        patch_codex_response_request("https://api.x.ai/v1", &mut grok);
        assert_eq!(grok, original);
    }

    async fn capture_response_body(streaming: bool) -> serde_json::Value {
        let (body_tx, body_rx) = oneshot::channel();
        let body_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(body_tx)));
        let app = Router::new().route(
            "/v1/responses",
            post(move |body: Bytes| {
                let body_tx = body_tx.clone();
                async move {
                    let _ = body_tx.lock().unwrap().take().unwrap().send(body);
                    if streaming {
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from("data: [DONE]\n\n"))
                            .unwrap()
                    } else {
                        axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(EMPTY_RESPONSE_JSON))
                            .unwrap()
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{addr}/v1"),
            api_backend: ApiBackend::Responses,
            extra_response_includes: vec!["no_inline_citations".to_owned()],
            ..minimal_config()
        })
        .unwrap();
        let request = rs::CreateResponse {
            input: rs::InputParam::Text("hi".to_owned()),
            include: Some(vec![rs::IncludeEnum::ReasoningEncryptedContent]),
            tools: Some(vec![rs::Tool::WebSearch(rs::WebSearchTool::default())]),
            ..Default::default()
        };
        let mut wrapper = CreateResponseWrapper::new(request.clone());
        wrapper.extra_tool_entries = vec![serde_json::json!({"type": "x_search"})];
        wrapper.reasoning_effort = Some(ReasoningEffort::Ultra);
        if streaming {
            let (_stream, _model_metadata, _doom_loop_collector) = client
                .create_response_stream(wrapper)
                .await
                .expect("streaming request should succeed");
        } else {
            wrapper.inner.tools = None;
            client
                .create_response(wrapper)
                .await
                .expect("unary request should succeed");
        }
        let body = body_rx.await.unwrap();
        server.abort();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn response_call_sites_emit_final_includes_and_stream_fields() {
        let unary = capture_response_body(false).await;
        assert_eq!(
            Some(&serde_json::json!([
                "reasoning.encrypted_content",
                "no_inline_citations"
            ])),
            unary.get("include"),
        );

        let stream = capture_response_body(true).await;
        for body in [&unary, &stream] {
            assert_eq!(body["reasoning"]["effort"], "max");
            assert_eq!(body["input"][0]["role"], "developer");
        }
        assert_eq!(
            Some(&serde_json::json!([
                "reasoning.encrypted_content",
                "no_inline_citations"
            ])),
            stream.get("include"),
        );
        assert_eq!(Some(true), stream.get("stream").and_then(|v| v.as_bool()));
        assert!(
            stream
                .get("tools")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .any(|tool| tool.get("type") == Some(&serde_json::json!("x_search")))
        );
    }

    const EMPTY_CHAT_COMPLETION_JSON: &str =
        r#"{"id":"chat","object":"chat.completion","created":0,"model":"test-model","choices":[]}"#;
    const EMPTY_MESSAGE_JSON: &str = r#"{"id":"msg","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0}}"#;

    /// One conversation request through `backend` (unary or SSE) against a mock
    /// that hands back the request's headers and raw body.
    async fn capture_request(
        backend: ApiBackend,
        streaming: bool,
        request_compression: RequestCompression,
        input: &str,
    ) -> (axum::http::HeaderMap, Bytes) {
        use distill_sampling_types::{ContentPart, ConversationItem, UserItem};

        let (content_type, reply) = match (streaming, &backend) {
            (true, _) => ("text/event-stream", "data: [DONE]\n\n"),
            (false, ApiBackend::Responses) => ("application/json", EMPTY_RESPONSE_JSON),
            (false, ApiBackend::ChatCompletions) => {
                ("application/json", EMPTY_CHAT_COMPLETION_JSON)
            }
            (false, ApiBackend::Messages) => ("application/json", EMPTY_MESSAGE_JSON),
        };
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let handler = post(move |headers: axum::http::HeaderMap, body: Bytes| {
            let tx = tx.clone();
            async move {
                let _ = tx.lock().unwrap().take().unwrap().send((headers, body));
                axum::response::Response::builder()
                    .header("content-type", content_type)
                    .body(axum::body::Body::from(reply))
                    .unwrap()
            }
        });
        let app = Router::new()
            .route("/v1/chat/completions", handler.clone())
            .route("/v1/responses", handler.clone())
            .route("/v1/messages", handler);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{addr}/v1"),
            api_backend: backend.clone(),
            request_compression,
            ..minimal_config()
        })
        .unwrap();
        let request = ConversationRequest {
            items: vec![ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: Arc::from(input),
                }],
                ..Default::default()
            })],
            ..Default::default()
        };
        let sent = match (streaming, &backend) {
            (false, ApiBackend::ChatCompletions) => client.conversation(request).await.map(drop),
            (true, ApiBackend::ChatCompletions) => {
                client.conversation_stream(request).await.map(drop)
            }
            (false, ApiBackend::Responses) => {
                client.conversation_responses(request).await.map(drop)
            }
            (true, ApiBackend::Responses) => client
                .conversation_stream_responses(request)
                .await
                .map(drop),
            (false, ApiBackend::Messages) => client.conversation_messages(request).await.map(drop),
            (true, ApiBackend::Messages) => {
                client.conversation_stream_messages(request).await.map(drop)
            }
        };
        sent.unwrap_or_else(|e| panic!("{backend:?} streaming={streaming}: {e}"));
        let captured = rx.await.unwrap();
        server.abort();
        captured
    }

    fn large_input() -> String {
        "x".repeat(2 * crate::request_compression::MIN_COMPRESS_BYTES)
    }

    fn header<'a>(headers: &'a axum::http::HeaderMap, name: HeaderName) -> Option<&'a str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn every_chat_route_compresses_large_bodies_when_configured() {
        let input = large_input();
        for backend in [
            ApiBackend::ChatCompletions,
            ApiBackend::Responses,
            ApiBackend::Messages,
        ] {
            for streaming in [false, true] {
                let route = format!("{backend:?} streaming={streaming}");
                let (headers, body) =
                    capture_request(backend.clone(), streaming, RequestCompression::Zstd, &input)
                        .await;
                assert_eq!(Some("zstd"), header(&headers, CONTENT_ENCODING), "{route}");
                assert_eq!(
                    Some("application/json"),
                    header(&headers, CONTENT_TYPE),
                    "{route}: the encoding wraps a JSON body"
                );
                // cli-chat-proxy rejects a zstd body it cannot attribute from headers.
                assert!(
                    header(&headers, HeaderName::from_static("x-grok-model-override"))
                        .is_some_and(|model| !model.is_empty()),
                    "{route}: a compressed body must carry the model override"
                );
                assert!(
                    body.len() < input.len() / 10,
                    "{route}: zstd body should shrink the padding"
                );
                let decoded = String::from_utf8(zstd::decode_all(body.as_ref()).unwrap()).unwrap();
                serde_json::from_str::<serde_json::Value>(&decoded).expect("decoded body is JSON");
                assert!(decoded.contains(&input), "{route}: payload lost");
            }
        }
    }

    /// Bodies past the offload threshold compress on the blocking pool; the
    /// wire result must be indistinguishable from the inline path.
    #[tokio::test]
    async fn offloaded_large_body_compresses_like_the_inline_path() {
        let input = "y".repeat(3 * 1024 * 1024);
        let (headers, body) = capture_request(
            ApiBackend::Responses,
            false,
            RequestCompression::Zstd,
            &input,
        )
        .await;
        assert_eq!(Some("zstd"), header(&headers, CONTENT_ENCODING));
        let decoded = String::from_utf8(zstd::decode_all(body.as_ref()).unwrap()).unwrap();
        assert!(decoded.contains(&input), "payload lost on the offload path");
    }

    #[tokio::test]
    async fn small_body_is_sent_plain_even_when_configured() {
        let (headers, body) = capture_request(
            ApiBackend::Responses,
            false,
            RequestCompression::Zstd,
            "small-plain-body",
        )
        .await;
        assert_eq!(None, header(&headers, CONTENT_ENCODING));
        assert_eq!(Some("application/json"), header(&headers, CONTENT_TYPE));
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("small-plain-body")
        );
    }

    #[tokio::test]
    async fn large_body_stays_plain_when_not_configured() {
        let input = large_input();
        let (headers, body) = capture_request(
            ApiBackend::Responses,
            true,
            RequestCompression::None,
            &input,
        )
        .await;
        assert_eq!(None, header(&headers, CONTENT_ENCODING));
        assert_eq!(Some("application/json"), header(&headers, CONTENT_TYPE));
        assert!(std::str::from_utf8(&body).unwrap().contains(&input));
    }

    #[test]
    fn append_response_includes_preserves_typed_values_and_deduplicates() {
        let typed = [
            "reasoning.encrypted_content",
            "web_search_call.action.sources",
        ];
        let mut body = serde_json::json!({ "include": typed });
        append_response_includes(
            &mut body,
            &[
                "no_inline_citations".to_owned(),
                "no_inline_citations".to_owned(),
            ],
        );
        assert_eq!(
            Some(&serde_json::json!([
                "reasoning.encrypted_content",
                "web_search_call.action.sources",
                "no_inline_citations",
            ])),
            body.get("include"),
        );

        let mut unchanged = serde_json::json!({ "include": typed });
        let expected = unchanged.clone();
        append_response_includes(&mut unchanged, &[]);
        assert_eq!(expected, unchanged);

        for mut body in [
            serde_json::json!({}),
            serde_json::json!({ "include": null }),
        ] {
            append_response_includes(&mut body, &["no_inline_citations".to_owned()]);
            assert_eq!(
                Some(&serde_json::json!(["no_inline_citations"])),
                body.get("include")
            );
        }
    }

    #[test]
    fn extract_retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(30));
    }

    #[test]
    fn extract_retry_after_caps_at_120() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(120));
    }

    #[test]
    fn extract_retry_after_zero_is_valid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(0));
    }

    #[test]
    fn extract_retry_after_ignores_http_date() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Fri, 31 Dec 2025 23:59:59 GMT".parse().unwrap(),
        );
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_retry_after_none_when_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_should_retry_true() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "true".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_true_case_insensitive() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "TRUE".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_false() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(false));
    }

    #[test]
    fn extract_should_retry_unknown_value_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "banana".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn extract_should_retry_absent_is_none() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn new_with_minimal_config_succeeds() {
        let client = SamplingClient::new(minimal_config()).expect("client should construct");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
    }

    #[test]
    fn apply_env_http_headers_resolves_trims_skips_and_overrides() {
        let mut map = IndexMap::new();
        map.insert("x-tenant-token".to_string(), "TENANT".to_string());
        map.insert("x-blank".to_string(), "BLANK".to_string());
        map.insert("x-missing".to_string(), "MISSING".to_string());
        map.insert("x-override".to_string(), "OVERRIDE".to_string());
        map.insert("x invalid".to_string(), "INVALID".to_string());

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-override"),
            HeaderValue::from_static("static"),
        );

        apply_env_http_headers(
            &map,
            |var| match var {
                // Leading space and trailing newline exercise trimming
                "TENANT" => Some(" tenant-secret\n".to_string()),
                "BLANK" => Some("   ".to_string()),
                "OVERRIDE" => Some("from-env".to_string()),
                "INVALID" => Some("value".to_string()),
                _ => None,
            },
            &mut headers,
        );

        assert_eq!(headers.get("x-tenant-token").unwrap(), "tenant-secret");
        assert!(headers.get("x-blank").is_none());
        assert!(headers.get("x-missing").is_none());
        // A resolved env value overrides an existing header of the same name.
        assert_eq!(headers.get("x-override").unwrap(), "from-env");
        // An invalid header name is skipped rather than panicking.
        assert!(headers.get("x invalid").is_none());
    }

    #[test]
    fn endpoint_appends_path_before_a_base_url_query_without_configured_params() {
        let template =
            EndpointTemplate::new("https://gateway.example/v1?api-version=x", &IndexMap::new());
        let url = template.url_for_path("responses");
        assert!(
            url.starts_with("https://gateway.example/v1/responses?"),
            "url: {url}"
        );
        assert!(url.contains("api-version=x"), "url: {url}");
        assert!(!url.contains("x/responses"), "url: {url}");
    }

    #[test]
    fn messages_plus_anthropic_api_key_uses_x_api_key_and_not_authorization() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_some()
        );
        assert!(client.default_headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn messages_plus_bearer_uses_authorization_and_not_x_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("bearer-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.default_headers.get(AUTHORIZATION).is_some());
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_none()
        );
    }

    // Regression: a past change dropped User-Agent from sampling requests.
    #[test]
    fn sampling_client_always_has_user_agent() {
        let client = SamplingClient::new(minimal_config()).expect("build");
        assert!(client.default_headers.contains_key(USER_AGENT));
    }

    // Regression: a past change dropped HeaderInjector (traceparent) from sampling requests.
    #[test]
    fn header_injector_is_called_in_post() {
        #[derive(Debug)]
        struct TestInjector;
        impl crate::config::HeaderInjector for TestInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    HeaderName::from_static("traceparent"),
                    HeaderValue::from_static("00-test-trace-id-00"),
                );
            }
        }

        let mut config = minimal_config();
        config.header_injector = Some(std::sync::Arc::new(TestInjector));
        let client = SamplingClient::new(config).expect("build");
        let SentRequest { builder, .. } = client.post("http://localhost/test");
        let req = builder.build().expect("build request");
        assert!(
            req.headers().contains_key("traceparent"),
            "HeaderInjector should inject traceparent into post() requests"
        );
    }

    /// Nothing listens on port 1: each send fails right after the hook runs.
    #[tokio::test]
    async fn stream_span_adopts_request_traceparent_on_every_backend() {
        #[derive(Debug)]
        struct RecordingInjector(tokio::sync::mpsc::UnboundedSender<String>);
        impl crate::config::HeaderInjector for RecordingInjector {
            fn inject(&self, _headers: &mut HeaderMap) {}
            fn set_span_parent(&self, _span: &tracing::Span, traceparent: &str) {
                self.0
                    .send(traceparent.to_owned())
                    .expect("test receiver alive");
            }
        }

        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let injector = Arc::new(RecordingInjector(seen_tx));
        let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let client = {
            let mut config = minimal_config();
            config.base_url = "http://127.0.0.1:1".to_string();
            config.header_injector = Some(injector);
            SamplingClient::new(config).expect("build")
        };
        let request = || ConversationRequest {
            items: vec![distill_sampling_types::ConversationItem::user("hi")],
            traceparent: Some(traceparent.to_owned()),
            ..Default::default()
        };

        // Second registered dispatcher: other tests' threads cannot cache callsite interest as
        // `never` for this one.
        let _interest_pin = tracing::Dispatch::new(tracing_subscriber::Registry::default());
        let disabled =
            tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
        let refused = client.conversation_stream(request()).await;
        assert!(refused.is_err(), "chat completions: port 1 refuses");
        assert!(
            seen_rx.try_recv().is_err(),
            "a disabled span must not reach the hook"
        );
        drop(disabled);

        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::Registry::default());
        let refused = client.conversation_stream(request()).await;
        assert!(refused.is_err(), "chat completions: port 1 refuses");
        let refused = client.conversation_stream_responses(request()).await;
        assert!(refused.is_err(), "responses: port 1 refuses");
        let refused = client.conversation_stream_messages(request()).await;
        assert!(refused.is_err(), "messages: port 1 refuses");

        let mut seen = Vec::new();
        while let Ok(tp) = seen_rx.try_recv() {
            seen.push(tp);
        }
        assert_eq!(vec![traceparent; 3], seen);
    }

    #[test]
    fn user_agent_includes_origin_and_agent_product() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: Some("1.2.3".to_string()),
        };
        let ua = user_agent_string_for(&origin);
        assert!(ua.contains("my-client/1.2.3"));
        assert!(ua.contains(AGENT_PRODUCT));
    }

    #[test]
    fn user_agent_omits_origin_version_when_absent() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: None,
        };
        let ua = user_agent_string_for(&origin);
        // No slash between product and the grok-shell agent product.
        assert!(ua.starts_with("my-client grok-shell/"));
    }

    #[test]
    fn user_agent_collapses_when_origin_matches_agent() {
        let agent_version = distill_version::VERSION.to_string();
        let origin = OriginClientInfo {
            product: AGENT_PRODUCT.to_string(),
            version: Some(agent_version.clone()),
        };
        let ua = user_agent_string_for(&origin);
        // Single product/version slot when the origin and agent match.
        assert!(ua.starts_with(&format!("{}/{}", AGENT_PRODUCT, agent_version)));
    }

    /// Counts callbacks for assertions in the tests below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<Vec<(crate::attribution::SamplingConsumer, Option<String>)>>,
    }

    #[derive(Debug)]
    struct StaticBearerResolver(&'static str);

    impl crate::config::BearerResolver for StaticBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(
            &self,
            consumer: crate::attribution::SamplingConsumer,
            sent_bearer: Option<&str>,
        ) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, sent_bearer.map(|s| s.to_string())));
        }
    }

    /// `post()` strips the `"Bearer "` scheme prefix off `Authorization` and captures the tail fragment (see `BEARER_SUFFIX_LEN`).
    #[test]
    fn post_captures_bearer_tail_for_openai_compat() {
        let cfg = SamplerConfig {
            api_key: Some("test-bearer-1234567890".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/chat/completions");
        assert_eq!(bearer.as_deref(), Some("r-1234567890"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
        );
    }

    /// `post()` captures `x-api-key` for Messages-API backends and keeps the value's tail fragment.
    #[test]
    fn post_captures_x_api_key_tail_for_messages() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/messages");
        assert_eq!(bearer.as_deref(), Some("c-key-abc123"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
        );
    }

    /// `post()` captures `None` when the request carries no auth header.
    #[test]
    fn post_captures_none_when_no_header() {
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/chat/completions");
        assert!(bearer.is_none());
    }

    /// The race this design closes: a 401 triggers a recovery that rotates the resolver.
    /// A record-time re-read would then attribute a bearer the rejected request never carried.
    /// The attributed fragment must be the one captured when the request was built.
    #[test]
    fn post_capture_is_immune_to_resolver_rotation_after_build() {
        #[derive(Debug)]
        struct RotatingResolver(std::sync::Mutex<String>);
        impl crate::config::BearerResolver for RotatingResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.lock().unwrap().clone())
            }
        }

        let resolver = std::sync::Arc::new(RotatingResolver(std::sync::Mutex::new(
            "rejected-token-oldtail1".to_string(),
        )));
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver.clone()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        let SentRequest {
            sent_bearer: sent_at_build,
            ..
        } = client.post("https://example.test/v1/responses");
        // The 401 kicks recovery; the resolver rotates before the callback runs.
        *resolver.0.lock().unwrap() = "fresh-token-newtail99".to_string();

        assert_eq!(
            sent_at_build.as_deref(),
            Some("ken-oldtail1"),
            "attribution must describe the bearer the rejected request carried"
        );
        // A record-time re-read would report the rotated token, not the build-time capture.
        assert_eq!(
            client.current_sent_bearer_suffix().as_deref(),
            Some("en-newtail99"),
            "sanity: the build-time capture and a live re-read now differ"
        );
    }

    #[test]
    fn live_bearer_resolver_uses_authorization_for_messages_plus_bearer() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest { builder, .. } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some("Bearer fresh-bearer"));
        assert!(request.headers().get("x-api-key").is_none());
    }

    /// Regression: `api_key` seeds `default_headers` with `Authorization: Bearer ...`.
    /// With a `bearer_resolver` also set, `post()` must produce exactly one `Authorization` header on the wire.
    /// `RequestBuilder::header(AUTHORIZATION, ...)` appends rather than replaces, causing two identical headers and a 400 from cli-chat-proxy.
    #[test]
    fn post_emits_single_authorization_with_api_key_and_bearer_resolver() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Responses,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest { builder, .. } = client.post("https://example.test/v1/responses");
        let request = builder.build().expect("request should build");
        let auth_count = request.headers().get_all(AUTHORIZATION).iter().count();
        assert_eq!(
            auth_count, 1,
            "expected exactly one Authorization header, got {auth_count}"
        );
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer fresh-bearer"),
        );
    }

    #[test]
    fn live_bearer_resolver_uses_x_api_key_for_messages_plus_anthropic_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("stale-anthropic".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-anthropic"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest { builder, .. } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        let api_key = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        assert_eq!(api_key, Some("fresh-anthropic"));
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    /// The callback receives the `post()`-captured fragment only; the full bearer never crosses the crate boundary.
    #[test]
    fn record_401_attribution_invokes_callback_with_captured_bearer() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let cfg = SamplerConfig {
            api_key: Some("the-bearer-1234567890-extra-tail".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: Some(cb_dyn),
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest { sent_bearer, .. } =
            client.post("https://example.test/v1/chat/completions");
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletionsStream,
            sent_bearer.as_deref(),
        );
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            nth(&calls, 0).0,
            crate::attribution::SamplingConsumer::ChatCompletionsStream
        );
        assert_eq!(nth(&calls, 0).1.as_deref(), Some("0-extra-tail"));
        assert_eq!(
            nth(&calls, 0).1.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
        );
    }

    /// When a bearer_resolver is wired but returns `None`, attribution must report no sent bearer (not the construction-time default header seed).
    #[test]
    fn bearer_resolver_none_attribution_ignores_default_headers() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-seed-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert_eq!(
            client.current_sent_bearer_suffix(),
            None,
            "resolver None must not attribute a stripped default seed"
        );
    }

    /// A wired bearer_resolver that returns `None` means a hard-expired session with no live access token.
    /// Default Authorization / x-api-key must be stripped so a stale seed key cannot ride the wire.
    #[test]
    fn bearer_resolver_none_strips_default_authorization() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            builder,
            sent_bearer: sent,
        } = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");
        assert_eq!(sent, None, "capture must agree: nothing was sent");
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "stale default Authorization must not be sent when resolver is empty"
        );
    }

    /// `response.completed` carrying `usage.context_details.{input_tokens, output_tokens}` rewrites `usage.total_tokens` in place.
    /// The new value is the live context length (`ctx.input + ctx.output`).
    /// Billing fields stay on the wire's cumulative values.
    #[test]
    fn deserialize_response_event_overrides_total_tokens_from_context_details() {
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "distill",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022,
                        "output_tokens": 571
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        // Billing fields stay cumulative, unchanged by context_details
        assert_eq!(usage.input_tokens, 6003);
        assert_eq!(usage.output_tokens, 711);
        assert_eq!(usage.input_tokens_details.cached_tokens, 1984);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 388);
        // total_tokens is rewritten to ctx.input + ctx.output (5022 + 571), not the wire's cumulative total (6714)
        assert_eq!(usage.total_tokens, 5_593);
    }

    #[test]
    fn deserialize_response_event_stashes_cost_in_metadata() {
        let make = |cost: Option<serde_json::Value>, legacy_ticks: Option<i64>| {
            let mut usage = serde_json::json!({
                "input_tokens": 10,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": 5,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 15
            });
            if let Some(cost) = cost {
                usage["cost"] = cost;
            }
            if let Some(ticks) = legacy_ticks {
                usage["cost_in_usd_ticks"] = serde_json::json!(ticks);
            }
            serde_json::json!({
                "type": "response.completed",
                "sequence_number": 0,
                "response": {
                    "id": "resp_1", "object": "response", "created_at": 0,
                    "model": "distill", "status": "completed", "output": [],
                    "usage": usage
                }
            })
            .to_string()
        };
        let metadata_cost = |sse: String| {
            let event = deserialize_response_event(&sse).expect("parse");
            let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
                panic!("expected ResponseCompleted");
            };
            e.response
                .metadata
                .and_then(|metadata| metadata.get(COST_USD_TICKS_METADATA_KEY).cloned())
        };

        assert_eq!(
            metadata_cost(make(Some(serde_json::json!(0.00012345)), None)).as_deref(),
            Some("1234500")
        );
        // An authoritative USD zero is a known free call, not an unknown cost.
        assert_eq!(
            metadata_cost(make(Some(serde_json::json!(0.0)), None)).as_deref(),
            Some("0")
        );
        // Missing and malformed USD costs remain unknown.
        assert_eq!(metadata_cost(make(None, None)), None);
        assert_eq!(
            metadata_cost(make(Some(serde_json::json!("not-a-cost")), None)),
            None
        );
        // Legacy ticks still work, while the REST mapper's zero backfill is unknown.
        assert_eq!(metadata_cost(make(None, Some(78))).as_deref(), Some("78"));
        assert_eq!(metadata_cost(make(None, Some(0))), None);
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_absent() {
        // Older / non-Responses backends omit `context_details`.
        // `total_tokens` passes through from the wire unchanged.
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "distill",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 10000,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 100,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 10100
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 10_100);
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_partial() {
        // Defensive: if the backend ever ships only one of the two context_details fields, we can't know the live context size
        // Leave `total_tokens` on the wire's cumulative value instead of guessing; treating the missing half as 0 would silently under-report
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "distill",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 6_714);
    }

    #[test]
    fn deserialize_response_event_ignores_context_details_on_non_terminal_events() {
        // Non-terminal events don't carry final usage; even if the backend ever echoed `context_details` on one, we don't touch it
        let sse = r#"{
            "type": "response.output_text.delta",
            "sequence_number": 0,
            "item_id": "item-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello",
            "logprobs": []
        }"#;
        let event = deserialize_response_event(sse).expect("non-terminal event parses");
        assert!(matches!(
            event,
            rs::ResponseStreamEvent::ResponseOutputTextDelta(_)
        ));
    }

    /// A request as the builder emits it: `reasoning.summary` already set to the built-in default.
    fn built_response_request() -> CreateResponseWrapper {
        CreateResponseWrapper::new(rs::CreateResponse {
            reasoning: Some(rs::Reasoning {
                effort: Some(rs::ReasoningEffort::High),
                summary: Some(rs::ReasoningSummary::Concise),
            }),
            ..Default::default()
        })
    }

    fn client_with_summary(
        summary: Option<distill_sampling_types::ReasoningSummary>,
    ) -> SamplingClient {
        SamplingClient::new(SamplerConfig {
            reasoning_summary: summary,
            ..minimal_config()
        })
        .expect("client should construct")
    }

    #[test]
    fn reasoning_summary_unset_keeps_the_built_request() {
        let client = client_with_summary(None);
        let mut request = built_response_request();
        client.apply_response_defaults(&mut request).unwrap();
        let reasoning = request.inner.reasoning.expect("reasoning block kept");
        assert_eq!(reasoning.effort, Some(rs::ReasoningEffort::High));
        assert_eq!(reasoning.summary, Some(rs::ReasoningSummary::Concise));
    }

    #[test]
    fn reasoning_summary_none_omits_the_field_but_keeps_effort() {
        let client = client_with_summary(Some(distill_sampling_types::ReasoningSummary::None));
        let mut request = built_response_request();
        client.apply_response_defaults(&mut request).unwrap();
        let body = serde_json::to_value(&request.inner).unwrap();
        assert_eq!(
            body.get("reasoning"),
            Some(&serde_json::json!({ "effort": "high" }))
        );
        let reasoning = request
            .inner
            .reasoning
            .expect("reasoning block kept for effort");
        assert_eq!(reasoning.effort, Some(rs::ReasoningEffort::High));
        assert_eq!(reasoning.summary, None);
    }

    #[test]
    fn reasoning_summary_override_replaces_the_built_value() {
        let client = client_with_summary(Some(distill_sampling_types::ReasoningSummary::Detailed));
        let mut request = built_response_request();
        client.apply_response_defaults(&mut request).unwrap();
        assert_eq!(
            request.inner.reasoning.unwrap().summary,
            Some(rs::ReasoningSummary::Detailed)
        );
    }

    #[test]
    fn reasoning_summary_adds_a_reasoning_block_only_when_there_is_something_to_send() {
        let with_summary =
            client_with_summary(Some(distill_sampling_types::ReasoningSummary::Auto));
        let mut request = CreateResponseWrapper::new(rs::CreateResponse::default());
        with_summary.apply_response_defaults(&mut request).unwrap();
        assert_eq!(
            request.inner.reasoning,
            Some(rs::Reasoning {
                effort: None,
                summary: Some(rs::ReasoningSummary::Auto),
            })
        );

        let without = client_with_summary(Some(distill_sampling_types::ReasoningSummary::None));
        let mut request = CreateResponseWrapper::new(rs::CreateResponse::default());
        without.apply_response_defaults(&mut request).unwrap();
        assert_eq!(request.inner.reasoning, None);
    }
}
