// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use distill_hooks::dispatcher::{
    AdditionalContext, OutputReplacement, PostToolUseBlock, PostToolUseResult, SelectedReplacement,
};
use distill_hooks::event::{MAX_HOOK_OUTPUT_REPLACEMENT_CHARS, clip_text};
use distill_hooks::result::HookRunResult;

#[derive(Debug, Default, PartialEq)]
pub(super) struct PostToolUseDelivery {
    pub model_output: Option<String>,
    pub additional_context: Vec<AdditionalContext>,
    pub blocks: Vec<PostToolUseBlock>,
}

struct Rejection {
    run_index: usize,
    reason: String,
}

pub(super) fn plan_post_tool_use_delivery(
    result: PostToolUseResult,
    output: &ToolsToolOutput,
    reminder_tag: &str,
    results: &mut [HookRunResult],
) -> PostToolUseDelivery {
    let tool_is_mcp = matches!(output, ToolsToolOutput::MCP(_));
    let selected = if tool_is_mcp {
        latest_replacement(result.builtin_replacement, result.mcp_replacement)
    } else {
        if let Some(mcp) = result.mcp_replacement {
            downgrade_run(
                results,
                mcp.run_index,
                "updatedMCPToolOutput does not match the tool's kind",
            );
        }
        result.builtin_replacement
    };
    let (model_output, rejection) = post_tool_use_model_output(selected, output, reminder_tag);
    if let Some(Rejection { run_index, reason }) = rejection {
        downgrade_run(results, run_index, &reason);
    }
    PostToolUseDelivery {
        model_output,
        additional_context: result.additional_context,
        blocks: result.blocks,
    }
}

fn downgrade_run(results: &mut [HookRunResult], run_index: usize, reason: &str) {
    if let Some(slot) = results.get_mut(run_index)
        && let HookRunResult::Success {
            hook_name,
            elapsed,
            http_info,
            system_message,
        } = slot
    {
        tracing::debug!(
            hook_name = %hook_name,
            run_index,
            reason,
            "downgrading the run that produced a rejected post_tool_use replacement"
        );
        *slot = HookRunResult::Failed {
            hook_name: hook_name.clone(),
            error: reason.to_string(),
            elapsed: *elapsed,
            http_info: http_info.clone(),
            system_message: system_message.clone(),
        };
    }
}

fn latest_replacement(
    builtin: Option<SelectedReplacement>,
    mcp: Option<SelectedReplacement>,
) -> Option<SelectedReplacement> {
    match (builtin, mcp) {
        (Some(builtin), Some(mcp)) => Some(if mcp.run_index > builtin.run_index {
            mcp
        } else {
            builtin
        }),
        (builtin, mcp) => builtin.or(mcp),
    }
}

fn post_tool_use_model_output(
    selected: Option<SelectedReplacement>,
    output: &ToolsToolOutput,
    reminder_tag: &str,
) -> (Option<String>, Option<Rejection>) {
    let Some(SelectedReplacement {
        replacement,
        run_index,
    }) = selected
    else {
        return (None, None);
    };
    match post_tool_use_rendered_replacement(replacement, output) {
        RenderedReplacement::Replace {
            hook_name,
            rendered,
        } => {
            if rendered.trim().is_empty() {
                tracing::warn!(
                    hook_name = %hook_name,
                    "post_tool_use output replacement renders empty"
                );
            }
            (
                Some(super::reminders::escape_reminder_tags(
                    &rendered,
                    reminder_tag,
                )),
                None,
            )
        }
        RenderedReplacement::Rejected { reason } => (None, Some(Rejection { run_index, reason })),
    }
}

enum RenderedReplacement {
    Replace { hook_name: String, rendered: String },
    Rejected { reason: String },
}

fn post_tool_use_rendered_replacement(
    replacement: OutputReplacement,
    output: &ToolsToolOutput,
) -> RenderedReplacement {
    let OutputReplacement {
        hook_name, value, ..
    } = replacement;

    if matches!(output, ToolsToolOutput::MCP(_)) {
        let rendered = match value {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        return RenderedReplacement::Replace {
            hook_name,
            rendered: clip_text(&rendered, MAX_HOOK_OUTPUT_REPLACEMENT_CHARS),
        };
    }

    match serde_json::from_value::<ToolsToolOutput>(value) {
        Ok(candidate) if std::mem::discriminant(&candidate) == std::mem::discriminant(output) => {
            RenderedReplacement::Replace {
                hook_name,
                rendered: clip_text(
                    &candidate.to_prompt_format(),
                    MAX_HOOK_OUTPUT_REPLACEMENT_CHARS,
                ),
            }
        }
        Ok(_) => {
            tracing::warn!(
                hook_name = %hook_name,
                "post_tool_use updatedToolOutput does not match the tool's output shape; keeping original"
            );
            RenderedReplacement::Rejected {
                reason: "updatedToolOutput does not match the tool's output shape".to_string(),
            }
        }
        Err(err) => {
            tracing::warn!(
                hook_name = %hook_name,
                %err,
                "post_tool_use updatedToolOutput failed to parse; keeping original"
            );
            RenderedReplacement::Rejected {
                reason: format!("updatedToolOutput failed to parse: {err}"),
            }
        }
    }
}

pub(super) fn substitute_rendered_output(
    prompt_text: &str,
    output: &ToolsToolOutput,
    replacement: String,
) -> String {
    let original = output.to_prompt_format();
    if original.is_empty() {
        return if prompt_text.is_empty() {
            replacement
        } else {
            format!("{replacement}\n\n{prompt_text}")
        };
    }
    match prompt_text.strip_prefix(&original) {
        Some(reminders) => format!("{replacement}{reminders}"),
        None => {
            tracing::warn!(
                "post_tool_use replacement: tool prompt did not start with its own output; dropping reminders"
            );
            debug_assert!(false, "tool prompt text did not start with its own output");
            replacement
        }
    }
}

#[cfg(test)]
#[path = "post_tool_use_delivery_tests.rs"]
mod tests;
