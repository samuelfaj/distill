// Modified for Distill by Samuel Fajreldines, 2026.
use crate::sampling::{
    ApiBackend, ChatCompletionRequest, ChatRequestMessage, Client as OaiCompatClient,
    ConversationRequest, ConversationToolChoice, HostedTool, SamplingError, ToolChoice,
    ToolDefinition, ToolSpec, conversation_to_chat_messages,
};
use agent_client_protocol as acp;
use async_openai::types::responses::{Response, ResponseStreamEvent};
use distill_sampler::SamplerConfig as SamplingConfig;
use distill_sampling_types::TokenUsage;
use distill_workspace::jev::types::UsageBilling;
use distill_workspace::jev::types::{
    AttemptGuard, AttemptObserver, AttemptStatus, Usage as JevUsage,
};
use futures_util::StreamExt;
use reqwest::StatusCode;

// Re-export compaction utilities from distill-chat-state so existing callers that import from this module continue to work
pub use distill_chat_state::compaction_utils::{
    AUTO_CONTINUE_PROMPT, extract_last_real_user_query, extract_last_user_query,
    extract_messages_since_last_user, extract_real_user_queries, is_synthetic_extracted_query,
};

/// Outcome of a failed `generate_session_compact` call, classified at the point of the typed upstream error.
/// The caller can short-circuit retries without re-parsing free-form error strings.
#[derive(Debug)]
pub(crate) enum CompactFailure {
    /// Retrying the same payload will hit the same failure.
    /// The retry loop in `run_compact_inner` should bail without sleeping or re-issuing.
    Deterministic(acp::Error),
    /// Deterministic size overflow: the same payload cannot help, but a
    /// smaller one can — the caller steps down its input ladder.
    Overflow(acp::Error),
    /// Failure may resolve on retry. The caller follows its existing
    /// N-attempt + backoff loop.
    Transient(acp::Error),
    /// User/stop cancelled the in-flight compact. Do not retry or suppress AUTO.
    Cancelled,
}

/// Stable cancel payload; the pager matches it to route manual `/compact` to "Compaction cancelled." instead of a failure.
pub const COMPACT_CANCELLED_MSG: &str = "compact cancelled";

/// Stamped on every compaction failure payload; the user-facing normalizer strips it.
pub(crate) const COMPACT_FAILED_PREFIX: &str = "compact failed: ";

/// Cancel-vs-failure discriminator in the compact RPC error's `data` (`{"kind": …, "message": …}`).
/// The pager routes on this, never the message text (upstream bodies can echo the cancel phrase).
/// The protocol's `RequestCancelled` code is feature-gated unstable and cancel-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactErrorKind {
    Cancelled,
    Failed,
}

