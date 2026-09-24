// Modified for Distill by Samuel Fajreldines, 2026.
//! ConversationRequest assembly — image compaction, pruning, repair, memory injection.

use std::collections::{BTreeMap, BTreeSet};

use distill_sampling_types::{ConversationItem, ConversationRequest, ToolSpec, TraceContext};

use super::ChatStateActor;
use crate::events::ChatStateEvent;
use crate::image_budget::{ImageBudgetOutcome, apply_image_budget};
use crate::persistence::ChatPersistence;
use crate::types::PruningConfig;

/// Placeholder inserted when a tool result is hard-cleared.
/// `pub(super)` so `mutations.rs` can use the same string on the retained conversation.
pub(super) const HARD_CLEAR_PLACEHOLDER: &str = "[Tool result omitted — too old]";

/// Separator inserted between head and tail in soft-trimmed results.
const SOFT_TRIM_SEPARATOR: &str = "\n\n[…trimmed…]\n\n";

impl ChatStateActor {
    /// Build a `ConversationRequest` from current actor state (image eviction, prune, memory reminder).
    /// The command handler already ran integrity repair on the actor conversation before this clone.
    /// Do not re-run dangling/dedup repair on the clone — those would be O(n) no-ops.
    pub(super) fn build_conversation_request(
        &mut self,
        tool_definitions: Vec<ToolSpec>,
        memory_reminder: Option<String>,
        persist_memory_reminder: bool,
        trace: Option<Box<dyn TraceContext>>,
        conv_id: String,
        req_id: String,
    ) -> ConversationRequest {
        let mut memory_reminder = memory_reminder;
        if let Some(reminder) = memory_reminder.as_deref()
            && persist_memory_reminder
        {
            // A live in-place inject can prepend a `System` item, shifting indices
            // under an active capture; snapshot + rebase like the other mutators.
            self.snapshot_turn_slice();
            let injected = inject_memory_reminder(&mut self.state.conversation, reminder);
            if injected {
                self.persistence.replace_history(&self.state.conversation);
                memory_reminder = None;
            }
            self.rebase_turn_capture_offset();
        }
        let budgeted = apply_image_budget(self.state.conversation.clone());
        let ImageBudgetOutcome {
            body_bytes,
            body_bytes_after,
            inline_images,
            needs_image_compaction,
            evicted,
        } = budgeted.outcome;
        let mut items = budgeted.items;
        if inline_images > 0 {
            self.send_event(ChatStateEvent::ImageBudget {
                body_bytes,
                trigger_bytes: crate::image_budget::IMAGE_COMPACT_TRIGGER_BYTES,
                reclaim_target_bytes: crate::image_budget::IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
                inline_images,
                needs_image_compaction,
                evicted,
                body_bytes_after,
            });
        }
        items = self.prune_items_for_turn_request(items);
        if let Some(reminder) = memory_reminder {
            inject_memory_reminder(&mut items, &reminder);
        }
        items = crate::compaction_utils::ModelRequestHistory::from_raw(items).into_items();

        // Step 4: Assemble request
        ConversationRequest {
            items,
            tools: tool_definitions,
            hosted_tools: vec![],
            tool_choice: None,
            model: Some(self.state.sampling_config.model.clone()),
            temperature: self.state.sampling_config.temperature,
            // The model catalogue's maximum is a sampler default, not proof
            // that this caller requested the whole ceiling. Keep the request
            // field empty so route preflight can distinguish an inherited
            // ceiling from an explicit/task-budget output pin.
            max_output_tokens: None,
            top_p: self.state.sampling_config.top_p,
            x_grok_conv_id: Some(conv_id),
            x_grok_req_id: Some(req_id),
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_transient_retry: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace,
            traceparent: None,
            prompt_cache_key: None,
            reasoning_effort: self.state.sampling_config.reasoning_effort,
            json_schema: None,
            // Execute completed tool calls on a Length-truncated turn instead
            // of failing it; text-only salvage stays behind `CompletePartial`.
            length_policy: distill_sampling_types::LengthPolicy::CompleteToolCalls,
        }
    }

    pub(super) fn prune_items_for_turn_request(
        &mut self,
        mut items: Vec<ConversationItem>,
    ) -> Vec<ConversationItem> {
        prune_conversation(&mut *self.persistence, &mut items, &self.pruning_config);
        items
    }
}

