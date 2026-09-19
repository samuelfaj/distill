// Modified for Distill by Samuel Fajreldines, 2026.
//! Single-writer subagent coordinator actor.
//!
//! The actor owns the command receiver, the admission queue, pending/active/
//! completed state, concrete blocking waiters, foreground deadlines,
//! cancellation, and the terminal delivery disposition. All hosts drive it
//! through `ChannelBackend`; only their `ChildRunner` implementations differ.
//!
//! There is intentionally no shared mutable state in this module. A runner's
//! associated futures may be `Send` or non-`Send`; the resulting actor future
//! inherits that property naturally on stable Rust.

pub(crate) mod active_message;
mod agent_quotas;
mod agent_targets;
mod completion;
mod graph;
mod query;
mod queue;
mod root_targets;
mod spawn;
mod wake;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot};

use super::active_message::ActiveMessageIngress;
use super::admission::Admission;
use super::coordinator_state::PendingDisposition;
use super::coordinator_state::{
    ActiveChild, BlockingWaiter, BufferedCompletion, ChildRecord, CompletedChild,
    DisplacedCompletedChild, InternalEvent, ListRequest, PendingChild, ProgressFuture,
    ProgressTarget, ReplyFuture, TaggedFuture, active_summary, background_at_deadline,
    background_if_caller_gone, completed_snapshot, sleep_until, workflow_outstanding,
};
use super::types::{
    ActiveAgentMessageOutcome, AgentAddress, SpawnedSubagentRef, SubagentCancelOutcome,
    SubagentCancelTarget, SubagentDescribeOutcome, SubagentEvent, SubagentOutstandingReply,
    SubagentRegistryCounts, SubagentRequest, SubagentResult, SubagentResumeLookup,
    SubagentResumeSource, SubagentValidateTypeOutcome,
};
pub use active_message::ActiveChildGeneration;
use active_message::{ActiveMessageFuture, ActiveMessageLifecycle, SpawnReadyMessages};
use agent_quotas::AgentMessageQuotas;
use graph::SpawnGraph;
use root_targets::{RootTargetKey, RootTargetLifecycle};

pub use super::coordinator_state::{
    ACTIVE_MESSAGE_ADMISSION_TIMEOUT, ACTIVE_MESSAGE_FINALIZATION_TIMEOUT,
    ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT, ActiveMessageAdmission, ChildCompletion, ChildControl,
    ChildReporter, ChildRunOutput, ChildRunRequest, ChildRunner, CompletionDisposition,
    CoordinatorConfig, LimitedSpawnOrigin, LocalBoxFuture, MAX_ACTIVE_MESSAGE_ADMISSIONS,
    MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD, MAX_COMPLETED_ENTRIES, SendBoxFuture, StartedChild,
    SubagentLimitDecision, SubagentLimitNotice, SubagentLimitSink, SubagentProgress, WakeOrigin,
};
use queue::{QUEUED_REAP_INTERVAL, QueuedCaller, SpawnQueue, StartOrigin};
use wake::PendingWake;

/// Channel-owned subagent lifecycle actor.
pub struct SubagentCoordinator<R: ChildRunner> {
    commands: mpsc::UnboundedReceiver<SubagentEvent>,
    active_message_ingress: Option<mpsc::UnboundedReceiver<ActiveMessageIngress>>,
    agent_message_sender_factory: Option<super::agent_message_sender::AgentMessageSenderFactory>,
    internal_tx: mpsc::UnboundedSender<InternalEvent<R::Control>>,
    internal_rx: mpsc::UnboundedReceiver<InternalEvent<R::Control>>,
    terminal_published_tx: mpsc::UnboundedSender<String>,
    terminal_published_rx: mpsc::UnboundedReceiver<String>,
    runner: R,
    config: CoordinatorConfig,
    admission: Admission,
    queued: SpawnQueue,
    /// When the queued sweep last ran; bounds how stale an out-of-band
    /// token cancel of a queued spawn can get (see [`Self::next_deadline`]).
    last_queued_reap: tokio::time::Instant,
    /// Re-entry latch: `finish_child` runs the queued sweep, and finishing a
    /// cancelled queued entry routes back through `finish_child`. The inner
    /// sweep is a no-op (a cancelled entry frees no running slot).
    draining_queued: bool,
    pending: HashMap<String, PendingChild>,
    active: HashMap<String, ActiveChild<R::Control>>,
    completed: HashMap<String, CompletedChild>,
    completed_order: VecDeque<String>,
    agent_message_quotas: AgentMessageQuotas,
    root_active_messages: HashMap<RootTargetKey, RootTargetLifecycle<R::RootControl>>,
    pending_wakes: HashMap<String, Vec<PendingWake>>,
    next_completion_age: u64,
    graph: SpawnGraph,
    waiters: HashMap<String, Vec<BlockingWaiter>>,
    drain_waiters: HashMap<PromptScope, Vec<oneshot::Sender<SubagentOutstandingReply>>>,
    workflow_cancel_waiters: HashMap<String, Vec<oneshot::Sender<SubagentCancelOutcome>>>,
    /// Per-parent delete-path teardown drain, present only while a responder-bearing `TeardownSession` (`/delete`) waits for the session's children
    /// to finish. While an entry exists, spawn admission stays closed and [`SubagentEvent::OpenSpawnAdmission`] cannot reopen it (a racing
    /// next-turn open would reopen Task spawns mid-delete).
    teardown_drains: HashMap<String, TeardownDrain>,
    /// Parent sessions that received `ParentSession` cancel. Non-workflow spawns are rejected until
    /// [`SubagentEvent::OpenSpawnAdmission`] (next turn) or teardown drain completes, so a detached
    /// late `TaskTool` spawn cannot outrun Stop / delete.
    spawn_blocked_sessions: HashSet<String>,
    usage_not_applied_prompts: HashSet<PromptScope>,
    pending_completions: Vec<BufferedCompletion>,
    runs: FuturesUnordered<
        TaggedFuture<futures::future::CatchUnwind<std::panic::AssertUnwindSafe<R::RunFuture>>>,
    >,
    validations: FuturesUnordered<ReplyFuture<R::ValidateFuture, SubagentValidateTypeOutcome>>,
    descriptions: FuturesUnordered<ReplyFuture<R::DescribeFuture, SubagentDescribeOutcome>>,
    active_messages: FuturesUnordered<ActiveMessageFuture>,
    /// Parked sends and the admission semaphore they re-acquire on start.
    spawn_ready: SpawnReadyMessages,
    terminal_outputs: HashMap<String, ChildRunOutput<R::CompletionData>>,
    progress: FuturesUnordered<ProgressFuture<<R::Control as ChildControl>::ProgressFuture>>,
    list_requests: HashMap<u64, ListRequest>,
    next_list_request_id: u64,
}

/// Backstop for a delete-path teardown hold: if a cancelled child never
/// finishes, force-reopen the session's spawn admission after this long (with a
/// warning) rather than blocking spawns for the process lifetime.
const TEARDOWN_DRAIN_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// In-flight delete-path teardown drain: responders to resolve once the last
/// child drains, and the backstop deadline that force-reopens admission.
struct TeardownDrain {
    waiters: Vec<oneshot::Sender<()>>,
    deadline: tokio::time::Instant,
}

