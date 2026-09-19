// Modified for Distill by Samuel Fajreldines, 2026.
//! Query handlers for the ChatStateActor.

use super::ChatStateActor;
use crate::compaction_utils::extract_last_user_query;
use crate::events::ChatStateEvent;
use crate::types::{AutoCompactTrigger, ChatStateSnapshot, ConversationCounts, NotificationMeta};

impl ChatStateActor {
    /// Build a notification meta from current timing state.
    pub(super) fn get_notification_meta(&self) -> NotificationMeta {
        NotificationMeta {
            stream_start_ms: self.state.stream_start_ms,
            turn_start_ms: self.state.turn_start_ms,
        }
    }

    /// Take a full snapshot of the actor's state.
    pub(super) fn snapshot(&self) -> ChatStateSnapshot {
        ChatStateSnapshot {
            conversation: self.state.conversation.clone(),
            sampling_config: self.state.sampling_config.clone(),
            prompt_index: self.state.prompt_index,
            total_tokens: self.state.total_tokens,
            estimate_at_last_response: self.state.estimate_at_last_response,
            agent_edited_paths: self.state.agent_edited_paths.clone(),
            prompt_texts: self.state.prompt_texts.clone(),
            stream_start_ms: self.state.stream_start_ms,
            turn_start_ms: self.state.turn_start_ms,
            last_compaction_prompt_index: self.state.last_compaction_prompt_index,
            credentials: self.state.credentials.clone(),
        }
    }

    /// Truncate conversation to a target prompt index (rewind).
    /// Walks to the Nth `User` item, drops everything from there on, and emits `ConversationReset`.
    /// `target_prompt_index = N` keeps items up to but not including the (N+1)th `User` message.
    pub(super) fn truncate_to_prompt_index(&mut self, target_prompt_index: usize) {
        if target_prompt_index >= self.state.prompt_index {
            // Nothing to truncate — already at or before the target.
            return;
        }

        // Find the conversation position of the Nth User item.
        // Items before that position are kept; from that position onward removed.
        let mut user_count = 0;
        let mut truncate_at = self.state.conversation.len();

        for (i, item) in self.state.conversation.iter().enumerate() {
            if matches!(item, distill_sampling_types::ConversationItem::User(_)) {
                if user_count == target_prompt_index {
                    truncate_at = i;
                    break;
                }
                user_count += 1;
            }
        }

        self.state.conversation.truncate(truncate_at);
        self.state.prompt_texts.truncate(target_prompt_index);
        self.state.prompt_index = target_prompt_index;
        let base_estimate = super::state::estimate_conversation_tokens(&self.state.conversation);
        self.state.total_tokens = self.reseed_total_tokens(base_estimate);
        self.state.estimated_tokens_since_model = 0;
        self.state.estimate_at_last_response = base_estimate;

        self.persistence.replace_history(&self.state.conversation);

        self.send_event(ChatStateEvent::ConversationReset {
            new_len: self.state.conversation.len(),
        });
    }

    /// Check if auto-compact is needed based on token utilization.
    /// `Some` when `total_tokens` exceeds `context_window * threshold_percent / 100`.
    pub(super) fn check_auto_compact_needed(
        &self,
        threshold_percent: u8,
    ) -> Option<AutoCompactTrigger> {
        let context_window = self.state.sampling_config.context_window;
        let cw = context_window.get();

        if distill_token_estimation::exceeds_threshold(self.state.total_tokens, cw, threshold_percent) {
            let utilization_percent =
                distill_token_estimation::usage_percentage_truncated_u8(self.state.total_tokens, cw);
            Some(AutoCompactTrigger {
                total_tokens: self.state.total_tokens,
                context_window,
                utilization_percent,
            })
        } else {
            None
        }
    }

