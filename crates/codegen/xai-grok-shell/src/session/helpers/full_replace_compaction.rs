//! remote-code's L5 wiring onto the shared full-replace engine (`xai_grok_compaction::code_compaction`).
//!
//! The shared engine drives the loop: sample, retry, then classify the result as degenerate or failed.
//! The loop lives in [`sample_full_replace_summary`](xai_grok_compaction::sample_full_replace_summary).
//! This module adapts remote-code's transport and telemetry to the engine's two traits: [`ShellCompactionSampler`] and [`ShellFullReplaceObserver`].
//!
//! The **input ladder** (verbatim, then fitted, then lossy) and auto-compaction suppression stay in L5 (`compaction.rs`).
//! Both are driven by the `context_overflow` / `deterministic` flags on [`FullReplaceError`](xai_grok_compaction::FullReplaceError).

use std::sync::Mutex;
use std::time::Duration;

use agent_client_protocol as acp;
use async_trait::async_trait;
use xai_grok_compaction::{
    CompactionPrompt, CompactionSampleError, CompactionSampler, FullReplaceAttemptOutcome,
    FullReplaceObserver, LlmCompactionOutput,
};
use xai_grok_sampler::SamplerConfig as SamplingConfig;
use xai_grok_sampling_types::{ConversationItem, HostedTool, ToolSpec};
use xai_grok_telemetry::events::{CompactionRetryDegraded, CompactionTrigger};

use xai_chat_state::compaction_utils::{
    CompactionAttempt, MAX_CAPTURED_SUMMARY_CHARS, bound_captured_output,
};

use crate::sampling::Client as OaiCompatClient;
use crate::sampling::error::acp_error_message;
use crate::session::helpers::prepared_compaction_history::{
    PreparedCompactionHistory, build_compaction_chat_history,
};
use crate::session::helpers::session_compact::{
    COMPACT_FAILED_PREFIX, CompactFailure, CompactOutput, generate_session_compact,
};

#[derive(Default)]
struct SamplerState {
    last_success: Option<CompactOutput>,
    last_attempted_items: Option<Vec<ConversationItem>>,
}

impl SamplerState {
    fn record_attempt(&mut self, history: &PreparedCompactionHistory) {
        self.last_attempted_items = Some(history.items.clone());
    }
}

/// Wraps `generate_session_compact` as the shared engine's [`CompactionSampler`] for remote-code's full-replace pass.
/// Holds the per-call request context the trait call does not carry (tools, client, session, config).
/// The engine's prompt equals what [`build_compaction_chat_history`] appends, and the short-prompt harness needs a variant the engine can't produce.
pub(crate) struct ShellCompactionSampler {
    use_short_prompt: bool,
    user_context: Option<String>,
    tools: Vec<ToolSpec>,
    hosted_tools: Vec<HostedTool>,
    compaction_tool_tokens: u64,
    client: OaiCompatClient,
    session_id: acp::SessionId,
    sampling_config: SamplingConfig,
    /// Per-chunk idle timeout forwarded to `generate_session_compact`.
    /// A stalled summarizer stream (no model-output chunk for this long) fails instead of hanging.
    idle_timeout: Duration,
    /// Wall-clock budget (secs) forwarded to `generate_session_compact` as the cap on runaway reasoning; `0` disables it.
    wall_clock_budget_secs: u64,
    tool_choice: crate::util::config::CompactionToolChoice,
    cancel: tokio_util::sync::CancellationToken,
    state: Mutex<SamplerState>,
}

impl ShellCompactionSampler {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        use_short_prompt: bool,
        user_context: Option<String>,
        tools: Vec<ToolSpec>,
        hosted_tools: Vec<HostedTool>,
        compaction_tool_tokens: u64,
        client: OaiCompatClient,
        session_id: acp::SessionId,
        sampling_config: SamplingConfig,
        idle_timeout: Duration,
        wall_clock_budget_secs: u64,
        tool_choice: crate::util::config::CompactionToolChoice,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            use_short_prompt,
            user_context,
            tools,
            hosted_tools,
            compaction_tool_tokens,
            client,
            session_id,
            sampling_config,
            idle_timeout,
            wall_clock_budget_secs,
            tool_choice,
            cancel,
            state: Mutex::new(SamplerState::default()),
        }
    }

    pub(crate) fn take_last_success(&self) -> Option<CompactOutput> {
        self.state.lock().unwrap().last_success.take()
    }

    /// Take the exact image-budgeted items from the latest transport attempt.
    pub(crate) fn take_last_attempted_items(&self) -> Option<Vec<ConversationItem>> {
        self.state.lock().unwrap().last_attempted_items.take()
    }
}

#[async_trait]
impl CompactionSampler for ShellCompactionSampler {
    type Item = ConversationItem;

