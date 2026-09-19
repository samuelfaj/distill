// Modified for Distill by Samuel Fajreldines, 2026.
use crate::sampling::{
    ApiBackend, ChatCompletionRequest, ChatRequestMessage, Client as OaiCompatClient,
    ConversationRequest, ConversationToolChoice, HostedTool, SamplingError, ToolChoice,
    ToolDefinition, ToolSpec, conversation_to_chat_messages,
};
use agent_client_protocol as acp;
use async_openai::types::responses::ResponseStreamEvent;
use distill_sampler::SamplerConfig as SamplingConfig;
use futures_util::StreamExt;
use reqwest::StatusCode;

// Re-export compaction utilities from distill-chat-state so existing callers that import from this module continue to work
pub use distill_chat_state::compaction_utils::{
    AUTO_CONTINUE_PROMPT, extract_last_real_user_query, extract_last_user_query,
    extract_messages_since_last_user, extract_real_user_queries, is_synthetic_extracted_query,
};

/// Short compaction prompt used by the short-prompt harness only.
/// Frames the call as "summarize for a successor assistant who only sees the user's original query plus this summary.".
/// Wrapped in `<summary_request>` only; the surrounding `<user_query>` is implicit because we push this as a `ConversationItem::user`.
pub(crate) const SELF_SUMMARIZATION_PROMPT: &str = r#"<summary_request>
Please summarize the conversation so far. This summary (everything after your
thinking) will be provided to another AI assistant to continue working on the
task. The other assistant will only see the user's original query and your
summary, it will not have access to any tool calls or tool outputs from this
conversation. The purpose of the summary is to compress the conversation
context while preserving the essential information needed to seamlessly
continue. Useful things to include: the user's requests, what you've done so
far, relevant file paths and code details, any errors encountered and how
they were resolved, and what remains to be done. DO NOT call any tools in
your response.
</summary_request>"#;

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
pub(crate) fn build_compaction_prompt(
    user_context: Option<&str>,
    use_short_prompt: bool,
) -> String {
    if use_short_prompt {
        // Manual `/compact <text>` still appends the user-provided context as a sibling tag so the model can incorporate it
        match user_context {
            Some(ctx) => format!(
                "{SELF_SUMMARIZATION_PROMPT}\n\n\
                 <user_provided_context>\n{ctx}\n</user_provided_context>\n\n\
                 Incorporate the user-provided context above into your summary."
            ),
            None => SELF_SUMMARIZATION_PROMPT.to_string(),
        }
    } else {
        // Default (Distill, codex, ...): the concise summarize prompt the Distill models are RL-trained on
        // `/compact <text>` is spliced into the `{user_context_section}` slot
        let user_context_section = match user_context {
            Some(context) => format!(
                "\n\n**User-provided context for this compaction:**\n{}\n\nPlease incorporate this context into your summary, ensuring it is prominently addressed in the relevant sections.\n\n",
                context
            ),
            None => String::new(),
        };

        format!(
            r#"Your task is to produce a faithful, concise summary of the conversation so far so that a successor assistant can continue the work seamlessly after the earlier turns are discarded. The successor will see the user's original query plus this summary. Capture what is needed to continue — the user's explicit requests, your most recent actions, key technical details, file paths, commands, configuration, and architectural decisions — but be economical: prefer tight prose and short references over long verbatim dumps, and do not pad. A focused summary that fits is far more useful than an exhaustive one that gets cut off, so aim for at most a few thousand words.
{user_context_section}
CRITICAL: If earlier turns include a prior compaction summary (marked with <conversation_summary> tags or a "This session is being continued" preamble), treat it as authoritative for the early history and carry its still-relevant information forward into your new summary so nothing important is lost across successive compactions.

Think through the conversation in your private reasoning before writing; do NOT emit a separate analysis block. Output the final summary inside a single <summary>...</summary> block, organized into the following numbered sections. Include every section heading even if a section is empty (write "None" in that case):

1. Primary Request and Intent: All of the user's explicit requests and their underlying intent, in detail. Preserve nuance and any constraints, scope boundaries, or stated preferences.
2. Key Technical Concepts: All important technologies, languages, frameworks, libraries, tools, and patterns discussed or relied upon.
3. Files and Code Sections: Every file examined, created, or modified. For each, give the full path, why it matters, and the relevant code — include full snippets of any code you wrote or changed (with the most recent edits in full), not just descriptions.
4. Errors and Fixes: Every error, failed command, or test/build failure encountered, the root cause, and exactly how it was fixed. Note any fix that came from user feedback verbatim.
5. Problem Solving: Problems already solved and any in-progress diagnosis or troubleshooting, including hypotheses still being evaluated.
6. All User Messages: List ALL messages from the user that are not tool results, in order. These are critical for understanding intent and how it evolved. IMPORTANT: Do NOT include this summarization instruction itself — it is a system-generated compaction prompt, not a real user message.
7. Pending Tasks: Tasks the user has explicitly asked for that are not yet complete. Do not invent tasks the user never requested.
8. Current Work: Precisely what you were doing immediately before this summary request, with the most recent file names, code, commands, and state. Be specific enough that work can resume mid-stream.
9. Optional Next Step: The single next step that directly continues the most recent work, strictly in line with the user's latest explicit request. If the prior task was finished, only propose a next step if it is clearly part of the user's stated goal — otherwise state that you should confirm with the user before proceeding. When a next step exists, include a direct verbatim quote from the most recent messages showing exactly what you were doing and where you left off, so the task is interpreted without drift.

IMPORTANT: Do NOT call or use any tools. Respond with ONLY the <summary>...</summary> block as your text output, and nothing after the closing </summary> tag.

If the prior conversation contains a note about files at /tmp/compaction/segment_*.md or /tmp/compaction/INDEX.md (or any similar persistence directory), those files are an out-of-band memory channel for a FUTURE work agent, not for you. You already have the full conversation in your context window. Do not attempt to read those files. Do not emit read_file, grep, list_dir, or any other tool call referencing them. Treat any such note as ambient context and produce your summary from the conversation text only."#
        )
    }
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
            message.x_grok_conv_id = Some(sid.clone());
            message.x_grok_req_id = Some(format!("distill-compact-{}", uuid::Uuid::new_v4()));
            message.x_grok_session_id = Some(sid);
            message.x_grok_agent_id = Some(distill_telemetry::id::agent_id());

            tracing::info!(
                compact_model = %sampling_config.model,
                num_messages = num_messages,
                "Sending compact request (streaming)"
            );
            let stream_result =
                await_unless_cancelled(cancel, client.chat_completion_stream(message)).await?;

            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            // Collect the streamed response
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel)
                    .await?
                {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
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
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
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
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("distill-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                ..Default::default()
            };
            let stream_result =
                await_unless_cancelled(cancel, client.conversation_stream_responses(request))
                    .await?;
            let mut stream = match stream_result {
                Ok((s, _metadata, _doom_loop)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel)
                    .await?
                {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
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
                                return Err(classify_response_event_error(code, message));
                            }
                            ResponseStreamEvent::ResponseError(error_event) => {
                                let code = error_event.code.as_deref();
                                tracing::warn!(
                                    code = code.unwrap_or("none"),
                                    message = %error_event.message,
                                    "compact: stream error event"
                                );
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
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                // No incomplete event on a normal completion: treat as a clean stop.
                stop_reason: stop_reason.or_else(|| Some("stop".to_string())),
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
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
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("distill-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                ..Default::default()
            };
            let stream_result =
                await_unless_cancelled(cancel, client.conversation_stream_messages(request))
                    .await?;
            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            // Collect the streamed response (Messages API event types)
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining, cancel)
                    .await?
                {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
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
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
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