impl CompactErrorKind {
    fn wire(self) -> &'static str {
        match self {
            CompactErrorKind::Cancelled => "compact_cancelled",
            CompactErrorKind::Failed => "compact_failed",
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "compact_cancelled" => Some(Self::Cancelled),
            "compact_failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Byte cap (truncation marker included) on the user-facing compaction error detail.
pub(crate) const COMPACT_ERROR_DETAIL_MAX_BYTES: usize = 300;

/// The one normalize sequence for user-facing compaction error details: single-line, scrub service names, cap.
/// Idempotent, so the wire chokepoint below can re-run it on pre-normalized text.
/// URLs stay: for custom-endpoint users the URL is the diagnosis.
pub(crate) fn normalize_compact_detail(raw: &str) -> String {
    let single_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let scrubbed = crate::sampling::error::rewrite_service_names(&single_line);
    distill_tools::util::truncate_str_with_marker(&scrubbed, COMPACT_ERROR_DETAIL_MAX_BYTES)
        .into_owned()
}

/// Typed compact-error `data` payload.
/// `message` is the key [`crate::sampling::error::error_detail_from_data`] reads first, so text-only consumers see the plain detail.
/// Normalized here at the wire boundary: typed-kind pagers render it verbatim, so no producer can ship raw upstream text.
pub fn compact_error_data(kind: CompactErrorKind, message: &str) -> serde_json::Value {
    serde_json::json!({ "kind": kind.wire(), "message": normalize_compact_detail(message) })
}

/// Read the typed discriminator back.
/// `None` for payloads from shells that predate it (bare strings) or for foreign shapes.
pub fn compact_error_kind(err: &acp::Error) -> Option<CompactErrorKind> {
    CompactErrorKind::from_wire(err.data.as_ref()?.get("kind")?.as_str()?)
}

impl CompactFailure {
    pub(crate) fn cancelled_error() -> acp::Error {
        acp::Error::internal_error().data(compact_error_data(
            CompactErrorKind::Cancelled,
            COMPACT_CANCELLED_MSG,
        ))
    }
}

// Single definition so turn-path and compaction size detection can't drift.
pub(crate) use distill_compaction::is_context_length_error;

/// Classify an upstream `SamplingError` for the compaction retry loop.
/// Size overflows (HTTP 413 by status, or size-worded error text) classify as [`CompactFailure::Overflow`] so the caller's input ladder engages.
/// Re-issuing the same request cannot change the outcome: auth state, config, payload shape, and stuck-model conditions all persist.
fn classify_sampling_error(err: SamplingError) -> CompactFailure {
    let acp_err = acp::Error::internal_error().data(format!("{COMPACT_FAILED_PREFIX}{err}"));
    // Size beats the generic 4xx rule so the input ladder sees it; 413 matches by status because proxies send it with generic body text.
    // Deliberately not laddering on `is_likely_body_rejected()`: the same signal fires on real network resets.
    if err.is_payload_too_large() || err.is_context_length_error() {
        return CompactFailure::Overflow(acp_err);
    }
    let deterministic = match &err {
        SamplingError::Auth { .. }
        | SamplingError::InvalidConfiguration(_)
        | SamplingError::MtlsConfiguration(_)
        | SamplingError::Serialization(_)
        | SamplingError::IdleTimeout { .. } => true,
        SamplingError::Api { status, .. } => {
            status.is_client_error()
                && *status != StatusCode::REQUEST_TIMEOUT
                && *status != StatusCode::TOO_MANY_REQUESTS
        }
        SamplingError::MaxTokensTruncation => true,
        // Loops are stochastic at sampling temperature; a retry may differ.
        SamplingError::Http(_)
        | SamplingError::EventStreamError(_)
        | SamplingError::StreamError { .. }
        | SamplingError::EmptyResponse { .. }
        | SamplingError::DoomLoopDetected { .. } => false,
    };
    if deterministic {
        CompactFailure::Deterministic(acp_err)
    } else {
        CompactFailure::Transient(acp_err)
    }
}

/// Classify a provider-style stream error event (`ResponseError` / `ResponseFailed.error`) for the compaction retry loop.
/// `code` is the structured `code` field on the event (typically a numeric HTTP status as a string, or an error-type string like `"invalid_request_error"`).
/// `message` is the human-readable detail.
fn classify_response_event_error(code: Option<&str>, message: &str) -> CompactFailure {
    let acp_err = acp::Error::internal_error().data(match code {
        Some(c) => format!("{COMPACT_FAILED_PREFIX}{c}: {message}"),
        None => format!("{COMPACT_FAILED_PREFIX}{message}"),
    });

    // Size intentionally outranks the `invalid_request_error` marker below: real overflows wear that marker WITH size text, so letting the marker veto the text would strand them off the ladder.
    // Residual echo risk is accepted — sticky Size is recoverable via manual /compact or rewind.
    if code.is_some_and(distill_sampling_types::is_size_overflow_error_code)
        || is_context_length_error(message)
    {
        return CompactFailure::Overflow(acp_err);
    }

    if matches!(code, Some("invalid_request_error")) || message.contains("invalid_request_error") {
        return CompactFailure::Deterministic(acp_err);
    }

    if let Some(status_code) = code.and_then(|c| c.parse::<u16>().ok())
        && (400..500).contains(&status_code)
        && status_code != 408
        && status_code != 429
    {
        return CompactFailure::Deterministic(acp_err);
    }

    CompactFailure::Transient(acp_err)
}

/// Build the bare summarization prompt text without appending it to history.
/// The text lives in `distill-compaction`, so every harness sends the same prompt.
pub(crate) fn build_compaction_prompt(
    user_context: Option<&str>,
    use_short_prompt: bool,
) -> String {
    let kind = if use_short_prompt {
        distill_compaction::SummaryPromptKind::SelfSummary
    } else {
        distill_compaction::SummaryPromptKind::Structured
    };
    distill_compaction::build_summary_prompt_kind(kind, user_context)
}

/// Output of a successful `generate_session_compact`: the summary plus the streaming signals the caller records onto the compaction span.
/// `truncated` is derived from the backend's typed stop reason; `stop_reason` is kept as the raw provider string for drill-down.
/// Latency is captured online (no per-token buffer); fleet percentiles are computed at query time.
pub(crate) struct CompactOutput {
    pub content: String,
    pub stop_reason: Option<String>,
    pub truncated: bool,
    pub ttft_ms: Option<u64>,
    pub stream_ms: Option<u64>,
    pub delta_count: u64,
    pub itl_max_ms: Option<u64>,
    pub usage: Option<TokenUsage>,
    pub cost_usd_ticks: Option<i64>,
    pub request_id: Option<String>,
    pub response_model: Option<String>,
}

impl CompactOutput {
    pub(crate) fn model_wait_ms(&self) -> Option<u64> {
        match (self.ttft_ms, self.stream_ms) {
            (None, None) => None,
            (ttft, stream) => Some(ttft.unwrap_or(0).saturating_add(stream.unwrap_or(0))),
        }
    }
}

/// Converted to a stable string only at the tracing boundary (tracing can't record a custom type directly).
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum CompactionOutcome {
    Success,
    Truncated,
    Deterministic,
    Transient,
    Degenerate,
    Failed,
}
/// O(1) streaming-latency accumulator: time-to-first-token, total stream span, delta count, and worst inter-token gap.
/// Everything is computed online so we never buffer per-token timestamps.
/// Fleet percentiles are computed at query time in log analytics.
struct StreamTiming {
    start: std::time::Instant,
    first: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
    count: u64,
    max_gap_ms: u64,
}

impl StreamTiming {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            first: None,
            last: None,
            count: 0,
            max_gap_ms: 0,
        }
    }