    async fn sample_compaction(
        &self,
        turns: &[ConversationItem],
        _prompt: &CompactionPrompt,
        _timeout: Duration,
    ) -> Result<LlmCompactionOutput, CompactionSampleError> {
        // Append the harness-selected summarization prompt as the final user message (compat short vs structured remote-code)
        // The shared engine's `_prompt` is ignored (see the struct doc)
        let chat_history = build_compaction_chat_history(
            turns.to_vec(),
            self.user_context.as_deref(),
            self.use_short_prompt,
            self.compaction_tool_tokens,
        );
        self.state.lock().unwrap().record_attempt(&chat_history);

        match generate_session_compact(
            chat_history,
            self.compaction_tool_tokens,
            self.tools.clone(),
            self.hosted_tools.clone(),
            self.client.clone(),
            self.session_id.clone(),
            &self.sampling_config,
            self.idle_timeout,
            self.wall_clock_budget_secs,
            self.tool_choice,
            &self.cancel,
        )
        .await
        {
            Ok(output) => {
                let response = output.content.clone();
                self.state.lock().unwrap().last_success = Some(output);
                Ok(LlmCompactionOutput {
                    response,
                    thinking: String::new(),
                })
            }
            Err(failure) => Err(compact_failure_to_sample_error(failure)),
        }
    }
}

/// Map remote-code's [`CompactFailure`] onto the shared engine's [`CompactionSampleError`].
/// `Overflow` → [`CompactionSampleError::ContextOverflow`] — sets the.
/// `false`), so the engine retries it.
fn compact_failure_to_sample_error(failure: CompactFailure) -> CompactionSampleError {
    match failure {
        CompactFailure::Overflow(err) => {
            CompactionSampleError::ContextOverflow(acp_error_message(&err))
        }
        CompactFailure::Deterministic(err) => CompactionSampleError::Build(acp_error_message(&err)),
        CompactFailure::Transient(err) => {
            CompactionSampleError::Other(anyhow::anyhow!(acp_error_message(&err)))
        }
        CompactFailure::Cancelled => {
            CompactionSampleError::Build(acp_error_message(&CompactFailure::cancelled_error()))
        }
    }
}

#[cfg(test)]
#[path = "full_replace_compaction_tests.rs"]
mod tests;

/// Collected telemetry from a full-replace pass, drained by the L5 loop after the shared engine returns.
pub(crate) struct FullReplaceTelemetry {
    pub attempts: u32,
    pub attempt_details: Vec<CompactionAttempt>,
    pub degenerate_rejections: u32,
    pub transient_rejections: u32,
    pub deterministic_rejections: u32,
    /// Raw text of the last degenerate (rejected) summary, for the artifact.
    pub last_rejected_summary: Option<String>,
}

#[derive(Default)]
struct ObserverState {
    attempts: u32,
    attempt_details: Vec<CompactionAttempt>,
    degenerate_rejections: u32,
    transient_rejections: u32,
    deterministic_rejections: u32,
    last_rejected_summary: Option<String>,
    last_error_msg: Option<String>,
}

/// [`FullReplaceObserver`] that reproduces remote-code's per-attempt telemetry without the shared engine depending on a telemetry backend.
/// It records `CompactionAttempt` rows, rejection counters, the `CompactionRetryDegraded` event, and the warn/error tracing.
pub(crate) struct ShellFullReplaceObserver {
    trigger: CompactionTrigger,
    context_window: u64,
    compaction_id: String,
    session_id: String,
    estimated_input_tokens: u64,
    retry_delay_secs: u64,
    state: Mutex<ObserverState>,
}

impl ShellFullReplaceObserver {
    pub(crate) fn new(
        trigger: CompactionTrigger,
        context_window: u64,
        compaction_id: String,
        session_id: String,
        estimated_input_tokens: u64,
        retry_delay_secs: u64,
    ) -> Self {
        Self {
            trigger,
            context_window,
            compaction_id,
            session_id,
            estimated_input_tokens,
            retry_delay_secs,
            state: Mutex::new(ObserverState::default()),
        }
    }

    /// Cumulative number of attempts so far (across all input-ladder stages).
    /// Read mid-loop to label the `input_overflow` retry event.
    pub(crate) fn attempt_count(&self) -> u32 {
        self.state.lock().unwrap().attempts
    }

    /// Lets the L5 loop tell retries exhausted on degenerate summaries apart from retries exhausted on empty output.
    pub(crate) fn degenerate_seen(&self) -> bool {
        self.state.lock().unwrap().degenerate_rejections > 0
    }

    /// The most recent rendered error/diagnostic detail, for `last_error`.
    pub(crate) fn last_error_message(&self) -> Option<String> {
        self.state.lock().unwrap().last_error_msg.clone()
    }

    /// The cumulative attempt count spans all input-ladder stages because the same observer instance is shared across every per-stage call.
    pub(crate) fn into_telemetry(self) -> FullReplaceTelemetry {
        let s = self.state.into_inner().unwrap();
        FullReplaceTelemetry {
            attempts: s.attempts,
            attempt_details: s.attempt_details,
            degenerate_rejections: s.degenerate_rejections,
            transient_rejections: s.transient_rejections,
            deterministic_rejections: s.deterministic_rejections,
            last_rejected_summary: s.last_rejected_summary,
        }
    }
}

