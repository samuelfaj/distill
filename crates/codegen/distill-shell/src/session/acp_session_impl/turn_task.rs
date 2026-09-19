// Modified for Distill by Samuel Fajreldines, 2026.
//! Turn-task data, the spawn and completion paths, and guards for `SessionActor`.

use super::*;

pub(super) struct TurnSubagentScopeGuard {
    current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    prompt_id: String,
}

impl TurnSubagentScopeGuard {
    pub(super) fn new(
        current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        prompt_id: String,
    ) -> Self {
        Self {
            current_prompt_id,
            prompt_id,
        }
    }
}

impl Drop for TurnSubagentScopeGuard {
    fn drop(&mut self) {
        let mut current_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned");
        if current_prompt_id.as_deref() == Some(self.prompt_id.as_str()) {
            *current_prompt_id = None;
        }
    }
}

/// RAII guard that stores `false` into an `is_turn_active` flag on drop.
/// Guarantees the flag is cleared on all exit paths (early returns, errors, panics).
pub(super) struct TurnActiveGuard(Option<Arc<std::sync::atomic::AtomicBool>>);

impl TurnActiveGuard {
    pub(super) fn activate(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> Self {
        if let Some(f) = flag {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Self(flag.cloned())
    }
}

impl Drop for TurnActiveGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) struct TurnInputRequest {
    pub(crate) prompt_id: String,
    pub(crate) input_origin: InputOrigin,
    pub(crate) prompt_blocks: Vec<ContentBlock>,
    pub(crate) prompt_mode: PromptMode,
    pub(crate) trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    pub(crate) client_identifier: Option<String>,
    pub(crate) screen_mode: Option<String>,
    pub(crate) verbatim: bool,
    pub(crate) send_now: bool,
    pub(crate) json_schema: Option<serde_json::Value>,
    pub(crate) persist_ack: Option<oneshot::Sender<()>>,
    pub(crate) parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    pub(crate) traceparent: Option<String>,
    pub(crate) start_gate: Option<oneshot::Receiver<()>>,
}

/// Pointer-identity token minted per task spawn: `Rc::ptr_eq` tells spawns
/// apart even when a prompt id and epoch are reused.
pub(super) type TaskIdentity = std::rc::Rc<()>;

/// Completion-channel payload: prompt id, turn result, elapsed ms from `started_at`.
/// `elapsed_ms` is `None` when no running task supplied a start time.
pub(crate) struct TurnCompletionMsg {
    pub(crate) prompt_id: String,
    pub(crate) epoch: TurnEpoch,
    pub(super) task_identity: TaskIdentity,
    pub(crate) result: PromptTurnResult,
    pub(crate) elapsed_ms: Option<u64>,
    #[cfg(test)]
    pub(crate) processed: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug)]
pub(super) enum FinalizationBinding {
    Task {
        prompt_id: String,
        epoch: TurnEpoch,
        task_identity: TaskIdentity,
    },
    NoTask(Option<String>),
}

impl FinalizationBinding {
    pub(super) fn epoch(&self) -> Option<TurnEpoch> {
        match self {
            Self::Task { epoch, .. } => Some(*epoch),
            Self::NoTask(_) => None,
        }
    }

    fn matches_task(&self, task: &AgentTask) -> bool {
        matches!(self, Self::Task { prompt_id, epoch, task_identity }
            if task.prompt_id == *prompt_id && task.epoch == *epoch
                && TaskIdentity::ptr_eq(&task.identity, task_identity))
    }
}

#[derive(Default)]
pub(super) struct FinalizationGate {
    next_token: u64,
    active: Option<u64>,
}

