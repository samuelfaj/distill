// Modified for Distill by Samuel Fajreldines, 2026.
//! Commands sent to the ChatStateActor.

use std::collections::BTreeSet;

use tokio::sync::oneshot;
use distill_sampling_types::{
    ConversationItem, ConversationRequest, DanglingToolCallReason, SamplingConfig, TokenUsage,
    ToolSpec, TraceContext,
};

use crate::types::{
    AutoCompactTrigger, ChatStateSnapshot, ConversationCounts, Credentials, NotificationMeta,
    TurnCapture,
};

#[derive(Debug, Clone, Default)]
pub struct ModelMetadata {
    pub resolved_model_id: Option<String>,
    pub model_fingerprint: Option<String>,
}

/// Refusal reply for [`ChatStateCommand::RepairHistory`]: a turn was in
/// flight, and in-flight tool calls must not be treated as dangling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairHistoryBlocked;

impl std::fmt::Display for RepairHistoryBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot repair history while a turn is in flight; stop the turn first"
        )
    }
}

impl std::error::Error for RepairHistoryBlocked {}

/// Result of a strict persistence-acknowledged working-directory switch append.
#[derive(Debug, Clone)]
pub enum StrictAppendAck {
    Appended,
    AlreadyPresent(ConversationItem),
}

#[derive(Debug)]
pub enum StrictAppendError {
    NotCommitted(std::io::Error),
    Committed {
        acknowledgement: StrictAppendAck,
        source: std::io::Error,
    },
    Indeterminate(std::io::Error),
}

/// Commands sent to the ChatStateActor via mpsc channel.
pub enum ChatStateCommand {
    // ═══ Mutations (fire-and-forget) ═══
    /// Push a user message into the conversation.
    PushUserMessage { item: ConversationItem },

    /// Push an ordered batch of user messages into the conversation.
    PushUserMessagesBatch { items: Vec<ConversationItem> },

    /// Push an ordered batch and acknowledge once every message is processed.
    PushUserMessagesBatchAndAck {
        items: Vec<ConversationItem>,
        reply: oneshot::Sender<()>,
    },

    /// Push a user message and acknowledge once the chat-state actor has
    /// accepted and processed it.
    PushUserMessageAndAck {
        item: ConversationItem,
        reply: oneshot::Sender<()>,
    },

    /// Append one working-directory switch without repair or pruning, then
    /// acknowledge only after persistence processes the generation-aware append.
    AppendWorkingDirectorySwitchAndAck {
        content: String,
        cwd_generation: std::num::NonZeroU64,
        reply: oneshot::Sender<Result<StrictAppendAck, StrictAppendError>>,
    },

    /// Push a user message with an explicit dangling-repair reason.
    PushUserMessageWithRepairReason {
        item: ConversationItem,
        reason: DanglingToolCallReason,
    },

    /// Record the assistant's response (text + tool calls).
    PushAssistantResponse { item: ConversationItem },

    /// Record a tool result.
    PushToolResult { item: ConversationItem },

    /// Persist model output already included in the provider's usage total.
    PushModelOutput { item: ConversationItem },

    /// Persist model output whose provider response omitted usage.
    PushUnreportedModelOutput { item: ConversationItem },

    /// Record accumulated token usage from a streaming response.
    RecordTokenUsage { total_tokens: u64 },

    /// Stash the per-turn `TokenUsage` from the most recent model response.
    /// Overwrites any previously stashed value.
    RecordLastTurnUsage { usage: TokenUsage },

    RecordModelCallUsage {
        model_id: Option<String>,
        usage: TokenUsage,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    },

    /// Record a main-loop response whose provider omitted token usage.
    RecordModelCallWithoutUsage {
        model_id: Option<String>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    },

    /// Record one non-main model call. Missing usage/cost stays explicit.
    RecordAuxiliaryCallUsage {
        model_id: Option<String>,
        usage: Option<TokenUsage>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
        attribute_to_prompt: bool,
        incomplete: bool,
    },

    /// Subagent usage into session (and prompt when attributable). Replies when applied.
    RecordSubagentUsage {
        by_model: Vec<(String, crate::usage::UsageTotals)>,
        attribute_to_prompt: bool,
        /// Nested subagent bill may under-count.
        incomplete: bool,
        reply: oneshot::Sender<()>,
    },

