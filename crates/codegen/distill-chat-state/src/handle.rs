// Modified for Distill by Samuel Fajreldines, 2026.
//! Handle to communicate with ChatStateActor.

use std::collections::BTreeSet;

use distill_sampling_types::{
    ConversationItem, ConversationRequest, DanglingToolCallReason, SamplingConfig, TokenUsage,
    ToolSpec, TraceContext,
};
use tokio::sync::{mpsc, oneshot};

use crate::commands::{ChatStateCommand, RepairHistoryBlocked, StrictAppendAck, StrictAppendError};
use crate::types::{
    AutoCompactTrigger, ChatStateSnapshot, ConversationCounts, Credentials, NotificationMeta,
    TurnCapture,
};

/// Handle to communicate with ChatStateActor.
/// This is cheap to clone and can be shared across tasks.
#[derive(Clone)]
pub struct ChatStateHandle {
    cmd_tx: mpsc::UnboundedSender<ChatStateCommand>,
}

/// Weak handle used by bounded background work that must not keep the chat-state
/// actor alive after its owning session is gone.
#[derive(Clone, Debug)]
pub struct WeakChatStateHandle {
    cmd_tx: mpsc::WeakUnboundedSender<ChatStateCommand>,
}

/// The chat-state actor can no longer accept commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatStateMailboxClosed;

impl std::fmt::Display for ChatStateMailboxClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chat-state actor mailbox is closed")
    }
}

impl std::error::Error for ChatStateMailboxClosed {}

impl ChatStateHandle {
    /// Create a new handle with the given command sender.
    pub(crate) fn new(cmd_tx: mpsc::UnboundedSender<ChatStateCommand>) -> Self {
        Self { cmd_tx }
    }