impl FinalizationGate {
    fn claim(&mut self, binding: FinalizationBinding) -> Option<FinalizationLease> {
        if self.active.is_some() {
            return None;
        }
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.active = Some(token);
        Some(FinalizationLease {
            token,
            binding,
            is_finished: false,
        })
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

impl State {
    fn front_prompt_id(&self) -> Option<&str> {
        self.pending_inputs
            .front()
            .map(|item| item.prompt_id.as_str())
    }

    pub(super) fn claim_task_finalization(
        &mut self,
        prompt_id: &str,
        epoch: TurnEpoch,
        task_identity: &TaskIdentity,
    ) -> Option<FinalizationLease> {
        let binding = FinalizationBinding::Task {
            prompt_id: prompt_id.to_owned(),
            epoch,
            task_identity: task_identity.clone(),
        };
        if self.front_prompt_id() != Some(prompt_id)
            || !self
                .running_task
                .as_ref()
                .is_some_and(|task| binding.matches_task(task))
        {
            return None;
        }
        self.finalization_gate.claim(binding)
    }

    /// Cancel claims whatever is current: the running task (which must still be
    /// the front prompt), else the possibly absent front prompt with no task.
    pub(super) fn claim_cancel_finalization(&mut self) -> Option<FinalizationLease> {
        let binding = match self.running_task.as_ref() {
            Some(task) => {
                if self.front_prompt_id() != Some(task.prompt_id.as_str()) {
                    return None;
                }
                FinalizationBinding::Task {
                    prompt_id: task.prompt_id.clone(),
                    epoch: task.epoch,
                    task_identity: task.identity.clone(),
                }
            }
            None => FinalizationBinding::NoTask(self.front_prompt_id().map(str::to_owned)),
        };
        self.finalization_gate.claim(binding)
    }

    fn finalization_matches(&self, lease: &FinalizationLease) -> bool {
        self.finalization_gate.active == Some(lease.token)
    }

    pub(super) fn finalization_binding_is_current(&self, lease: &FinalizationLease) -> bool {
        self.finalization_matches(lease)
            && match &lease.binding {
                FinalizationBinding::Task { prompt_id, .. } => {
                    self.front_prompt_id() == Some(prompt_id.as_str())
                        && self
                            .running_task
                            .as_ref()
                            .is_some_and(|task| lease.binding.matches_task(task))
                }
                FinalizationBinding::NoTask(prompt_id) => {
                    self.running_task.is_none() && self.front_prompt_id() == prompt_id.as_deref()
                }
            }
    }

    pub(super) fn release_finalizing_task(&mut self, lease: &FinalizationLease) -> bool {
        if !self.finalization_matches(lease) {
            return false;
        }
        self.running_task
            .take_if(|task| lease.binding.matches_task(task))
            .is_some()
    }

    pub(super) fn finish_finalization(&mut self, lease: &mut FinalizationLease) -> bool {
        if !self.finalization_matches(lease) || self.running_task.is_some() {
            return false;
        }
        self.finalization_gate.active = None;
        lease.is_finished = true;
        true
    }
}

#[must_use = "a committed finalization lease must be finished"]
pub(super) struct FinalizationLease {
    token: u64,
    pub(super) binding: FinalizationBinding,
    is_finished: bool,
}

impl Drop for FinalizationLease {
    fn drop(&mut self) {
        if !self.is_finished {
            tracing::error!(
                token = self.token,
                binding = ?self.binding,
                "committed turn finalization dropped before exact release"
            );
        }
    }
}

pub(crate) struct AgentTask {
    pub(crate) prompt_id: String,
    pub(super) epoch: TurnEpoch,
    pub(super) identity: TaskIdentity,
    pub(crate) handle: tokio::task::AbortHandle,
    /// Monotonic start of this running turn; complete and cancel both read it.
    pub(crate) started_at: std::time::Instant,
}

/// Saturating `Instant` delta in ms. No panic on overflow.
pub(crate) fn elapsed_ms_saturating(start: std::time::Instant, now: std::time::Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod elapsed_ms_tests {
    use super::elapsed_ms_saturating;

    #[test]
    fn elapsed_ms_saturating_uses_injected_instants() {
        let start = std::time::Instant::now();
        let later = start + std::time::Duration::from_millis(42);
        assert_eq!(elapsed_ms_saturating(start, later), 42);
        assert_eq!(elapsed_ms_saturating(later, start), 0);
    }
}

impl SessionActor {
    /// Clear `current_prompt_id` (and the matching tool-bridge resource) only
    /// when it still names `prompt_id`. Leaves the goal-loop gate alone.
    pub(super) async fn clear_pinned_prompt_if_current(&self, prompt_id: &str) {
        let cleared = {
            let mut current = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            if current.as_deref() == Some(prompt_id) {
                *current = None;
                true
            } else {
                false
            }
        };
        if !cleared {
            return;
        }
        let prompt_id = prompt_id.to_owned();
        self.agent
            .borrow()
            .tool_bridge()
            .update_resources_with(move |resources| {
                use distill_tools::implementations::distill::task::types::CurrentPromptIdResource;
                if resources
                    .get::<CurrentPromptIdResource>()
                    .is_some_and(|current| current.0 == prompt_id)
                {
                    resources.insert(CurrentPromptIdResource(String::new()));
                }
            })
            .await;
    }

    pub(super) async fn clear_exact_turn_resources(&self, prompt_id: &str) {
        {
            let mut current = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            if current.as_deref() == Some(prompt_id) {
                *current = None;
            }
        }
        self.tool_context
            .goal_loop_active_gate
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let prompt_id = prompt_id.to_owned();
        self.agent
            .borrow()
            .tool_bridge()
            .update_resources_with(move |resources| {
                use distill_tools::implementations::distill::task::types::{
                    CurrentPromptIdResource, GoalLoopActive,
                };
                if resources
                    .get::<CurrentPromptIdResource>()
                    .is_some_and(|current| current.0 == prompt_id)
                {
                    resources.insert(CurrentPromptIdResource(String::new()));
                }
                resources.insert(GoalLoopActive(false));
            })
            .await;
    }

    pub(super) async fn finish_finalization_lease(&self, lease: &mut FinalizationLease) -> bool {
        if matches!(lease.binding, FinalizationBinding::Task { .. })
            && !self.state.lock().await.release_finalizing_task(lease)
        {
            return false;
        }
        if let FinalizationBinding::Task { prompt_id, .. } = &lease.binding {
            self.clear_exact_turn_resources(prompt_id).await;
        } else if let FinalizationBinding::NoTask(Some(prompt_id)) = &lease.binding {
            // Pin-only cancel: drop the live pin when it is this front. Do not
            // force the goal-loop gate off unless we actually owned that pin
            // (a never-started queued row must not disturb another turn).
            self.clear_pinned_prompt_if_current(prompt_id).await;
        }
        self.state.lock().await.finish_finalization(lease)
    }
}

impl AgentTask {
    #[cfg(test)]
    pub(crate) fn new(prompt_id: &str, handle: tokio::task::AbortHandle) -> Self {
        Self::new_at_epoch(prompt_id, TurnEpoch::default(), handle)
    }

    #[cfg(test)]
    pub(super) fn new_at_epoch(
        prompt_id: &str,
        epoch: TurnEpoch,
        handle: tokio::task::AbortHandle,
    ) -> Self {
        Self {
            prompt_id: prompt_id.to_string(),
            epoch,
            identity: TaskIdentity::new(()),
            handle,
            started_at: std::time::Instant::now(),
        }
    }

    pub(super) fn new_prompt(
        session: Arc<SessionActor>,
        request: TurnInputRequest,
        epoch: TurnEpoch,
        completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    ) -> Self {
        let started_at = std::time::Instant::now();
        // Open the turn here, not in the spawned future: a cancel that wins the finalization
        // lease before the future first runs must still snapshot this turn's number and this
        // request's mode (the future resolves the final mode once it runs).
        session.signals_handle().increment_turn();
        *session.turn_start_prompt_mode.lock() = request.prompt_mode;
        *session.turn_prompt_mode.lock() = request.prompt_mode;
        let identity = TaskIdentity::new(());
        Self {
            prompt_id: request.prompt_id.clone(),
            epoch,
            identity: identity.clone(),
            handle: distill_telemetry::session_ctx::spawn_local_in_session_ctx(run_task(
                session,
                request,
                epoch,
                identity,
                completion_tx,
                started_at,
            ))
            .abort_handle(),
            started_at,
        }
    }

    pub(super) fn abort(&self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

/// Holds at most one spawned task; arming a new one aborts the previous.
/// `Cell` interior mutability because `SessionActor` is `!Send` (single-threaded LocalSet).
/// Backs both the deferred user-message prefix (`take`s the handle to await its result) and the idle-notification debounce (`cancel`s it).
pub(crate) struct TaskSlot<T> {
    handle: std::cell::Cell<Option<tokio::task::JoinHandle<T>>>,
}

impl<T> TaskSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            handle: std::cell::Cell::new(None),
        }
    }

    /// Abort any pending task and store the new one.
    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<T>) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
        self.handle.set(Some(handle));
    }