    /// Mark open prompt and/or session ledgers incomplete.
    MarkUsageIncomplete {
        prompt: bool,
        session: bool,
        reply: oneshot::Sender<()>,
    },

    /// Increment prompt_index (called at start of each user turn).
    IncrementPromptIndex,

    /// Update the sampling config (e.g., model switch).
    UpdateSamplingConfig { config: Box<SamplingConfig> },

    /// Track that the agent edited a file path.
    RecordAgentEditedPath { path: String },

    /// Record stream timing metadata.
    RecordStreamStart { timestamp_ms: i64 },

    /// Record turn timing metadata.
    RecordTurnStart { timestamp_ms: i64 },

    /// Replace conversation history.
    ReplaceConversation {
        items: Vec<ConversationItem>,
        is_compaction: bool,
    },

    /// Out-of-band history repair; `dry_run` only reports.
    /// `turn_active` is re-checked inside the handler: a caller-side check races turn start.
    /// At processing time the command is refused or runs on pre-turn state.
    RepairHistory {
        dry_run: bool,
        turn_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        reply: oneshot::Sender<
            Result<crate::compaction_utils::HistoryRepairReport, RepairHistoryBlocked>,
        >,
    },

    /// Persist a URL-scoped strip. In-actor so it serializes with turn pushes.
    /// Replies with [`crate::StripOutcome`] after the disk ack: `Applied` means backup and rewrite both landed.
    StripConversationImages {
        urls: Vec<std::sync::Arc<str>>,
        reply: tokio::sync::oneshot::Sender<crate::StripOutcome>,
    },

    /// Atomically align the leading `System` message with `prompt`, persisting inside the actor.
    /// Serializes with turn pushes so a mid-turn reconnect cannot lose updates the way RMW would.
    /// A changed head re-bases `total_tokens`; acceptable because it invalidates the KV prefix anyway.
    ReplaceSystemHead {
        prompt: String,
        reply: oneshot::Sender<bool>,
    },

    /// Cache prompt text for rewind preview.
    CachePromptText { text: String },

    /// Record compaction boundary for rewind.
    RecordCompactionAt { prompt_index: usize },

    /// Flush pending persistence writes to disk (end of turn).
    Flush,

    /// Update opaque credential secrets held by the actor.
    UpdateCredentials { credentials: Credentials },

    /// Restore from a snapshot.
    RestoreSnapshot(Box<ChatStateSnapshot>),

    /// Start capturing turn messages. Clears any previous buffer.
    BeginTurnCapture,

    /// Append synthetic `task` pairs for a harness-spawned subagent to the in-progress trace phase.
    /// Accumulated independently of the live `conversation` and of `turn_capture`.
    AppendHarnessTraceItems { items: Vec<ConversationItem> },

    /// Seal the harness items accumulated since the last flush into one
    /// standalone trace turn. Issued once per harness phase (after the planner,
    /// after each verifier panel). No-op when nothing was recorded.
    FlushHarnessTraceTurn,