    fn record_delta(&mut self) {
        let now = std::time::Instant::now();
        if self.first.is_none() {
            self.first = Some(now);
        }
        if let Some(prev) = self.last {
            self.max_gap_ms = self
                .max_gap_ms
                .max(now.duration_since(prev).as_millis() as u64);
        }
        self.last = Some(now);
        self.count += 1;
    }

    fn ttft_ms(&self) -> Option<u64> {
        self.first
            .map(|f| f.duration_since(self.start).as_millis() as u64)
    }

    fn stream_ms(&self) -> Option<u64> {
        match (self.first, self.last) {
            (Some(f), Some(l)) => Some(l.duration_since(f).as_millis() as u64),
            _ => None,
        }
    }

    /// Worst inter-token gap; `None` until there are at least two deltas.
    fn itl_max_ms(&self) -> Option<u64> {
        if self.count >= 2 {
            Some(self.max_gap_ms)
        } else {
            None
        }
    }

    /// Wall-clock seconds since the stream started; drives the compaction wall-clock budget (the backstop against runaway reasoning).
    fn elapsed_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}

fn jev_usage_from_tokens(usage: &TokenUsage) -> JevUsage {
    JevUsage {
        input_tokens: Some(u64::from(usage.prompt_tokens)),
        output_tokens: Some(u64::from(usage.completion_tokens)),
    }
}

fn jev_billing_from_tokens(usage: &TokenUsage, cost_usd_ticks: Option<i64>) -> UsageBilling {
    UsageBilling {
        cached_input_tokens: Some(u64::from(usage.cached_prompt_tokens)),
        cache_creation_input_tokens: Some(u64::from(usage.cache_creation_prompt_tokens)),
        reasoning_tokens: Some(u64::from(usage.reasoning_tokens)),
        cost_usd_ticks,
    }
}

fn merge_normalized_cost(
    previous: Option<i64>,
    usage: &distill_sampling_types::Usage,
) -> Option<i64> {
    match (previous, usage.normalized_cost_ticks()) {
        (_, Some(cost)) => Some(cost),
        (previous, None) => previous,
    }
}

fn responses_usage(response: &Response) -> Option<TokenUsage> {
    response.usage.as_ref().map(|usage| TokenUsage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        reasoning_tokens: usage.output_tokens_details.reasoning_tokens,
        cached_prompt_tokens: usage.input_tokens_details.cached_tokens,
        cache_creation_prompt_tokens: 0,
    })
}

fn responses_cost(response: &Response) -> Option<i64> {
    response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("xai.cost_usd_ticks"))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|&value| value >= 0)
}

fn retain_nonempty_identity(previous: &mut Option<String>, candidate: &str) {
    if !candidate.is_empty() {
        *previous = Some(candidate.to_owned());
    }
}

fn update_response_attempt(
    guard: &mut Option<AttemptGuard>,
    response: &Response,
    usage: &mut Option<TokenUsage>,
    cost_usd_ticks: &mut Option<i64>,
    response_id: &mut Option<String>,
    response_model: &mut Option<String>,
) {
    retain_nonempty_identity(response_id, &response.id);
    retain_nonempty_identity(response_model, &response.model);
    if let Some(value) = responses_usage(response) {
        *usage = Some(value);
    }
    if let Some(value) = responses_cost(response) {
        *cost_usd_ticks = Some(value);
    }
    if let Some(guard) = guard.as_mut() {
        guard.set_response(
            response_id.clone(),
            response_model.clone(),
            usage.as_ref().map(jev_usage_from_tokens),
        );
        if let Some(value) = usage.as_ref() {
            guard.set_billing(jev_billing_from_tokens(value, *cost_usd_ticks));
        }
    }
}

fn messages_usage(usage: &distill_sampling_types::messages::MessagesUsage) -> TokenUsage {
    let cached = usage.cache_read_input_tokens;
    let cache_creation = usage.cache_creation_input_tokens;
    TokenUsage {
        prompt_tokens: usage
            .input_tokens
            .saturating_add(cached)
            .saturating_add(cache_creation),
        completion_tokens: usage.output_tokens,
        total_tokens: usage
            .input_tokens
            .saturating_add(cached)
            .saturating_add(cache_creation)
            .saturating_add(usage.output_tokens),
        reasoning_tokens: 0,
        cached_prompt_tokens: cached,
        cache_creation_prompt_tokens: cache_creation,
    }
}

enum StreamStep<T> {
    Item(T),
    Ended,
    IdleTimeout,
}

async fn next_stream_step<S, T>(
    stream: &mut S,
    idle_timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<StreamStep<T>, CompactFailure>
where
    S: futures_util::Stream<Item = T> + Unpin,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CompactFailure::Cancelled),
        step = tokio::time::timeout(idle_timeout, stream.next()) => Ok(match step {
            Ok(Some(item)) => StreamStep::Item(item),
            Ok(None) => StreamStep::Ended,
            Err(_) => StreamStep::IdleTimeout,
        }),
    }
}

