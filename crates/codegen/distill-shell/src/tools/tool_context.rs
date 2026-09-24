// Modified for Distill by Samuel Fajreldines, 2026.
//! Session context, kept under its legacy name "ToolContext".
//!
//! The session actor needs it for non-tool operations (ACP communication, git, rewind, etc.).
//! Tool execution goes through the ToolBridge, which has its own SessionContext from distill-tools.
use crate::terminal::AsyncTerminalRunner;
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_hunk_tracker::HunkTrackerHandle;
use distill_paths::AbsPathBuf;
use distill_tty_utils::ProcessScope;
use distill_workspace::file_system::{AsyncFileSystem, AsyncFsWrapper};
use distill_workspace::session::file_state::FileStateHandle;
use std::collections::HashMap;
use std::sync::Arc;
#[derive(Debug, Clone, Default)]
pub struct TaskOutputTokenBudget {
    inner: Arc<parking_lot::Mutex<TaskOutputTokenBudgetState>>,
}
#[derive(Debug, Default)]
struct TaskOutputTokenBudgetState {
    total: Option<u64>,
    spent: u64,
    incomplete: bool,
}
impl TaskOutputTokenBudget {
    pub fn limited(total: u64) -> Self {
        debug_assert!(total > 0, "task output grant must be positive");
        Self {
            inner: Arc::new(parking_lot::Mutex::new(TaskOutputTokenBudgetState {
                total: Some(total),
                spent: 0,
                incomplete: false,
            })),
        }
    }
    pub fn remaining(&self) -> Option<u64> {
        let state = self.inner.lock();
        state.total.map(|total| total.saturating_sub(state.spent))
    }
    pub(crate) fn clamp_request(&self, configured: Option<u32>) -> Option<u32> {
        let remaining = self.remaining()?;
        if remaining == 0 {
            return Some(0);
        }
        let remaining = u32::try_from(remaining).unwrap_or(u32::MAX);
        Some(configured.map_or(remaining, |configured| configured.min(remaining)))
    }
    pub(crate) fn record_reported_output(&self, output_tokens: u64) {
        let mut state = self.inner.lock();
        state.spent = state.spent.saturating_add(output_tokens);
        if let Some(total) = state.total
            && state.spent > total
        {
            state.spent = total;
            state.incomplete = true;
        }
    }
    pub(crate) fn mark_incomplete_and_exhaust(&self) {
        let mut state = self.inner.lock();
        state.incomplete = true;
        if let Some(total) = state.total {
            state.spent = state.spent.max(total);
        }
    }
    pub fn usage(&self) -> (u64, bool) {
        let state = self.inner.lock();
        (state.spent, state.incomplete)
    }
}
pub struct BlockingWaitState(std::sync::Mutex<BlockingWaitInner>);
#[derive(Default)]
struct BlockingWaitInner {
    depth: usize,
    generation: u64,
    /// Union of waits aborted by a mid-turn interjection.
    /// Concurrent aborts merge so a side-work wait cannot replace the implement set.
    /// Extras drop only when the next interruptible wait contains every remembered id plus at least one new one.
    interrupted_wait_ids: Option<Vec<String>>,
}
impl BlockingWaitState {
    pub(crate) fn new() -> Self {
        Self(std::sync::Mutex::new(BlockingWaitInner::default()))
    }
    pub(crate) fn depth(&self) -> usize {
        self.0
            .lock()
            .expect("blocking wait state mutex poisoned")
            .depth
    }
    #[cfg(test)]
    pub(crate) fn set_depth_for_test(&self, depth: usize) {
        self.0
            .lock()
            .expect("blocking wait state mutex poisoned")
            .depth = depth;
    }
    pub(crate) fn generation(&self) -> u64 {
        self.0
            .lock()
            .expect("blocking wait state mutex poisoned")
            .generation
    }
    /// Hold the wait-memory lock for one read-and-maybe-write so a parallel abort cannot land between a check and a clear.
    /// No-op if `generation` does not match (cancel already reset).
    pub(crate) fn update_interrupted_wait<R>(
        &self,
        generation: u64,
        f: impl FnOnce(&mut Option<Vec<String>>) -> R,
    ) -> Option<R> {
        let mut inner = self.0.lock().expect("blocking wait state mutex poisoned");
        if inner.generation != generation {
            return None;
        }
        Some(f(&mut inner.interrupted_wait_ids))
    }
    #[cfg(test)]
    pub(crate) fn note_interrupted_wait(&self, ids: Vec<String>) {
        self.update_interrupted_wait(self.generation(), |remembered| {
            if ids.is_empty() {
                return;
            }
            match remembered {
                Some(existing) => {
                    for id in ids {
                        if !existing.contains(&id) {
                            existing.push(id);
                        }
                    }
                }
                None => *remembered = Some(ids),
            }
        });
    }
    #[cfg(test)]
    pub(crate) fn interrupted_wait_ids(&self) -> Option<Vec<String>> {
        self.0
            .lock()
            .expect("blocking wait state mutex poisoned")
            .interrupted_wait_ids
            .clone()
    }
    pub(crate) fn reset(&self) {
        let mut state = self.0.lock().expect("blocking wait state mutex poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.depth = 0;
        state.interrupted_wait_ids = None;
    }
}
pub(crate) struct BlockingWaitGuard {
    state: Arc<BlockingWaitState>,
    generation: u64,
}
impl BlockingWaitGuard {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn enter(state: Arc<BlockingWaitState>) -> Self {
        let generation = {
            let mut inner = state.0.lock().expect("blocking wait state mutex poisoned");
            inner.depth = inner.depth.saturating_add(1);
            inner.generation
        };
        Self { state, generation }
    }
}
impl Drop for BlockingWaitGuard {
    fn drop(&mut self) {
        let mut inner = self
            .state
            .0
            .lock()
            .expect("blocking wait state mutex poisoned");
        if inner.generation == self.generation {
            inner.depth = inner.depth.saturating_sub(1);
        }
    }
}
pub(crate) fn subagent_foreground_wait(
    state: Arc<BlockingWaitState>,
) -> distill_tools::implementations::distill::task::types::SubagentForegroundWait {
    distill_tools::implementations::distill::task::types::SubagentForegroundWait::new(move || {
        Box::new(BlockingWaitGuard::enter(Arc::clone(&state)))
    })
}
/// Session-level context. NOT used for tool execution (bridge handles that).
#[derive(Clone)]
pub struct ToolContext {
    pub gateway: Option<GatewaySender>,
    pub session_id: Option<acp::SessionId>,
    pub fs: AsyncFsWrapper,
    pub terminal: Arc<dyn AsyncTerminalRunner>,
    pub cwd: AbsPathBuf,
    pub file_state_handle: Option<FileStateHandle>,
    pub session_env: Arc<HashMap<String, String>>,
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// `false` when the resolved mode is `off`/`disabled`.
    /// `hunk_tracker_handle` is then a `noop()` and the fs-notify loop skips forwarding to avoid per-event cost.
    pub hunk_tracking_enabled: bool,
    pub prompt_index: Arc<tokio::sync::Mutex<usize>>,
    /// Lock-free mirror of `prompt_index`, read synchronously to attribute an active message to its parent prompt.
    pub(crate) active_message_parent_prompt_index: Arc<std::sync::atomic::AtomicUsize>,
    /// Top-level sessions start at 0; child sessions are parent_depth + 1.
    pub subagent_depth: u32,
    /// Carries spawn, query, cancel, list-active, completions, and outstanding messages to the coordinator.
    /// `None` if subagent support is not enabled.
    pub subagent_event_tx: Option<
        tokio::sync::mpsc::UnboundedSender<
            distill_tools::implementations::distill::task::types::SubagentEvent,
        >,
    >,
    pub subagent_coordinator_sender:
        Option<distill_tools::implementations::distill::task::backend::SubagentCoordinatorSender>,
    /// Shared LSP runtime, cloned cheaply (Arc) from parent to child.
    pub lsp: Option<Arc<dyn distill_tools::implementations::lsp::LspBackend>>,
    /// LSP server names captured at session creation (not updated mid-session).
    pub lsp_server_names: Vec<String>,
    /// Shared turn-active flag: set `true` at turn start, `false` at turn end.
    /// Used by the between-turn completion drain in `handle_prompt`.
    pub is_turn_active: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) unattributed_background_usage: Arc<std::sync::atomic::AtomicBool>,
    /// The session turn loop (`inject_pending_monitor_events`) drains events pushed here.
    /// They are injected as ONE hidden synthetic user message before the next sampling step.
    pub monitor_event_buffer:
        Option<distill_tools::implementations::distill::monitor::types::MonitorEventBuffer>,
    pub task_completion_reservations:
        Option<distill_tools::reminders::task_completion::TaskCompletionReservations>,
    pub task_wake_suppressed: Option<distill_tools::reminders::task_completion::TaskWakeSuppressed>,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    pub(crate) synthetic_trace_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    /// Populated by `start_subagent_coordinator` after the notification bridge is spawned.
    /// The notification bridge reads from this slot on each completion event.
    pub(crate) synthetic_trace_tx_shared: Option<
        std::sync::Arc<
            std::sync::Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedSender<
                        crate::upload::turn::SyntheticTurnTraceRequest,
                    >,
                >,
            >,
        >,
    >,
    /// Resolved name of the `BackgroundTaskAction` tool in the current toolset.
    /// Used by auto-wake to format completion messages with the correct tool name.
    pub task_output_tool_name: String,
    /// Resolved name of the scheduled-task deletion tool, when available.
    pub scheduler_delete_tool_name: Option<String>,
    pub scheduler_create_tool_name: Option<String>,
    /// When `false`, background task and subagent completions fall back to the idle-gated notification drain.
    pub auto_wake_enabled: bool,
    /// When set, bash and subagent auto-wake synthetic prompts are suppressed.
    /// Shared `Arc` written at one chokepoint; see `SessionActor::set_goal_loop_active_resource` for the rationale.
    pub goal_loop_active_gate: Arc<std::sync::atomic::AtomicBool>,
    /// Count of interruptible blocking waits the running turn is parked in (via [`BlockingWaitGuard`]).
    /// `queue_input` reads it: a prompt arriving while non-zero takes the send-now path.
    pub blocking_wait_depth: Arc<BlockingWaitState>,
    pub task_output_token_budget: Option<TaskOutputTokenBudget>,
    pub(crate) sampler_retry_only_before_output: bool,
    /// This session's child-process reaper, set at session spawn; `None` for contexts without one (subagents, defaults).
    /// Spawn sites enroll children into it; enrolled children are killed when the session closes.
    pub process_scope: Option<ProcessScope>,
    /// Same Arc as `SessionRegistry`'s retained heal lock so the actor tick and tray `list_running` cannot double-emit `SubagentFinished`.
    pub(crate) live_orphan_heal_lock: Arc<tokio::sync::Mutex<()>>,
}
impl ToolContext {
    pub(crate) fn clamp_task_model_request(
        &self,
        requested: Option<u32>,
        configured: Option<u32>,
    ) -> Result<Option<u32>, &'static str> {
        match self.task_output_token_budget.as_ref() {
            Some(budget) => match budget.clamp_request(requested.or(configured)) {
                Some(0) => Err("workflow child output-token budget exhausted"),
                clamped => Ok(clamped),
            },
            None => Ok(requested),
        }
    }
    pub(crate) fn record_task_model_output(&self, output_tokens: u64) {
        if let Some(budget) = self.task_output_token_budget.as_ref() {
            budget.record_reported_output(output_tokens);
        }
    }
    pub(crate) fn fail_task_output_usage_closed(&self) {
        if let Some(budget) = self.task_output_token_budget.as_ref() {
            budget.mark_incomplete_and_exhaust();
        }
    }
    /// Test-only: empty session env.
    /// Production goes through [`Self::with_preloaded_env`].
    #[cfg(test)]
    pub(crate) fn new(
        cwd: AbsPathBuf,
        gateway: Option<GatewaySender>,
        session_id: Option<acp::SessionId>,
        fs: Arc<dyn AsyncFileSystem>,
        terminal: Arc<dyn AsyncTerminalRunner>,
        hunk_tracker_handle: HunkTrackerHandle,
    ) -> Self {
        Self::with_preloaded_env(
            cwd,
            gateway,
            session_id,
            fs,
            terminal,
            hunk_tracker_handle,
            HashMap::new(),
        )
    }
    pub(crate) fn with_preloaded_env(
        cwd: AbsPathBuf,
        gateway: Option<GatewaySender>,
        session_id: Option<acp::SessionId>,
        fs: Arc<dyn AsyncFileSystem>,
        terminal: Arc<dyn AsyncTerminalRunner>,
        hunk_tracker_handle: HunkTrackerHandle,
        session_env: HashMap<String, String>,
    ) -> Self {
        Self {
            gateway,
            session_id,
            fs: AsyncFsWrapper::new(fs),
            terminal,
            cwd,
            file_state_handle: None,
            session_env: Arc::new(session_env),
            hunk_tracker_handle,
            hunk_tracking_enabled: true,
            prompt_index: Arc::new(tokio::sync::Mutex::new(0)),
            active_message_parent_prompt_index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            subagent_depth: 0,
            subagent_event_tx: None,
            subagent_coordinator_sender: None,
            lsp: None,
            lsp_server_names: Vec::new(),
            is_turn_active: None,
            unattributed_background_usage: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            monitor_event_buffer: None,
            task_completion_reservations: None,
            task_wake_suppressed: None,
            synthetic_trace_tx: None,
            synthetic_trace_tx_shared: None,
            task_output_tool_name:
                distill_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string(),
            scheduler_delete_tool_name: None,
            scheduler_create_tool_name: None,
            auto_wake_enabled: true,
            goal_loop_active_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            blocking_wait_depth: Arc::new(BlockingWaitState::new()),
            task_output_token_budget: None,
            sampler_retry_only_before_output: false,
            process_scope: None,
            live_orphan_heal_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub(crate) fn with_file_state_handle(mut self, handle: FileStateHandle) -> Self {
        self.file_state_handle = Some(handle);
        self
    }
    /// `false` pairs with a `noop()` `hunk_tracker_handle` so the fs-notify loop skips the per-event forward.
    pub(crate) fn with_hunk_tracking_enabled(mut self, enabled: bool) -> Self {
        self.hunk_tracking_enabled = enabled;
        self
    }
}
#[cfg(test)]
mod output_budget_tests {
    use super::TaskOutputTokenBudget;
    #[test]
    fn clamps_every_request_to_remaining_and_stops_at_zero() {
        let budget = TaskOutputTokenBudget::limited(10);
        assert_eq!(budget.clamp_request(None), Some(10));
        assert_eq!(budget.clamp_request(Some(7)), Some(7));
        budget.record_reported_output(6);
        assert_eq!(budget.clamp_request(None), Some(4));
        assert_eq!(budget.clamp_request(Some(9)), Some(4));
        budget.record_reported_output(4);
        assert_eq!(budget.clamp_request(None), Some(0));
    }
    #[test]
    fn provider_output_not_context_drives_spend() {
        let budget = TaskOutputTokenBudget::limited(100);
        let provider_prompt_tokens = 90_000u64;
        budget.record_reported_output(25);
        assert_eq!(budget.usage(), (25, false));
        assert_eq!(provider_prompt_tokens, 90_000);
        assert_eq!(budget.remaining(), Some(75));
    }
    #[test]
    fn unknown_usage_exhausts_grant_pessimistically() {
        let budget = TaskOutputTokenBudget::limited(50);
        budget.record_reported_output(7);
        budget.mark_incomplete_and_exhaust();
        assert_eq!(budget.usage(), (50, true));
        assert_eq!(budget.clamp_request(None), Some(0));
    }
}
#[cfg(test)]
mod tests {
    use super::BlockingWaitState;
    use crate::{terminal::AsyncTerminalRunner, tools::ToolContext};
    use distill_hunk_tracker::HunkTrackerHandle;
    use distill_paths::AbsPathBuf;
    use distill_workspace::file_system::{AsyncFileSystem, AsyncFsWrapper};
    use std::collections::HashMap;
    use std::sync::Arc;
    impl ToolContext {
        pub(crate) fn new_local_context(
            cwd: AbsPathBuf,
            fs: Arc<dyn AsyncFileSystem>,
            terminal: Arc<dyn AsyncTerminalRunner>,
        ) -> Self {
            Self {
                gateway: None,
                session_id: None,
                fs: AsyncFsWrapper::new(fs),
                terminal,
                cwd,
                file_state_handle: None,
                session_env: Arc::new(HashMap::new()),
                hunk_tracker_handle: HunkTrackerHandle::noop(),
                hunk_tracking_enabled: true,
                prompt_index: Arc::new(tokio::sync::Mutex::new(0)),
                active_message_parent_prompt_index: Arc::new(std::sync::atomic::AtomicUsize::new(
                    0,
                )),
                subagent_depth: 0,
                subagent_event_tx: None,
                subagent_coordinator_sender: None,
                lsp: None,
                lsp_server_names: Vec::new(),
                is_turn_active: None,
                unattributed_background_usage: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_event_buffer: None,
                task_completion_reservations: None,
                task_wake_suppressed: None,
                synthetic_trace_tx: None,
                synthetic_trace_tx_shared: None,
                task_output_tool_name:
                    distill_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string(),
                scheduler_delete_tool_name: None,
                scheduler_create_tool_name: None,
                auto_wake_enabled: true,
                goal_loop_active_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                blocking_wait_depth: Arc::new(BlockingWaitState::new()),
                task_output_token_budget: None,
                sampler_retry_only_before_output: false,
                process_scope: None,
                live_orphan_heal_lock: Arc::new(tokio::sync::Mutex::new(())),
            }
        }
    }
}