impl FullReplaceObserver for ShellFullReplaceObserver {
    fn on_attempt(&self, _attempt: u32, outcome: &FullReplaceAttemptOutcome<'_>) {
        let mut s = self.state.lock().unwrap();
        // The shared `attempt` resets per ladder stage; keep a cumulative count so artifact rows match the pre-migration numbering
        s.attempts += 1;
        let attempt = s.attempts;

        match outcome {
            FullReplaceAttemptOutcome::Success { summary } => {
                s.attempt_details.push(CompactionAttempt {
                    attempt,
                    outcome: "success".to_string(),
                    summary_chars: summary.chars().count() as u64,
                    summary: None,
                    error: None,
                });
            }
            FullReplaceAttemptOutcome::Degenerate {
                summary,
                will_retry,
            } => {
                s.degenerate_rejections += 1;
                let summary_chars = summary.chars().count();
                s.attempt_details.push(CompactionAttempt {
                    attempt,
                    outcome: "degenerate".to_string(),
                    summary_chars: summary_chars as u64,
                    summary: Some(bound_captured_output(summary, MAX_CAPTURED_SUMMARY_CHARS)),
                    error: None,
                });
                s.last_rejected_summary = Some((*summary).to_string());
                s.last_error_msg = Some(format!(
                    "{COMPACT_FAILED_PREFIX}degenerate summary \
                     ({summary_chars} chars for ~{} input tokens)",
                    self.estimated_input_tokens
                ));
                if *will_retry {
                    xai_grok_telemetry::session_ctx::log_event(CompactionRetryDegraded {
                        trigger: self.trigger,
                        reason: "degenerate_summary",
                        from_stage: None,
                        to_stage: None,
                        summary_chars: Some(summary_chars as u64),
                        attempt,
                        context_window: self.context_window,
                        compaction_id: self.compaction_id.clone(),
                    });
                    tracing::warn!(
                        session_id = %self.session_id,
                        attempt,
                        summary_chars,
                        estimated_input_tokens = self.estimated_input_tokens,
                        retry_delay_secs = self.retry_delay_secs,
                        "Compaction produced a degenerate summary, retrying in {} seconds...",
                        self.retry_delay_secs
                    );
                } else {
                    tracing::error!(
                        session_id = %self.session_id,
                        attempt,
                        summary_chars,
                        estimated_input_tokens = self.estimated_input_tokens,
                        "Compaction produced only degenerate summaries after max retries"
                    );
                }
            }
            FullReplaceAttemptOutcome::EmptyResponse { .. } => {
                // The shell reports an empty response as a transient error (`generate_session_compact` returns `Transient`)
                // It never reaches the shared `Ok("")` branch; handle defensively
                s.transient_rejections += 1;
                let msg = format!("{COMPACT_FAILED_PREFIX}model returned empty response");
                s.attempt_details.push(CompactionAttempt {
                    attempt,
                    outcome: "transient".to_string(),
                    summary_chars: 0,
                    summary: None,
                    error: Some(msg.clone()),
                });
                s.last_error_msg = Some(msg);
            }
            FullReplaceAttemptOutcome::Failure {
                message,
                deterministic,
                context_overflow,
                will_retry,
            } => {
                // A context overflow is recorded as a `deterministic` attempt (matching the pre-migration row)
                // It does NOT count toward `deterministic_rejections`; the L5 ladder steps down on it and tracks its `input_overflow_rejections`
                if *deterministic {
                    if !*context_overflow {
                        s.deterministic_rejections += 1;
                        tracing::error!(
                            session_id = %self.session_id,
                            attempt,
                            error = %message,
                            "Compaction failed (deterministic error class, no further retries)"
                        );
                    }
                    s.attempt_details.push(CompactionAttempt {
                        attempt,
                        outcome: "deterministic".to_string(),
                        summary_chars: 0,
                        summary: None,
                        error: Some((*message).to_string()),
                    });
                } else {
                    s.transient_rejections += 1;
                    s.attempt_details.push(CompactionAttempt {
                        attempt,
                        outcome: "transient".to_string(),
                        summary_chars: 0,
                        summary: None,
                        error: Some((*message).to_string()),
                    });
                    if *will_retry {
                        tracing::warn!(
                            session_id = %self.session_id,
                            attempt,
                            retry_delay_secs = self.retry_delay_secs,
                            error = %message,
                            "Compaction attempt {} failed, retrying in {} seconds...",
                            attempt,
                            self.retry_delay_secs
                        );
                    } else {
                        tracing::error!(
                            session_id = %self.session_id,
                            attempt,
                            error = %message,
                            "Compaction failed after max retries"
                        );
                    }
                }
                s.last_error_msg = Some((*message).to_string());
            }
        }
    }
}