    /// Take the pending task handle (e.g. to await its result).
    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<T>> {
        self.handle.take()
    }

    /// Abort and drop any pending task (e.g. because a new turn started).
    pub(crate) fn cancel(&self) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
    }
}

/// The background prefix build plus the wait it was started with, so the consumer sizes its budget from what the task is actually doing.
pub(crate) struct DeferredPrefix {
    task: TaskSlot<String>,
    full_mcp_wait: std::cell::Cell<bool>,
}

impl DeferredPrefix {
    pub(crate) fn new() -> Self {
        Self {
            task: TaskSlot::new(),
            full_mcp_wait: std::cell::Cell::new(false),
        }
    }

    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<String>, full_mcp_wait: bool) {
        self.full_mcp_wait.set(full_mcp_wait);
        self.task.arm(handle);
    }

    /// The pending build and whether it holds the full MCP wait.
    pub(crate) fn take(&self) -> Option<(tokio::task::JoinHandle<String>, bool)> {
        let handle = self.task.take()?;
        Some((handle, self.full_mcp_wait.get()))
    }

    pub(crate) fn cancel(&self) {
        self.task.cancel();
    }
}

async fn run_task(
    session: Arc<SessionActor>,
    request: TurnInputRequest,
    epoch: TurnEpoch,
    task_identity: TaskIdentity,
    completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    started_at: std::time::Instant,
) {
    let mut request = request;
    let prompt_id = request.prompt_id.clone();
    let result = if let Some(start_gate) = request.start_gate.take()
        && start_gate.await.is_err()
    {
        Ok(PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens: 0,
            turn_snapshot: None,
            completion_kind: PromptCompletionKind::RemovedFromQueue,
            structured_output: None,
            usage: None,
            tool_overrides: None,
        })
    } else {
        session.handle_turn_input(request).await
    };
    let elapsed_ms = elapsed_ms_saturating(started_at, std::time::Instant::now());
    let _ = completion_tx.send(TurnCompletionMsg {
        prompt_id,
        epoch,
        task_identity,
        result,
        elapsed_ms: Some(elapsed_ms),
        #[cfg(test)]
        processed: None,
    });
}

