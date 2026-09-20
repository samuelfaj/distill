// Modified for Distill by Samuel Fajreldines, 2026.
//! `SessionCommand` defines the message protocol used to drive a session actor.
//! The actor implementation lives in `acp_session.rs`.
use super::acp_types::*;
use super::plan_mode::PromptMode;
use crate::extensions::notification::SessionNotification;
use crate::session::signals::TurnDeltaSnapshot;
use agent_client_protocol as acp;
use distill_tools::types::skill_discovery_tracker::SkillUpdateKind;
use tokio::sync::oneshot;
/// Structured context for a cancelled turn.
/// Clients deserialize into this same type.
/// Absent fields are skipped.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CancellationContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_name: Option<String>,
    /// What triggered the cancel (e.g. `"send_now"`, `"esc"`, `"mouse"`), sent as `cancelTrigger` on the turn-end `_meta`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}
/// The ways a `/btw` side question can fail.
/// Kept typed until the ACP boundary so model errors keep their typed rate-limit and auth codes instead of flattening to a string.
/// `handle_btw` maps them with [`map_sampling_err_to_acp`](crate::sampling::error::map_sampling_err_to_acp).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SideQuestionError {
    #[error("side question model call failed: {0}")]
    Sampling(#[from] distill_sampling_types::SamplingError),
    #[error("failed to prepare client: {0}")]
    PrepareClient(String),
    #[error("No response from model")]
    EmptyResponse,
}
/// Prompt completion kind returned to the ACP layer.
#[derive(Debug, Clone)]
pub enum PromptCompletionKind {
    Completed,
    /// Silent EndTurn after stationarity/true-noop thrash.
    /// Distinct from Completed so goal continuation is not re-queued under an active goal.
    StationarityEnded,
    Cancelled {
        category: Option<distill_session_events::types::CancellationCategory>,
        context: Option<CancellationContext>,
    },
    MaxTurnsReached {
        limit: usize,
    },
    Rewound,
    /// A queued prompt was removed (or cleared) from the server-authoritative queue before it ever ran.
    /// The prompt never started a turn, so the `prompt_complete` broadcast and the roster `Idle` delta must be skipped.
    /// The `Idle` delta would flip the dashboard off `Working` while the real turn is still in flight.
    RemovedFromQueue,
}
/// `_meta.completionKind` on a `PromptResponse`. Distinguishes a queued prompt
/// that never ran from a real cancelled turn (both use `StopReason::Cancelled`).
pub const COMPLETION_KIND_KEY: &str = "completionKind";
/// `_meta.completionKind` value for [`PromptCompletionKind::RemovedFromQueue`].
pub const REMOVED_FROM_QUEUE_KIND: &str = "removedFromQueue";
/// `_meta.cancellationCategory` of a hook-denied cancel; the pager matches it to render the blocked-by-a-hook marker.
pub const HOOK_DENIED_CATEGORY: &str = "HookDenied";
/// `_meta.cancellationCategory` of a max-turns end; headless matches it to drive the max-turns exit code.
pub const MAX_TURNS_REACHED_CATEGORY: &str = "max_turns_reached";
/// `_meta.cancellationCategory` of a stationarity end.
pub const ACTION_STATIONARITY_CATEGORY: &str = "action_stationarity";
/// `_meta.cancellationCategory` of a permission reject that ended the turn.
pub const PERMISSION_REJECTED_CATEGORY: &str = "PermissionRejected";
/// `_meta.cancellationCategory` of a dismissed permission prompt that ended the turn.
pub const PERMISSION_CANCELLED_CATEGORY: &str = "PermissionCancelled";
/// `_meta.cancellationCategory` of a mid-turn abort (user stop or unnamed interrupt).
pub const MID_TURN_ABORT_CATEGORY: &str = "MidTurnAbort";
/// `_meta.cancellationCategory` wire name of a cancel category: an explicit match so a variant rename cannot silently change the wire.
/// This is deliberately a second vocabulary next to the serde snake_case of the events.jsonl / after-turn rails.
/// `_meta` shipped PascalCase and clients match it.
pub fn meta_category_str(
    category: distill_session_events::types::CancellationCategory,
) -> &'static str {
    use distill_session_events::types::CancellationCategory;
    match category {
        CancellationCategory::HookDenied => HOOK_DENIED_CATEGORY,
        CancellationCategory::PermissionRejected => PERMISSION_REJECTED_CATEGORY,
        CancellationCategory::PermissionCancelled => PERMISSION_CANCELLED_CATEGORY,
        CancellationCategory::MidTurnAbort => MID_TURN_ABORT_CATEGORY,
    }
}
impl PromptCompletionKind {
    /// The completion's `_meta.cancellationCategory`, shared by every terminal rail so the wires never disagree.
    /// The rails: `PromptResponse` `_meta`, the legacy `prompt_complete`, and the durable `TurnCompleted`.
    pub fn cancellation_category_meta(&self) -> Option<String> {
        match self {
            Self::Cancelled { category, .. } => {
                category.map(|cat| meta_category_str(cat).to_string())
            }
            Self::MaxTurnsReached { .. } => Some(MAX_TURNS_REACHED_CATEGORY.to_string()),
            Self::StationarityEnded => Some(ACTION_STATIONARITY_CATEGORY.to_string()),
            Self::Completed | Self::Rewound | Self::RemovedFromQueue => None,
        }
    }
    /// The completion's `_meta.cancellationContext` (hook name, reason, trigger), stamped beside `cancellationCategory`.
    /// It lets a client show WHY a turn was blocked without scraping annotations.
    /// Additive: shipped clients ignore unknown `_meta` keys.
    pub fn cancellation_context_meta(&self) -> Option<serde_json::Value> {
        match self {
            Self::Cancelled {
                context: Some(ctx), ..
            } => serde_json::to_value(ctx).ok(),
            _ => None,
        }
    }
}
/// Successful prompt/turn payload returned to the ACP layer and trace uploaders.
#[derive(Debug, Clone)]
pub struct PromptTurnOk {
    pub stop_reason: acp::StopReason,
    pub total_tokens: u64,
    pub turn_snapshot: Option<TurnDeltaSnapshot>,
    pub completion_kind: PromptCompletionKind,
    /// Schema-validated `--json-schema` output, delivered to the client in the prompt-response `_meta`.
    /// `None` unless a schema was requested; `Some(Err)` carries a parse/validation error message.
    pub structured_output: Option<Result<serde_json::Value, String>>,
    pub usage: Option<crate::extensions::notification::PromptUsage>,
    pub tool_overrides: Option<distill_sampling_types::ToolOverrides>,
}
pub(crate) type PromptTurnResult = Result<PromptTurnOk, acp::Error>;
pub(crate) fn ok_end_turn(tokens: u64, snapshot: Option<TurnDeltaSnapshot>) -> PromptTurnResult {
    Ok(PromptTurnOk {
        stop_reason: acp::StopReason::EndTurn,
        total_tokens: tokens,
        turn_snapshot: snapshot,
        completion_kind: PromptCompletionKind::Completed,
        structured_output: None,
        usage: None,
        tool_overrides: None,
    })
}
/// Bound on awaiting [`ParsedPromptInfo`] for the metadata upload, shared by the main-session and subagent paths so the two bounds can't drift.
/// The prompt is parsed early in the turn, so this only fires when the turn never dispatched (or the actor is wedged).
pub(crate) const PARSED_PROMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// Pre-parsed prompt metadata sent back to the caller after `parse_prompt`.
pub struct ParsedPromptInfo {
    /// Post-truncation text (what the model sees).
    pub text: String,
    /// Pre-truncation text, only `Some` when truncated.
    pub full_text: Option<String>,
    /// Local disk path embedded in truncated message, only `Some` when truncated.
    pub local_path: Option<std::path::PathBuf>,
}
/// Ordering: `Next < Later` (derived from declaration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationPriority {
    /// Drain mid-turn (between tool calls), for urgent monitor events.
    Next,
    /// Drain only at end-of-turn or when idle. Used for bash task completions.
    Later,
}
#[derive(Debug, Clone)]
pub enum NotificationSource {
    MonitorEvent { task_id: String },
    MonitorCompleted { task_id: String },
    BashTaskCompleted { task_id: String },
}
impl NotificationSource {
    pub fn task_id(&self) -> &str {
        match self {
            Self::MonitorEvent { task_id }
            | Self::MonitorCompleted { task_id }
            | Self::BashTaskCompleted { task_id } => task_id,
        }
    }
}
#[derive(Debug)]
pub struct TaskWakeFallback {
    pub prompt_id: String,
    pub prompt_blocks: Vec<acp::ContentBlock>,
    pub source: NotificationSource,
}
#[derive(Debug)]
pub struct TaskWakeAdmission {
    pub respond_to: oneshot::Sender<bool>,
    pub fallback: TaskWakeFallback,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    /// Running work survives (idle unload, process quiesce, subagent teardown).
    Graceful,
    CancelRunningTurn,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTrigger {
    Esc,
    CtrlC,
    SendNow,
    Shutdown,
    SessionClose,
    SessionDelete,
    Client(String),
}
/// What a cancel means for the session, derived from its trigger by [`CancelTrigger::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    StopGesture,
    Replace,
    Teardown,
}
impl CancelTrigger {
    /// Parse a client's `_meta.cancelTrigger`.
    /// Internal spellings land in [`Self::Client`], so a client-supplied string never maps to an internal trigger.
    pub fn from_client(s: &str) -> Self {
        match s {
            "esc" => Self::Esc,
            "ctrl_c" => Self::CtrlC,
            other => Self::Client(other.to_string()),
        }
    }
    /// The one place a trigger is classified, so a new variant is a single decision.
    /// `Client(_)` lands on `StopGesture`, so an unrecognized wire name fails closed.
    pub fn kind(&self) -> CancelKind {
        match self {
            Self::Esc | Self::CtrlC | Self::Client(_) => CancelKind::StopGesture,
            Self::SendNow => CancelKind::Replace,
            Self::Shutdown | Self::SessionClose | Self::SessionDelete => CancelKind::Teardown,
        }
    }
    pub fn as_str(&self) -> &str {
        match self {
            Self::Esc => "esc",
            Self::CtrlC => "ctrl_c",
            Self::SendNow => "send_now",
            Self::Shutdown => "shutdown",
            Self::SessionClose => "session_close",
            Self::SessionDelete => "session_delete",
            Self::Client(s) => s,
        }
    }
    /// Wire names that are a user Stop. One list for the shell and the pager banner.
    pub fn is_user_gesture_name(name: &str) -> bool {
        matches!(name, "esc" | "ctrl_c" | "mouse" | "dashboard_stop")
    }
    /// Stop click / key only. Unknown `Client` strings stay programmatic so a new wire name cannot claim "by user".
    pub fn is_user_gesture(&self) -> bool {
        Self::is_user_gesture_name(self.as_str())
    }
}
/// What a cancel does to the in-memory conversation history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CancelHistoryDisposition {
    /// Leave the cancelled turn's user message in history.
    #[default]
    Keep,
    /// Pop the named front if the no-output window is still open. `None` is a legacy client.
    RewindIfNoOutput { prompt_id: Option<String> },
}
#[derive(Debug, Clone, Default)]
pub struct CancelOptions {
    pub cancel_subagents: bool,
    pub kill_background_tasks: bool,
    pub history: CancelHistoryDisposition,
    pub trigger: Option<CancelTrigger>,
    /// Drives the cancel-rate metric, and marks an untriggered cancel as the user's.
    pub user_initiated: bool,
}
/// Why the catalog is being (re-)advertised; labels the `slash.advertise` unified-log line.
#[derive(Debug, Clone, Copy)]
pub enum AdvertiseTrigger {
    /// A new session pushed its first catalog.
    SessionStart,
    /// A loaded session re-pushed its catalog to the reconnecting client.
    SessionLoad,
    /// The skill baseline was replaced (disk reload, plugins, bundle sync, `/clear`).
    SkillsReload,
    /// A tool call surfaced new skills mid-turn.
    SkillDiscovery,
    /// A workflow watcher or an explicit workflow reload saw a change.
    WorkflowsChanged,
    /// `set_session_model` swapped in a harness with a different toolset.
    HarnessRebuild,
    /// Per-response token-usage meta refresh; the catalog itself is unchanged.
    UsageMeta,
}
impl AdvertiseTrigger {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionLoad => "session_load",
            Self::SkillsReload => "skills_reload",
            Self::SkillDiscovery => "skill_discovery",
            Self::WorkflowsChanged => "workflows_changed",
            Self::HarnessRebuild => "harness_rebuild",
            Self::UsageMeta => "usage_meta",
        }
    }
}
impl From<SkillUpdateKind> for AdvertiseTrigger {
    fn from(kind: SkillUpdateKind) -> Self {
        match kind {
            SkillUpdateKind::Discovery => Self::SkillDiscovery,
            SkillUpdateKind::BaselineChange => Self::SkillsReload,
        }
    }
}
pub struct SessionModelSwitch {
    pub sampling_config: distill_sampler::SamplerConfig,
    /// Canonical catalogue identity selected by the caller. This stays
    /// separate from `sampling_config.model`, which is the provider wire name.
    pub canonical_model_id: Option<acp::ModelId>,
    pub use_concise: bool,
    /// The two models declare differing `model_family`s, so a lossy compaction runs at switch end.
    pub is_family_switch: bool,
    /// When `false`, skip the system prompt rewrite (concise/default swap).
    /// Set to `false` for forked sessions so mid-session model switches cannot contaminate the inherited prompt configuration.
    pub apply_prompt_override: bool,
    /// When `true`, suppress the system prompt rewrite even though `apply_prompt_override` may be `true`.
    /// Set by the model-switch orchestrator immediately after a successful `RebuildAgentForDefinition`.
    /// The rebuild handler already installed the fresh harness's prompt; the concise/default swap must not clobber it.
    pub skip_prompt_rewrite: bool,
    /// Computed by `MvpAgent` against the new model id.
    /// Per-model remote settings and per-model user TOML overrides then target the right model after a `/model` switch.
    /// The session actor stores this on `compaction.threshold_percent` (which is `Cell<u8>` so it can update without `&mut self`).
    pub auto_compact_threshold_percent: u8,
    pub system_prompt_label: String,
}
#[derive(Debug, Clone, Default)]
pub struct CurrentModel {
    pub id: String,
    pub reasoning_effort: Option<distill_sampling_types::ReasoningEffort>,
    /// Stable catalogue identity, distinct from the provider wire model in `id`.
    pub canonical_id: Option<String>,
    /// Whether this child explicitly pins its model for Jev routing.
    pub model_routing_locked: bool,
}
pub enum SessionCommand {
    Initialize {
        system_prompt: String,
    },
    /// Non-destructive system-prompt sync on session attach: swaps only the leading `System` message, keeping user/assistant turns.
    /// Backed by the atomic `ChatStateCommand::ReplaceSystemHead` (see its doc for the serialization guarantees).
    /// No-op when the live head already matches.
    ReplaceSystemPrompt {
        system_prompt: String,
    },
    /// Sent when a client attaches to a resident session, which the transient `SessionStatus` notification would otherwise never reach.
    EmitStatusSnapshot,
    /// Resume hook: after a session is restored with `awaiting_plan_approval == true`, re-issue the `exit_plan_mode` reverse-request.
    /// The client then re-shows its approval UI over a real live waiter.
    /// Fire-and-forget; the actor spawns the round-trip and the decision.
    RestorePlanApproval,
    /// A `/rename` landed for this resident session.
    /// `manual: true` (a user title) freezes the auto title refresh and aborts any in-flight one.
    /// `manual: false` (`/rename --auto`) reopens it so the whole-conversation refresh can re-title.
    TitleRenamed {
        manual: bool,
    },
    GetToolOverrides {
        respond_to: oneshot::Sender<Option<distill_sampling_types::ToolOverrides>>,
    },
    /// Establish the per-turn tool-overrides state before the first prompt runs.
    /// Sent once by `handle_subagent_request` ahead of the child's first `Prompt`.
    /// A spawned subagent's inherited cutoff is then applied and published (for its own subagents to read) before any turn.
    SetToolOverrides {
        overrides: distill_sampling_types::ToolOverrides,
    },
    Prompt {
        prompt_id: String,
        prompt_blocks: Vec<acp::ContentBlock>,
        /// Prompt mode parsed from request `_meta.mode`.
        prompt_mode: PromptMode,
        #[allow(private_interfaces)]
        artifact_upload_ctx: Option<crate::upload::manifest::ArtifactUploadContext>,
        /// Optional client identifier from the prompt request meta (overrides session-level one)
        client_identifier: Option<String>,
        /// Optional screen mode from the prompt request meta (`_meta.screenMode`, pager-only: `fullscreen` | `inline` | `minimal` | `headless`).
        /// Telemetry-only; `None` for other clients and synthetic prompts.
        screen_mode: Option<String>,
        /// Skip `<user_query>` wrapping and large-prompt truncation.
        verbatim: bool,
        /// W3C traceparent from the caller's OTEL span context, used to link `session.handle_prompt` back to `agent.prompt` across the channel hop.
        traceparent: Option<String>,
        json_schema: Option<serde_json::Value>,
        /// Cancel-and-send: cancel the running turn and run this prompt next.
        /// Also derived server-side during an interruptible wait (see [`SessionActor::queue_input`]).
        send_now: bool,
        /// Actor-authoritative admission and deferred fallback for terminal task wakes.
        admission: Option<TaskWakeAdmission>,
        tool_overrides_update: Option<distill_sampling_types::ToolOverridesUpdate>,
        respond_to: oneshot::Sender<PromptTurnResult>,
        /// Optional initial-child readiness signal.
        /// Carried onto the queued item and resolved only when that exact row is promoted, or closed on removal.
        prompt_admitted: Option<oneshot::Sender<oneshot::Sender<()>>>,
        /// Optional oneshot fired once the prompt's persistence is settled, before LLM inference begins.
        /// When a `UserPromptSubmit` hook blocked the prompt it fires immediately: nothing was stored, so there is nothing to flush.
        /// Used by callers that need `chat_history.jsonl` settled before trace snapshots or `session/load`; a blocked prompt never appears there.
        persist_ack: Option<oneshot::Sender<()>>,
        /// Pre-parsed prompt content blocks from `parse_prompt`, sent back to the caller.
        /// The caller can then use the fully-rendered prompt for metadata.json without re-parsing.
        /// The session sends on this channel right after parsing.
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    },
    /// Admit an owning parent's message as an ordinary protected turn or running-turn Steer.
    ParentAgentMessage {
        delivery: distill_tools::implementations::distill::task::types::ActiveAgentMessageDelivery,
        #[allow(private_interfaces)]
        receipt_sink: tokio::sync::mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: distill_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<
            distill_tools::implementations::distill::task::coordinator::ActiveMessageAdmission,
        >,
    },
    SessionMode {
        session_mode: acp::SessionModeId,
        responds_to: oneshot::Sender<()>,
    },
    SetSessionModel {
        switch: SessionModelSwitch,
        responds_to: oneshot::Sender<Result<acp::ModelId, acp::Error>>,
    },
    /// Set only the reasoning effort on the session's live model. Carrying no
    /// model keeps a concurrent `SetSessionModel` from being reverted.
    SetReasoningEffort {
        effort: distill_sampling_types::ReasoningEffort,
        responds_to: oneshot::Sender<Result<acp::ModelId, acp::Error>>,
    },
    /// Zero-turn harness rebuild: build a brand-new `Agent` from the session's `AgentRebuildSpec` and the new `AgentDefinition`.
    /// Re-register MCP tools, swap the live `Agent`, and rewrite the system message in the conversation.
    /// Triggered by `MvpAgent::set_session_model` when the new model's `agent_type` differs from the session's current one.
    RebuildAgentForDefinition {
        definition: distill_agent::AgentDefinition,
        system_prompt_label: String,
        responds_to: oneshot::Sender<Result<(), acp::Error>>,
    },
    /// Signals then report the override model rather than the agent-level default.
    /// `SetSessionModel` does NOT update `primaryModelId` in signals; the resolved model is already tracked via inference responses.
    /// Keeps the existing base_url, api_key, and other config; only the `model` field in the `x-grok-model-override` header changes.
    OverrideModelName {
        model_name: String,
        extra_headers: indexmap::IndexMap<String, String>,
        /// Override the context window size for the new model.
        /// Without this, forked sessions inherit the source session's context window.
        /// Auto-compact and context-usage signals then use the wrong threshold.
        context_window: Option<std::num::NonZeroU64>,
    },
    GetCurrentModel {
        responds_to: oneshot::Sender<CurrentModel>,
    },
    GetCurrentPromptMode {
        responds_to: oneshot::Sender<PromptMode>,
    },
    GetModelMetadata {
        responds_to: oneshot::Sender<distill_chat_state::ModelMetadata>,
    },
    /// Snapshot for `/session-info`.
    GetSessionInfo {
        responds_to: oneshot::Sender<SessionInfoData>,
    },
    CompactSession {
        user_context: Option<String>,
        respond_to: oneshot::Sender<acp::Result<()>>,
    },
    /// Reload plugin hooks and registry mid-session.
    ReloadPlugins {
        registry: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    },
    /// Re-discover the session's own project hooks (`.grok/hooks`, `.cursor/hooks.json`, …) mid-session, re-evaluating folder trust.
    /// Used by the interactive folder-trust grant so a granted folder's repo-local hooks start without a session restart.
    /// Plugin-contributed hooks are handled by `ReloadPlugins`; this covers the non-plugin project hook registry.
    ReloadHooks,
    /// Re-discover skills from disk and update the session's skill baseline.
    RefreshSkillBaseline,
    /// Capture every completed turn now for `x.ai/memory/flush`.
    FlushMemory {
        respond_to: oneshot::Sender<crate::extensions::memory::MemoryFlushResponse>,
    },
    /// Consolidate memory now for `x.ai/memory/dream`, bypassing the automatic gates.
    MemoryDream {
        respond_to: oneshot::Sender<crate::extensions::memory::MemoryDreamResponse>,
    },
    /// List memory files and state for `x.ai/memory/list`.
    MemoryList {
        respond_to: oneshot::Sender<Result<crate::extensions::memory::MemoryListing, String>>,
    },
    /// Turn memory on or off for `x.ai/memory/toggle`.
    MemoryToggle {
        enabled: bool,
        respond_to: oneshot::Sender<crate::extensions::memory::MemoryToggleResponse>,
    },
    /// Delete one memory note for `x.ai/memory/forget`.
    MemoryForget {
        path: String,
        expected_content_hash: String,
        respond_to: oneshot::Sender<crate::extensions::memory::MemoryForgetResponse>,
    },
    /// Auto-approve all permission prompts when `enabled`.
    SetYoloMode {
        enabled: bool,
    },
    /// Set auto permission mode (LLM classifier for non-fast-path tools).
    SetAutoMode {
        enabled: bool,
    },
    ResetPermissionState,
    Rewind {
        request: RewindRequest,
        respond_to: oneshot::Sender<anyhow::Result<RewindResponse>>,
    },
    /// Out-of-band history repair (`x.ai/session/repair`): fix tool-pairing violations that would otherwise 400 on every request.
    /// The violations: orphaned or displaced `ToolResult`s, duplicates, and unanswered calls.
    /// `dry_run` only reports.
    RepairHistory {
        dry_run: bool,
        respond_to: oneshot::Sender<
            anyhow::Result<distill_chat_state::compaction_utils::HistoryRepairReport>,
        >,
    },
    GetRewindPoints {
        respond_to: oneshot::Sender<RewindPointsResponse>,
    },
    /// Local file-snapshot counts keyed by `prompt_index`, read straight from the file-state tracker.
    /// The tracker is independent of the chat-state prompt index, which is empty in bridge mode.
    /// The bridge joins these onto the server's rewind points so `num_file_snapshots`/`has_file_changes` match what local-mode rewind reports.
    GetRewindFileCounts {
        respond_to: oneshot::Sender<std::collections::HashMap<usize, usize>>,
    },
    /// Reconcile the file-state rewind tracker after a bridge-mode `ConversationOnly` rewind that already committed server-side.
    /// Runs the same tracker bookkeeping `handle_rewind` does for `ConversationOnly`.
    /// The bridge therefore does not block its response on the merge.
    ReconcileRewindTracker {
        target_prompt_index: usize,
    },
    /// Acquire the image-strip rewrite boundary before a bridge rewind.
    AcquireImageStripRewrite {
        respond_to: oneshot::Sender<tokio::sync::OwnedRwLockWriteGuard<()>>,
    },
    /// Invalidate pending image-strip work after a server-side conversation rewind commits.
    /// Acknowledged for ordering.
    InvalidateImageStripsForRewind {
        respond_to: oneshot::Sender<()>,
    },
    /// Grok extension session notification: client-side events to store in persistence
    XaiSessionNotification {
        notification: SessionNotification,
    },
    /// Apply subagent usage into parent ledgers.
    /// Acks `()` once chat state has applied it (prompt-attributed or session-only).
    /// Drop the oneshot on failure so the child treats the fold as not landed.
    RecordSubagentUsage {
        by_model: Vec<(String, distill_chat_state::UsageTotals)>,
        parent_prompt_id: Option<String>,
        /// Nested subagent bill may under-count.
        incomplete: bool,
        respond_to: oneshot::Sender<()>,
    },
    /// Set the sticky incomplete flag for a parent prompt (or the live pin when `None`). Acks when marked.
    MarkSubagentUsageNotApplied {
        parent_prompt_id: Option<String>,
        respond_to: oneshot::Sender<()>,
    },
    /// Attach usage on the error path, under the same policy as the durable TurnCompleted.
    ErrorPathUsageFallback {
        prompt_id: Option<String>,
        respond_to: oneshot::Sender<Option<crate::extensions::notification::PromptUsage>>,
    },
    /// Persist the monotonic telemetry turn counter ("next trace turn") for the session.
    SetNextTraceTurn {
        next_trace_turn: u64,
        request_id: Option<String>,
    },
    /// Flush pending writes and copy the current session directory contents to memory.
    /// The caller can then tar.gz and upload to GCS (or similar).
    CopyFile {
        respond_to: oneshot::Sender<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
    },
    /// Flush the replay buffer and persistence, then signal completion.
    /// Used during reconnect to ensure all buffered content is persisted before replay.
    FlushComplete {
        respond_to: oneshot::Sender<std::io::Result<()>>,
    },
    #[cfg(test)]
    InjectTurnCompletion {
        prompt_id: String,
        #[allow(private_interfaces)]
        epoch: crate::session::TurnEpoch,
        result: Box<PromptTurnResult>,
        elapsed_ms: Option<u64>,
        processed: oneshot::Sender<()>,
    },
    /// Update MCP servers for an existing session (used during reconnect or mid-session via the `x.ai/session/update_mcp_servers` extension method).
    /// This replaces the current MCP server configuration and triggers re-initialization.
    /// The caller is notified via `respond_to` once MCP re-initialization completes (or immediately if configs are unchanged).
    UpdateMcpServers {
        mcp_servers: Vec<acp::McpServer>,
        respond_to: oneshot::Sender<Result<(), acp::Error>>,
    },
    /// Re-apply per-attachment policy (MCP init strategy, delivery tools) from a resident `session/load` that carried explicit `startupHints`.
    /// Spawn-time structural hints are NOT touched.
    /// Sent fire-and-forget alongside `UpdateMcpServers` on the reconnect rail.
    UpdateAttachPolicy {
        startup_hints: Box<crate::session::StartupHints>,
    },
    /// Toggle an MCP server on/off within the session actor's event loop.
    /// Atomic read-modify-write avoids TOCTOU races with background config refreshes.
    /// Those refreshes can change `mcp_state.configs` between a snapshot read and an `UpdateMcpServers` command.
    ToggleMcpServer {
        server_name: String,
        enabled: bool,
        /// Fully-formed server config to add when re-enabling.
        /// Built by the caller via `merge_managed_mcp_servers` (with OAuth headers injected).
        /// `None` when disabling.
        server_config: Option<acp::McpServer>,
        respond_to: oneshot::Sender<Result<(), acp::Error>>,
    },
    /// Toggle a single MCP tool on/off within a server.
    /// The server stays connected; only the tool's registration in ToolBridge is affected.
    ToggleMcpTool {
        server_name: String,
        tool_name: String,
        enabled: bool,
        is_managed_gateway: bool,
        respond_to: oneshot::Sender<Result<(), acp::Error>>,
    },
    /// Read MCP status: which servers are configured, which clients are healthy, what tools.
    GetMcpStatus {
        respond_to: oneshot::Sender<crate::extensions::mcp::McpStatusSnapshot>,
    },
    GetManagedGatewayDisabledTools {
        respond_to:
            oneshot::Sender<std::collections::HashMap<String, std::collections::HashSet<String>>>,
    },
    /// Snapshot the session's live MCP client pool for subagent inheritance.
    SnapshotMcpPool {
        respond_to: oneshot::Sender<Option<crate::session::mcp_servers::SharedMcpPool>>,
    },
    /// Snapshot the session's client-registered hooks so a subagent inherits the same PreToolUse gate and observe hooks over the parent's connection.
    SnapshotClientHooks {
        respond_to: oneshot::Sender<crate::extensions::hooks::ClientHooks>,
    },
    /// Snapshot the session's resolved tool schema (the same list the parent's own turn sends).
    /// A verbatim-fork child can then present a byte-identical tool prefix.
    SnapshotToolDefinitions {
        respond_to: oneshot::Sender<Vec<distill_sampling_types::ToolSpec>>,
    },
    /// Replace the session's client-registered hooks.
    /// Sent on `load_session` reconnect to a live actor so a client can re-register (or clear) its hooks without a fresh session.
    SetClientHooks {
        hooks: crate::extensions::hooks::ClientHooks,
    },
    /// Client-driven MCP tool call outside the LLM loop.
    CallMcpTool {
        server_name: String,
        server_url: Option<String>,
        tool_name: String,
        arguments: serde_json::Value,
        respond_to: oneshot::Sender<Result<crate::extensions::mcp::McpCallResponse, String>>,
    },
    ReadMcpResource {
        server_name: String,
        uri: String,
        respond_to:
            oneshot::Sender<Result<crate::extensions::mcp::McpReadResourceResponse, String>>,
    },
    McpAuthStatus {
        respond_to: oneshot::Sender<Vec<crate::extensions::mcp::McpAuthStatusEntry>>,
    },
    McpAuthTrigger {
        server_name: String,
        respond_to: oneshot::Sender<Result<(), String>>,
    },
    RetryAuthRequiredServers {
        respond_to: oneshot::Sender<()>,
    },
    RefreshMcpSearchIndex,
    /// Move a foreground bash command to background by tool_call_id.
    /// Unblocks the agent loop so it can continue with the next action.
    BackgroundForegroundCommand {
        tool_call_id: String,
        respond_to: oneshot::Sender<bool>,
    },
    /// Routes through the ToolBridge's TerminalBackend (lock-free, Arc-shared).
    KillBackgroundTask {
        task_id: String,
        source: distill_tools::types::KillSource,
        respond_to: oneshot::Sender<Result<distill_tools::types::KillOutcome, String>>,
    },
    DeleteScheduledTask {
        task_id: String,
        respond_to: oneshot::Sender<Result<bool, String>>,
    },
    /// Routes through the ToolBridge's TerminalBackend.
    ListTasks {
        respond_to: oneshot::Sender<Option<Vec<distill_tools::types::TaskSnapshot>>>,
    },
    /// Persist + broadcast the current background-task list via
    /// `x.ai/session_notification` (`SessionUpdate::BackgroundTasks`).
    ///
    /// `respond_to` means load enqueued the persist+broadcast before returning.
    /// It is not a client-delivery ack. Live incremental follow-ups leave it `None`.
    ///
    /// `pending` is the bridge coalesce bit. Live incrementals pass `Some` so
    /// a burst collapses to one emit; load leaves it `None` (forced flush).
    EmitBackgroundTasksSnapshot {
        respond_to: Option<oneshot::Sender<()>>,
        pending: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    },
    /// Query whether the session has work in flight: a running turn
    /// (`running_task.is_some()`) **or** queued inputs
    /// (`pending_inputs` non-empty). Used by the leader's idle-unload decision
    /// on client disconnect (the no-evict keystone) to avoid unloading a
    /// session that still has pending work.
    IsBusy {
        respond_to: oneshot::Sender<bool>,
    },
    GetHooksList {
        respond_to: oneshot::Sender<distill_hooks_plugins_types::HooksListResponse>,
    },
    /// Execute a hooks management action from the pager modal.
    HooksAction {
        action: distill_hooks_plugins_types::HooksAction,
        respond_to: oneshot::Sender<distill_hooks_plugins_types::ActionOutcome>,
    },
    NotifyPluginUpdates {
        updates: Vec<(String, String, String)>,
    },
    /// Execute a plugins management action from the pager modal.
    PluginsAction {
        action: distill_hooks_plugins_types::PluginsAction,
        respond_to: oneshot::Sender<distill_hooks_plugins_types::ActionOutcome>,
    },
    /// This session's plugin registry, as served by `x.ai/plugins/list`.
    PluginsList {
        respond_to: oneshot::Sender<Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>>,
    },
    /// Inject a notification (monitor event or bash task completion) into the session's notification queue.
    /// Notifications wait for an idle session and are batched by `maybe_drain_notifications`.
    InjectNotification {
        prompt_id: String,
        prompt_blocks: Vec<acp::ContentBlock>,
        priority: NotificationPriority,
        source: NotificationSource,
    },
    /// Drop queued or mid-turn-buffered `MonitorEvent` notifications for a task.
    /// Used when a natural monitor exit already auto-woke via `TaskCompleted`.
    /// Stdout and terminal pipeline events then do not start a second `NotificationDrain` turn for the same completion.
    DropMonitorNotifications {
        task_id: String,
    },
    /// Dispatch a compat `Notification` hook (e.g. `task_complete` from the notification bridge, which does not go through `send_xai_notification`).
    DispatchNotificationHook {
        notification_type: String,
        message: Option<String>,
        title: Option<String>,
        level: Option<String>,
    },
    /// Record background-task ids reparented from a harness-internal verifier/planner subagent's surviving dev server on subagent exit.
    /// The handler inserts them into `goal_turn_task_ids` whenever the goal harness is enabled (not gated on the racy `Active` status).
    /// Their late auto-wake completions are then suppressed by `maybe_drain_notifications`.
    RecordGoalTurnTaskIds {
        task_ids: Vec<String>,
    },
    /// Versioned and idempotent: a stale `expected_version` or an already-drained `id` is a no-op.
    /// On a no-op the actor just re-broadcasts the current queue so the client reconciles.
    /// When `owner` is `Some`, the removal only applies if the item's attribution matches (edit authority: a client edits its own items).
    RemoveQueuedPrompt {
        id: String,
        expected_version: u64,
        owner: Option<String>,
    },
    /// Reorder the queued (not-yet-running) prompts to match `ordered_ids`.
    /// Ids not present in the live queue are ignored; queued items missing from `ordered_ids` keep their relative order at the back.
    /// The actor re-broadcasts the resulting queue. Idempotent.
    ReorderQueue {
        ordered_ids: Vec<String>,
    },
    /// Clear queued (not-yet-running) prompts.
    /// When `owner` is `Some`, only that client's items are cleared.
    /// The running turn is never touched.
    ClearQueue {
        owner: Option<String>,
    },
    /// Replace the text of a queued (not-yet-running) prompt in place (server-side LWW).
    /// Last write wins via the actor's serialized mailbox; the rebroadcast of `x.ai/queue/changed` is the truth signal for every attached client.
    /// The original `owner` attribution is preserved; `editor` is recorded as the most recent editor (for future "alice edited this" UX).
    EditQueuedPrompt {
        id: String,
        new_text: String,
        editor: Option<String>,
    },
    /// Hold a queued prompt while a client edits it in the composer: skip it as a combine follower **and** block promote while it is the queue front.
    /// Released via [`Self::ReleaseEdit`], or cleared by edit / remove / interject.
    HoldEdit {
        id: String,
    },
    /// Release a previous [`Self::HoldEdit`].
    /// Re-kicks the promoter so a previously held front can start when the session is idle.
    ReleaseEdit {
        id: String,
    },
    /// Atomically interject a queued (not-yet-running) prompt into the running turn.
    /// The in-flight turn merges it at the next safe point, and the prompt can never both interject *and* later run as its own turn.
    /// Versioned and idempotent like [`RemoveQueuedPrompt`].
    InterjectQueuedPrompt {
        id: String,
        expected_version: u64,
        owner: Option<String>,
        /// Optional replacement text (client-edited row).
        /// When `Some`, it is interjected INSTEAD of the stored queue text, under the same single version check.
        /// Edit and interject are one atomic op: a stale version no-ops the whole thing, edited text included.
        new_text: Option<String>,
    },
    Cancel(CancelOptions),
    Shutdown(ShutdownKind),
    PersistResumeStatus {
        respond_to: oneshot::Sender<()>,
    },
    /// Force-trigger a feedback request notification for local client testing.
    /// Bypasses all heuristics, sampling, and cooldown checks.
    TriggerTestFeedback {
        tier: crate::session::feedback::FeedbackTier,
        mode: crate::session::feedback::FeedbackMode,
        respond_to: oneshot::Sender<anyhow::Result<acp::ExtResponse>>,
    },
    /// Persist a local feedback entry via the persistence actor.
    /// feedback.jsonl is then written through the same channel as other session files and included in GCS CopyFile snapshots.
    PersistFeedback(Box<crate::session::persistence::LocalFeedbackEntry>),
    AdvertiseCommands {
        trigger: AdvertiseTrigger,
    },
    GetWorkflowCatalogState {
        respond_to: oneshot::Sender<(bool, bool)>,
    },
    ListAvailableCommands {
        respond_to: oneshot::Sender<crate::session::slash_commands::ListCommandsResponse>,
    },
    /// Re-discover skills from disk, update the SkillManager baseline, and re-advertise slash commands to the client.
    ReloadSkills,
    /// Dispatch session_start hook using the actor's loaded HookRegistry.
    DispatchSessionStartHook {
        /// "new" for brand new sessions, "load" for sessions loaded from disk.
        source: String,
    },
    /// Retrieve session context for enriching a feedback Slack notification.
    GetFeedbackContext {
        turn_number: Option<i64>,
        responds_to: oneshot::Sender<FeedbackContext>,
    },
    /// Returns the name of the `AgentDefinition` that was used to initialize this session (or the most recent one applied via `handle_session_mode`).
    /// Used by `mvp_agent.set_session_model` to check whether a model's `agent_type` is compatible with the current session before switching.
    GetActiveAgent {
        responds_to: oneshot::Sender<Option<String>>,
    },
    /// Ask a side question without interrupting the current turn.
    /// The session snapshots the conversation context, makes a single tool-free model call, and returns the response text.
    SideQuestion {
        question: String,
        /// Images attached to this side question. Empty for the legacy text-only path.
        images: Vec<acp::ImageContent>,
        respond_to: oneshot::Sender<Result<String, SideQuestionError>>,
    },
    /// Generate a session recap (a short "where was I" summary) and broadcast it to clients via `SessionUpdate::SessionRecap`.
    /// Fire-and-forget: the session snapshots the conversation, makes a single tool-free model call, and emits the result for display only.
    /// It never mutates the conversation, so unlike `SideQuestion` it needs no reply channel; the answer travels back as a notification.
    Recap {
        /// `true` when triggered automatically on return-from-away, `false` for an explicit `/recap`.
        auto: bool,
    },
    /// Request an AI-generated shell command suggestion.
    /// The session actor builds a minimal prompt from `prefix` and `cwd`.
    /// It calls the sampler with low temperature and low max_tokens, and returns the suggested completion via `respond_to`.
    AISuggest {
        prefix: String,
        cwd: String,
        model_override: Option<String>,
        respond_to: oneshot::Sender<Option<String>>,
    },
    /// The session builds a compact text-only transcript of the recent conversation and makes one tool-free model call.
    /// The call defaults to `grok-4.6` when available via `model_override`, else it uses the session model.
    /// Best-effort: any failure returns `None`.
    SuggestPrompt {
        model_override: Option<String>,
        respond_to: oneshot::Sender<Option<String>>,
    },
    /// Rewrite a raw memory note into well-structured markdown via a one-shot LLM call.
    /// The session uses `prepare_chat_completion()` with the `grok-4.6` model, low temperature, and capped output tokens.
    RewriteMemoryNote {
        raw_text: String,
        context_summary: String,
        respond_to: oneshot::Sender<Result<String, String>>,
    },
    /// Inject a user message into the active turn without canceling it.
    /// The text is queued in `pending_interjections` and drained at the next safe point in `process_conversation_turn`.
    /// Fire-and-forget: no response channel needed since the command just pushes to a Mutex.
    Interject {
        text: String,
        /// Client-minted id echoed back on the broadcast `x.ai/session/interjection` so the originating pager can dedup its optimistic local block.
        /// `None` from older clients.
        id: Option<String>,
        /// Pasted images attached to the interjection.
        /// Empty from text-only or older clients.
        images: Vec<acp::ImageContent>,
    },
    /// Trigger a model turn so the model can print a visible goal progress summary.
    /// The goal orchestrator injects a system reminder into context (via `push_parent_reminder`) *before* sending this command.
    /// The session actor queues a short synthetic prompt instructing the model to summarize the reminder, then calls `maybe_start_running_task`.
    GoalSummaryTurn {
        /// Short instruction appended as a verbatim user message.
        prompt_text: String,
    },
    WorkflowCompletionTurn {
        run_id: String,
        revision: u64,
    },
    /// Take turn messages from the chat state actor (proxied from mvp_agent).
    TakeTurnMessages {
        respond_to: oneshot::Sender<Option<distill_chat_state::TurnCapture>>,
    },
    /// Drain the sealed harness trace turns (goal planner and verifier panels) from the chat state actor (proxied from mvp_agent).
    /// Routed through the session actor (like `TakeTurnMessages`) so the drain is ordered ahead of any subsequent turn's harness recording.
    /// Each `Vec` is one turn's synthetic `task` pairs, uploaded as its own sibling `turn_{N}` artifact.
    TakeHarnessTraceTurns {
        respond_to:
            oneshot::Sender<Vec<Vec<distill_sampling_types::conversation::ConversationItem>>>,
    },
    /// Returns `Some(...)` when the current turn streamed reasoning or text but the canonical assistant response never reached `chat_state`.
    /// The consumer uploads it as `streaming_partial.json` for trace inspection; `chat_state` is never mutated by this command.
    /// `prompt_id` lets the handler detect a race.
    TakeStreamingCapture {
        prompt_id: String,
        #[allow(private_interfaces)]
        respond_to: oneshot::Sender<Option<crate::session::acp_session::StreamingTurnCapture>>,
    },
    /// Persist the current git HEAD commit and branch to summary.json.
    ///
    /// Sent at the end of each prompt turn so `--restore-code` sees the latest HEAD even when the `GitHeadChanged` filesystem watcher misses events.
    PersistGitHead {
        commit: Option<String>,
        branch: Option<String>,
    },
}
#[cfg(test)]
mod cancellation_category_meta_tests {
    use super::PromptCompletionKind;
    use distill_session_events::types::CancellationCategory;
    /// Pins every `_meta.cancellationCategory` wire name: shipped clients string-match these, so a rename is a wire break the compiler can't see.
    #[test]
    fn pins_every_wire_name() {
        use super::{
            HOOK_DENIED_CATEGORY, MID_TURN_ABORT_CATEGORY, PERMISSION_CANCELLED_CATEGORY,
            PERMISSION_REJECTED_CATEGORY,
        };
        let cancelled = |category| PromptCompletionKind::Cancelled {
            category,
            context: None,
        };
        for (kind, expected) in [
            (
                cancelled(Some(CancellationCategory::HookDenied)),
                Some(HOOK_DENIED_CATEGORY),
            ),
            (
                cancelled(Some(CancellationCategory::MidTurnAbort)),
                Some(MID_TURN_ABORT_CATEGORY),
            ),
            (
                cancelled(Some(CancellationCategory::PermissionRejected)),
                Some(PERMISSION_REJECTED_CATEGORY),
            ),
            (
                cancelled(Some(CancellationCategory::PermissionCancelled)),
                Some(PERMISSION_CANCELLED_CATEGORY),
            ),
            (cancelled(None), None),
            (
                PromptCompletionKind::MaxTurnsReached { limit: 1 },
                Some("max_turns_reached"),
            ),
            (
                PromptCompletionKind::StationarityEnded,
                Some("action_stationarity"),
            ),
            (PromptCompletionKind::Completed, None),
            (PromptCompletionKind::Rewound, None),
            (PromptCompletionKind::RemovedFromQueue, None),
        ] {
            assert_eq!(
                kind.cancellation_category_meta().as_deref(),
                expected,
                "{kind:?}"
            );
        }
    }
}
#[cfg(test)]
mod cancel_trigger_tests {
    use super::{CancelKind, CancelTrigger};
    #[test]
    fn classifies_every_trigger() {
        for (trigger, expected) in [
            (CancelTrigger::Esc, CancelKind::StopGesture),
            (CancelTrigger::CtrlC, CancelKind::StopGesture),
            (CancelTrigger::from_client("mouse"), CancelKind::StopGesture),
            (
                CancelTrigger::from_client("some_future_gesture"),
                CancelKind::StopGesture,
            ),
            (CancelTrigger::SendNow, CancelKind::Replace),
            (CancelTrigger::Shutdown, CancelKind::Teardown),
            (CancelTrigger::SessionClose, CancelKind::Teardown),
            (CancelTrigger::SessionDelete, CancelKind::Teardown),
        ] {
            assert_eq!(trigger.kind(), expected, "{trigger:?}");
        }
    }
    #[test]
    fn user_gesture_is_stop_clicks_and_keys_only() {
        assert!(CancelTrigger::is_user_gesture_name("esc"));
        assert!(CancelTrigger::is_user_gesture_name("mouse"));
        assert!(!CancelTrigger::is_user_gesture_name("send_now"));
        assert!(CancelTrigger::Esc.is_user_gesture());
        assert!(CancelTrigger::CtrlC.is_user_gesture());
        assert!(CancelTrigger::from_client("mouse").is_user_gesture());
        assert!(CancelTrigger::from_client("dashboard_stop").is_user_gesture());
        assert!(!CancelTrigger::from_client("some_future_gesture").is_user_gesture());
        assert!(!CancelTrigger::from_client("host_interrupt").is_user_gesture());
        assert!(!CancelTrigger::SendNow.is_user_gesture());
        assert!(!CancelTrigger::Shutdown.is_user_gesture());
        assert!(!CancelTrigger::SessionClose.is_user_gesture());
        assert!(!CancelTrigger::SessionDelete.is_user_gesture());
    }
}
