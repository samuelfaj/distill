// Modified for Distill by Samuel Fajreldines, 2026.
//! Internal state types for the ChatStateActor.

use std::collections::BTreeSet;

use distill_sampling_types::{
    ConversationItem, DanglingToolCallReason, SamplingConfig, TokenUsage, ToolSpec,
    dedup_duplicate_tool_results, repair_dangling_tool_calls,
};

use crate::types::Credentials;
use crate::usage::UsageLedger;

/// Bytes/4 estimate of the system prompt portion of a [`ConversationItem`].
/// Returns 0 for non-system items so callers can pipe through whatever they
/// have without unwrapping.
pub fn estimate_system_message_tokens(item: &ConversationItem) -> u64 {
    match item {
        ConversationItem::System(s) => distill_token_estimation::estimate_tokens(&s.content),
        _ => 0,
    }
}

fn estimate_tool_tokens(
    name: &str,
    description: Option<&str>,
    parameters: &serde_json::Value,
) -> u64 {
    let desc_len = description.map_or(0, str::len);
    let params_len = parameters.to_string().len();
    ((name.len() + desc_len + params_len) as u64) / distill_token_estimation::BYTES_PER_TOKEN
}

/// Bytes/4 estimate of one tool definition (name + description + the
/// JSON-serialized parameters).
pub fn estimate_tool_definition_tokens(td: &distill_sampling_types::ToolDefinition) -> u64 {
    estimate_tool_tokens(
        &td.function.name,
        td.function.description.as_deref(),
        &td.function.parameters,
    )
}

/// Sum [`estimate_tool_definition_tokens`] across a slice.
pub fn estimate_tool_definitions_tokens(tds: &[distill_sampling_types::ToolDefinition]) -> u64 {
    tds.iter().map(estimate_tool_definition_tokens).sum()
}

/// Bytes/4 estimate of the exact tool specs serialized on a request.
pub fn estimate_tool_specs_tokens(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|tool| estimate_tool_tokens(&tool.name, tool.description.as_deref(), &tool.parameters))
        .sum()
}

/// Bytes/4 estimate for a single [`ConversationItem`].
/// Images count at [`distill_token_estimation::IMAGE_TOKEN_ESTIMATE`] each.
/// Shared so the per-variant arithmetic stays in one place.
pub fn estimate_item_tokens(item: &ConversationItem) -> u64 {
    use distill_sampling_types::ContentPart;
    match item {
        ConversationItem::System(s) => distill_token_estimation::estimate_tokens(&s.content),
        ConversationItem::User(u) => {
            let mut bytes: usize = 0;
            let mut images: u64 = 0;
            for p in &u.content {
                match p {
                    ContentPart::Text { text } => bytes += text.len(),
                    ContentPart::Image { .. } => images += 1,
                }
            }
            (bytes as u64) / distill_token_estimation::BYTES_PER_TOKEN
                + distill_token_estimation::estimate_image_tokens(images)
        }
        ConversationItem::Assistant(a) => {
            let bytes = a.content.len()
                + a.tool_calls
                    .iter()
                    .map(|tc| tc.arguments.len())
                    .sum::<usize>();
            (bytes as u64) / distill_token_estimation::BYTES_PER_TOKEN
        }
        ConversationItem::ToolResult(tr) => distill_token_estimation::estimate_tokens(&tr.content),
        ConversationItem::BackendToolCall(b) => {
            distill_token_estimation::estimate_tokens(&b.text_summary())
        }
        ConversationItem::Reasoning(r) => {
            // Text and encrypted blob are the same reasoning twice: take the
            // larger, not the sum. The ciphertext is base64, ~4/3 over raw bytes.
            let text_bytes = distill_sampling_types::reasoning_item_text(r).len();
            let enc_bytes = r.encrypted_content.as_deref().map(str::len).unwrap_or(0);
            (text_bytes.max(enc_bytes * 3 / 4) as u64) / distill_token_estimation::BYTES_PER_TOKEN
        }
    }
}

/// Estimate token footprint: text bytes / 4, images at the per-image
/// constant defined by [`distill_token_estimation::IMAGE_TOKEN_ESTIMATE`].
pub fn estimate_conversation_tokens(items: &[ConversationItem]) -> u64 {
    items.iter().map(estimate_item_tokens).sum()
}