#[cfg(test)]
mod start_gate_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn closed_start_gate_leaves_prior_transcript_byte_identical() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let transcript = tempfile::NamedTempFile::new().expect("transcript");
                std::fs::write(transcript.path(), b"prior transcript\n").expect("seed transcript");
                let prior = std::fs::read(transcript.path()).expect("read prior transcript");
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _) = super::super::support::create_test_actor_with_chat_persistence(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx.clone(),
                    Box::new(
                        crate::session::chat_persistence::ChannelChatPersistence::new(
                            persistence_tx,
                        ),
                    ),
                )
                .await;
                let transcript_path = transcript.path().to_path_buf();
                let persistence_drain = tokio::task::spawn_local(async move {
                    while let Some(message) = persistence_rx.recv().await {
                        if let PersistenceMsg::Chat(item) = message {
                            use std::io::Write as _;
                            let mut file = std::fs::OpenOptions::new()
                                .append(true)
                                .open(&transcript_path)
                                .expect("open transcript");
                            serde_json::to_writer(&mut file, &item).expect("serialize chat item");
                            file.write_all(b"\n").expect("append chat item");
                        }
                    }
                });
                let (start_release, start_gate) = oneshot::channel();
                drop(start_release);
                let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
                run_task(
                    Arc::new(actor),
                    TurnInputRequest {
                        prompt_id: "rejected-wake".to_owned(),
                        input_origin: InputOrigin::new(PromptOrigin::User),
                        prompt_blocks: vec![ContentBlock::Text(acp::TextContent::new(
                            "must not persist",
                        ))],
                        prompt_mode: PromptMode::Agent,
                        trace_gcs_config: None,
                        artifact_tracker: None,
                        client_identifier: None,
                        screen_mode: None,
                        verbatim: true,
                        send_now: false,
                        json_schema: None,
                        persist_ack: None,
                        parsed_prompt_tx: None,
                        traceparent: None,
                        start_gate: Some(start_gate),
                    },
                    TurnEpoch::default(),
                    TaskIdentity::new(()),
                    completion_tx,
                    std::time::Instant::now(),
                )
                .await;
                let completion = completion_rx.recv().await.expect("turn completion");
                assert!(matches!(
                    completion.result,
                    Ok(PromptTurnOk {
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        ..
                    })
                ));
                tokio::task::yield_now().await;
                assert_eq!(
                    std::fs::read(transcript.path()).expect("read transcript after rejection"),
                    prior
                );
                persistence_drain.abort();
            })
            .await;
    }
}

#[cfg(test)]
mod task_slot_tests {
    // Exercises the shared `TaskSlot<T>` primitive that backs both the deferred prefix and the idle-notification debounce: arm, take, cancel, re-arm
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawn a 60s task that bumps `fired` by `by`, armed into `slot`.
    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let f = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            f.fetch_add(by, Ordering::SeqCst);
        }));
    }

    /// A task left armed runs to completion once its delay elapses.
    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    /// `cancel()` aborts a still-pending task (the new-user-prompt reset path).
    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    /// Arming a new task aborts the previous one so only the latest fires.
    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