// ============================================================================
// Pruning (standalone functions, no actor state needed)
// ============================================================================

/// The request copy is eligible once old output exceeds the existing
/// per-result trim threshold. This deliberately does not depend on the
/// model's context window: a 1M window remains available to the sampler.
pub(crate) fn should_prune(eligible_old_output_bytes: usize, config: &PruningConfig) -> bool {
    config.enabled && eligible_old_output_bytes > config.soft_trim_threshold
}

/// Prune old, large tool results from a request copy only.
///
/// The canonical conversation is never changed. Ambiguous IDs fail open, and
/// the persistence seam owns producer and safety eligibility before any lossy
/// projection is applied.
pub(crate) fn prune_conversation(
    persistence: &mut dyn ChatPersistence,
    conversation: &mut [ConversationItem],
    config: &PruningConfig,
) {
    if !config.enabled {
        return;
    }

    let Some(result_indices) = unique_result_indices(conversation) else {
        return;
    };

    let mut seen_call_ids = BTreeSet::new();
    let mut completed_groups_seen = 0usize;
    let mut candidates = Vec::new();
    let mut eligible_old_output_bytes = 0usize;
    for (assistant_index, item) in conversation.iter().enumerate().rev() {
        let ConversationItem::Assistant(assistant) = item else {
            continue;
        };
        if assistant.tool_calls.is_empty() {
            continue;
        }

        if assistant
            .tool_calls
            .iter()
            .any(|call| !seen_call_ids.insert(call.id.to_string()))
        {
            return;
        }

        let completed = assistant.tool_calls.iter().all(|call| {
            let call_id = call.id.to_string();
            result_indices
                .get(&call_id)
                .is_some_and(|result_index| *result_index > assistant_index)
        });
        if !completed {
            continue;
        }
        if completed_groups_seen < config.keep_last_n_turns {
            completed_groups_seen += 1;
            continue;
        }

        for call in &assistant.tool_calls {
            let call_id = call.id.to_string();
            let Some(&result_index) = result_indices.get(&call_id) else {
                continue;
            };
            let ConversationItem::ToolResult(result) = &conversation[result_index] else {
                continue;
            };
            if result.content.len() > config.soft_trim_threshold {
                eligible_old_output_bytes += result.content.len();
                candidates.push((
                    result_index,
                    call.name.clone(),
                    call.arguments.to_string(),
                ));
            }
        }
    }

    if !should_prune(eligible_old_output_bytes, config) {
        return;
    }

    let mut projections = Vec::new();
    for (result_index, tool_name, tool_arguments) in candidates {
        let ConversationItem::ToolResult(result) = &conversation[result_index] else {
            continue;
        };
        let original = result.content.as_ref();
        let Some((path, body_range)) =
            persistence.archive_tool_result(&tool_name, &tool_arguments, original)
        else {
            continue;
        };
        let Some(body) = original.get(body_range.clone()) else {
            continue;
        };
        let Some(prefix) = original.get(..body_range.start) else {
            continue;
        };
        let Some(suffix) = original.get(body_range.end..) else {
            continue;
        };
        let head = safe_char_slice(body, 0, config.soft_trim_head);
        let tail = safe_char_slice_tail(body, config.soft_trim_tail);
        let projected = format!(
            "{prefix}{head}{SOFT_TRIM_SEPARATOR}{tail}{suffix}\n[full output stored at {path} — read that file for complete output]"
        );
        if projected.len() < original.len() {
            projections.push((result_index, projected));
        }
    }

    if !projections.is_empty() {
        let original_bytes: usize = projections
            .iter()
            .map(|(index, _)| match &conversation[*index] {
                ConversationItem::ToolResult(result) => result.content.len(),
                _ => 0,
            })
            .sum();
        let projected_bytes: usize = projections.iter().map(|(_, text)| text.len()).sum();
        tracing::debug!(
            results = projections.len(),
            original_bytes,
            projected_bytes,
            first_changed_item = projections.iter().map(|(index, _)| *index).min().unwrap_or(0),
            "old tool results shortened in this request; cached prefix may change"
        );
    }
    for (result_index, projection) in projections {
        if let ConversationItem::ToolResult(result) = &mut conversation[result_index] {
            result.content = std::sync::Arc::<str>::from(projection);
        }
    }
}