/// Abort `fut` if stop wins while the compact HTTP stream is still opening.
async fn await_unless_cancelled<F, T>(
    cancel: &tokio_util::sync::CancellationToken,
    fut: F,
) -> Result<T, CompactFailure>
where
    F: std::future::Future<Output = T>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CompactFailure::Cancelled),
        result = fut => Ok(result),
    }
}

#[cfg(test)]
#[path = "session_compact_compact_cancel_await_tests.rs"]
mod compact_cancel_await_tests;

/// Smallest output budget worth sending; below it the request keeps the
/// route's own reservation and its overflow walks the input ladder.
const MIN_COMPACTION_OUTPUT_TOKENS: u64 = 1_024;

/// The output budget a compaction request reserves when the route's ceiling
/// would not fit next to `input_tokens`: the window minus the input and a
/// margin that grows with it. `None` keeps the route's own value. Without it a
/// 943K catalogue ceiling plus a 600K summary input overflowed a 1M window, so
/// every two-pass pass 1 failed with a tokenless 400.
fn compaction_output_cap(config: &SamplingConfig, input_tokens: u64) -> Option<u32> {
    let configured = distill_sampler::effective_conversation_output_tokens(
        config,
        &ConversationRequest::default(),
    )?;
    if config.context_window == 0 {
        return None;
    }
    let margin = 2_048u64.max(input_tokens / 10);
    let available = config
        .context_window
        .saturating_sub(input_tokens)
        .saturating_sub(margin);
    if available < MIN_COMPACTION_OUTPUT_TOKENS {
        return None;
    }
    let bounded = configured.min(u32::try_from(available).unwrap_or(u32::MAX));
    (bounded < configured).then_some(bounded)
}

#[cfg(test)]
mod output_cap_tests {
    use super::*;

    fn route(context_window: u64, ceiling: Option<u32>) -> SamplingConfig {
        SamplingConfig {
            context_window,
            max_completion_tokens: ceiling,
            ..Default::default()
        }
    }

    /// The muse-spark failure: 627K of input plus the 943K catalogue ceiling
    /// asked for 1.58M tokens of a 1M window. The cap must make input plus
    /// output fit, with room for estimator drift.
    #[test]
    fn a_ceiling_that_cannot_fit_beside_the_input_is_bounded() {
        let config = route(1_048_576, Some(943_718));
        let input = 636_227;
        let cap = compaction_output_cap(&config, input).expect("bounded");
        assert!(input + u64::from(cap) + input / 10 <= 1_048_576, "cap {cap}");
        assert!(cap >= 300_000, "a summary still has ample room: {cap}");
    }

    /// Fallback: when the ceiling already fits, the request is exactly today's.
    #[test]
    fn a_ceiling_that_fits_is_left_alone() {
        assert_eq!(compaction_output_cap(&route(1_048_576, Some(32_768)), 600_000), None);
        assert_eq!(compaction_output_cap(&route(0, Some(943_718)), 600_000), None);
        assert_eq!(compaction_output_cap(&route(1_048_576, None), 600_000), None);
    }

    /// An input that alone fills the window keeps today's request: its
    /// overflow error is what steps the input ladder down, and a 1-token
    /// budget would only come back truncated.
    #[test]
    fn an_input_that_fills_the_window_keeps_the_overflow_path() {
        assert_eq!(compaction_output_cap(&route(200_000, Some(64_000)), 199_000), None);
    }
}