/// distill's token counter for the shared compaction engine.
/// Exposes the same bytes/4 estimate the triggers already use, so budgeting stays consistent.
/// Other hosts may plug a real BPE tokenizer into the same seam.
pub struct EstimatedItemTokenCounter;

impl distill_compaction::ItemTokenCounter<ConversationItem> for EstimatedItemTokenCounter {
    fn count_item_tokens(&self, item: &ConversationItem) -> u32 {
        // The estimate is a `u64`; a single item never approaches `u32::MAX`
        // tokens, but saturate rather than wrap if one somehow does.
        estimate_item_tokens(item).try_into().unwrap_or(u32::MAX)
    }
}

/// Bytes/4 estimate of every non-system item in `items`.
pub fn estimate_messages_tokens(items: &[ConversationItem]) -> u64 {
    items
        .iter()
        .filter(|i| !matches!(i, ConversationItem::System(_)))
        .map(estimate_item_tokens)
        .sum()
}

/// Internal mutable state for the ChatStateActor.
///
/// All fields are owned exclusively by the actor task — no locks needed.
pub(crate) struct ChatState {
    /// The full conversation history.
    pub conversation: Vec<ConversationItem>,
    /// Current sampling configuration (model, context window, etc.).
    pub sampling_config: SamplingConfig,
    /// Current prompt index (incremented per user turn).
    pub prompt_index: usize,
    /// Cached prompt texts for rewind preview.
    pub prompt_texts: Vec<String>,
    /// Accumulated token usage.
    pub total_tokens: u64,
    /// Timestamp when the current stream started (epoch ms).
    pub stream_start_ms: Option<i64>,
    /// Timestamp when the current turn started (epoch ms).
    pub turn_start_ms: Option<i64>,
    /// File paths the agent has edited.
    pub agent_edited_paths: BTreeSet<String>,
    /// Prompt index at which the last compaction occurred.
    pub last_compaction_prompt_index: Option<usize>,
    /// Opaque credential secrets (api key, optional extra auth, client version).
    /// Stored opaquely — the actor never interprets them.
    pub credentials: Credentials,
    /// Bytes/4 estimate of tokens added since the last `record_token_usage`.
    /// Used by `check_preflight_overflow` to detect context window overflows
    /// between model responses.
    pub estimated_tokens_since_model: u64,
    /// Bytes/4 estimate of the conversation as of the last `record_token_usage`
    /// (or last reseed). `total_tokens − estimate_at_last_response` is the
    /// provider-side overhead carried across compaction.
    pub estimate_at_last_response: u64,
    /// Per-turn token usage from the most recent model response, for `PromptResponse` `_meta`.
    /// `None` means no model turn has completed (or a restore that did not persist it).
    /// Always overwritten by the most recent turn — historical turns are not retained here.
    pub last_turn_usage: Option<TokenUsage>,
    /// Billing for the open prompt (cleared on next prompt; not persisted).
    pub prompt_usage: Option<UsageLedger>,
    /// Lifetime session billing (not persisted).
    pub session_usage: UsageLedger,
    /// Offset-based turn capture state. `Some` = capture active, `None` = inactive.
    /// Cleared on `TakeTurnMessages` (consumed), `BeginTurnCapture` (new turn),
    /// and `TruncateToPromptIndex` (rewind abandons the turn).
    pub(super) turn_capture: Option<TurnCaptureState>,
    /// Accumulator for the in-progress harness-subagent trace phase.
    /// Independent of `turn_capture` (the planner runs ahead of `BeginTurnCapture`)
    /// and never enters the live `conversation`.
    pub(super) harness_trace_buffer: Vec<ConversationItem>,
    /// Sealed harness trace turns awaiting drain by the agent.
    /// Uploaded as sibling `turn_{N}` artifacts so orchestrators can discover harness subagents.
    /// Drained by `TakeHarnessTraceTurns` at the end of the user-facing turn.
    pub(super) harness_trace_turns: Vec<Vec<ConversationItem>>,
}

/// Tracks which conversation items belong to the current turn without cloning each push.
/// Records `turn_start_offset`; take clones `conversation[offset..]` once.
/// On mid-turn replace/restore, snapshot the tail into `pre_replacement_messages` before the old vec drops.
pub(super) struct TurnCaptureState {
    /// Index into `conversation` where this turn's messages start.
    pub turn_start_offset: usize,
    /// Messages saved from before a conversation replacement (compaction,
    /// snapshot restore).  Extended (not replaced) if multiple replacements
    /// occur in one turn.
    pub pre_replacement_messages: Vec<ConversationItem>,
    /// Whether compaction occurred during this capture.
    pub compaction_occurred: bool,
}