fn unique_result_indices(conversation: &[ConversationItem]) -> Option<BTreeMap<String, usize>> {
    let mut result_indices = BTreeMap::new();
    for (result_index, item) in conversation.iter().enumerate() {
        let ConversationItem::ToolResult(result) = item else {
            continue;
        };
        let result_id = result.tool_call_id.to_string();
        if result_indices.insert(result_id, result_index).is_some() {
            return None;
        }
    }
    Some(result_indices)
}

// ============================================================================
// Memory reminder injection
// ============================================================================

use crate::types::MEMORY_CONTEXT_OPEN_TAG;

/// Upsert a memory reminder into the conversation's system message.
/// Replaces a prior reminder section in-place, or prepends a `System` item if none exists.
/// Returns `true` when the conversation was changed.
pub(super) fn inject_memory_reminder(items: &mut Vec<ConversationItem>, reminder: &str) -> bool {
    let reminder = reminder.trim();
    if reminder.is_empty() {
        return false;
    }

    if let Some(ConversationItem::System(sys)) = items.first_mut() {
        upsert_memory_reminder_text(&mut sys.content, reminder)
    } else {
        items.insert(0, ConversationItem::system(reminder));
        true
    }
}

fn upsert_memory_reminder_text(system_prompt: &mut std::sync::Arc<str>, reminder: &str) -> bool {
    let existing_start = system_prompt.find(MEMORY_CONTEXT_OPEN_TAG).map(|idx| {
        system_prompt
            .get(..idx)
            .unwrap_or("")
            .trim_end_matches('\n')
            .len()
    });

    let updated: String = if let Some(prefix_len) = existing_start {
        let prefix = system_prompt
            .get(..prefix_len)
            .unwrap_or("")
            .trim_end_matches('\n');
        if prefix.is_empty() {
            reminder.to_string()
        } else {
            format!("{prefix}\n\n{reminder}")
        }
    } else if system_prompt.trim_end() == reminder {
        system_prompt.as_ref().to_owned()
    } else if system_prompt.is_empty() {
        reminder.to_string()
    } else {
        format!("{}\n\n{reminder}", system_prompt.trim_end_matches('\n'))
    };

    if system_prompt.as_ref() == updated.as_str() {
        false
    } else {
        *system_prompt = std::sync::Arc::<str>::from(updated);
        true
    }
}

// ============================================================================
// String helpers
// ============================================================================

fn safe_char_slice(s: &str, start: usize, count: usize) -> String {
    s.chars().skip(start).take(count).collect()
}

fn safe_char_slice_tail(s: &str, count: usize) -> String {
    let total = s.chars().count();
    if count >= total {
        return s.to_string();
    }
    s.chars().skip(total - count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_prune_gating() {
        let config = PruningConfig {
            soft_trim_threshold: 4_000,
            ..Default::default()
        };
        assert!(!should_prune(4_000, &config));
        assert!(should_prune(4_001, &config));
        assert!(!should_prune(5_000, &PruningConfig { enabled: false, ..config }));
    }

    #[test]
    fn prune_disabled_is_noop() {
        let mut conv = vec![ConversationItem::tool_result("c1", "x".repeat(10_000))];
        let config = PruningConfig {
            enabled: false,
            ..Default::default()
        };
        let mut persistence = crate::persistence::NullChatPersistence;
        prune_conversation(&mut persistence, &mut conv, &config);
        let [ConversationItem::ToolResult(tr)] = conv.as_slice() else {
            panic!("expected one tool result: {conv:?}")
        };
        assert_eq!(tr.content.len(), 10_000);
    }

    #[test]
    fn inject_memory_into_existing_system() {
        let mut items = vec![
            ConversationItem::system("You are helpful."),
            ConversationItem::user("hi"),
        ];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        if let Some(ConversationItem::System(sys)) = items.first() {
            assert!(sys.content.contains("Remember: user likes rust"));
            assert!(sys.content.starts_with("You are helpful."));
        }
        assert_eq!(items.len(), 2); // no new item added
    }

    #[test]
    fn inject_memory_prepends_when_no_system() {
        let mut items = vec![ConversationItem::user("hi")];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        assert_eq!(items.len(), 2);
        assert!(matches!(items.first(), Some(ConversationItem::System(_))));
    }
}