/// `chat_history` must already include the summarization prompt as its final user message.
/// The split lets callers persist the exact request payload before issuing it.
/// Omitting them would shift the entire prefix and force a full prefill on the summarizer call.
pub(crate) async fn generate_session_compact(
    chat_history: impl Into<
        crate::session::helpers::prepared_compaction_history::CompactionHistoryInput,
    >,
    compaction_tool_tokens: u64,
    tools: Vec<ToolSpec>,
    hosted_tools: Vec<HostedTool>,
    client: OaiCompatClient,
    session_id: acp::SessionId,
    sampling_config: &SamplingConfig,
    idle_timeout: std::time::Duration,
    wall_clock_budget_secs: u64,
    tool_choice: crate::util::config::CompactionToolChoice,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CompactOutput, CompactFailure> {
    generate_session_compact_with_observer(
        chat_history,
        compaction_tool_tokens,
        tools,
        hosted_tools,
        client,
        session_id,
        sampling_config,
        idle_timeout,
        wall_clock_budget_secs,
        tool_choice,
        cancel,
        None,
    )
    .await
}

pub(crate) async fn generate_session_compact_with_observer(
    chat_history: impl Into<
        crate::session::helpers::prepared_compaction_history::CompactionHistoryInput,
    >,
    compaction_tool_tokens: u64,
    tools: Vec<ToolSpec>,
    hosted_tools: Vec<HostedTool>,
    client: OaiCompatClient,
    session_id: acp::SessionId,
    sampling_config: &SamplingConfig,
    idle_timeout: std::time::Duration,
    wall_clock_budget_secs: u64,
    tool_choice: crate::util::config::CompactionToolChoice,
    cancel: &tokio_util::sync::CancellationToken,
    observer: Option<&AttemptObserver>,
) -> Result<CompactOutput, CompactFailure> {
    if cancel.is_cancelled() {
        return Err(CompactFailure::Cancelled);
    }
    let prepared_history = chat_history.into().prepare(compaction_tool_tokens);
    let budget = prepared_history.image_budget;
    if budget.inline_images > 0 {
        tracing::info!(
            body_bytes = budget.body_bytes,
            body_bytes_after = budget.body_bytes_after,
            inline_images = budget.inline_images,
            evicted = budget.evicted,
            needs_image_compaction = budget.needs_image_compaction,
            "Applied image budget to compaction request"
        );
    }
    let chat_history = prepared_history.items;
    let num_messages = chat_history.len();
    let output_cap = compaction_output_cap(
        sampling_config,
        distill_chat_state::estimate_conversation_tokens(&chat_history)
            .saturating_add(compaction_tool_tokens),
    );
    let wire_tool_choice = match tool_choice {
        crate::util::config::CompactionToolChoice::Auto => ToolChoice::auto(),
        crate::util::config::CompactionToolChoice::None => ToolChoice::none(),
    };
    let conversation_tool_choice = match tool_choice {
        crate::util::config::CompactionToolChoice::Auto => ConversationToolChoice::Auto,
        crate::util::config::CompactionToolChoice::None => ConversationToolChoice::None,
    };

    let output = match sampling_config.api_backend {
        ApiBackend::ChatCompletions => {
            // Fold `Reasoning` siblings into the following assistant via `conversation_to_chat_messages`.
            let chat_messages: Vec<ChatRequestMessage> =
                conversation_to_chat_messages(chat_history);
            let mut message =
                ChatCompletionRequest::new(sampling_config.model.to_owned(), chat_messages)
                    .with_temperature(1.0);
            message.max_tokens = output_cap.or(message.max_tokens);
            // Prefix-cache alignment (see doc comment)
            // `tool_choice` is set only when tools are present; Chat Completions rejects it otherwise
            if !tools.is_empty() {
                message = message
                    .with_tools(
                        tools
                            .into_iter()
                            .map(|t| ToolDefinition::function(t.name, t.description, t.parameters))
                            .collect(),
                    )
                    .with_tool_choice(wire_tool_choice);
            }

            let sid = session_id.to_string();
            let request_id = format!("distill-compact-{}", uuid::Uuid::new_v4());
            message.x_grok_conv_id = Some(sid.clone());
            message.x_grok_req_id = Some(request_id.clone());
            message.x_grok_session_id = Some(sid);
            message.x_grok_agent_id = Some(distill_telemetry::id::agent_id());

            tracing::info!(
                compact_model = %sampling_config.model,
                num_messages = num_messages,
                "Sending compact request (streaming)"
            );
            let mut attempt_guard = AttemptGuard::new(
                observer,
                sampling_config.model.clone(),
                format!(
                    "{}/chat/completions",
                    sampling_config.base_url.trim_end_matches('/')
                ),
                sampling_config
                    .reasoning_effort
                    .as_ref()
                    .map(|effort| effort.as_ref().to_owned()),
            );
            if let Some(guard) = attempt_guard.as_mut() {
                guard.set_response(Some(request_id.clone()), None, None);
            }
            let stream_result = match await_unless_cancelled(
                cancel,
                client.chat_completion_stream(message),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => return Err(error),
            };

            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => {
                    if let Some(guard) = attempt_guard.take() {
                        guard.finish(AttemptStatus::Failed);
                    }
                    return Err(classify_sampling_error(e));
                }
            };
            // Collect the streamed response
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut usage: Option<TokenUsage> = None;
            let mut cost_usd_ticks = None;
            let mut response_model = None;
            let mut response_id = Some(request_id);
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel).await
                {
                    Err(error) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(error);
                    }
                    Ok(StreamStep::Item(item)) => item,
                    Ok(StreamStep::Ended) => break,
                    Ok(StreamStep::IdleTimeout) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(CompactFailure::Transient(
                            acp::Error::internal_error().data(format!(
                                "{COMPACT_FAILED_PREFIX}stream idle timeout after {idle_timeout:?} ({} chars received)",
                                content.chars().count()
                            )),
                        ));
                    }
                };
                // Wall-clock backstop (0 disables it): cut a runaway, including a reasoning spiral that token limits miss, and let it retry
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(CompactFailure::Transient(
                        acp::Error::internal_error().data(format!(
                            "{COMPACT_FAILED_PREFIX}exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                        )),
                    ));
                }
                match chunk_result {
                    Ok(chunk) => {
                        retain_nonempty_identity(&mut response_id, &chunk.id);
                        retain_nonempty_identity(&mut response_model, &chunk.model);
                        if let Some(wire_usage) = chunk.usage.as_ref() {
                            usage = Some(TokenUsage::from(wire_usage.clone()));
                            cost_usd_ticks = merge_normalized_cost(cost_usd_ticks, wire_usage);
                        }
                        if let Some(guard) = attempt_guard.as_mut() {
                            guard.set_response(
                                response_id.clone(),
                                response_model.clone(),
                                usage.as_ref().map(|value| jev_usage_from_tokens(value)),
                            );
                            if let Some(value) = usage.as_ref() {
                                guard.set_billing(jev_billing_from_tokens(value, cost_usd_ticks));
                            }
                        }
                        if let Some(choice) = chunk.choices.first() {
                            let delta = &choice.delta;
                            if choice.finish_reason.is_some()
                                || delta.content.as_deref().is_some_and(|s| !s.is_empty())
                                || delta
                                    .reasoning_content
                                    .as_deref()
                                    .is_some_and(|s| !s.is_empty())
                                || !delta.tool_calls.is_empty()
                            {
                                last_progress_at = std::time::Instant::now();
                            }
                            if let Some(delta_content) = &choice.delta.content {
                                timing.record_delta();
                                content.push_str(delta_content);
                            }
                            if let Some(fr) = choice.finish_reason {
                                let sr = distill_sampling_types::StopReason::from(fr);
                                truncated =
                                    matches!(sr, distill_sampling_types::StopReason::Length);
                                stop_reason = Some(sr.as_ref().to_string());
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(classify_sampling_error(e));
                    }
                }
            }
            let output = CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
                usage,
                cost_usd_ticks,
                request_id: response_id,
                response_model,
            };
            if let Some(guard) = attempt_guard.take() {
                guard.finish(if output.truncated || output.content.is_empty() {
                    AttemptStatus::Rejected
                } else {
                    AttemptStatus::Completed
                });
            }
            output
        }
        ApiBackend::Responses => {
            // Send `ConversationItem`s directly; this preserves encrypted reasoning
            let request = ConversationRequest {
                items: chat_history,
                tool_choice: (!tools.is_empty()).then_some(conversation_tool_choice),
                tools,
                hosted_tools,
                model: Some(sampling_config.model.to_owned()),
                temperature: Some(1.0),
                max_output_tokens: output_cap,
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("distill-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                ..Default::default()
            };
            let request_id = request.x_grok_req_id.clone().unwrap_or_default();
            let mut attempt_guard = AttemptGuard::new(
                observer,
                sampling_config.model.clone(),
                format!(
                    "{}/responses",
                    sampling_config.base_url.trim_end_matches('/')
                ),
                sampling_config
                    .reasoning_effort
                    .as_ref()
                    .map(|effort| effort.as_ref().to_owned()),
            );
            if let Some(guard) = attempt_guard.as_mut() {
                guard.set_response(Some(request_id.clone()), None, None);
            }
            let stream_result =
                match await_unless_cancelled(cancel, client.conversation_stream_responses(request))
                    .await
                {
                    Ok(result) => result,
                    Err(error) => return Err(error),
                };
            let mut stream = match stream_result {
                Ok((s, _metadata, _doom_loop)) => s,
                Err(e) => {
                    if let Some(guard) = attempt_guard.take() {
                        guard.finish(AttemptStatus::Failed);
                    }
                    return Err(classify_sampling_error(e));
                }
            };
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut usage: Option<TokenUsage> = None;
            let mut cost_usd_ticks = None;
            let mut response_id = Some(request_id);
            let mut response_model = None;
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel).await
                {
                    Err(error) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(error);
                    }
                    Ok(StreamStep::Item(item)) => item,
                    Ok(StreamStep::Ended) => break,
                    Ok(StreamStep::IdleTimeout) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(CompactFailure::Transient(
                            acp::Error::internal_error().data(format!(
                                "{COMPACT_FAILED_PREFIX}stream idle timeout after {idle_timeout:?} ({} chars received)",
                                content.chars().count()
                            )),
                        ));
                    }
                };
                // Wall-clock backstop (0 disables it): cut a runaway, including a reasoning spiral that token limits miss, and let it retry
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(CompactFailure::Transient(
                        acp::Error::internal_error().data(format!(
                            "{COMPACT_FAILED_PREFIX}exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                        )),
                    ));
                }
                match chunk_result {
                    Ok(chunk) => {
                        match &chunk {
                            ResponseStreamEvent::ResponseCreated(event) => {
                                update_response_attempt(
                                    &mut attempt_guard,
                                    &event.response,
                                    &mut usage,
                                    &mut cost_usd_ticks,
                                    &mut response_id,
                                    &mut response_model,
                                );
                            }
                            ResponseStreamEvent::ResponseInProgress(event) => {
                                update_response_attempt(
                                    &mut attempt_guard,
                                    &event.response,
                                    &mut usage,
                                    &mut cost_usd_ticks,
                                    &mut response_id,
                                    &mut response_model,
                                );
                            }
                            ResponseStreamEvent::ResponseCompleted(event) => {
                                update_response_attempt(
                                    &mut attempt_guard,
                                    &event.response,
                                    &mut usage,
                                    &mut cost_usd_ticks,
                                    &mut response_id,
                                    &mut response_model,
                                );
                            }
                            ResponseStreamEvent::ResponseFailed(event) => {
                                update_response_attempt(
                                    &mut attempt_guard,
                                    &event.response,
                                    &mut usage,
                                    &mut cost_usd_ticks,
                                    &mut response_id,
                                    &mut response_model,
                                );
                            }
                            ResponseStreamEvent::ResponseIncomplete(event) => {
                                update_response_attempt(
                                    &mut attempt_guard,
                                    &event.response,
                                    &mut usage,
                                    &mut cost_usd_ticks,
                                    &mut response_id,
                                    &mut response_model,
                                );
                            }
                            _ => {}
                        }
                        if !matches!(
                            &chunk,
                            ResponseStreamEvent::ResponseCreated(_)
                                | ResponseStreamEvent::ResponseInProgress(_)
                                | ResponseStreamEvent::ResponseQueued(_)
                        ) {
                            last_progress_at = std::time::Instant::now();
                        }
                        match &chunk {
                            ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                                timing.record_delta();
                                content.push_str(&text_delta_event.delta);
                            }
                            ResponseStreamEvent::ResponseFailed(failed_event) => {
                                let event_error = failed_event.response.error.as_ref();
                                let code = event_error.map(|e| e.code.as_str());
                                let message = event_error
                                    .map(|e| e.message.as_str())
                                    .unwrap_or("unknown error");
                                tracing::warn!(
                                    code = code.unwrap_or("none"),
                                    message = %message,
                                    status = ?failed_event.response.status,
                                    "compact: response.failed event"
                                );
                                if let Some(guard) = attempt_guard.take() {
                                    guard.finish(AttemptStatus::Failed);
                                }
                                return Err(classify_response_event_error(code, message));
                            }
                            ResponseStreamEvent::ResponseError(error_event) => {
                                let code = error_event.code.as_deref();
                                tracing::warn!(
                                    code = code.unwrap_or("none"),
                                    message = %error_event.message,
                                    "compact: stream error event"
                                );
                                if let Some(guard) = attempt_guard.take() {
                                    guard.finish(AttemptStatus::Failed);
                                }
                                return Err(classify_response_event_error(
                                    code,
                                    &error_event.message,
                                ));
                            }
                            ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                                let reason = incomplete_event
                                    .response
                                    .incomplete_details
                                    .as_ref()
                                    .map(|d| d.reason.clone())
                                    .unwrap_or_else(|| "unknown".to_string());
                                tracing::warn!(
                                    reason = %reason,
                                    "compact: response.incomplete event"
                                );
                                stop_reason = Some(reason);
                                truncated = true;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(classify_sampling_error(e));
                    }
                }
            }
            let output = CompactOutput {
                content,
                // No incomplete event on a normal completion: treat as a clean stop.
                stop_reason: stop_reason.or_else(|| Some("stop".to_string())),
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
                usage,
                cost_usd_ticks,
                request_id: response_id,
                response_model,
            };
            if let Some(guard) = attempt_guard.take() {
                guard.finish(if output.truncated || output.content.is_empty() {
                    AttemptStatus::Rejected
                } else {
                    AttemptStatus::Completed
                });
            }
            output
        }
        ApiBackend::Messages => {
            // Messages API uses similar streaming to Responses.
            let request = ConversationRequest {
                items: chat_history,
                // Prefix-cache alignment (see doc comment).
                tools,
                hosted_tools,
                model: Some(sampling_config.model.to_owned()),
                temperature: Some(1.0),
                max_output_tokens: output_cap,
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("distill-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                ..Default::default()
            };
            let request_id = request.x_grok_req_id.clone().unwrap_or_default();
            let mut attempt_guard = AttemptGuard::new(
                observer,
                sampling_config.model.clone(),
                format!(
                    "{}/messages",
                    sampling_config.base_url.trim_end_matches('/')
                ),
                sampling_config
                    .reasoning_effort
                    .as_ref()
                    .map(|effort| effort.as_ref().to_owned()),
            );
            if let Some(guard) = attempt_guard.as_mut() {
                guard.set_response(Some(request_id.clone()), None, None);
            }
            let stream_result =
                match await_unless_cancelled(cancel, client.conversation_stream_messages(request))
                    .await
                {
                    Ok(result) => result,
                    Err(error) => return Err(error),
                };
            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => {
                    if let Some(guard) = attempt_guard.take() {
                        guard.finish(AttemptStatus::Failed);
                    }
                    return Err(classify_sampling_error(e));
                }
            };
            // Collect the streamed response (Messages API event types)
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut usage: Option<TokenUsage> = None;
            let mut response_id = Some(request_id);
            let mut response_model = None;
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel).await
                {
                    Err(error) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(error);
                    }
                    Ok(StreamStep::Item(item)) => item,
                    Ok(StreamStep::Ended) => break,
                    Ok(StreamStep::IdleTimeout) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(CompactFailure::Transient(
                            acp::Error::internal_error().data(format!(
                                "{COMPACT_FAILED_PREFIX}stream idle timeout after {idle_timeout:?} ({} chars received)",
                                content.chars().count()
                            )),
                        ));
                    }
                };
                // Wall-clock backstop (0 disables it): cut a runaway, including a reasoning spiral that token limits miss, and let it retry
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(CompactFailure::Transient(
                        acp::Error::internal_error().data(format!(
                            "{COMPACT_FAILED_PREFIX}exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                        )),
                    ));
                }
                match chunk_result {
                    Ok(event) => {
                        match &event {
                            distill_sampling_types::messages::MessageStreamEvent::MessageStart {
                                message,
                            } => {
                                response_id = Some(message.id.clone());
                                response_model = Some(message.model.clone());
                                usage = Some(messages_usage(&message.usage));
                            }
                            distill_sampling_types::messages::MessageStreamEvent::MessageDelta {
                                usage: delta_usage,
                                ..
                            } => {
                                usage = Some(messages_usage(&distill_sampling_types::messages::MessagesUsage {
                                    input_tokens: delta_usage.input_tokens.unwrap_or(0),
                                    output_tokens: delta_usage.output_tokens,
                                    cache_creation_input_tokens: delta_usage
                                        .cache_creation_input_tokens
                                        .unwrap_or(0),
                                    cache_read_input_tokens: delta_usage
                                        .cache_read_input_tokens
                                        .unwrap_or(0),
                                }));
                            }
                            _ => {}
                        }
                        if let Some(guard) = attempt_guard.as_mut() {
                            guard.set_response(
                                response_id.clone(),
                                response_model.clone(),
                                usage.as_ref().map(jev_usage_from_tokens),
                            );
                            if let Some(value) = usage.as_ref() {
                                guard.set_billing(jev_billing_from_tokens(value, None));
                            }
                        }
                        if !matches!(
                            &event,
                            distill_sampling_types::messages::MessageStreamEvent::Ping
                        ) {
                            last_progress_at = std::time::Instant::now();
                        }
                        match event {
                        distill_sampling_types::messages::MessageStreamEvent::ContentBlockDelta {
                            delta: distill_sampling_types::messages::StreamDelta::TextDelta { text },
                            ..
                        } => {
                            timing.record_delta();
                            content.push_str(&text);
                        }
                        distill_sampling_types::messages::MessageStreamEvent::MessageDelta { delta, .. } => {
                            if let Some(sr) = delta.stop_reason {
                                truncated = matches!(
                                    sr,
                                    distill_sampling_types::messages::StopReason::MaxTokens
                                        | distill_sampling_types::messages::StopReason::ModelContextWindowExceeded
                                );
                                stop_reason = Some(sr.wire_str());
                            }
                        }
                        _ => {}
                        }
                    }
                    Err(e) => {
                        if let Some(guard) = attempt_guard.take() {
                            guard.finish(AttemptStatus::Failed);
                        }
                        return Err(classify_sampling_error(e));
                    }
                }
            }
            let output = CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
                usage,
                cost_usd_ticks: None,
                request_id: response_id,
                response_model,
            };
            if let Some(guard) = attempt_guard.take() {
                guard.finish(if output.truncated || output.content.is_empty() {
                    AttemptStatus::Rejected
                } else {
                    AttemptStatus::Completed
                });
            }
            output
        }
    };

    if output.content.is_empty() {
        // Empty response is treated as transient: sampling variance and mid-stream drops are both plausible and may resolve on retry.
        // Content-filter refusals (provider returns 200 with no body) are a known counterexample.
        // They are not currently distinguishable from stream blips at this layer; revisit if stop_reason or finish_reason gets threaded through.
        Err(CompactFailure::Transient(
            acp::Error::internal_error().data(format!(
                "{COMPACT_FAILED_PREFIX}model returned empty response"
            )),
        ))
    } else {
        Ok(output)
    }
}

/// Tests for `classify_sampling_error` and `classify_response_event_error`.
/// Pin the deterministic-vs-transient mapping for every `SamplingError` variant and for the meaningful branches of the response-event classifier.
/// Also covers `StreamTiming` boundaries and `CompactionOutcome::as_str`.
#[cfg(test)]
#[path = "session_compact_classify_tests.rs"]
mod classify_tests;

/// Tests that reconstruct the compacted conversation history exactly as `run_compact` in `acp_session.rs` assembles it.
/// The compaction summary is wrapped in `<user_query>` tags (consistent with normal user messages).
/// `<system-reminder>` state context is placed outside, matching the standard format: `<user_query>...summary...</user_query>\n\n<system-reminder>...</system-reminder>`.
#[cfg(test)]
#[path = "session_compact_compacted_history_shape_tests.rs"]
mod compacted_history_shape_tests;

#[cfg(test)]
#[path = "session_compact_large_body_tests.rs"]
mod large_body_tests;

/// Regression: ChatCompletions compaction must not panic on a standalone `Reasoning` sibling.
#[cfg(test)]
#[path = "session_compact_reasoning_compaction_regression_tests.rs"]
mod reasoning_compaction_regression_tests;