    /// Create a no-op handle that discards all commands.
    /// Useful for tests and situations where chat state tracking is not needed.
    pub fn noop() -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        Self { cmd_tx }
    }

    /// Downgrade this handle so a background task can observe session teardown
    /// without creating a persistence-to-chat-state lifetime cycle.
    pub fn downgrade(&self) -> WeakChatStateHandle {
        WeakChatStateHandle {
            cmd_tx: self.cmd_tx.downgrade(),
        }
    }

    // ═══ Fire-and-forget mutations ═══

    /// Push a user message into the conversation.
    pub fn push_user_message(&self, item: ConversationItem) {
        let _ = self.cmd_tx.send(ChatStateCommand::PushUserMessage { item });
    }

    /// Enqueue an ordered batch of user messages as one actor command.
    pub fn try_push_user_messages_batch(
        &self,
        items: Vec<ConversationItem>,
    ) -> Result<(), ChatStateMailboxClosed> {
        self.cmd_tx
            .send(ChatStateCommand::PushUserMessagesBatch { items })
            .map_err(|_| ChatStateMailboxClosed)
    }

    /// Enqueue an ordered batch and await acknowledgement after every item is processed.
    pub async fn push_user_messages_batch_and_ack(
        &self,
        items: Vec<ConversationItem>,
    ) -> Option<()> {
        self.query("PushUserMessagesBatchAndAck", |reply| {
            ChatStateCommand::PushUserMessagesBatchAndAck { items, reply }
        })
        .await
    }

    /// Push a user message and await acknowledgement that the chat-state actor
    /// has accepted and processed it.
    pub async fn push_user_message_and_ack(&self, item: ConversationItem) -> Option<()> {
        self.query("PushUserMessageAndAck", |reply| {
            ChatStateCommand::PushUserMessageAndAck { item, reply }
        })
        .await
    }

    /// Strictly append one working-directory switch and await persistence.
    /// A matching generation returns `AlreadyPresent`; indeterminate errors must be retried.
    pub async fn append_working_directory_switch_and_ack(
        &self,
        content: String,
        cwd_generation: std::num::NonZeroU64,
    ) -> Result<StrictAppendAck, StrictAppendError> {
        self.query("AppendWorkingDirectorySwitchAndAck", |reply| {
            ChatStateCommand::AppendWorkingDirectorySwitchAndAck {
                content,
                cwd_generation,
                reply,
            }
        })
        .await
        .unwrap_or_else(|| {
            Err(StrictAppendError::Indeterminate(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "chat-state actor unavailable; retry by generation",
            )))
        })
    }

    /// Push a user message with an explicit dangling-repair reason.
    pub fn push_user_message_with_repair_reason(
        &self,
        item: ConversationItem,
        reason: DanglingToolCallReason,
    ) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::PushUserMessageWithRepairReason { item, reason });
    }

    /// Record the assistant's response.
    pub fn push_assistant_response(&self, item: ConversationItem) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::PushAssistantResponse { item });
    }

    /// Record a tool result.
    pub fn push_tool_result(&self, item: ConversationItem) {
        let _ = self.cmd_tx.send(ChatStateCommand::PushToolResult { item });
    }

    /// Persist model output already included in the provider's usage total.
    pub fn push_model_output(&self, item: ConversationItem) {
        let _ = self.cmd_tx.send(ChatStateCommand::PushModelOutput { item });
    }

    /// Persist model output whose provider response omitted usage.
    pub fn push_unreported_model_output(&self, item: ConversationItem) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::PushUnreportedModelOutput { item });
    }

    /// Record accumulated token usage.
    pub fn record_token_usage(&self, total_tokens: u64) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordTokenUsage { total_tokens });
    }

    /// Stash the per-turn `TokenUsage` from the most recent model response.
    /// Fire-and-forget — no ack returned.
    pub fn record_last_turn_usage(&self, usage: TokenUsage) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordLastTurnUsage { usage });
    }

    pub fn record_model_call_usage(
        &self,
        model_id: Option<String>,
        usage: TokenUsage,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) {
        let _ = self.cmd_tx.send(ChatStateCommand::RecordModelCallUsage {
            model_id,
            usage,
            api_duration_ms,
            cost_usd_ticks,
        });
    }

    /// Record a main-loop response whose provider omitted token usage.
    /// The call remains visible and incomplete instead of looking free.
    pub fn record_model_call_without_usage(
        &self,
        model_id: Option<String>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordModelCallWithoutUsage {
                model_id,
                api_duration_ms,
                cost_usd_ticks,
            });
    }

    /// Record one auxiliary model call. The command is ordered with the rest
    /// of this session's usage mutations and is intentionally fire-and-forget.
    pub fn record_auxiliary_call_usage(
        &self,
        model_id: Option<String>,
        usage: Option<TokenUsage>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
        attribute_to_prompt: bool,
        incomplete: bool,
    ) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordAuxiliaryCallUsage {
                model_id,
                usage,
                api_duration_ms,
                cost_usd_ticks,
                attribute_to_prompt,
                incomplete,
            });
    }

    /// Record one dispatched attempt with its identity and provider metadata.
    /// The actor deduplicates by `attempt_id` before folding totals.
    pub fn record_usage_attribution(
        &self,
        attribution: crate::usage::UsageAttribution,
        attribute_to_prompt: bool,
    ) {
        let _ = self.cmd_tx.send(ChatStateCommand::RecordUsageAttribution {
            attribution,
            attribute_to_prompt,
        });
    }

    /// Admit one provider attempt before dispatch so an early usage snapshot
    /// remains explicitly incomplete until its terminal attribution arrives.
    pub fn register_pending_usage_attempt(
        &self,
        attempt_id: String,
        attribute_to_prompt: bool,
    ) -> Result<(), ChatStateMailboxClosed> {
        self.cmd_tx
            .send(ChatStateCommand::RegisterPendingUsageAttempt {
                attempt_id,
                attribute_to_prompt,
            })
            .map_err(|_| ChatStateMailboxClosed)
    }

    /// Apply subagent usage; returns false if the actor did not acknowledge.
    pub async fn record_subagent_usage(
        &self,
        by_model: Vec<(String, crate::usage::UsageTotals)>,
        attribute_to_prompt: bool,
        incomplete: bool,
    ) -> bool {
        self.record_subagent_usage_with_attributions(
            by_model,
            Vec::new(),
            attribute_to_prompt,
            incomplete,
        )
        .await
    }

    /// Apply child attempt rows when the child can prove that its rows cover
    /// every counted call. The aggregate fallback remains available for old
    /// or partially observed child sessions.
    pub async fn record_subagent_usage_with_attributions(
        &self,
        by_model: Vec<(String, crate::usage::UsageTotals)>,
        attributions: Vec<crate::usage::UsageAttribution>,
        attribute_to_prompt: bool,
        incomplete: bool,
    ) -> bool {
        self.record_subagent_usage_with_attributions_and_pending(
            by_model,
            attributions,
            Vec::new(),
            attribute_to_prompt,
            incomplete,
        )
        .await
    }

    /// Apply child attempt rows and carry admitted pending IDs across the
    /// parent boundary. Terminal rows remove their own pending IDs by identity.
    pub async fn record_subagent_usage_with_attributions_and_pending(
        &self,
        by_model: Vec<(String, crate::usage::UsageTotals)>,
        attributions: Vec<crate::usage::UsageAttribution>,
        pending_attempts: Vec<String>,
        attribute_to_prompt: bool,
        incomplete: bool,
    ) -> bool {
        self.query("RecordSubagentUsage", |reply| {
            ChatStateCommand::RecordSubagentUsage {
                by_model,
                attributions,
                attribute_to_prompt,
                incomplete,
                pending_attempts,
                reply,
            }
        })
        .await
        .is_some()
    }

    /// Mark open prompt and/or session ledgers incomplete.
    pub async fn mark_usage_incomplete(&self, prompt: bool, session: bool) -> bool {
        self.query("MarkUsageIncomplete", |reply| {
            ChatStateCommand::MarkUsageIncomplete {
                prompt,
                session,
                reply,
            }
        })
        .await
        .is_some()
    }

    /// Send-only [`Self::mark_usage_incomplete`]: enqueues synchronously so
    /// the mark orders ahead of any later command or query on this handle
    /// (e.g. the turn's billing epilogue), without awaiting the actor.
    pub fn mark_usage_incomplete_nowait(&self, prompt: bool, session: bool) {
        let (reply, _rx) = oneshot::channel();
        let cmd = ChatStateCommand::MarkUsageIncomplete {
            prompt,
            session,
            reply,
        };
        if self.cmd_tx.send(cmd).is_err() {
            // Benign during session teardown when a turn epilogue races the
            // actor's shutdown; the session's usage ledger is final by then.
            tracing::warn!("ChatStateActor dead: MarkUsageIncomplete dropped");
        }
    }

    /// Increment prompt index (called at start of each user turn).
    pub fn increment_prompt_index(&self) {
        let _ = self.cmd_tx.send(ChatStateCommand::IncrementPromptIndex);
    }

    /// Update the sampling config (e.g., model switch).
    pub fn update_sampling_config(&self, config: SamplingConfig) {
        let _ = self.cmd_tx.send(ChatStateCommand::UpdateSamplingConfig {
            config: Box::new(config),
        });
    }

    /// Track that the agent edited a file path.
    pub fn record_agent_edited_path(&self, path: String) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordAgentEditedPath { path });
    }

    /// Record stream timing metadata.
    pub fn record_stream_start(&self, timestamp_ms: i64) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordStreamStart { timestamp_ms });
    }

    /// Record turn timing metadata.
    pub fn record_turn_start(&self, timestamp_ms: i64) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordTurnStart { timestamp_ms });
    }

    /// Replace conversation history.
    pub fn replace_conversation(&self, items: Vec<ConversationItem>) {
        self.send_replace(items, false);
    }

    /// Replace conversation history for compaction.
    /// Sets `compaction_occurred` on the active turn capture.
    pub fn replace_conversation_for_compaction(&self, items: Vec<ConversationItem>) {
        self.send_replace(items, true);
    }

    fn send_replace(&self, items: Vec<ConversationItem>, is_compaction: bool) {
        let _ = self.cmd_tx.send(ChatStateCommand::ReplaceConversation {
            items,
            is_compaction,
        });
    }

    /// See [`ChatStateCommand::StripConversationImages`]. Outcome is typed and disk-acknowledged.
    /// `Applied` means backup and rewrite both reached disk; a dead actor is `ActorUnavailable`, never a no-op.
    pub async fn strip_conversation_images(
        &self,
        urls: Vec<std::sync::Arc<str>>,
    ) -> crate::StripOutcome {
        self.query("StripConversationImages", |reply| {
            ChatStateCommand::StripConversationImages { urls, reply }
        })
        .await
        .unwrap_or(crate::StripOutcome::ActorUnavailable)
    }

    /// Out-of-band history repair (`x.ai/session/repair`); see
    /// [`ChatStateCommand::RepairHistory`]. Returns `None` if the actor is
    /// dead, `Some(Err(_))` if a turn was in flight at processing time.
    pub async fn repair_history(
        &self,
        dry_run: bool,
        turn_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Option<Result<crate::compaction_utils::HistoryRepairReport, RepairHistoryBlocked>> {
        self.query("RepairHistory", |reply| ChatStateCommand::RepairHistory {
            dry_run,
            turn_active,
            reply,
        })
        .await
    }

    /// Atomically align the leading `System` message with `prompt`, persisting when changed.
    /// Serializes with turn pushes so a mid-turn reconnect cannot drop concurrent updates.
    /// `Some(changed)`, or `None` if the actor is dead.
    pub async fn replace_system_head(&self, prompt: &str) -> Option<bool> {
        let prompt = prompt.to_owned();
        self.query("ReplaceSystemHead", |reply| {
            ChatStateCommand::ReplaceSystemHead { prompt, reply }
        })
        .await
    }

    /// Cache prompt text for rewind preview.
    pub fn cache_prompt_text(&self, text: String) {
        let _ = self.cmd_tx.send(ChatStateCommand::CachePromptText { text });
    }

    /// Record compaction boundary for rewind.
    pub fn record_compaction_at(&self, prompt_index: usize) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RecordCompactionAt { prompt_index });
    }

    /// Flush pending persistence writes to disk.
    pub fn flush(&self) {
        let _ = self.cmd_tx.send(ChatStateCommand::Flush);
    }

    /// Update opaque credential secrets held by the actor.
    pub fn update_credentials(&self, credentials: Credentials) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::UpdateCredentials { credentials });
    }

    /// Restore from a snapshot.
    pub fn restore_snapshot(&self, snapshot: ChatStateSnapshot) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RestoreSnapshot(Box::new(snapshot)));
    }

    /// Begin capturing turn messages. Call at the start of a real user turn
    /// (in `handle_prompt`), before `push_user_message`.
    pub fn begin_turn_capture(&self) {
        let _ = self.cmd_tx.send(ChatStateCommand::BeginTurnCapture);
    }

    /// Append synthetic `task` pairs for a harness-spawned subagent to the in-progress trace phase.
    /// Sealed into a standalone trace turn; never enter the live `conversation`. No-op on empty input.
    pub fn append_harness_trace_items(&self, items: Vec<ConversationItem>) {
        if items.is_empty() {
            return;
        }
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::AppendHarnessTraceItems { items });
    }

    /// Seal harness items accumulated since the last flush into one trace turn.
    /// Call once per harness phase so each becomes its own uploaded `turn_{N}` artifact.
    /// No-op when nothing was recorded since the last flush.
    pub fn flush_harness_trace_turn(&self) {
        let _ = self.cmd_tx.send(ChatStateCommand::FlushHarnessTraceTurn);
    }

    /// Repair dangling tool calls after a harness-initiated halt.
    pub fn repair_dangling_after_harness_halt(&self, class: &'static str) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::RepairDanglingAfterHarnessHalt { class });
    }

    /// Drop a trailing continue reminder whose continuation will never
    /// sample. Fire-and-forget; mailbox order puts the pop before any
    /// subsequent command's view of history.
    pub fn pop_stranded_continue_reminder(&self) {
        let _ = self
            .cmd_tx
            .send(ChatStateCommand::PopStrandedContinueReminder);
    }

    // ═══ Async queries (via oneshot) ═══

    /// Send a query to the actor and await the reply.
    /// `None` when the actor is dead (send failure or dropped reply). Both modes are logged at `error`.
    async fn query<T>(
        &self,
        cmd_name: &str,
        make_cmd: impl FnOnce(oneshot::Sender<T>) -> ChatStateCommand,
    ) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(make_cmd(tx)).is_err() {
            tracing::error!(cmd_name, "ChatStateActor dead: send failed");
            return None;
        }
        match rx.await {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::error!(cmd_name, "ChatStateActor dead: reply dropped");
                None
            }
        }
    }

    /// Build a ConversationRequest from the current state.
    /// Prunes, repairs, injects memory, and returns a ready-to-send request.
    pub async fn build_request(
        &self,
        tool_definitions: Vec<ToolSpec>,
        memory_reminder: Option<String>,
        persist_memory_reminder: bool,
        trace: Option<Box<dyn TraceContext>>,
        conv_id: String,
        req_id: String,
    ) -> Option<ConversationRequest> {
        self.query("BuildConversationRequest", |reply| {
            ChatStateCommand::BuildConversationRequest {
                tool_definitions,
                memory_reminder,
                persist_memory_reminder,
                trace,
                conv_id,
                req_id,
                reply,
            }
        })
        .await
    }

    /// Get a clone of the full conversation.
    pub async fn get_conversation(&self) -> Vec<ConversationItem> {
        self.query("GetConversation", |reply| {
            ChatStateCommand::GetConversation { reply }
        })
        .await
        .unwrap_or_default()
    }

    /// Get current prompt index.
    pub async fn get_prompt_index(&self) -> usize {
        self.query("GetPromptIndex", |reply| ChatStateCommand::GetPromptIndex {
            reply,
        })
        .await
        .unwrap_or(0)
    }

    /// Get the prompt index at which the last compaction occurred.
    /// `Some` means the context currently holds a compaction summary.
    pub async fn get_last_compaction_prompt_index(&self) -> Option<usize> {
        self.query("GetLastCompactionPromptIndex", |reply| {
            ChatStateCommand::GetLastCompactionPromptIndex { reply }
        })
        .await
        .flatten()
    }

    /// Get total accumulated tokens.
    pub async fn get_total_tokens(&self) -> u64 {
        self.query("GetTotalTokens", |reply| ChatStateCommand::GetTotalTokens {
            reply,
        })
        .await
        .unwrap_or(0)
    }

    /// Retrieve the most recent stashed per-turn `TokenUsage`. Returns
    /// `None` if no model turn has completed in this session yet, or if
    /// the actor channel is closed.
    pub async fn get_last_turn_usage(&self) -> Option<TokenUsage> {
        self.query("GetLastTurnUsage", |reply| {
            ChatStateCommand::GetLastTurnUsage { reply }
        })
        .await
        .flatten()
    }

    /// Fail-closed prompt bill read.
    /// `Ok(None)` means the actor answered "no ledger"; `Err(())` means it did not answer.
    /// Never collapse `Err` to `None`: an unreadable bill must not be mistaken for a free prompt.
    pub async fn try_get_prompt_usage(&self) -> Result<Option<crate::usage::UsageLedger>, ()> {
        self.query("GetPromptUsage", |reply| ChatStateCommand::GetPromptUsage {
            reply,
        })
        .await
        .ok_or(())
    }

    /// Fail-closed session bill read. `Err(())` if the actor is dead.
    pub async fn try_get_session_usage(&self) -> Result<crate::usage::UsageLedger, ()> {
        self.query("GetSessionUsage", |reply| {
            ChatStateCommand::GetSessionUsage { reply }
        })
        .await
        .ok_or(())
    }

    /// `total_tokens` plus bytes/4 estimate of tool results pushed since the
    /// last model response. Used by `check_preflight_overflow`.
    pub async fn get_estimated_total_tokens(&self) -> u64 {
        self.try_get_estimated_total_tokens().await.unwrap_or(0)
    }

    /// The same count, distinguishing "nothing yet" from "the actor did not
    /// answer": a caller that reports occupancy cannot treat an unreadable
    /// actor as an empty context.
    pub async fn try_get_estimated_total_tokens(&self) -> Option<u64> {
        self.query("GetEstimatedTotalTokens", |reply| {
            ChatStateCommand::GetEstimatedTotalTokens { reply }
        })
        .await
    }

    /// Bytes/4 estimate of all non-system conversation items.
    pub async fn get_estimated_messages_tokens(&self) -> u64 {
        self.query("GetEstimatedMessagesTokens", |reply| {
            ChatStateCommand::GetEstimatedMessagesTokens { reply }
        })
        .await
        .unwrap_or(0)
    }

    /// Get sampling config.
    pub async fn get_sampling_config(&self) -> Option<SamplingConfig> {
        self.query("GetSamplingConfig", |reply| {
            ChatStateCommand::GetSamplingConfig { reply }
        })
        .await
    }

    /// Apply the same tool-result prune a turn request uses (`total_tokens` over half the window).
    /// Returns `items` unchanged when the mailbox is closed. If the actor dies after
    /// accepting the command, returns empty.
    pub async fn apply_turn_request_pruning(
        &self,
        items: Vec<ConversationItem>,
    ) -> Vec<ConversationItem> {
        let (tx, rx) = oneshot::channel();
        if let Err(error) = self
            .cmd_tx
            .send(ChatStateCommand::ApplyTurnRequestPruning { items, reply: tx })
        {
            tracing::error!(
                cmd_name = "ApplyTurnRequestPruning",
                "ChatStateActor dead: send failed"
            );
            return match error.0 {
                ChatStateCommand::ApplyTurnRequestPruning { items, .. } => items,
                _ => unreachable!("sent ApplyTurnRequestPruning"),
            };
        }
        match rx.await {
            Ok(pruned) => pruned,
            Err(_) => {
                tracing::error!(
                    cmd_name = "ApplyTurnRequestPruning",
                    "ChatStateActor dead: reply dropped"
                );
                Vec::new()
            }
        }
    }

    /// Get the set of agent-edited file paths.
    pub async fn get_agent_edited_paths(&self) -> BTreeSet<String> {
        self.query("GetAgentEditedPaths", |reply| {
            ChatStateCommand::GetAgentEditedPaths { reply }
        })
        .await
        .unwrap_or_default()
    }

    /// Get notification meta (timing info).
    pub async fn get_notification_meta(&self) -> Option<NotificationMeta> {
        self.query("GetNotificationMeta", |reply| {
            ChatStateCommand::GetNotificationMeta { reply }
        })
        .await
    }

    /// Snapshot state for forking or rewind.
    pub async fn snapshot(&self) -> Option<ChatStateSnapshot> {
        self.query("Snapshot", |reply| ChatStateCommand::Snapshot { reply })
            .await
    }

    /// Truncate conversation to a target prompt index (for rewind).
    pub async fn truncate_to_prompt_index(&self, target: usize) {
        self.query("TruncateToPromptIndex", |reply| {
            ChatStateCommand::TruncateToPromptIndex {
                target_prompt_index: target,
                reply,
            }
        })
        .await;
    }

    /// Get credential secrets.
    pub async fn get_credentials(&self) -> Credentials {
        self.query("GetCredentials", |reply| ChatStateCommand::GetCredentials {
            reply,
        })
        .await
        .unwrap_or_default()
    }

    pub async fn get_last_model_metadata(&self) -> crate::commands::ModelMetadata {
        self.query("GetLastModelMetadata", |reply| {
            ChatStateCommand::GetLastModelMetadata { reply }
        })
        .await
        .unwrap_or_default()
    }

    /// Take the accumulated turn messages and end the capture.
    /// Returns `None` if no capture was active.
    pub async fn take_turn_messages(&self) -> Option<TurnCapture> {
        self.query("TakeTurnMessages", |reply| {
            ChatStateCommand::TakeTurnMessages { reply }
        })
        .await
        .flatten()
    }

    /// Drain the sealed harness trace turns. Each `Vec` is one turn's synthetic `task` pairs.
    /// A trailing un-flushed accumulator is sealed defensively before draining.
    /// Empty when nothing was recorded.
    pub async fn take_harness_trace_turns(&self) -> Vec<Vec<ConversationItem>> {
        self.query("TakeHarnessTraceTurns", |reply| {
            ChatStateCommand::TakeHarnessTraceTurns { reply }
        })
        .await
        .unwrap_or_default()
    }

    /// Check if auto-compact is needed.
    pub async fn check_auto_compact_needed(
        &self,
        threshold_percent: u8,
    ) -> Option<AutoCompactTrigger> {
        self.query("CheckAutoCompactNeeded", |reply| {
            ChatStateCommand::CheckAutoCompactNeeded {
                threshold_percent,
                reply,
            }
        })
        .await
        .flatten()
    }

    // ═══ Narrow targeted queries ═══

    /// Get the number of items in the conversation.
    /// Cheaper than [`get_conversation`]: the actor returns a `usize` without cloning items.
    pub async fn get_conversation_len(&self) -> usize {
        self.query("GetConversationLen", |reply| {
            ChatStateCommand::GetConversationLen { reply }
        })
        .await
        .unwrap_or(0)
    }

    /// Whether any assistant tool call lacks a matching `ToolResult`.
    /// Returns `false` if the actor is dead. The actor scans in place and returns a single `bool`.
    pub async fn has_dangling_tool_calls(&self) -> bool {
        self.query("HasDanglingToolCalls", |reply| {
            ChatStateCommand::HasDanglingToolCalls { reply }
        })
        .await
        .unwrap_or(false)
    }

    /// Get the text of the last assistant message with non-empty text.
    /// `None` if none exists or the actor is dead. Cheaper than cloning the conversation.
    pub async fn get_last_assistant_text(&self) -> Option<String> {
        self.query("GetLastAssistantText", |reply| {
            ChatStateCommand::GetLastAssistantText { reply }
        })
        .await
        .flatten()
    }

    /// Joins trailing assistant segments, walking past mid-turn synthetics.
    /// The join crosses only `LengthContinue` user items and `Reasoning`; anything else bounds it.
    /// `None` when no trailing text or the actor is dead.
    pub async fn get_trailing_assistant_report(&self) -> Option<String> {
        self.query("GetTrailingAssistantReport", |reply| {
            ChatStateCommand::GetTrailingAssistantReport { reply }
        })
        .await
        .flatten()
    }

    /// Get the current turn's last assistant message text.
    /// Turn-scoped, unlike [`get_last_assistant_text`]. `None` if the turn produced none or the actor is dead.
    pub async fn get_last_assistant_text_in_turn(&self) -> Option<String> {
        self.query("GetLastAssistantTextInTurn", |reply| {
            ChatStateCommand::GetLastAssistantTextInTurn { reply }
        })
        .await
        .flatten()
    }

    /// Concatenate every non-empty assistant message in the current turn (`"\n"`-joined).
    /// Same turn boundary as [`get_last_assistant_text_in_turn`]. `None` if none or the actor is dead.
    pub async fn get_assistant_text_in_turn(&self) -> Option<String> {
        self.query("GetAssistantTextInTurn", |reply| {
            ChatStateCommand::GetAssistantTextInTurn { reply }
        })
        .await
        .flatten()
    }

    /// Get the text of the first `Text` part in the first `User` message.
    /// `None` if none exists or the actor is dead. Cheaper than cloning the conversation.
    pub async fn get_first_user_text(&self) -> Option<String> {
        self.query("GetFirstUserText", |reply| {
            ChatStateCommand::GetFirstUserText { reply }
        })
        .await
        .flatten()
    }

    /// Get a single conversation item by index (0-based).
    /// `None` if out of bounds or the actor is dead. Cheaper than cloning the conversation.
    pub async fn get_conversation_item_at(&self, index: usize) -> Option<ConversationItem> {
        self.query("GetConversationItemAt", |reply| {
            ChatStateCommand::GetConversationItemAt { index, reply }
        })
        .await
        .flatten()
    }

    /// Get the processed text of the last user query (metadata tags stripped).
    /// Equivalent to `extract_last_user_query` without cloning the full conversation.
    pub async fn get_last_user_query_text(&self) -> Option<String> {
        self.query("GetLastUserQueryText", |reply| {
            ChatStateCommand::GetLastUserQueryText { reply }
        })
        .await
        .flatten()
    }

    /// Get item counts for the conversation by role.
    /// Returns [`ConversationCounts`] without cloning any items.
    pub async fn get_conversation_counts(&self) -> ConversationCounts {
        self.query("GetConversationCounts", |reply| {
            ChatStateCommand::GetConversationCounts { reply }
        })
        .await
        .unwrap_or_default()
    }

    /// Get the first `System` message in the conversation, if any.
    /// Cheaper than [`get_conversation`] when only the system prompt is needed.
    pub async fn get_system_message(&self) -> Option<ConversationItem> {
        self.query("GetSystemMessage", |reply| {
            ChatStateCommand::GetSystemMessage { reply }
        })
        .await
        .flatten()
    }
}

impl WeakChatStateHandle {
    /// Temporarily acquire a strong handle for one bounded operation.
    pub fn upgrade(&self) -> Option<ChatStateHandle> {
        self.cmd_tx
            .upgrade()
            .map(|cmd_tx| ChatStateHandle { cmd_tx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_handle_does_not_keep_command_channel_alive() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let handle = ChatStateHandle::new(cmd_tx);
        let weak = handle.downgrade();

        assert!(weak.upgrade().is_some());
        drop(handle);
        assert!(weak.upgrade().is_none());
    }
}