/// Receiver paired with one coordinator-capable sender.
pub struct SubagentCoordinatorReceiver {
    commands: mpsc::UnboundedReceiver<SubagentEvent>,
    pub(crate) active_messages: mpsc::UnboundedReceiver<ActiveMessageIngress>,
    active_message_permits: Arc<tokio::sync::Semaphore>,
    active_message_capacity: usize,
    agent_message_sender_factory: super::agent_message_sender::AgentMessageSenderFactory,
}

impl SubagentCoordinatorReceiver {
    /// Discard coordinator-only ingress and retain the ordinary event receiver.
    #[doc(hidden)]
    pub fn into_event_receiver(self) -> mpsc::UnboundedReceiver<SubagentEvent> {
        self.commands
    }

    fn new() -> (
        crate::implementations::distill::task::backend::SubagentCoordinatorSender,
        Self,
    ) {
        Self::paired_with_capacity(MAX_ACTIVE_MESSAGE_ADMISSIONS)
    }

    #[cfg(test)]
    pub(crate) fn active_message_available_permits(&self) -> usize {
        self.active_message_permits.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(
        capacity: usize,
    ) -> (
        crate::implementations::distill::task::backend::SubagentCoordinatorSender,
        Self,
    ) {
        Self::paired_with_capacity(capacity)
    }

    fn paired_with_capacity(
        capacity: usize,
    ) -> (
        crate::implementations::distill::task::backend::SubagentCoordinatorSender,
        Self,
    ) {
        let (tx, commands) = mpsc::unbounded_channel();
        let (active_message_tx, active_messages) = mpsc::unbounded_channel();
        let active_message_capacity = capacity.max(1);
        let active_message_permits = Arc::new(tokio::sync::Semaphore::new(active_message_capacity));
        let agent_message_sender_factory =
            super::agent_message_sender::AgentMessageSenderFactory::new(
                active_message_tx.downgrade(),
                Arc::clone(&active_message_permits),
                active_message_capacity,
            );
        (
            crate::implementations::distill::task::backend::SubagentCoordinatorSender::from_paired_channels(
                tx,
                active_message_tx,
                Arc::clone(&active_message_permits),
                active_message_capacity,
            ),
            Self {
                commands,
                active_messages,
                active_message_permits,
                active_message_capacity,
                agent_message_sender_factory,
            },
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptScope {
    parent_session_id: String,
    prompt_id: String,
}

impl PromptScope {
    fn new(parent_session_id: String, prompt_id: String) -> Self {
        Self {
            parent_session_id,
            prompt_id,
        }
    }
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub fn channel() -> (
        crate::implementations::distill::task::backend::SubagentCoordinatorSender,
        SubagentCoordinatorReceiver,
    ) {
        SubagentCoordinatorReceiver::new()
    }

    /// Build a legacy coordinator that rejects public active-message events.
    pub fn new(
        commands: mpsc::UnboundedReceiver<SubagentEvent>,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        Self::from_receivers(commands, None, None, 0, None, runner, config)
    }

    /// Build a coordinator from the receiver paired by [`Self::channel`].
    pub fn from_channel(
        receiver: SubagentCoordinatorReceiver,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        Self::from_receivers(
            receiver.commands,
            Some(receiver.active_messages),
            Some(receiver.active_message_permits),
            receiver.active_message_capacity,
            Some(receiver.agent_message_sender_factory),
            runner,
            config,
        )
    }

    fn from_receivers(
        commands: mpsc::UnboundedReceiver<SubagentEvent>,
        active_message_ingress: Option<mpsc::UnboundedReceiver<ActiveMessageIngress>>,
        active_message_permits: Option<Arc<tokio::sync::Semaphore>>,
        active_message_capacity: usize,
        agent_message_sender_factory: Option<
            super::agent_message_sender::AgentMessageSenderFactory,
        >,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (terminal_published_tx, terminal_published_rx) = mpsc::unbounded_channel();
        Self {
            commands,
            active_message_ingress,
            agent_message_sender_factory,
            internal_tx,
            internal_rx,
            terminal_published_tx,
            terminal_published_rx,
            runner,
            admission: Admission::new(config.limits),
            queued: SpawnQueue::default(),
            last_queued_reap: tokio::time::Instant::now(),
            draining_queued: false,
            config,
            pending: HashMap::new(),
            active: HashMap::new(),
            completed: HashMap::new(),
            completed_order: VecDeque::new(),
            agent_message_quotas: AgentMessageQuotas::default(),
            root_active_messages: HashMap::new(),
            pending_wakes: HashMap::new(),
            next_completion_age: 0,
            graph: SpawnGraph::default(),
            waiters: HashMap::new(),
            drain_waiters: HashMap::new(),
            workflow_cancel_waiters: HashMap::new(),
            teardown_drains: HashMap::new(),
            spawn_blocked_sessions: HashSet::new(),
            usage_not_applied_prompts: HashSet::new(),
            pending_completions: Vec::new(),
            runs: FuturesUnordered::new(),
            validations: FuturesUnordered::new(),
            descriptions: FuturesUnordered::new(),
            active_messages: FuturesUnordered::new(),
            spawn_ready: SpawnReadyMessages::new(active_message_permits, active_message_capacity),
            terminal_outputs: HashMap::new(),
            progress: FuturesUnordered::new(),
            list_requests: HashMap::new(),
            next_list_request_id: 0,
        }
    }

    pub async fn run(mut self) {
        let mut commands_open = true;
        let mut active_message_ingress_open = self.active_message_ingress.is_some();
        loop {
            // Queued spawns need no exit check: queued non-empty means some
            // session is at capacity, so `runs` is non-empty, and the last
            // `finish_child` drains the queue before `runs` empties.
            if !commands_open
                && !active_message_ingress_open
                && self.runs.is_empty()
                && self.validations.is_empty()
                && self.descriptions.is_empty()
                && self.active_messages.is_empty()
                && self.spawn_ready.is_empty()
                && self.terminal_outputs.is_empty()
                && self.progress.is_empty()
            {
                debug_assert!(
                    self.queued.is_empty(),
                    "actor exiting with spawns still queued"
                );
                break;
            }

            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                Some(event) = self.internal_rx.recv() => self.handle_internal(event),
                Some(subagent_id) = self.terminal_published_rx.recv() => {
                    self.handle_terminal_published(subagent_id);
                },
                Some(completion) = self.active_messages.next(), if !self.active_messages.is_empty() => {
                    self.finish_active_message(completion);
                }
                Some((id, output)) = self.runs.next(), if !self.runs.is_empty() => {
                    match output {
                        Ok(output) => self.begin_terminalization(&id, output),
                        Err(_) => self.begin_panicked_terminalization(&id),
                    }
                }
                ingress = async {
                    #[expect(
                        clippy::expect_used,
                        reason = "active_message_ingress_open is initialized from Option::is_some and the receiver is never taken"
                    )]
                    self.active_message_ingress
                        .as_mut()
                        .expect(
                            "active_message_ingress_open is initialized from Option::is_some and the receiver is never taken",
                        )
                        .recv()
                        .await
                }, if active_message_ingress_open => {
                    match ingress {
                        Some(ingress) => self.handle_send_active_message(ingress),
                        None => active_message_ingress_open = false,
                    }
                }
                Some((respond_to, outcome)) = self.validations.next(), if !self.validations.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((respond_to, outcome)) = self.descriptions.next(), if !self.descriptions.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((seed, target, progress)) = self.progress.next(), if !self.progress.is_empty() => {
                    self.finish_progress(seed, target, progress);
                }
                command = self.commands.recv(), if commands_open => {
                    match command {
                        Some(command) => {
                            self.reap_abandoned_callers();
                            self.handle_command(command);
                        }
                        None => commands_open = false,
                    }
                }
                // A foreground caller dropping its spawn-reply receiver must wake the loop here: the reap that clears it from the
                // turn-blocking set, and any parked drain waiting on it, would otherwise stall until the next command or the far-later
                // foreground deadline.
                _ = std::future::poll_fn(|cx| {
                    poll_caller_abandoned(&mut self.pending, &mut self.active, cx)
                }) => self.reap_abandoned_callers(),
                _ = sleep_until(deadline), if deadline.is_some() => self.process_deadlines(),
            }
            self.resolve_drain_waiters();
            self.evict_completed_overflow();
        }

        self.cancel_all_children();
        self.active_messages.clear();
    }

    fn handle_command(&mut self, command: SubagentEvent) {
        match command {
            SubagentEvent::Spawn(command) => self.handle_spawn(command),
            SubagentEvent::Query(query) => {
                self.handle_query(
                    query.subagent_id,
                    query.parent_session_id,
                    query.block,
                    query.timeout_ms,
                    query.respond_to,
                );
            }
            SubagentEvent::Cancel(request) => match request.target {
                SubagentCancelTarget::SubagentId(id) => {
                    let outcome = self.cancel_one(&id, request.parent_session_id.as_deref(), true);
                    let _ = request.respond_to.send(outcome);
                }
                SubagentCancelTarget::ParentPromptId(prompt_id) => {
                    self.cancel_parent_prompt(&prompt_id, request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                }
                SubagentCancelTarget::ParentSession => {
                    let outcome = self.cancel_parent_session(request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(outcome);
                }
                SubagentCancelTarget::WorkflowRunId(run_id) => {
                    self.cancel_workflow_children(&run_id, request.parent_session_id.as_deref());
                    if workflow_outstanding(&self.pending, &self.active, &run_id) == 0 {
                        let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                    } else {
                        self.workflow_cancel_waiters
                            .entry(run_id)
                            .or_default()
                            .push(request.respond_to);
                    }
                }
            },
            SubagentEvent::ListActive(request) => {
                let summaries = self
                    .active
                    .values()
                    .filter(|child| {
                        self.graph
                            .is_reachable_from(&child.request.id, &request.parent_session_id)
                            && !child.request.owner.is_workflow()
                    })
                    .map(active_summary)
                    .collect();
                let _ = request.respond_to.send(summaries);
            }
            SubagentEvent::ListRunning(request) => {
                self.handle_list_running(request.parent_session_id, request.respond_to);
            }
            SubagentEvent::Completions(request) => {
                // Suppressed owned completions stay buffered for the requester's next unsuppressed request
                let (drained, kept): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.pending_completions)
                        .into_iter()
                        .partition(|completion| {
                            completion.is_owned_by(request.parent_session_id.as_deref())
                                && !request
                                    .suppress_ids
                                    .iter()
                                    .any(|id| id == completion.summary.subagent_id())
                        });
                self.pending_completions = kept;
                let completions = drained
                    .into_iter()
                    .map(|completion| completion.summary)
                    .collect();
                let _ = request.respond_to.send(completions);
            }
            SubagentEvent::PeekCompletions(request) => {
                let completions = self
                    .pending_completions
                    .iter()
                    .filter(|completion| {
                        completion.is_owned_by(request.parent_session_id.as_deref())
                    })
                    .map(|completion| completion.summary.clone())
                    .collect();
                let _ = request.respond_to.send(completions);
            }
            SubagentEvent::TeardownSession {
                parent_session_id,
                respond_to,
            } => {
                self.pending_completions
                    .retain(|completion| completion.parent_session_id != parent_session_id);
                self.teardown_session_children(&parent_session_id);
                // Only the delete path (responder present) holds spawn admission closed until children drain, so a next-turn
                // OpenSpawnAdmission cannot reopen Task spawns mid-delete. Close / idle unload (no responder) keep the pre-existing
                // behavior: cancel children and leave admission untouched.
                if let Some(respond_to) = respond_to {
                    if self.session_has_children(&parent_session_id) {
                        self.begin_teardown_drain(parent_session_id, respond_to);
                    } else {
                        let _ = respond_to.send(());
                    }
                }
            }
            SubagentEvent::OpenSpawnAdmission { parent_session_id } => {
                // Next-turn reopen after Stop is intentional even while cancelled
                // children finish — but not while a delete-path TeardownSession
                // is draining.
                if !self.teardown_drains.contains_key(&parent_session_id) {
                    self.spawn_blocked_sessions.remove(&parent_session_id);
                }
            }
            SubagentEvent::Outstanding(request) => {
                let reply = self.outstanding_reply(&request.parent_session_id, &request.prompt_id);
                let _ = request.respond_to.send(reply);
            }
            SubagentEvent::WaitPromptDrained(request) => {
                let reply = self.outstanding_reply(&request.parent_session_id, &request.prompt_id);
                if reply.live_ids.is_empty() {
                    let _ = request.respond_to.send(reply);
                } else {
                    self.drain_waiters
                        .entry(PromptScope::new(
                            request.parent_session_id,
                            request.prompt_id,
                        ))
                        .or_default()
                        .push(request.respond_to);
                }
            }
            SubagentEvent::ClearUsageNotApplied(request) => {
                self.usage_not_applied_prompts.remove(&PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
            }
            SubagentEvent::MarkUsageNotApplied(request) => {
                self.usage_not_applied_prompts.insert(PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
                let _ = request.respond_to.send(());
            }
            SubagentEvent::RegistryCounts(request) => {
                let _ = request.respond_to.send(SubagentRegistryCounts {
                    pending: self.pending.len(),
                    active: self.active.len(),
                    completed: self.completed.len(),
                    queued: self.queued.len(),
                });
            }
            SubagentEvent::Inspect(request) => {
                self.handle_inspect(
                    request.subagent_id,
                    request.parent_session_id,
                    request.respond_to,
                );
            }
            SubagentEvent::SpawnedRefs(request) => {
                let mut refs: Vec<_> = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                    })
                    .map(|child| SpawnedSubagentRef {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        description: child.request.description.clone(),
                        persona: child.persona.clone(),
                        resumed_from: child.resumed_from.clone(),
                    })
                    .chain(
                        self.completed
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                            })
                            .map(|child| SpawnedSubagentRef {
                                subagent_id: child.request.id.clone(),
                                child_session_id: child.child_session_id.clone(),
                                subagent_type: child.request.subagent_type.clone(),
                                description: child.request.description.clone(),
                                persona: child.persona.clone(),
                                resumed_from: child.resumed_from.clone(),
                            }),
                    )
                    .collect();
                refs.sort_by(|a, b| a.subagent_id.cmp(&b.subagent_id));
                let _ = request.respond_to.send(refs);
            }
            SubagentEvent::ValidateType(request) => {
                self.validations.push(ReplyFuture {
                    future: Box::pin(
                        self.runner
                            .validate_type(request.subagent_type, request.parent_session_id),
                    ),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::DescribeType(request) => {
                self.descriptions.push(ReplyFuture {
                    future: Box::pin(self.runner.describe_type(
                        request.subagent_type,
                        request.harness_agent_type,
                        request.parent_session_id,
                    )),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::LoopUnitActive(request) => {
                let is_active = self.pending.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                }) || self.active.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                }) || self.queued.iter().any(|queued| {
                    queued.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                });
                let _ = request.respond_to.send(is_active);
            }
        }
    }

    fn evict_completed_overflow(&mut self) {
        while self.completed.len() > MAX_COMPLETED_ENTRIES {
            let Some(id) = self.completed_order.pop_front() else {
                break;
            };
            self.completed.remove(&id);
            self.reject_pending_wakes_for_id(&id);
            self.graph.remove(&id);
            self.clear_target_incarnation(&id);
        }
    }

    fn handle_terminal_published(&mut self, subagent_id: String) {
        let is_blocked = self.completed.get(&subagent_id).is_some_and(|completed| {
            let parent_session_id = &completed.request.parent_session_id;
            self.spawn_blocked_sessions.contains(parent_session_id)
                || self.teardown_drains.contains_key(parent_session_id)
        });
        if is_blocked {
            self.reject_pending_wakes_for_id(&subagent_id);
            return;
        }
        for pending in self.pending_wakes.remove(&subagent_id).unwrap_or_default() {
            let (decision, _) = self.resolve_active_message_target(
                &pending.ingress.request,
                Some(&pending.incarnation),
            );
            self.handle_resolved_send(
                decision,
                pending.route,
                pending.quota_admission,
                pending.ingress,
            );
        }
    }

    fn reject_pending_wakes_for_id(&mut self, subagent_id: &str) -> usize {
        let pending = self.pending_wakes.remove(subagent_id).unwrap_or_default();
        let count = pending.len();
        for pending in pending {
            let _ = pending
                .ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        count
    }

    fn reject_pending_wakes_for_session(&mut self, parent_session_id: &str) -> usize {
        let doomed: Vec<String> = self
            .pending_wakes
            .keys()
            .filter(|subagent_id| {
                self.completed.get(*subagent_id).is_some_and(|completed| {
                    completed.request.parent_session_id == parent_session_id
                })
            })
            .cloned()
            .collect();
        doomed
            .iter()
            .map(|subagent_id| self.reject_pending_wakes_for_id(subagent_id))
            .sum()
    }

    fn handle_internal(&mut self, event: InternalEvent<R::Control>) {
        match event {
            InternalEvent::Started {
                subagent_id,
                child,
                defer_admission,
                respond_to,
            } => {
                let StartedChild {
                    child_session_id,
                    persona,
                    resumed_from,
                    child_cwd,
                    worktree_path,
                    effective_model_id,
                    definition_background,
                    control,
                } = child;
                let Some(pending) = self.pending.remove(&subagent_id) else {
                    let _ = respond_to.send(false);
                    return;
                };
                if pending.cancellation.is_cancelled() {
                    self.pending.insert(subagent_id.clone(), pending);
                    let _ = respond_to.send(false);
                    // Never becomes active: human parked sends are terminal.
                    self.reject_spawn_ready_ids(&[subagent_id]);
                    return;
                }
                let promoted_id = subagent_id.clone();
                #[cfg(not(test))]
                debug_assert_eq!(child_session_id, pending.request.id);
                if child_session_id != pending.request.id {
                    tracing::warn!(
                        subagent_id = %pending.request.id,
                        child_session_id,
                        "child session identity diverged; disabling agent messaging",
                    );
                }
                let is_wake_promotion = pending.wake_of.is_some();
                let deferred_wake = defer_admission.then_some(pending.wake_of).flatten();
                self.active.insert(
                    subagent_id,
                    ActiveChild {
                        request: pending.request,
                        started_at: pending.started_at,
                        cancellation: pending.cancellation,
                        spawn_reply: pending.spawn_reply,
                        foreground_deadline: pending.foreground_deadline,
                        handle_only: pending.handle_only,
                        definition_background,
                        explicitly_killed: pending.explicitly_killed,
                        disposition: pending.disposition,
                        child_session_id,
                        persona,
                        resumed_from,
                        child_cwd,
                        worktree_path,
                        effective_model_id,
                        attempt_id: pending.attempt_id,
                        generation: pending.generation,
                        agent_address: pending.agent_address,
                        spawner_session_id: pending.spawner_session_id,
                        active_messages: ActiveMessageLifecycle::default(),
                        deferred_wake,
                        control,
                    },
                );
                if is_wake_promotion {
                    if !defer_admission {
                        self.activate_wake_incarnation(&promoted_id);
                    }
                } else {
                    let _ = self.current_target_incarnation(&promoted_id);
                }
                let _ = respond_to.send(true);
                if !defer_admission {
                    self.admit_spawn_ready_messages(&promoted_id);
                }
            }
            InternalEvent::SettleDeferredStart {
                subagent_id,
                is_committed,
                respond_to,
            } => {
                let Some(mut child) = self.active.remove(&subagent_id) else {
                    let _ = respond_to.send(false);
                    return;
                };
                let Some(displaced) = child.deferred_wake.take() else {
                    self.active.insert(subagent_id, child);
                    let _ = respond_to.send(false);
                    return;
                };
                if is_committed {
                    self.activate_wake_incarnation(&subagent_id);
                    self.active.insert(subagent_id.clone(), child);
                    self.admit_spawn_ready_messages(&subagent_id);
                } else {
                    child.request.surface_completion = false;
                    child.cancellation.cancel();
                    child.control.cancel();
                    child.deferred_wake = Some(displaced);
                    let _ = child.active_messages.start_terminalizing();
                    self.active.insert(subagent_id.clone(), child);
                    self.reject_spawn_ready_ids(std::slice::from_ref(&subagent_id));
                }
                let _ = respond_to.send(true);
            }
            InternalEvent::Finalizing {
                subagent_id,
                respond_to,
            } => self.handle_active_message_finalizing(subagent_id, respond_to),
            InternalEvent::DropSpawnerClaim { subagent_id } => {
                self.graph.drop_advertise(&subagent_id);
            }
            InternalEvent::ResumeSource {
                source_id,
                parent_session_id,
                respond_to,
            } => {
                let source_is_active = self.pending
                        .get(&source_id)
                        .is_some_and(|child| child.request.parent_session_id == parent_session_id)
                        || self.active.get(&source_id).is_some_and(|child| {
                            child.request.parent_session_id == parent_session_id
                        })
                        // Queued spawns resolve as "still running", matching
                        // the query path's Initializing, not as missing.
                        || self.queued.iter().any(|queued| {
                            queued.request.id == source_id
                                && queued.request.parent_session_id == parent_session_id
                        });
                let lookup = if source_is_active {
                    SubagentResumeLookup::Active
                } else if let Some(child) = self.completed.get(&source_id)
                    && child.request.parent_session_id == parent_session_id
                {
                    SubagentResumeLookup::Completed(Box::new(SubagentResumeSource {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        child_cwd: child.child_cwd.clone(),
                        worktree_path: child.worktree_path.clone(),
                        snapshot_ref: child.snapshot_ref.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        persona: child.persona.clone(),
                        model_id: Some(child.effective_model_id.clone()),
                    }))
                } else {
                    SubagentResumeLookup::Missing
                };
                let _ = respond_to.send(lookup);
            }
        }
    }

    fn start_child(
        &mut self,
        request: SubagentRequest,
        spawn_reply: Option<oneshot::Sender<SubagentResult>>,
        registered_tx: Option<oneshot::Sender<()>>,
        origin: StartOrigin,
        agent_address: Option<AgentAddress>,
        spawner_session_id: Option<String>,
        wake_origin: Option<WakeOrigin>,
        wake_of: Option<DisplacedCompletedChild>,
    ) {
        let id = request.id.clone();
        let cancellation = request.cancel_token.clone();
        // `spawn_reply: None`: the caller was auto-backgrounded while queued.
        let handle_only = request.run_in_background || spawn_reply.is_none();
        let agent_address = agent_address.or_else(|| {
            distill_message_delivery_core::mint_child_address(
                request.owner.is_workflow(),
                uuid::Uuid::new_v4().as_u128(),
            )
        });
        let (queued_for, foreground_deadline) = match origin {
            StartOrigin::Direct => (
                None,
                (spawn_reply.is_some() && request.awaits_in_foreground())
                    .then(|| tokio::time::Instant::now() + self.config.foreground_budget),
            ),
            StartOrigin::Dequeued {
                queued_for,
                deadline,
            } => (Some(queued_for), deadline),
        };
        // Wake passes the completed record's spawner; a fresh spawn takes the
        // graph advertise target (main's lineage source of truth).
        let spawner_session_id =
            spawner_session_id.or_else(|| self.graph.advertise_target(&id).map(str::to_owned));
        let identity = super::agent_message_sender::AgentMessageSender::mint_for_child(
            self.agent_message_sender_factory.as_ref(),
            &id,
            !request.owner.is_workflow() && self.runner.supports_agent_message_sender(),
        );
        let attempt_id = identity.attempt_id;
        let generation = identity.generation;
        let agent_message_sender = identity.sender;
        self.pending.insert(
            id.clone(),
            PendingChild {
                request: request.clone(),
                started_at: std::time::Instant::now(),
                cancellation: cancellation.clone(),
                spawn_reply,
                foreground_deadline,
                handle_only,
                explicitly_killed: false,
                disposition: PendingDisposition::Live,
                launched: true,
                attempt_id: attempt_id.clone(),
                generation,
                agent_address: agent_address.clone(),
                spawner_session_id: spawner_session_id.clone(),
                wake_of,
            },
        );
        self.running_count_changed();
        if let Some(tx) = registered_tx {
            let _ = tx.send(());
        }
        // Computed after the pending insert, so a non-workflow spawn counts
        // itself; max over launches gives a session's peak concurrency.
        let session_running = self.session_running_count(&request.parent_session_id);
        let reporter = ChildReporter {
            subagent_id: id.clone(),
            tx: self.internal_tx.clone(),
        };
        self.runs.push(TaggedFuture {
            subagent_id: id,
            future: Box::pin(
                std::panic::AssertUnwindSafe(self.runner.run(ChildRunRequest {
                    request,
                    cancellation,
                    reporter,
                    attempt_id,
                    generation,
                    agent_message_sender,
                    wake_origin,
                    queued_for,
                    session_running,
                    agent_address,
                    spawner_session_id,
                }))
                .catch_unwind(),
            ),
        });
    }

    /// Workflow agents draw from their run's own pool, not session slots.
    fn session_running_count(&self, parent_session_id: &str) -> usize {
        self.pending
            .values()
            .map(|child| &child.request)
            .chain(self.active.values().map(|child| &child.request))
            .filter(|request| {
                request.parent_session_id == parent_session_id && !request.owner.is_workflow()
            })
            .count()
    }

    fn live_turn_blocking_ids<'a>(
        &'a self,
        parent_session_id: &'a str,
        prompt_id: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.pending
            .values()
            .filter(move |child| {
                request_in_scope(&child.request, parent_session_id, prompt_id) && !child.handle_only
            })
            .map(|child| child.request.id.as_str())
            .chain(
                self.active
                    .values()
                    .filter(move |child| {
                        request_in_scope(&child.request, parent_session_id, prompt_id)
                            && !child.handle_only
                            && !child.definition_background
                    })
                    .map(|child| child.request.id.as_str()),
            )
            .chain(
                self.queued
                    .iter()
                    .filter(move |queued| {
                        request_in_scope(&queued.request, parent_session_id, prompt_id)
                            && !queued.caller.is_backgrounded()
                            && !queued.request.run_in_background
                    })
                    .map(|queued| queued.request.id.as_str()),
            )
    }

    fn outstanding_reply(
        &self,
        parent_session_id: &str,
        prompt_id: &str,
    ) -> SubagentOutstandingReply {
        let mut live_ids: Vec<String> = self
            .live_turn_blocking_ids(parent_session_id, prompt_id)
            .map(str::to_owned)
            .collect();
        live_ids.sort();
        let scoped =
            |request: &SubagentRequest| request_in_scope(request, parent_session_id, prompt_id);
        let background_live = self
            .pending
            .values()
            .any(|child| scoped(&child.request) && child.handle_only)
            || self.active.values().any(|child| {
                scoped(&child.request) && (child.handle_only || child.definition_background)
            })
            || self.queued.iter().any(|queued| {
                scoped(&queued.request)
                    && (queued.request.run_in_background || queued.caller.is_backgrounded())
            });
        let scope = PromptScope::new(parent_session_id.to_owned(), prompt_id.to_owned());
        SubagentOutstandingReply {
            live_ids,
            background_live,
            subagent_usage_not_applied: self.usage_not_applied_prompts.contains(&scope),
        }
    }

    fn scope_has_live_turn_blocking(&self, parent_session_id: &str, prompt_id: &str) -> bool {
        self.live_turn_blocking_ids(parent_session_id, prompt_id)
            .next()
            .is_some()
    }

    /// Parking (the `WaitPromptDrained` arm) and this wake share the one actor
    /// loop, so a check-then-park never misses a wake and needs no lock.
    fn resolve_drain_waiters(&mut self) {
        if self.drain_waiters.is_empty() {
            return;
        }
        let mut parked = std::mem::take(&mut self.drain_waiters);
        parked.retain(|scope, waiters| {
            if self.scope_has_live_turn_blocking(&scope.parent_session_id, &scope.prompt_id) {
                waiters.retain(|w| !w.is_closed());
                return !waiters.is_empty();
            }
            let reply = self.outstanding_reply(&scope.parent_session_id, &scope.prompt_id);
            for respond_to in waiters.drain(..) {
                let _ = respond_to.send(reply.clone());
            }
            false
        });
        self.drain_waiters = parked;
    }

    fn begin_terminalization(&mut self, id: &str, output: ChildRunOutput<R::CompletionData>) {
        let Some(child) = self.active.get_mut(id) else {
            self.finish_child(id, output);
            return;
        };
        match child.active_messages.start_terminalizing() {
            Some(is_clean) => self.finish_terminalized_child(id, output, is_clean),
            None => {
                // Stays in `self.active` while buffered, so a parked drain keeps
                // counting it live until the last admission settles.
                self.terminal_outputs.insert(id.to_owned(), output);
            }
        }
    }

    fn begin_panicked_terminalization(&mut self, id: &str) {
        let request = self
            .active
            .get(id)
            .map(|child| child.request.clone())
            .or_else(|| self.pending.get(id).map(|child| child.request.clone()));
        let Some(request) = request else {
            return;
        };
        tracing::error!(subagent_id = id, "subagent child runner panicked");
        self.begin_terminalization(
            id,
            ChildRunOutput {
                result: SubagentResult::failed(
                    request.id.clone(),
                    request.id,
                    "Subagent runtime panicked",
                ),
                completion_data: R::CompletionData::default(),
                snapshot_ref: None,
            },
        );
    }

    fn finish_terminalized_child(
        &mut self,
        id: &str,
        mut output: ChildRunOutput<R::CompletionData>,
        is_clean: bool,
    ) {
        if !is_clean {
            output.result.success = false;
            output.result.cancelled = true;
            output.result.error.get_or_insert_with(|| {
                "Active-message admission could not be proven settled".to_owned()
            });
            if let Some(child) = self.active.get_mut(id) {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        self.finish_child(id, output);
    }

    fn finish_child(&mut self, id: &str, output: ChildRunOutput<R::CompletionData>) {
        // Child is leaving the registry: human parked sends must not retry.
        self.reject_spawn_ready_ids(&[id.to_owned()]);
        let mut record = if let Some(child) = self.active.remove(id) {
            ChildRecord::Active(child)
        } else if let Some(child) = self.pending.remove(id) {
            ChildRecord::Pending(child)
        } else {
            return;
        };

        let explicitly_killed = record.explicitly_killed();
        let (was_cancelled, disposition) = match &record {
            ChildRecord::Pending(child) => (child.cancellation.is_cancelled(), child.disposition),
            ChildRecord::Active(child) => (child.cancellation.is_cancelled(), child.disposition),
        };
        if let Some(mut displaced) = record.take_failed_pre_start_wake(output.result.success) {
            tracing::warn!(
                subagent_id = %id,
                error = ?output.result.error,
                "subagent wake failed before start; keeping prior record",
            );
            // Host rejections also cancel the token, so only a recorded user or owner cancel counts.
            displaced.completed.wake_eligible &=
                !explicitly_killed && disposition != PendingDisposition::Cancelled;
            let parent_session_id = displaced.completed.request.parent_session_id.clone();
            self.restore_displaced_completion(displaced);
            self.running_count_changed();
            self.resolve_teardown_drain_waiters(&parent_session_id);
            self.start_queued_within_capacity();
            return;
        }

        let request = record.request().clone();
        let launched = match &record {
            ChildRecord::Active(_) => true,
            ChildRecord::Pending(child) => child.launched,
        };
        // Single owner for child-result failures after start. The TaskTool
        // detached waiter only logs join/transport errors.
        if launched
            && request.run_in_background
            && !output.result.success
            && !output.result.cancelled
        {
            tracing::error!(
                subagent_id = %id,
                subagent_type = %request.subagent_type,
                error = ?output.result.error,
                "background subagent failed after start",
            );
        }
        self.clear_message_sender_attempt(id, &record);
        self.complete_target_incarnation(id);
        let (
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            effective_model_id,
            agent_address,
            spawner_session_id,
            mut spawn_reply,
            mut handle_only,
        ) = match record {
            ChildRecord::Pending(child) => (
                child.started_at,
                output.result.child_session_id.clone(),
                child.request.runtime_overrides.persona.clone(),
                child.request.resume_from.clone(),
                child.request.cwd.clone().unwrap_or_default(),
                output.result.worktree_path.clone(),
                String::new(),
                child.agent_address,
                child.spawner_session_id,
                child.spawn_reply,
                child.handle_only,
            ),
            ChildRecord::Active(child) => (
                child.started_at,
                child.child_session_id,
                child.persona,
                child.resumed_from,
                child.child_cwd,
                child.worktree_path,
                child.effective_model_id,
                child.agent_address,
                child.spawner_session_id,
                child.spawn_reply,
                child.handle_only,
            ),
        };

        let persisted_output_ref = self.runner.persisted_output_ref(&output.completion_data);
        let completion_age = self.next_completion_age;
        self.next_completion_age = self
            .next_completion_age
            .checked_add(1)
            .expect("subagent completion age exhausted");
        let terminal_published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut completed = CompletedChild {
            completion_age,
            terminal_published: Arc::clone(&terminal_published),
            request: request.clone(),
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            snapshot_ref: output.snapshot_ref,
            persisted_output_ref,
            effective_model_id,
            agent_address,
            spawner_session_id,
            wake_eligible: !explicitly_killed && !was_cancelled && !output.result.cancelled,
            result: output.result.clone(),
        };
        let snapshot = completed_snapshot(&completed, None);

        let mut waiter_delivered = false;
        for waiter in self.waiters.remove(id).unwrap_or_default() {
            waiter_delivered |= waiter.respond_to.send(Some(snapshot.clone())).is_ok();
        }

        let mut foreground_delivered = false;
        if let Some(respond_to) = spawn_reply.take() {
            let sent = respond_to.send(output.result.clone()).is_ok();
            if !handle_only {
                foreground_delivered = sent;
                handle_only = !sent;
            }
        } else if !handle_only {
            handle_only = true;
        }

        self.buffer_completion(&request, &output.result, &snapshot);
        if completed.persisted_output_ref.is_some() {
            completed.result.output = Arc::from("");
        }

        // Root-scoped: the root is never woken for a grandchild.
        let should_surface = request.surface_completion
            && handle_only
            && !output.result.cancelled
            && !waiter_delivered
            && !explicitly_killed;
        let disposition = CompletionDisposition {
            foreground_delivered,
            backgrounded: handle_only,
            waiter_delivered,
            explicitly_killed,
            should_surface,
        };
        let finished_session = completed.child_session_id.clone();
        self.completed.insert(id.to_owned(), completed);
        self.completed_order.push_back(id.to_owned());
        self.purge_completions_for_spawner(&finished_session);
        self.running_count_changed();
        let workflow_run_id = request.owner.workflow_run_id().map(str::to_owned);
        let parent_session_id = request.parent_session_id.clone();
        let published_tx = self.terminal_published_tx.clone();
        let published_id = id.to_owned();
        self.runner.on_completed(
            ChildCompletion {
                request,
                result: output.result,
                snapshot,
                completion_data: output.completion_data,
                disposition,
            },
            Box::new(move || {
                terminal_published.store(true, std::sync::atomic::Ordering::Release);
                let _ = published_tx.send(published_id);
            }),
        );
        if let Some(run_id) = workflow_run_id {
            self.resolve_workflow_cancel_waiters(&run_id);
        }
        self.resolve_teardown_drain_waiters(&parent_session_id);
        self.start_queued_within_capacity();
    }

    fn cancel_one(
        &mut self,
        id: &str,
        parent_session_id: Option<&str>,
        explicit: bool,
    ) -> SubagentCancelOutcome {
        if !self.is_reachable_from_session(id, parent_session_id) {
            return SubagentCancelOutcome::NotFound;
        }
        if let Some(child) = self.active.get_mut(id) {
            child.explicitly_killed |= explicit;
            child.disposition = PendingDisposition::Cancelled;
            child.cancellation.cancel();
            child.control.cancel();
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.pending.get_mut(id) {
            child.explicitly_killed |= explicit;
            child.disposition = PendingDisposition::Cancelled;
            child.cancellation.cancel();
            self.reject_spawn_ready_ids(&[id.to_owned()]);
            return SubagentCancelOutcome::Cancelled;
        }
        if self.remove_queued(|request| request.id == id) > 0 {
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.completed.get(id) {
            return SubagentCancelOutcome::AlreadyFinished {
                status: child.result.status().to_owned(),
            };
        }
        SubagentCancelOutcome::NotFound
    }

    fn cancel_parent_prompt(&mut self, parent_prompt_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values_mut() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        let mut doomed = Vec::new();
        for child in self.pending.values_mut() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                doomed.push(child.request.id.clone());
            }
        }
        self.reject_spawn_ready_ids(&doomed);
        self.remove_queued(|request| {
            request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(request, parent_session_id)
        });
    }

    fn teardown_session_children(&mut self, parent_session_id: &str) {
        let mut cancelled = self.reject_pending_wakes_for_session(parent_session_id);
        for child in self.active.values_mut() {
            if child.request.parent_session_id == parent_session_id {
                // Parent is gone: do not rebuffer this completion for a later
                // resume of the same session id.
                child.request.surface_completion = false;
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                child.control.cancel();
                cancelled += 1;
            }
        }
        let mut doomed = Vec::new();
        for child in self.pending.values_mut() {
            if child.request.parent_session_id == parent_session_id {
                child.request.surface_completion = false;
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                doomed.push(child.request.id.clone());
                cancelled += 1;
            }
        }
        self.reject_spawn_ready_ids(&doomed);
        // Parent is gone here too: a queued spawn's cancelled completion must
        // not be rebuffered for a later resume of the same session id.
        for queued in self.queued.iter_mut() {
            if queued.request.parent_session_id == parent_session_id {
                queued.request.surface_completion = false;
            }
        }
        cancelled += self.remove_queued(|request| request.parent_session_id == parent_session_id);
        if cancelled > 0 {
            tracing::info!(
                parent_session_id,
                cancelled,
                "cancelled subagents on session teardown"
            );
        }
    }

    /// Whether any child or parked wake still belongs to the session. Unlike
    /// [`Self::session_running_count`] it counts every owner, including workflow,
    /// since teardown drains all children and parked wake callers.
    fn session_has_children(&self, parent_session_id: &str) -> bool {
        self.active
            .values()
            .map(|child| &child.request)
            .chain(self.pending.values().map(|child| &child.request))
            .chain(self.queued.iter().map(|queued| queued.request.as_ref()))
            .any(|request| request.parent_session_id == parent_session_id)
            || self.pending_wakes.keys().any(|subagent_id| {
                self.completed.get(subagent_id).is_some_and(|completed| {
                    completed.request.parent_session_id == parent_session_id
                })
            })
    }

    /// Latch spawn admission closed for a delete-path teardown and park the
    /// responder until the last child drains (or the backstop deadline fires).
    fn begin_teardown_drain(&mut self, parent_session_id: String, respond_to: oneshot::Sender<()>) {
        let deadline = tokio::time::Instant::now() + TEARDOWN_DRAIN_MAX;
        self.spawn_blocked_sessions
            .insert(parent_session_id.clone());
        self.teardown_drains
            .entry(parent_session_id)
            .or_insert_with(|| TeardownDrain {
                waiters: Vec::new(),
                deadline,
            })
            .waiters
            .push(respond_to);
    }

    /// Clear a delete-path hold: reopen the session's spawn admission and
    /// resolve every parked drain responder.
    fn clear_teardown_drain(&mut self, parent_session_id: &str) {
        self.spawn_blocked_sessions.remove(parent_session_id);
        if let Some(drain) = self.teardown_drains.remove(parent_session_id) {
            for respond_to in drain.waiters {
                let _ = respond_to.send(());
            }
        }
    }

    fn resolve_teardown_drain_waiters(&mut self, parent_session_id: &str) {
        // Cheap precondition (one lookup) before the three-collection scan on
        // every child completion: only a delete-path teardown holds a drain.
        if !self.teardown_drains.contains_key(parent_session_id) {
            return;
        }
        if self.session_has_children(parent_session_id) {
            return;
        }
        self.clear_teardown_drain(parent_session_id);
    }

    /// All non-workflow children for the parent session (user Stop / Esc). Requires a concrete
    /// session id — unbound (`None`) is rejected so a wildcard cannot cancel every session on a
    /// shared coordinator.
    fn cancel_parent_session(&mut self, parent_session_id: Option<&str>) -> SubagentCancelOutcome {
        let Some(parent_session_id) = parent_session_id else {
            return SubagentCancelOutcome::NotFound;
        };
        self.spawn_blocked_sessions
            .insert(parent_session_id.to_owned());
        for child in self.active.values_mut() {
            if child.request.parent_session_id == parent_session_id
                && !child.request.owner.is_workflow()
            {
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        let mut doomed = Vec::new();
        for child in self.pending.values_mut() {
            if child.request.parent_session_id == parent_session_id
                && !child.request.owner.is_workflow()
            {
                child.disposition = PendingDisposition::Cancelled;
                child.cancellation.cancel();
                doomed.push(child.request.id.clone());
            }
        }
        self.reject_spawn_ready_ids(&doomed);
        self.remove_queued(|request| {
            request.parent_session_id == parent_session_id && !request.owner.is_workflow()
        });
        self.reject_pending_wakes_for_session(parent_session_id);
        SubagentCancelOutcome::Cancelled
    }

    fn cancel_workflow_children(&mut self, run_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        let mut doomed = Vec::new();
        for child in self.pending.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                doomed.push(child.request.id.clone());
            }
        }
        self.reject_spawn_ready_ids(&doomed);
    }

    fn resolve_workflow_cancel_waiters(&mut self, run_id: &str) {
        if workflow_outstanding(&self.pending, &self.active, run_id) != 0 {
            return;
        }
        for respond_to in self
            .workflow_cancel_waiters
            .remove(run_id)
            .unwrap_or_default()
        {
            let _ = respond_to.send(SubagentCancelOutcome::Cancelled);
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .filter_map(|child| child.foreground_deadline)
            .chain(
                self.active
                    .values()
                    .filter_map(|child| child.foreground_deadline),
            )
            .chain(
                self.queued
                    .iter()
                    .filter_map(|queued| queued.caller.deadline()),
            )
            .chain(
                // Anchored to the last sweep, not `now`: a stream of other
                // wakes must not keep pushing the next sweep further out.
                (!self.queued.is_empty()).then(|| self.last_queued_reap + QUEUED_REAP_INTERVAL),
            )
            .chain(
                self.waiters
                    .values()
                    .flatten()
                    .map(|waiter| waiter.deadline),
            )
            .chain(self.teardown_drains.values().map(|drain| drain.deadline))
            .chain(self.spawn_ready.deadlines())
            .min()
    }

    fn reap_abandoned_callers(&mut self) {
        self.last_queued_reap = tokio::time::Instant::now();
        for child in self.pending.values_mut() {
            background_if_caller_gone(child);
        }
        for child in self.active.values_mut() {
            background_if_caller_gone(child);
        }
        // The queued leg of the same sweep.
        for queued in self.queued.iter_mut() {
            if let QueuedCaller::Awaiting { result_tx, .. } = &queued.caller
                && result_tx.is_closed()
            {
                queued.caller = QueuedCaller::Backgrounded;
            }
        }
        self.remove_queued(|request| request.cancel_token.is_cancelled());
    }

    fn process_deadlines(&mut self) {
        self.reap_abandoned_callers();
        let now = tokio::time::Instant::now();
        self.expire_spawn_ready_messages(now);
        // Backstop: a delete-path hold whose drain deadline elapsed force-clears
        // so a child that never finishes cannot block spawns forever.
        let stale: Vec<String> = self
            .teardown_drains
            .iter()
            .filter(|(_, drain)| drain.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            tracing::warn!(
                parent_session_id = %id,
                still_draining = self.session_has_children(&id),
                "teardown drain deadline elapsed; force-reopening spawn admission",
            );
            self.clear_teardown_drain(&id);
        }
        for child in self.pending.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        for child in self.active.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        // The spawn stays queued; only its caller is handed off.
        for queued in self.queued.iter_mut() {
            if queued
                .caller
                .deadline()
                .is_none_or(|deadline| deadline > now)
            {
                continue;
            }
            let caller = std::mem::replace(&mut queued.caller, QueuedCaller::Backgrounded);
            let Some(result_tx) = caller.into_spawn_reply() else {
                continue;
            };
            tracing::warn!(
                subagent_id = %queued.request.id,
                budget_ms = self.config.foreground_budget.as_millis() as u64,
                "queued subagent exceeded await budget; auto-backgrounding (spawn stays queued)",
            );
            let _ = result_tx.send(SubagentResult::backgrounded(
                queued.request.id.clone(),
                queued.request.id.clone(),
            ));
        }

        let ids: Vec<_> = self.waiters.keys().cloned().collect();
        for id in ids {
            let waiters = self.waiters.remove(&id).unwrap_or_default();
            let (due, live): (Vec<_>, Vec<_>) = waiters
                .into_iter()
                .partition(|waiter| waiter.deadline <= now);
            if !live.is_empty() {
                self.waiters.insert(id.clone(), live);
            }
            for waiter in due {
                if waiter.respond_to.is_closed() {
                    continue;
                }
                if self.active.contains_key(&id) {
                    self.queue_active_progress(&id, ProgressTarget::Query(waiter.respond_to));
                } else {
                    let _ = waiter.respond_to.send(self.ready_snapshot(&id));
                }
            }
        }
    }

    fn running_count_changed(&self) {
        self.runner
            .running_count_changed(self.pending.len() + self.active.len());
    }

    fn cancel_all_children(&self) {
        for child in self.active.values() {
            child.cancellation.cancel();
            child.control.cancel();
        }
        for child in self.pending.values() {
            child.cancellation.cancel();
        }
    }

    /// `None` (an unbound backend) matches every child.
    fn is_reachable_from_session(&self, child_id: &str, session_id: Option<&str>) -> bool {
        session_id.is_none_or(|id| self.graph.is_reachable_from(child_id, id))
    }

    pub(super) fn active_child_for_session(
        &self,
        session_id: &str,
    ) -> Option<&ActiveChild<R::Control>> {
        self.active
            .values()
            .find(|child| child.child_session_id == session_id)
    }
}

/// Root only by design; lineage never widens these scopes.
fn belongs_to_session(request: &SubagentRequest, parent_session_id: Option<&str>) -> bool {
    parent_session_id.is_none_or(|id| request.parent_session_id == id)
}

fn request_in_scope(request: &SubagentRequest, parent_session_id: &str, prompt_id: &str) -> bool {
    request.parent_session_id == parent_session_id
        && request.parent_prompt_id.as_deref() == Some(prompt_id)
        && !request.owner.is_workflow()
}

/// Ready as soon as any still-foreground caller drops its spawn-reply receiver. The actor `select!` uses this so an
/// abandonment wakes the loop rather than waiting on the next command or the foreground deadline. Backgrounded children
/// are skipped: their callers no longer gate the turn, and a lingering closed reply on one must not busy-wake the loop.
fn poll_caller_abandoned<C: ChildControl>(
    pending: &mut HashMap<String, PendingChild>,
    active: &mut HashMap<String, ActiveChild<C>>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<()> {
    for child in pending.values_mut() {
        if !child.handle_only
            && let Some(reply) = child.spawn_reply.as_mut()
            && reply.poll_closed(cx).is_ready()
        {
            return std::task::Poll::Ready(());
        }
    }
    for child in active.values_mut() {
        if !child.handle_only
            && let Some(reply) = child.spawn_reply.as_mut()
            && reply.poll_closed(cx).is_ready()
        {
            return std::task::Poll::Ready(());
        }
    }
    std::task::Poll::Pending
}

impl<R: ChildRunner> Drop for SubagentCoordinator<R> {
    fn drop(&mut self) {
        // Not `remove_queued`: that routes through `finish_child`, which runs
        // host completion callbacks — off-limits from a destructor (the
        // host's storage may already be tearing down).
        self.resolve_queued_at_drop();
        for pending in self.pending_wakes.drain().flat_map(|(_, pending)| pending) {
            let _ = pending
                .ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        self.cancel_all_children();
        self.active_messages.clear();
        self.spawn_ready.clear();
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
pub(super) mod tests;
#[cfg(test)]
#[path = "coordinator/wake_tests.rs"]
mod wake_tests;
