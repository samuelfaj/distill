// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use crate::session::commands::{PromptCompletionKind, PromptTurnResult};
use distill_tools::implementations::distill::task::types::SubagentResult;

pub(super) enum PromptTurnResultMode {
    Initial { requires_structured_output: bool },
    ParentFollowup,
}

pub(super) struct PromptTurnResultSummaries<'a> {
    pub success: &'a dyn Fn() -> String,
    pub max_turns: &'a dyn Fn(usize) -> String,
    pub cancelled: &'a dyn Fn() -> String,
}

pub(super) struct PromptTurnResultInput<'a> {
    pub result: SubagentResult,
    pub turn_result: Result<PromptTurnResult, tokio::sync::oneshot::error::RecvError>,
    pub mode: PromptTurnResultMode,
    pub final_text: String,
    pub was_cancelled: bool,
    pub summaries: PromptTurnResultSummaries<'a>,
    pub result_tokens: u64,
}

pub(super) struct PromptTurnResultOutput {
    pub result: SubagentResult,
    pub cancellation_may_hide_usage: bool,
}

pub(super) fn reduce_prompt_turn_result(
    input: PromptTurnResultInput<'_>,
) -> PromptTurnResultOutput {
    let PromptTurnResultInput {
        mut result,
        turn_result,
        mode,
        final_text,
        was_cancelled,
        summaries,
        result_tokens,
    } = input;
    result.tokens_used = result_tokens;
    let mut cancellation_may_hide_usage = false;

    match turn_result {
        Ok(Ok(turn)) => match turn.completion_kind {
            PromptCompletionKind::Completed | PromptCompletionKind::StationarityEnded => {
                // MaxTokens means the report is real but the output token limit cut it off
                // The validated-Ok arm stays raw JSON because a note would corrupt the payload
                let truncated = turn.stop_reason == agent_client_protocol::StopReason::MaxTokens;
                match (mode, turn.structured_output) {
                    (
                        PromptTurnResultMode::Initial {
                            requires_structured_output: true,
                        },
                        Some(Ok(value)),
                    ) => {
                        result.success = true;
                        result.cancelled = false;
                        result.error = None;
                        result.output = Arc::from(value.to_string());
                    }
                    (
                        PromptTurnResultMode::Initial {
                            requires_structured_output: true,
                        },
                        Some(Err(error)),
                    ) => {
                        result.success = false;
                        result.cancelled = false;
                        result.error = Some(with_truncation_error(
                            format!("structured output validation failed: {error}"),
                            truncated,
                        ));
                        result.output = Arc::from(final_text);
                    }
                    (
                        PromptTurnResultMode::Initial {
                            requires_structured_output: true,
                        },
                        None,
                    ) => {
                        result.success = false;
                        result.cancelled = false;
                        result.error = Some(with_truncation_error(
                            "structured output requested but none produced".to_string(),
                            truncated,
                        ));
                        result.output = Arc::from(final_text);
                    }
                    (
                        PromptTurnResultMode::Initial {
                            requires_structured_output: false,
                        },
                        _,
                    ) => {
                        result.success = true;
                        result.cancelled = false;
                        result.error = None;
                        result.output = with_truncation_note(
                            text_or_summary(&final_text, summaries.success),
                            truncated,
                        );
                    }
                    (PromptTurnResultMode::ParentFollowup, None) => {
                        result.success = true;
                        result.cancelled = false;
                        result.error = None;
                        result.output = with_truncation_note(
                            text_or_summary(&final_text, summaries.success),
                            truncated,
                        );
                    }
                    (PromptTurnResultMode::ParentFollowup, Some(_)) => {
                        result.success = false;
                        result.cancelled = false;
                        result.error = Some(with_truncation_error(
                            "Parent follow-up unexpectedly produced structured output".to_string(),
                            truncated,
                        ));
                        result.output = Arc::from(final_text);
                        result.output_usage_incomplete = true;
                    }
                }
            }
            PromptCompletionKind::Cancelled { category, context } => {
                result.success = false;
                result.cancelled = true;
                result.error = Some(super::cancellation_error_message(
                    category,
                    context.as_ref(),
                ));
                result.output = text_or_summary(&final_text, summaries.cancelled);
                result.output_usage_incomplete = true;
                cancellation_may_hide_usage = true;
            }
            PromptCompletionKind::MaxTurnsReached { limit } => {
                result.success = false;
                result.cancelled = true;
                result.error = Some(format!("max turns reached (limit: {limit})"));
                result.output = text_or_summary(&final_text, || (summaries.max_turns)(limit));
                result.output_usage_incomplete = true;
            }
            PromptCompletionKind::Rewound => {
                result.success = false;
                result.cancelled = true;
                result.error = Some("Subagent turn was rewound".to_string());
                result.output = Arc::from(final_text);
                result.output_usage_incomplete = true;
                cancellation_may_hide_usage = true;
            }
            PromptCompletionKind::RemovedFromQueue => {
                result.success = false;
                result.cancelled = true;
                result.error = Some("Subagent turn was removed before it ran".to_string());
                result.output = Arc::from(final_text);
                result.output_usage_incomplete = true;
            }
        },
        Ok(Err(error)) => {
            result.success = false;
            result.cancelled = was_cancelled;
            result.error = Some(if was_cancelled {
                "Subagent was cancelled".to_string()
            } else {
                format!("Session error: {error}")
            });
            result.output = Arc::from(final_text);
            result.output_usage_incomplete = true;
            cancellation_may_hide_usage = was_cancelled;
        }
        Err(_) => {
            result.success = false;
            result.cancelled = was_cancelled;
            result.error = Some(if was_cancelled {
                "Subagent was cancelled".to_string()
            } else {
                "Child session dropped unexpectedly".to_string()
            });
            result.output = Arc::from(final_text);
            result.output_usage_incomplete = true;
            cancellation_may_hide_usage = true;
        }
    }

    PromptTurnResultOutput {
        result,
        cancellation_may_hide_usage,
    }
}

/// Marks a truncated report so the parent cannot treat it as complete.
fn with_truncation_note(output: Arc<str>, truncated: bool) -> Arc<str> {
    if truncated {
        Arc::from(format!(
            "{output}\n\n[Note: this subagent's final report was truncated by the output \
             token limit and may be incomplete.]"
        ))
    } else {
        output
    }
}

fn with_truncation_error(error: String, truncated: bool) -> String {
    if truncated {
        format!(
            "{error}; the subagent's turn was truncated by the output token limit — the \
             structured answer is likely incomplete"
        )
    } else {
        error
    }
}

fn text_or_summary(final_text: &str, summary: impl FnOnce() -> String) -> Arc<str> {
    if final_text.is_empty() {
        Arc::from(summary())
    } else {
        Arc::from(final_text)
    }
}

#[cfg(test)]
#[path = "prompt_turn_result_tests.rs"]
mod tests;