impl ChatState {
    /// Create a `ChatState`, repairing dangling tool calls in the initial conversation.
    /// Handles a kill mid-tool-execution leaving unmatched call IDs in `chat_history.jsonl`.
    /// Without this, broken history would sit in memory until the next `build_request`.
    pub fn new(mut conversation: Vec<ConversationItem>, sampling_config: SamplingConfig) -> Self {
        let deduped = dedup_duplicate_tool_results(&mut conversation);
        if deduped > 0 {
            tracing::info!(
                deduped_count = deduped,
                "Removed duplicate tool results in initial conversation"
            );
        }
        let repaired =
            repair_dangling_tool_calls(&mut conversation, DanglingToolCallReason::UserCancelled);
        if repaired > 0 {
            tracing::info!(
                repaired_count = repaired,
                "Repaired dangling tool calls in initial conversation (likely from a previous crash)"
            );
        }

        let initial_tokens = estimate_conversation_tokens(&conversation);

        Self {
            conversation,
            sampling_config,
            prompt_index: 0,
            prompt_texts: Vec::new(),
            total_tokens: initial_tokens,
            stream_start_ms: None,
            turn_start_ms: None,
            agent_edited_paths: BTreeSet::new(),
            last_compaction_prompt_index: None,
            credentials: Credentials::default(),
            estimated_tokens_since_model: 0,
            estimate_at_last_response: initial_tokens,
            last_turn_usage: None,
            prompt_usage: None,
            session_usage: UsageLedger::default(),
            turn_capture: None,
            harness_trace_buffer: Vec::new(),
            harness_trace_turns: Vec::new(),
        }
    }