    /// Repair dangling tool calls after a harness-initiated halt.
    RepairDanglingAfterHarnessHalt { class: &'static str },

    /// Drop a trailing continue reminder whose continuation will never
    /// sample (the turn is completing truncated after a failed
    /// continuation), so the dead cue does not persist into later turns.
    PopStrandedContinueReminder,

    /// Build a ConversationRequest ready to send to the API.
    /// Clones, prunes old tool results, repairs dangling calls, injects the memory reminder.
    BuildConversationRequest {
        tool_definitions: Vec<ToolSpec>,
        memory_reminder: Option<String>,
        persist_memory_reminder: bool,
        trace: Option<Box<dyn TraceContext>>,
        conv_id: String,
        req_id: String,
        reply: oneshot::Sender<ConversationRequest>,
    },

    /// Get a clone of the full conversation.
    GetConversation {
        reply: oneshot::Sender<Vec<ConversationItem>>,
    },

    /// Get current prompt index.
    GetPromptIndex { reply: oneshot::Sender<usize> },

    /// Get the prompt index at which the last compaction occurred.
    /// `Some` means the context currently holds a compaction summary.
    GetLastCompactionPromptIndex {
        reply: oneshot::Sender<Option<usize>>,
    },

    /// Get total accumulated tokens.
    GetTotalTokens { reply: oneshot::Sender<u64> },

    /// Retrieve the most recent stashed per-turn `TokenUsage`. Returns
    /// `None` until at least one `RecordLastTurnUsage` has been processed.
    GetLastTurnUsage {
        reply: oneshot::Sender<Option<TokenUsage>>,
    },

    GetPromptUsage {
        reply: oneshot::Sender<Option<crate::usage::UsageLedger>>,
    },

    GetSessionUsage {
        reply: oneshot::Sender<crate::usage::UsageLedger>,
    },

    /// `total_tokens` + bytes/4 delta from tool results since last model response.
    GetEstimatedTotalTokens { reply: oneshot::Sender<u64> },

    /// Bytes/4 estimate of all non-system conversation items.
    GetEstimatedMessagesTokens { reply: oneshot::Sender<u64> },

    /// Get sampling config.
    GetSamplingConfig {
        reply: oneshot::Sender<SamplingConfig>,
    },

    /// Soft-trim / hard-clear old tool results the same way a turn request does.
    ApplyTurnRequestPruning {
        items: Vec<ConversationItem>,
        reply: oneshot::Sender<Vec<ConversationItem>>,
    },

    /// Get the set of agent-edited file paths.
    GetAgentEditedPaths {
        reply: oneshot::Sender<BTreeSet<String>>,
    },

    /// Get notification meta (timing info).
    GetNotificationMeta {
        reply: oneshot::Sender<NotificationMeta>,
    },

    /// Snapshot state for forking or rewind.
    Snapshot {
        reply: oneshot::Sender<ChatStateSnapshot>,
    },

    /// Truncate conversation to a target prompt index (for rewind).
    TruncateToPromptIndex {
        target_prompt_index: usize,
        reply: oneshot::Sender<()>,
    },

    /// Check if auto-compact is needed (returns token info).
    CheckAutoCompactNeeded {
        threshold_percent: u8,
        reply: oneshot::Sender<Option<AutoCompactTrigger>>,
    },

    /// Get credential secrets.
    GetCredentials { reply: oneshot::Sender<Credentials> },

    GetLastModelMetadata {
        reply: oneshot::Sender<ModelMetadata>,
    },

    /// Take the accumulated turn messages and end the capture.
    /// Returns `None` if no capture was active.
    TakeTurnMessages {
        reply: oneshot::Sender<Option<TurnCapture>>,
    },

    /// Drain the sealed harness trace turns (goal planner + verifier panels).
    /// Each `Vec` is one turn's synthetic `task` pairs. Seals a trailing un-flushed accumulator first.
    TakeHarnessTraceTurns {
        reply: oneshot::Sender<Vec<Vec<ConversationItem>>>,
    },

    // ═══ Narrow targeted queries (avoid full-conversation clone) ═══
    /// Get the number of items in the conversation.
    /// Cheaper than `GetConversation` when only the length is needed.
    GetConversationLen { reply: oneshot::Sender<usize> },

    /// Whether any assistant tool call lacks a matching `ToolResult` (i.e. the
    /// dangling-tool-call repair would fire on the next request build).
    /// Cheaper than `GetConversation` when only this predicate is needed.
    HasDanglingToolCalls { reply: oneshot::Sender<bool> },

    /// Get the text content of the last assistant message with non-empty text.
    /// Returns `None` if no such message exists.
    /// Cheaper than `GetConversation` when only the final assistant response is needed.
    GetLastAssistantText {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Like `GetLastAssistantText`, but joins trailing assistant segments
    /// separated only by `SyntheticReason::LengthContinue` user items (any
    /// other user item is a hard boundary). `None` when no trailing text.
    GetTrailingAssistantReport {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Like `GetLastAssistantText`, but bounded to the current prompt turn:
    /// returns `None` when the turn produced no assistant text (the walk stops
    /// at the first turn-starting user item).
    GetLastAssistantTextInTurn {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Concatenate every non-empty assistant message in the current prompt
    /// turn (`"\n"`-joined). Same turn boundary as `GetLastAssistantTextInTurn`.
    GetAssistantTextInTurn {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Get the text of the first `Text` part in the first `User` message.
    /// `None` if there is no user message or that message has no text part.
    /// Cheaper than `GetConversation` when only the initial query is needed.
    GetFirstUserText {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Get a single conversation item by index (0-based).
    /// Returns `None` if the index is out of bounds.
    /// Cheaper than `GetConversation` when only one item is needed.
    GetConversationItemAt {
        index: usize,
        reply: oneshot::Sender<Option<ConversationItem>>,
    },

    /// Get the processed text of the last user query (metadata tags stripped).
    /// Equivalent to `extract_last_user_query` without cloning the full conversation.
    GetLastUserQueryText {
        reply: oneshot::Sender<Option<String>>,
    },

    /// Get item counts for the conversation by role.
    /// Returns `ConversationCounts` without cloning any items.
    GetConversationCounts {
        reply: oneshot::Sender<ConversationCounts>,
    },

    /// Get the first `System` message in the conversation, if any.
    /// Cheaper than `GetConversation` when only the system prompt is needed.
    GetSystemMessage {
        reply: oneshot::Sender<Option<ConversationItem>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that every command variant is constructible (compile-time check).
    #[test]
    fn command_variants_are_constructible() {
        // Mutations
        let _ = ChatStateCommand::PushUserMessage {
            item: ConversationItem::user("hello"),
        };
        let _ = ChatStateCommand::PushUserMessagesBatch {
            items: vec![ConversationItem::user("hello")],
        };
        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::PushUserMessagesBatchAndAck {
            items: vec![ConversationItem::user("hello")],
            reply: tx,
        };
        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::PushUserMessageAndAck {
            item: ConversationItem::user("hello"),
            reply: tx,
        };
        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::AppendWorkingDirectorySwitchAndAck {
            content: "moved".into(),
            cwd_generation: std::num::NonZeroU64::new(1).unwrap(),
            reply: tx,
        };
        let _ = ChatStateCommand::PushAssistantResponse {
            item: ConversationItem::assistant("hi"),
        };
        let _ = ChatStateCommand::PushToolResult {
            item: ConversationItem::tool_result("call-1", "result"),
        };
        let _ = ChatStateCommand::PushModelOutput {
            item: ConversationItem::assistant("model output"),
        };
        let _ = ChatStateCommand::PushUnreportedModelOutput {
            item: ConversationItem::assistant("unreported output"),
        };
        let _ = ChatStateCommand::RecordTokenUsage { total_tokens: 100 };
        let _ = ChatStateCommand::IncrementPromptIndex;
        let _ = ChatStateCommand::UpdateSamplingConfig {
            config: Box::new(SamplingConfig {
                context_window: std::num::NonZeroU64::new(128_000).unwrap(),
                ..Default::default()
            }),
        };
        let _ = ChatStateCommand::RecordAgentEditedPath {
            path: "src/main.rs".to_string(),
        };
        let _ = ChatStateCommand::RecordStreamStart {
            timestamp_ms: 12345,
        };
        let _ = ChatStateCommand::RecordTurnStart {
            timestamp_ms: 12345,
        };
        let _ = ChatStateCommand::ReplaceConversation {
            items: vec![],
            is_compaction: false,
        };
        let _ = ChatStateCommand::CachePromptText {
            text: "prompt".to_string(),
        };
        let _ = ChatStateCommand::RecordCompactionAt { prompt_index: 0 };
        let _ = ChatStateCommand::Flush;

        // Queries
        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetConversation { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetPromptIndex { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetLastCompactionPromptIndex { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetTotalTokens { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetEstimatedTotalTokens { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetSamplingConfig { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::ApplyTurnRequestPruning {
            items: vec![],
            reply: tx,
        };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetAgentEditedPaths { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::BuildConversationRequest {
            tool_definitions: vec![],
            memory_reminder: None,
            persist_memory_reminder: false,
            trace: None,
            conv_id: String::new(),
            req_id: String::new(),
            reply: tx,
        };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetNotificationMeta { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::Snapshot { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::TruncateToPromptIndex {
            target_prompt_index: 0,
            reply: tx,
        };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::CheckAutoCompactNeeded {
            threshold_percent: 85,
            reply: tx,
        };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetLastModelMetadata { reply: tx };

        let _ = ChatStateCommand::BeginTurnCapture;

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::TakeTurnMessages { reply: tx };

        // Narrow targeted queries
        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetConversationLen { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetLastAssistantText { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetTrailingAssistantReport { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetLastAssistantTextInTurn { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetAssistantTextInTurn { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetFirstUserText { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetConversationItemAt {
            index: 0,
            reply: tx,
        };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetLastUserQueryText { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetConversationCounts { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetSystemMessage { reply: tx };

        let (tx, _rx) = oneshot::channel();
        let _ = ChatStateCommand::GetEstimatedMessagesTokens { reply: tx };
    }
}