    pub(super) fn get_last_model_metadata(&self) -> crate::commands::ModelMetadata {
        self.state
            .conversation
            .iter()
            .rev()
            .find_map(|item| {
                if let distill_sampling_types::ConversationItem::Assistant(a) = item {
                    Some(crate::commands::ModelMetadata {
                        resolved_model_id: a.model_id.clone(),
                        model_fingerprint: a.model_fingerprint.clone(),
                    })
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    // ─── Narrow targeted queries ─────────────────────────────────────────────

    /// Return the number of items in the conversation.
    pub(super) fn get_conversation_len(&self) -> usize {
        self.state.conversation.len()
    }

    /// Whether the conversation has any assistant tool call without a matching
    /// `ToolResult` (the dangling-tool-call repair would fire on the next build).
    pub(super) fn has_dangling_tool_calls(&self) -> bool {
        distill_sampling_types::has_dangling_tool_calls(&self.state.conversation)
    }

    /// Return the text of the last assistant message with non-empty text.
    /// Walks backwards; `None` when no such item exists.
    pub(super) fn get_last_assistant_text(&self) -> Option<String> {
        self.state.conversation.iter().rev().find_map(|item| {
            if let distill_sampling_types::ConversationItem::Assistant(a) = item
                && !a.content.trim().is_empty()
            {
                return Some(a.content.as_ref().to_owned());
            }
            None
        })
    }

    /// Reassemble a Length-salvaged report from this turn's assistant texts.
    /// Joined forward with no separator — Length cuts mid-token and continuations carry their own whitespace.
    pub(super) fn get_trailing_assistant_report(&self) -> Option<String> {
        let mut items = self.state.conversation.iter().rev();
        let mut segments: Vec<&str> = Vec::new();
        // Seek the last assistant text of this turn, old-query semantics.
        // A tool-call-carrying item is still a joinable segment: a
        // continuation may finish the cut sentence and then call a tool.
        for item in items.by_ref() {
            match item {
                distill_sampling_types::ConversationItem::Assistant(a)
                    if !a.content.trim().is_empty() =>
                {
                    segments.push(a.content.as_ref());
                    break;
                }
                distill_sampling_types::ConversationItem::User(u)
                    if u.prompt_index.is_some() || u.synthetic_reason.starts_prompt_turn() =>
                {
                    return None;
                }
                _ => {}
            }
        }
        // Join earlier salvage segments. The reminder is injected only on the first continue,
        // so later segments sit adjacent (with at most `Reasoning` siblings between).
        // Bare-`Reasoning` adjacency must join or a multi-continue report loses every middle segment.
        for item in items {
            match item {
                distill_sampling_types::ConversationItem::Assistant(a) => {
                    // An earlier tool-call step is a real boundary.
                    if !a.tool_calls.is_empty() {
                        break;
                    }
                    if !a.content.trim().is_empty() {
                        segments.push(a.content.as_ref());
                    }
                }
                // Committed between salvage segments on reasoning models;
                // not report content, not a boundary.
                distill_sampling_types::ConversationItem::Reasoning(_) => {}
                // Join only across the salvage reminder; any other reminder
                // separates distinct answers.
                distill_sampling_types::ConversationItem::User(u)
                    if u.synthetic_reason
                        == distill_sampling_types::SyntheticReason::LengthContinue => {}
                // Boundary — deliberately including `BackendToolCall`: a
                // hosted-tool step between segments is a real step boundary.
                _ => break,
            }
        }
        if segments.is_empty() {
            return None;
        }
        Some(segments.iter().rev().copied().collect())
    }

    /// Non-empty assistant texts in the current turn, chronological.
    /// The backwards walk stops at a real turn boundary; mid-turn synthetics are walked past.
    /// Whitespace-only assistant items are skipped.
    fn assistant_texts_in_turn(&self) -> Vec<String> {
        let mut texts = Vec::new();
        for item in self.state.conversation.iter().rev() {
            match item {
                distill_sampling_types::ConversationItem::Assistant(a)
                    if !a.content.trim().is_empty() =>
                {
                    texts.push(a.content.as_ref().to_owned());
                }
                distill_sampling_types::ConversationItem::User(u)
                    if u.prompt_index.is_some() || u.synthetic_reason.starts_prompt_turn() =>
                {
                    break;
                }
                _ => {}
            }
        }
        texts.reverse();
        texts
    }

    /// Return the current turn's last assistant message with non-empty text.
    /// Bounded to the current prompt turn; see [`Self::assistant_texts_in_turn`].
    pub(super) fn get_last_assistant_text_in_turn(&self) -> Option<String> {
        self.assistant_texts_in_turn().pop()
    }

    /// Concatenate every non-empty assistant message in the current turn (`"\n"`-joined).
    /// Same turn-boundary rules as [`Self::get_last_assistant_text_in_turn`].
    /// Use this when export must keep earlier bubbles of a multi-round tool turn.
    pub(super) fn get_assistant_text_in_turn(&self) -> Option<String> {
        let texts = self.assistant_texts_in_turn();
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    /// Text of the first content part of the first `User` message, only if that part is `Text`.
    /// A leading non-text part returns `None` rather than scanning further.
    pub(super) fn get_first_user_text(&self) -> Option<String> {
        self.state.conversation.iter().find_map(|item| {
            if let distill_sampling_types::ConversationItem::User(u) = item {
                // Only return text if the first part is Text — behaviour-preserving
                // w.r.t. the original `content.first().and_then(|p| if Text { … })`.
                u.content.first().and_then(|part| {
                    if let distill_sampling_types::ContentPart::Text { text } = part {
                        Some(text.as_ref().to_owned())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
    }

    /// Return the conversation item at `index`, or `None` if out of bounds.
    pub(super) fn get_conversation_item_at(
        &self,
        index: usize,
    ) -> Option<distill_sampling_types::ConversationItem> {
        self.state.conversation.get(index).cloned()
    }

    /// Return the processed text of the last user query (metadata tags stripped).
    /// Delegates to [`extract_last_user_query`] so the caller does not need a full conversation clone.
    pub(super) fn get_last_user_query_text(&self) -> Option<String> {
        extract_last_user_query(&self.state.conversation)
    }

    /// Return conversation item counts by role without cloning any items.
    pub(super) fn get_conversation_counts(&self) -> ConversationCounts {
        let mut counts = ConversationCounts {
            total: self.state.conversation.len(),
            ..Default::default()
        };
        for item in &self.state.conversation {
            match item {
                distill_sampling_types::ConversationItem::User(_) => counts.user += 1,
                distill_sampling_types::ConversationItem::Assistant(_) => {
                    counts.assistant += 1;
                }
                distill_sampling_types::ConversationItem::ToolResult(_) => {
                    counts.tool_result += 1;
                }
                distill_sampling_types::ConversationItem::System(_) => {}
                distill_sampling_types::ConversationItem::BackendToolCall(_) => {}
                distill_sampling_types::ConversationItem::Reasoning(_) => {}
            }
        }
        counts
    }

    /// Return the first `System` message in the conversation, or `None`.
    pub(super) fn get_system_message(&self) -> Option<distill_sampling_types::ConversationItem> {
        self.state
            .conversation
            .iter()
            .find(|item| matches!(item, distill_sampling_types::ConversationItem::System(_)))
            .cloned()
    }
}