    /// Seal items accumulated since the last flush into one harness trace turn.
    /// No-op when nothing was recorded. Shared by the explicit flush and the defensive seal on take.
    pub(super) fn seal_harness_trace_turn(&mut self) {
        if !self.harness_trace_buffer.is_empty() {
            let turn = std::mem::take(&mut self.harness_trace_buffer);
            self.harness_trace_turns.push(turn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sampling_config() -> SamplingConfig {
        SamplingConfig {
            base_url: "https://api.example.com".to_string(),
            model: "test-model".to_string(),
            context_window: std::num::NonZeroU64::new(128_000).unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn estimated_item_token_counter_matches_estimate_item_tokens() {
        use distill_compaction::ItemTokenCounter;

        let counter = EstimatedItemTokenCounter;
        let items = vec![
            ConversationItem::system("you are a helpful assistant"),
            ConversationItem::user("fix the login bug in auth.rs"),
            ConversationItem::assistant("let me look at the file"),
            ConversationItem::tool_result("tc1", "fn login() {}"),
        ];
        for item in &items {
            assert_eq!(
                u64::from(counter.count_item_tokens(item)),
                estimate_item_tokens(item),
                "counter must report the same trusted count as estimate_item_tokens"
            );
        }
    }

    #[test]
    fn reasoning_estimate_takes_max_of_text_and_encrypted_not_sum() {
        // max(4000, 4000*3/4)/4 = 1000, not the (4000+4000)/4 = 2000 double-count.
        let mut r = distill_sampling_types::synthesized_reasoning_item("x".repeat(4000));
        r.encrypted_content = Some("e".repeat(4000));
        assert_eq!(estimate_item_tokens(&ConversationItem::Reasoning(r)), 1000);
    }

    #[test]
    fn reasoning_estimate_encrypted_only_scales_base64_down() {
        // No visible text: base64-corrected size, 4000*3/4/4 = 750.
        let mut r = distill_sampling_types::synthesized_reasoning_item("");
        r.summary.clear();
        r.encrypted_content = Some("e".repeat(4000));
        assert_eq!(estimate_item_tokens(&ConversationItem::Reasoning(r)), 750);
    }

    #[test]
    fn reasoning_estimate_text_only_is_plain_bytes_per_token() {
        let r = distill_sampling_types::synthesized_reasoning_item("x".repeat(4000));
        assert_eq!(estimate_item_tokens(&ConversationItem::Reasoning(r)), 1000);
    }

    #[test]
    fn new_state_has_correct_defaults() {
        let state = ChatState::new(vec![], test_sampling_config());
        assert_eq!(state.prompt_index, 0);
        assert_eq!(state.total_tokens, 0); // empty conversation → 0
        assert!(state.conversation.is_empty());
        assert!(state.agent_edited_paths.is_empty());
        assert!(state.prompt_texts.is_empty());
        assert!(state.stream_start_ms.is_none());
        assert!(state.turn_start_ms.is_none());
        assert!(state.last_compaction_prompt_index.is_none());
    }

    #[test]
    fn new_state_preserves_initial_conversation() {
        let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let state = ChatState::new(items, test_sampling_config());
        assert_eq!(state.conversation.len(), 2);
    }

    #[test]
    fn new_state_estimates_tokens_from_conversation() {
        // 4000 bytes of text per item, bytes / 4 = 1000 tokens each
        let items = vec![
            ConversationItem::system("x".repeat(4000).as_str()),
            ConversationItem::user("y".repeat(4000).as_str()),
            ConversationItem::assistant("z".repeat(4000).as_str()),
            ConversationItem::tool_result("call-1", "w".repeat(4000).as_str()),
        ];
        let state = ChatState::new(items, test_sampling_config());
        assert_eq!(state.total_tokens, 4000); // 4 * (4000/4)
    }

    #[test]
    fn estimate_system_message_tokens_only_counts_system_items() {
        let sys = ConversationItem::system("a".repeat(400));
        assert_eq!(estimate_system_message_tokens(&sys), 100);
        let user = ConversationItem::user("hello");
        assert_eq!(estimate_system_message_tokens(&user), 0);
        let asst = ConversationItem::assistant("hi");
        assert_eq!(estimate_system_message_tokens(&asst), 0);
        let tr = ConversationItem::tool_result("call-1", "x".repeat(4000).as_str());
        assert_eq!(estimate_system_message_tokens(&tr), 0);
    }

    #[test]
    fn estimate_tool_definition_tokens_counts_name_desc_params() {
        // Empty parameters serialize to "null" (4 bytes) in the JSON-string len
        let td = distill_sampling_types::ToolDefinition::function(
            "search",
            Some("find a file"),
            serde_json::json!({}),
        );
        // name=6 + desc=11 + params=`{}`.len()=2 = 19, /4 = 4
        assert_eq!(estimate_tool_definition_tokens(&td), 4);
    }

    #[test]
    fn estimate_tool_specs_tokens_counts_only_provided_specs() {
        let kept = distill_sampling_types::ToolDefinition::function(
            "search",
            Some("find a file"),
            serde_json::json!({"type": "object"}),
        );
        let dropped = distill_sampling_types::ToolDefinition::function(
            "web_search",
            Some("search the web"),
            serde_json::json!({"type": "object"}),
        );
        let tools = vec![ToolSpec::from(kept.clone())];
        let actual = estimate_tool_specs_tokens(&tools);
        let expected = estimate_tool_definitions_tokens(std::slice::from_ref(&kept));
        let unfiltered = estimate_tool_definitions_tokens(&[kept, dropped]);

        assert_eq!(actual, expected);
        assert!(actual < unfiltered);
    }

    #[test]
    fn estimate_messages_tokens_excludes_system_and_sums_rest() {
        // 4000 bytes per item -> 1000 tokens each.
        let items = vec![
            ConversationItem::system("x".repeat(4000).as_str()),
            ConversationItem::user("y".repeat(4000).as_str()),
            ConversationItem::assistant("z".repeat(4000).as_str()),
            ConversationItem::tool_result("call-1", "w".repeat(4000).as_str()),
        ];
        // Total = 4000 (4 items * 1000), system = 1000, messages = 3000.
        assert_eq!(estimate_conversation_tokens(&items), 4000);
        assert_eq!(estimate_messages_tokens(&items), 3000);
    }

    #[test]
    fn estimate_messages_tokens_zero_when_only_system() {
        let items = vec![ConversationItem::system("x".repeat(4000).as_str())];
        assert_eq!(estimate_messages_tokens(&items), 0);
    }

    #[test]
    fn estimate_tool_definitions_tokens_sums_across_slice() {
        let a = distill_sampling_types::ToolDefinition::function(
            "a",
            None::<&str>,
            serde_json::json!({}),
        );
        let b = distill_sampling_types::ToolDefinition::function(
            "b",
            None::<&str>,
            serde_json::json!({}),
        );
        let single = estimate_tool_definition_tokens(&a);
        assert_eq!(estimate_tool_definitions_tokens(&[a, b]), single * 2);
    }
}
