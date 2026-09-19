// Modified for Distill by Samuel Fajreldines, 2026.
//! `SessionHandle`: the `Clone + Send` proxy for interacting with a session actor.
//!
//! Callers hold a `SessionHandle` and send `SessionCommand` messages via the internal channel.
use super::commands::SessionCommand;
use super::persistence::{LocalFeedbackEntry, PersistenceMsg};
use agent_client_protocol as acp;
use distill_file_utils::queue::UploadQueue;
use distill_hunk_tracker::HunkTrackerHandle;
use distill_sampling_types::ReasoningEffort;
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, oneshot};
/// Coarse lifecycle state of a session as known to the leader/agent.
/// A grok session is a resumable log on disk with no terminal status field of its own, so "liveness" is residency plus turn state, not a pid.
/// The agent's join-handle supervisor tracks this per session so a panicked actor is demoted to `Dormant` instead of lingering in the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLiveState {
    /// Resident actor, a turn is currently running.
    Working,
    /// Resident actor, no turn in flight.
    IdleResident,
    /// On disk, not resident (idle-unloaded or never loaded this run).
    Dormant,
    /// Finished and resumable (terminal marker on disk).
    Completed,
    /// Actor panicked / load failed: the `JoinHandle` ended with no terminal marker.
    /// Harmless to reap; the conversation persists and demotes to `Dormant` on the next disk scan.
    DeadFailed,
    /// A load or resume is building the actor.
    Attaching,
}
/// `_meta` key carrying the persistent-memory implementation pinned at session spawn.
pub const MEMORY_MODE_META_KEY: &str = "x.ai/memoryMode";
/// Everything the `session/new` reply reads from session state; built before the actor task starts so the reply cannot wait on it.
#[derive(Clone)]
pub struct SpawnSnapshot {
    pub applied_tool_overrides: Option<distill_sampling_types::ToolOverrides>,
    /// Persistent-memory implementation pinned when the session was spawned.
    pub memory_mode: Option<crate::config::MemoryMode>,
}
pub(crate) struct WorkGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl WorkGuard {
    pub(crate) fn new(active_work: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        active_work.fetch_add(1, std::sync::atomic::Ordering::Release);
        Self(active_work)
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}
#[derive(Clone)]
pub struct SessionHandle {
    pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub(crate) persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
    /// Shared with the actor so external cancellation can target subagents launched by the active turn only.
    pub current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared `Arc` with the actor (insert on issue, remove on resolve); never persisted.
    pub pending_interactions: crate::session::pending_interaction::PendingInteractions,
    pub(crate) active_work: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub info: crate::session::info::Info,
    /// `None` means unlimited.
    pub max_turns: Option<usize>,
    pub resolved_tool_overrides:
        std::sync::Arc<arc_swap::ArcSwapOption<distill_sampling_types::ToolOverrides>>,
    pub spawn_snapshot: SpawnSnapshot,
    pub hunk_tracker_handle: HunkTrackerHandle,
    pub chat_state_handle: distill_chat_state::ChatStateHandle,
    /// Handle to session signals (used for completion tracking)
    pub signals_handle: super::signals::SessionSignalsHandle,
    /// Shared gate controlling whether the session actor forwards notifications to the client via the gateway.
    /// See [`SessionActor::gateway_enabled`] for details.
    pub gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When `false`, suppress local `background_tasks` snapshot emits.
    /// Shared with the notification bridge and session actor; flipped off for
    /// gateway-backed sessions so an empty local registry cannot clear remote Running.
    pub emit_local_background_tasks: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Status-line and live user-echo gates. Shared with [`super::notifications::NotificationSender`].
    pub(crate) client_caps: super::notifications::SessionClientCaps,
    /// MCP server configs for this session (merged local and client-provided).
    /// Stored on the handle so forked sessions can inherit the parent's MCP servers without a round-trip through the session actor.
    pub mcp_servers: Vec<acp::McpServer>,
    /// Client-provided MCP servers as admitted by the vendor `mcps` kill-switch, before merging with disk/plugin/managed servers.
    /// Hot-reloads re-merge from this seed; a server the kill-switch rejected cannot reappear because its on-disk attribution vanished mid-session.
    pub initial_client_mcp_servers: Vec<acp::McpServer>,
    /// Stable display path for forked sessions (original project path).
    /// When set, the hunk tracker extension handler rewrites worktree paths in API responses to this path.
    /// The client UI then shows the original project path, not the worktree path.
    pub display_cwd: Option<String>,
    /// Feedback manager for periodic signal sync.
    /// Exposed so callers can attach GCS upload queue stats for snapshotting into signals.
    pub feedback_manager: std::sync::Arc<crate::session::feedback_manager::FeedbackManager>,
    /// Session-scoped upload queue. Lazily initialized on the first turn that enables trace uploads.
    /// `Arc<OnceLock<_>>` ensures all `SessionHandle` clones share the same underlying queue instance.
    pub(crate) upload_queue: std::sync::Arc<std::sync::OnceLock<UploadQueue>>,
    /// Consecutive upload failures with no confirmed upload in between, driving this session's upload-failure log suppression.
    /// Shared across handle clones but per-session, so one session's bucket outage cannot mute another session's first-failure log.
    /// Each session's unified_log artifact must carry evidence of its own failures.
    pub(crate) upload_failures_since_success: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Session context captured at spawn time so callers can inherit shared runtime state.
    pub tool_context: crate::tools::ToolContext,
    /// The model this session was created with (or switched to via setModel).
    /// Per-session tracking prevents cross-client contamination in leader mode where `MvpAgent.current_model_id` is shared mutable state.
    pub model_id: acp::ModelId,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// YOLO (auto-approve) mode for this session.
    /// Per-session tracking prevents cross-client contamination in leader mode where one client enabling YOLO could affect another client's sessions.
    pub yolo_mode: bool,
    /// Explicit origin client metadata captured when the session was created.
    /// Used for per-session User-Agent rendering and for scoping leader-mode client behaviors like yolo broadcasts.
    pub origin_client: Option<crate::http::OriginClientInfo>,
    /// Whether the client that created this session advertised `x.ai/codeNavigation.enabled`.
    /// Stored per-session for leader mode.
    /// A later `initialize()` from a different client cannot retroactively change code-nav eligibility for already-running sessions.
    pub code_nav_enabled: bool,
    /// Whether the `ask_user_question` tool is exposed for this session.
    /// The gates are `_meta.askUserQuestion` / `--no-ask-user` and the remote settings / config / env gate.
    /// Subagents deliberately do not inherit it.
    pub ask_user_question_enabled: bool,
    /// Whether this session was spawned non-interactive (`startupHints.nonInteractive`, e.g. headless `-p` / SDK).
    /// Stored per-session so subagents inherit it at spawn.
    pub non_interactive: bool,
    /// Plan mode tracker, shared with the session actor via Arc.
    /// Exposed so the `x.ai/toggle_plan_mode` handler can toggle plan mode without going through the session command channel.
    pub plan_mode: std::sync::Arc<parking_lot::Mutex<crate::session::plan_mode::PlanModeTracker>>,
    /// Debug flag: when set to `true`, the next turn unconditionally triggers auto-compaction regardless of context window usage.
    /// Consumed (reset to `false`) atomically on use via `compare_exchange`.
    /// Set via `x.ai/debug/arm_auto_compact`.
    pub force_compact: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub permission_handle: distill_workspace::permission::PermissionHandle,
    /// The parent SessionActor's live `Auth401AttributionCallback` (if any).
    /// Exposed on the handle so `MvpAgent::build_subagent_spawn_context` can copy it into the spawn context.
    /// Subagents then inherit the parent's callback rather than getting a fresh one, preserving the parent's session_id on the child's emits.
    pub attribution_callback: Option<distill_sampler::SharedAttributionCallback>,
    /// The agent definition name for this session.
    pub agent_name: String,
    pub managed_mcp_proxy_base_url: String,
    pub session_default_agent_profile: Option<String>,
    /// Subagent types this agent can spawn (from Agent(t1, t2) in tools).
    pub allowed_subagent_types: Option<Vec<String>>,
    /// Hook registry for this session (snapshot from spawn time).
    pub hook_registry: Option<std::sync::Arc<distill_hooks::discovery::HookRegistry>>,
    /// Typed workspace operations handle (agent sessions use local ops).
    pub workspace_ops: distill_workspace::WorkspaceOps,
    /// Subagents inherit the parent's backend so background tasks and monitors survive the subagent's exit.
    pub terminal_backend:
        Option<std::sync::Arc<dyn distill_tools::computer::types::TerminalBackend>>,
    /// Notification handle for this session's tool bridge.
    /// Subagents use this to reparent surviving tasks' notification handles on exit so events route to the parent's notification bridge.
    pub tools_notification_handle:
        Option<distill_tools::notification::types::ToolNotificationHandle>,
    /// Subagents inherit the parent's handle so scheduled tasks survive the subagent's exit.
    pub scheduler_handle:
        Option<distill_tools::implementations::distill::scheduler::types::SchedulerHandle>,
    pub registry_write_order: RegistryWriteOrder,
}
#[derive(Clone, Default)]
pub struct RegistryWriteOrder {
    inner: std::sync::Arc<RegistryWriteOrderInner>,
}
struct RegistryWriteOrderInner {
    tail: std::sync::Mutex<Option<RegistryChainLink>>,
    restorable_apply: tokio::sync::Mutex<()>,
    last_turn_floor: std::sync::atomic::AtomicI32,
    restorable_floor: std::sync::atomic::AtomicI32,
}
impl Default for RegistryWriteOrderInner {
    fn default() -> Self {
        Self {
            tail: std::sync::Mutex::new(None),
            restorable_apply: tokio::sync::Mutex::new(()),
            last_turn_floor: std::sync::atomic::AtomicI32::new(-1),
            restorable_floor: std::sync::atomic::AtomicI32::new(-1),
        }
    }
}
struct RegistryChainLink(oneshot::Receiver<Box<RegistryChainLink>>);
impl RegistryChainLink {
    async fn wait(&mut self) {
        while let Ok(next) = (&mut self.0).await {
            *self = *next;
        }
    }
}
impl Drop for RegistryChainLink {
    fn drop(&mut self) {
        let mut link = self.0.try_recv().ok();
        while let Some(mut boxed) = link {
            link = boxed.0.try_recv().ok();
        }
    }
}
#[must_use = "dropping a RegistryTurnClaim without driving it skips this turn's registry writes; the error path drops it deliberately to forward the chain"]
pub struct RegistryTurnClaim {
    prev_done: Option<RegistryChainLink>,
    done: Option<oneshot::Sender<Box<RegistryChainLink>>>,
}
impl RegistryTurnClaim {
    pub(crate) async fn wait_predecessor(&mut self) {
        if let Some(prev) = self.prev_done.as_mut() {
            prev.wait().await;
        }
        self.prev_done = None;
    }
}
impl Drop for RegistryTurnClaim {
    fn drop(&mut self) {
        if let (Some(prev), Some(done)) = (self.prev_done.take(), self.done.take()) {
            let _ = done.send(Box::new(prev));
        }
    }
}
impl RegistryWriteOrder {
    pub(crate) fn begin_turn_end(&self) -> RegistryTurnClaim {
        let (done, done_rx) = oneshot::channel();
        let mut tail = self
            .inner
            .tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RegistryTurnClaim {
            prev_done: tail.replace(RegistryChainLink(done_rx)),
            done: Some(done),
        }
    }
    pub(crate) async fn lock_restorable_apply(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.restorable_apply.lock().await
    }
    pub(crate) fn should_write_last_turn(&self, turn: i32) -> bool {
        turn > self
            .inner
            .last_turn_floor
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn commit_last_turn(&self, turn: i32) {
        self.inner
            .last_turn_floor
            .fetch_max(turn, std::sync::atomic::Ordering::AcqRel);
    }
    pub(crate) fn should_write_restorable(&self, turn: i32) -> bool {
        turn > self
            .inner
            .restorable_floor
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn commit_restorable(&self, turn: i32) {
        self.inner
            .restorable_floor
            .fetch_max(turn, std::sync::atomic::Ordering::AcqRel);
    }
}
impl SessionHandle {
    pub(crate) fn message_delivery(&self) -> super::message_delivery::MessageDeliveryHandle {
        super::message_delivery::MessageDeliveryHandle::new(
            self.cmd_tx.clone(),
            self.info.id.0.to_string(),
        )
    }
    /// Last assistant `model_id` / `model_fingerprint` in conversation (global, not turn-scoped).
    pub(crate) async fn get_model_metadata(&self) -> distill_chat_state::ModelMetadata {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::GetModelMetadata { responds_to: tx })
            .is_ok()
        {
            rx.await.unwrap_or_default()
        } else {
            distill_chat_state::ModelMetadata::default()
        }
    }
    /// Move a foreground bash command to background by tool_call_id.
    /// Returns `true` if a matching foreground process was found and unblocked.
    pub(crate) async fn background_foreground_command(&self, tool_call_id: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::BackgroundForegroundCommand {
                tool_call_id: tool_call_id.to_string(),
                respond_to: tx,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
    /// Routes through the session actor to the ToolBridge's TerminalBackend.
    pub(crate) async fn kill_background_task(
        &self,
        task_id: &str,
        source: distill_tools::types::KillSource,
    ) -> Result<distill_tools::types::KillOutcome, String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::KillBackgroundTask {
                task_id: task_id.to_string(),
                source,
                respond_to: tx,
            })
            .is_err()
        {
            return Err("session not found".to_string());
        }
        rx.await.unwrap_or(Err("session actor died".to_string()))
    }
    pub(crate) async fn persist_resume_status(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::PersistResumeStatus { respond_to: tx })
            .is_err()
        {
            return;
        }
        let _ = tokio::time::timeout(crate::session::resume_status::PERSIST_ACK_TIMEOUT, rx).await;
    }
    pub(crate) async fn delete_scheduled_task(&self, task_id: &str) -> Result<bool, String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::DeleteScheduledTask {
                task_id: task_id.to_string(),
                respond_to: tx,
            })
            .is_err()
        {
            return Err("session not found".to_string());
        }
        rx.await.unwrap_or(Err("session actor died".to_string()))
    }
    pub async fn is_busy(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::IsBusy { respond_to: tx })
            .is_err()
        {
            return true;
        }
        rx.await.unwrap_or(true)
    }
    /// List all background tasks.
    /// Routes through the session actor to the ToolBridge's TerminalBackend.
    pub async fn list_tasks(&self) -> Option<Vec<distill_tools::types::TaskSnapshot>> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ListTasks { respond_to: tx })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }
    /// Get hooks list for the pager modal.
    pub(crate) async fn get_hooks_list(
        &self,
    ) -> Option<distill_hooks_plugins_types::HooksListResponse> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::GetHooksList { respond_to: tx })
            .is_err()
        {
            return None;
        }
        rx.await.ok()
    }
    /// Execute a hooks management action from the pager modal.
    pub(crate) async fn execute_hooks_action(
        &self,
        action: distill_hooks_plugins_types::HooksAction,
    ) -> Option<distill_hooks_plugins_types::ActionOutcome> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::HooksAction {
                action,
                respond_to: tx,
            })
            .is_err()
        {
            return None;
        }
        rx.await.ok()
    }
    /// Execute a plugins management action from the pager modal.
    pub(crate) async fn execute_plugins_action(
        &self,
        action: distill_hooks_plugins_types::PluginsAction,
    ) -> Option<distill_hooks_plugins_types::ActionOutcome> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::PluginsAction {
                action,
                respond_to: tx,
            })
            .is_err()
        {
            return None;
        }
        rx.await.ok()
    }
    /// This session's plugin registry, including plugins loaded via `_meta.pluginDirs`.
    pub(crate) async fn plugins_list(
        &self,
    ) -> Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::PluginsList { respond_to: tx })
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }
    /// Snapshot the session's live MCP client pool for subagent inheritance.
    pub(crate) async fn snapshot_mcp_pool(
        &self,
    ) -> Option<crate::session::mcp_servers::SharedMcpPool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SnapshotMcpPool { respond_to: tx })
            .ok()?;
        rx.await.ok().flatten()
    }
    /// Snapshot the session's client-registered hooks for subagent inheritance.
    /// A dead actor or dropped reply fails open to no hooks, warned since it drops the inherited deny gate.
    pub(crate) async fn snapshot_client_hooks(&self) -> crate::extensions::hooks::ClientHooks {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::SnapshotClientHooks { respond_to: tx })
            .is_err()
        {
            tracing::warn!(
                "snapshot_client_hooks: session actor gone; subagent inherits no client hooks"
            );
            return Default::default();
        }
        rx.await.unwrap_or_else(|_| {
            tracing::warn!(
                "snapshot_client_hooks: reply dropped; subagent inherits no client hooks"
            );
            Default::default()
        })
    }
    /// Snapshot the session's resolved tool schema for verbatim-fork inheritance.
    /// A dead actor or dropped reply fails open to an empty list (child then builds its own toolset, same as a non-fork spawn).
    pub(crate) async fn snapshot_tool_definitions(&self) -> Vec<distill_sampling_types::ToolSpec> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::SnapshotToolDefinitions { respond_to: tx })
            .is_err()
        {
            tracing::warn!(
                "snapshot_tool_definitions: session actor gone; fork child inherits no parent tools"
            );
            return Vec::new();
        }
        rx.await.unwrap_or_else(|_| {
            tracing::warn!(
                "snapshot_tool_definitions: reply dropped; fork child inherits no parent tools"
            );
            Vec::new()
        })
    }
    pub(crate) async fn workflow_catalog_state(&self) -> (bool, bool) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::GetWorkflowCatalogState { respond_to: tx })
            .is_err()
        {
            return (false, false);
        }
        rx.await.unwrap_or((false, false))
    }
    pub(crate) async fn list_available_commands(
        &self,
    ) -> crate::session::slash_commands::ListCommandsResponse {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ListAvailableCommands { respond_to: tx })
            .is_err()
        {
            return crate::session::slash_commands::ListCommandsResponse::default();
        }
        rx.await
            .unwrap_or_else(|_| crate::session::slash_commands::ListCommandsResponse::default())
    }
    /// Record whether the client now on this session draws a status row.
    /// Assigned rather than raised and lowered from separate events.
    /// An attach that only raised the flag would leave the previous client's row enabled, and the session would keep building payloads nobody draws.
    pub(crate) fn set_status_line_wanted(&self, wanted: bool) {
        self.client_caps
            .status_line
            .store(wanted, std::sync::atomic::Ordering::Relaxed);
    }
    /// Assigned rather than raised: an attach that only raised would leave the previous client's echo enabled.
    pub(crate) fn set_user_message_echo_wanted(&self, wanted: bool) {
        self.client_caps
            .user_message_echo
            .store(wanted, std::sync::atomic::Ordering::Relaxed);
    }
    /// Ask for a fresh status-line snapshot.
    /// Used when a client attaches: the notification is transient, so there is nothing to replay.
    /// The emitter re-reads the capability when the wake lands, so [`Self::set_status_line_wanted`] has to be stored before this is sent.
    pub(crate) fn request_status_snapshot(&self) {
        let _ = self.cmd_tx.send(SessionCommand::EmitStatusSnapshot);
    }
    /// Replace the live session's client-registered hooks (see `SessionCommand::SetClientHooks`).
    pub(crate) fn set_client_hooks(&self, hooks: crate::extensions::hooks::ClientHooks) {
        let _ = self.cmd_tx.send(SessionCommand::SetClientHooks { hooks });
    }
    pub(crate) async fn get_mcp_status(&self) -> crate::extensions::mcp::McpStatusSnapshot {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::GetMcpStatus { respond_to: tx })
            .is_err()
        {
            return Default::default();
        }
        rx.await.unwrap_or_default()
    }
    pub(crate) async fn toggle_mcp_server(
        &self,
        server_name: String,
        enabled: bool,
        server_config: Option<agent_client_protocol::McpServer>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ToggleMcpServer {
                server_name,
                enabled,
                server_config,
                respond_to: tx,
            })
            .is_err()
        {
            return Err(agent_client_protocol::Error::internal_error().data("session closed"));
        }
        rx.await
            .map_err(|_| agent_client_protocol::Error::internal_error().data("session closed"))?
    }
    pub(crate) async fn toggle_mcp_tool(
        &self,
        server_name: String,
        tool_name: String,
        enabled: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        self.toggle_mcp_tool_with_source(server_name, tool_name, enabled, false)
            .await
    }
    pub(crate) async fn toggle_managed_gateway_tool(
        &self,
        server_name: String,
        tool_name: String,
        enabled: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        self.toggle_mcp_tool_with_source(server_name, tool_name, enabled, true)
            .await
    }
    async fn toggle_mcp_tool_with_source(
        &self,
        server_name: String,
        tool_name: String,
        enabled: bool,
        is_managed_gateway: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ToggleMcpTool {
                server_name,
                tool_name,
                enabled,
                is_managed_gateway,
                respond_to: tx,
            })
            .is_err()
        {
            return Err(agent_client_protocol::Error::internal_error().data("session closed"));
        }
        rx.await
            .map_err(|_| agent_client_protocol::Error::internal_error().data("session closed"))?
    }
    pub(crate) async fn managed_gateway_disabled_tool_names(
        &self,
    ) -> HashMap<String, HashSet<String>> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::GetManagedGatewayDisabledTools { respond_to: tx })
            .is_err()
        {
            return HashMap::new();
        }
        rx.await.unwrap_or_default()
    }
    pub(crate) async fn retry_auth_required_servers(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::RetryAuthRequiredServers { respond_to: tx })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }
    pub async fn call_mcp_tool(
        &self,
        server_name: String,
        server_url: Option<String>,
        tool_name: String,
        arguments: serde_json::Value,
    ) -> Result<crate::extensions::mcp::McpCallResponse, String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::CallMcpTool {
                server_name,
                server_url,
                tool_name,
                arguments,
                respond_to: tx,
            })
            .is_err()
        {
            return Err("session closed".to_string());
        }
        rx.await
            .unwrap_or_else(|_| Err("session closed".to_string()))
    }
    pub(crate) async fn read_mcp_resource(
        &self,
        server_name: String,
        uri: String,
    ) -> Result<crate::extensions::mcp::McpReadResourceResponse, String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ReadMcpResource {
                server_name,
                uri,
                respond_to: tx,
            })
            .is_err()
        {
            return Err("session closed".to_string());
        }
        rx.await
            .unwrap_or_else(|_| Err("session closed".to_string()))
    }
    pub(crate) async fn mcp_auth_status(&self) -> Vec<crate::extensions::mcp::McpAuthStatusEntry> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::McpAuthStatus { respond_to: tx })
            .is_err()
        {
            return vec![];
        }
        rx.await.unwrap_or_default()
    }
    pub(crate) async fn mcp_auth_trigger(&self, server_name: String) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::McpAuthTrigger {
                server_name,
                respond_to: tx,
            })
            .is_err()
        {
            return Err("session closed".to_string());
        }
        rx.await
            .unwrap_or_else(|_| Err("session closed".to_string()))
    }
    /// Emit a PluginUpdatesInstalled notification to the session.
    /// Fire-and-forget; no response expected.
    pub(crate) async fn notify_plugin_updates(&self, updates: Vec<(String, String, String)>) {
        let _ = self
            .cmd_tx
            .send(SessionCommand::NotifyPluginUpdates { updates });
    }
    /// Send a feedback entry to the persistence actor; logs on a closed channel.
    pub(crate) fn persist_feedback(&self, entry: LocalFeedbackEntry) {
        if self
            .persistence_tx
            .send(PersistenceMsg::Feedback(entry))
            .is_err()
        {
            tracing::warn!(
                session_id = %self.info.id.0,
                "feedback persistence channel closed; entry dropped",
            );
        }
    }
}
