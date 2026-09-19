// Modified for Distill by Samuel Fajreldines, 2026.
//! Conversation types: re-exports the canonical set from `distill_sampling_types` plus grok-shell-specific additions.

use std::collections::HashSet;

pub use distill_sampling_types::conversation::*;

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;

/// Tracing context for conversation requests; satisfies `TraceContext` through its blanket impl.
/// Lives in grok-shell because it references shell-internal config and upload types.
#[derive(Debug, Clone)]
pub struct ConversationRequestTrace {
    pub gcs_config: crate::session::repo_changes::TraceExportConfig,
    #[expect(
        dead_code,
        reason = "retained for snapshot compat; wire when sampler path uploads traces"
    )]
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
}

/// Filters chat history copied into a fork. Drops synthetic user messages, then truncates at the last complete turn so the child never sees a partial one.
/// A turn is complete when the Assistant's tool calls are all answered; Reasoning and BackendToolCall items are transparent to the scan.
/// Keep the "complete turn" definition in sync with `count_complete_turns` in `distill-subagent-resolution/src/context.rs`.
pub(crate) fn fork_filter_chat(items: &mut Vec<ConversationItem>) {
    items.retain(|item| match item {
        ConversationItem::User(u) => u.synthetic_reason.is_human(),
        _ => true,
    });

    // Only Assistant advances the boundary; everything else is transparent.
    let mut last_complete_end = 0;
    let mut i = 0;
    while let Some(item) = items.get(i) {
        match item {
            ConversationItem::System(_) => {
                last_complete_end = i + 1;
                i += 1;
            }
            ConversationItem::Assistant(asst) => {
                let expected: HashSet<&str> =
                    asst.tool_calls.iter().map(|tc| tc.id.as_ref()).collect();
                let mut found = HashSet::new();
                let mut j = i + 1;
                while let Some(next) = items.get(j) {
                    match next {
                        ConversationItem::ToolResult(tr) => {
                            if expected.contains(tr.tool_call_id.as_str()) {
                                found.insert(tr.tool_call_id.as_str());
                            }
                            j += 1;
                        }
                        ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_) => {
                            j += 1;
                        }
                        _ => break,
                    }
                }
                if found == expected {
                    last_complete_end = j;
                    i = j;
                } else {
                    break; // Dangling tool calls: stop at the last complete boundary
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    items.truncate(last_complete_end);
}
