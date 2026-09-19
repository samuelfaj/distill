// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::coordinator::ActiveChildGeneration;
use super::coordinator::active_message::ActiveMessageLifecycle;
use super::types::{
    ActiveAgentMessageDelivery, ActiveSubagentSummary, AgentAddress, SubagentCompletionSummary,
    SubagentDescribeOutcome, SubagentInspection, SubagentRequest, SubagentResult,
    SubagentResumeLookup, SubagentSnapshot, SubagentSnapshotStatus, SubagentValidateTypeOutcome,
};

/// Cap on retained completed-subagent entries before the oldest are evicted.
pub const MAX_COMPLETED_ENTRIES: usize = 1024;
pub(super) const OUTPUT_UNAVAILABLE_PLACEHOLDER: &str = "[subagent output no longer available]";

pub type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;
pub type SendBoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Runtime-specific live progress for one active child.
#[derive(Debug, Clone, Default)]
pub struct SubagentProgress {
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_usage_pct: u8,
    pub tools_used: Vec<String>,
    pub error_count: u32,
}

pub use super::active_message::ActiveMessageAdmission;

/// Maximum host admissions retained for one active child.
pub const MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD: usize = 8;

/// Default absolute active-message capacity for one coordinator.
pub const MAX_ACTIVE_MESSAGE_ADMISSIONS: usize = 64;

/// Maximum wait for a host to confirm or reject admission.
pub const ACTIVE_MESSAGE_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum wait for an owned spawning child to become active before a parked
/// agent send is released. Session bootstrap routinely exceeds the admission
/// timeout, so this backstop is separate and much longer.
pub const ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(120);

/// Maximum wait for the coordinator to drain selected admissions.
pub const ACTIVE_MESSAGE_FINALIZATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(6);

/// Runtime handle retained while a child is active.
pub trait ChildControl: 'static {
    type ProgressFuture: Future<Output = SubagentProgress> + 'static;

    fn progress(&self) -> Self::ProgressFuture;

    /// Admit a coordinator-authorized message to this child. `Admitted` is valid only when
    /// protected-row insertion succeeds inside [`ActiveAgentMessageDelivery::commit_admission`].
    /// The returned future must be `Send` so multithreaded hosts can spawn the coordinator.
    fn send_active_message(
        &self,
        _delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        Box::pin(std::future::ready(ActiveMessageAdmission::Unsupported))
    }

    fn cancel(&self);
}

/// Data reported when runtime initialization has produced a live child.
pub struct StartedChild<C> {
    pub child_session_id: String,
    pub persona: Option<String>,
    pub resumed_from: Option<String>,
    pub child_cwd: String,
    pub worktree_path: Option<String>,
    pub effective_model_id: String,
    /// The resolved agent definition declares `background: true`. Folded into `Outstanding`
    /// accounting (background, never turn-blocking) while the foreground await budget stays gated
    /// on the tool's own `run_in_background` flag.
    pub definition_background: bool,
    pub control: C,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeOrigin {
    pub agent_id: String,
    pub source: super::types::ActiveAgentMessageSource,
    pub message_id: String,
}

/// Input to one runtime-specific child run.
pub struct ChildRunRequest<C> {
    pub request: SubagentRequest,
    pub cancellation: CancellationToken,
    pub reporter: ChildReporter<C>,
    pub attempt_id: distill_message_delivery_core::AttemptId,
    pub generation: super::root_control::AgentMessageGeneration,
    pub agent_message_sender: Option<super::types::AgentMessageSender>,
    pub wake_origin: Option<WakeOrigin>,
    /// Time parked in the admission queue; `None` if admitted immediately.
    pub queued_for: Option<std::time::Duration>,
    /// The session's running non-workflow children when this spawn started,
    /// including itself when it is one of them.
    pub session_running: usize,
    /// Coordinator-minted address for this generation. Advertised at spawn
    /// and resolvable on the pending record before promotion.
    pub agent_address: Option<AgentAddress>,
    /// Pre-reparent spawner session, when the live address must also be
    /// advertised there. Cleared if that session cannot be targeted.
    pub spawner_session_id: Option<String>,
}

/// Terminal output from one runtime-specific child run.
pub struct ChildRunOutput<D> {
    pub result: SubagentResult,
    pub completion_data: D,
    pub snapshot_ref: Option<String>,
}

/// Coordinator-owned delivery decision passed to host presentation.
#[derive(Debug, Clone)]
pub struct CompletionDisposition {
    pub foreground_delivered: bool,
    pub backgrounded: bool,
    pub waiter_delivered: bool,
    pub explicitly_killed: bool,
    pub should_surface: bool,
}

/// Terminal event delivered to the runtime adapter after state is committed.
pub struct ChildCompletion<D> {
    pub request: SubagentRequest,
    pub result: SubagentResult,
    pub snapshot: SubagentSnapshot,
    pub completion_data: D,
    pub disposition: CompletionDisposition,
}

/// The only host-specific seam. Associated future types intentionally carry no unconditional `Send`
/// bound. A local runner may return non-`Send` futures, while a multithreaded runner may return
/// `Send` futures.
pub trait ChildRunner: 'static {
    type Control: ChildControl;
    type RootControl: super::root_control::RootControl;
    type CompletionData: Default + 'static;
    type RunFuture: Future<Output = ChildRunOutput<Self::CompletionData>> + 'static;
    type ValidateFuture: Future<Output = SubagentValidateTypeOutcome> + 'static;
    type DescribeFuture: Future<Output = SubagentDescribeOutcome> + 'static;

    fn run(&self, request: ChildRunRequest<Self::Control>) -> Self::RunFuture;

    fn validate_type(
        &self,
        subagent_type: String,
        parent_session_id: String,
    ) -> Self::ValidateFuture;

    fn describe_type(
        &self,
        subagent_type: String,
        harness_agent_type: Option<String>,
        parent_session_id: String,
    ) -> Self::DescribeFuture;

    /// Present the terminal lifecycle, then invoke `terminal_published`.
    fn on_completed(
        &self,
        completion: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    );

    /// Whether `run` can continue a woken agent's persisted session in place.
    fn supports_wake(&self) -> bool;

    fn supports_agent_message_sender(&self) -> bool {
        false
    }

    fn resolve_root(
        &self,
        _agent_id: &distill_message_delivery_core::AgentId,
    ) -> Option<Self::RootControl> {
        None
    }

    fn resolve_root_session(&self, _session_id: &str) -> Option<Self::RootControl> {
        None
    }

    fn running_count_changed(&self, _running: usize) {}

    fn persisted_output_ref(&self, _completion_data: &Self::CompletionData) -> Option<String> {
        None
    }

    fn load_persisted_output(&self, _reference: &str) -> Option<Arc<str>> {
        None
    }
}

/// What an admission limit did to a spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentLimitDecision {
    QueuedAtConcurrentLimit { limit: usize },
    RejectedAtConcurrentLimit { limit: usize },
}

/// What asked for the spawn that hit a limit. Workflow-owned spawns bypass
/// admission, so a workflow arm never appears here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitedSpawnOrigin {
    Task,
    SchedulerLoop,
}

/// One admission decision at a session limit; workflow-owned spawns bypass
/// admission and never appear here.
#[derive(Debug, Clone)]
pub struct SubagentLimitNotice {
    pub parent_session_id: String,
    pub decision: SubagentLimitDecision,
    /// Session children running against the limit at decision time.
    pub running: usize,
    /// The session's queue depth after the decision applies: a queued spawn
    /// counts itself, a rejected spawn does not.
    pub queue_depth: usize,
    pub origin: LimitedSpawnOrigin,
}

/// Host-injected observer for admission decisions.
pub type SubagentLimitSink = Arc<dyn Fn(SubagentLimitNotice) + Send + Sync>;

/// Host-configurable lifecycle policy. The transition logic remains shared.
#[derive(Clone)]
pub struct CoordinatorConfig {
    pub foreground_budget: std::time::Duration,
    /// Session-scoped spawn limits enforced by the coordinator.
    pub limits: super::admission::SubagentLimits,
    /// Observer for spawns that hit an admission limit.
    pub limit_sink: Option<SubagentLimitSink>,
    /// Whether the host drains completion summaries between turns.
    pub buffer_completions: bool,
    /// Extra cap applied to BUFFERED summary outputs only (the request's own `completion_output_cap` still applies first).
    /// Buffered entries pin the child's output `Arc` until drained; hosts whose reminder rendering never inlines the output
    /// (a polling tool exists, e.g. the callback tools-server) should bound it.
    pub buffered_completion_output_cap: Option<usize>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            foreground_budget: std::time::Duration::from_secs(45),
            limits: super::admission::SubagentLimits::default(),
            limit_sink: None,
            buffer_completions: false,
            buffered_completion_output_cap: None,
        }
    }
}

impl std::fmt::Debug for CoordinatorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorConfig")
            .field("foreground_budget", &self.foreground_budget)
            .field("limits", &self.limits)
            .field("limit_sink", &self.limit_sink.is_some())
            .field("buffer_completions", &self.buffer_completions)
            .field(
                "buffered_completion_output_cap",
                &self.buffered_completion_output_cap,
            )
            .finish()
    }
}

/// Runner-side channel back into the actor.
pub struct ChildReporter<C> {
    pub(super) subagent_id: String,
    pub(super) tx: mpsc::UnboundedSender<InternalEvent<C>>,
}

impl<C> Clone for ChildReporter<C> {
    fn clone(&self) -> Self {
        Self {
            subagent_id: self.subagent_id.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl<C: 'static> ChildReporter<C> {
    /// Promote the pending child to active. The acknowledgement closes the
    /// cancel-at-promote race: `false` means cancellation won and the adapter
    /// must tear down the half-initialized runtime.
    pub async fn started(&self, child: StartedChild<C>) -> bool {
        self.started_with_deferred_admission(child, false).await
    }

    /// Promote a wake while keeping its initial message parked until durable
    /// start publication succeeds.
    pub async fn started_deferred(&self, child: StartedChild<C>) -> bool {
        self.started_with_deferred_admission(child, true).await
    }

    async fn started_with_deferred_admission(
        &self,
        child: StartedChild<C>,
        defer_admission: bool,
    ) -> bool {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(InternalEvent::Started {
                subagent_id: self.subagent_id.clone(),
                child,
                defer_admission,
                respond_to,
            })
            .is_err()
        {
            return false;
        }
        response_rx.await.unwrap_or(false)
    }

    /// Commit or reject a wake whose active promotion deferred message admission.
    pub async fn settle_deferred_start(&self, is_committed: bool) -> bool {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(InternalEvent::SettleDeferredStart {
                subagent_id: self.subagent_id.clone(),
                is_committed,
                respond_to,
            })
            .is_err()
        {
            return false;
        }
        response_rx.await.unwrap_or(false)
    }

    /// Close admission and wait a bounded interval for selected sends to settle.
    pub async fn finalizing(&self) -> bool {
        let Some(response_rx) = self.request_finalizing() else {
            return false;
        };
        tokio::time::timeout(ACTIVE_MESSAGE_FINALIZATION_TIMEOUT, response_rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    }

    pub(super) fn request_finalizing(&self) -> Option<oneshot::Receiver<bool>> {
        let (respond_to, response_rx) = oneshot::channel();
        self.tx
            .send(InternalEvent::Finalizing {
                subagent_id: self.subagent_id.clone(),
                respond_to,
            })
            .ok()
            .map(|_| response_rx)
    }

    /// Stop advertising the live address to the spawner session; lineage is unaffected.
    pub fn drop_spawner_claim(&self) {
        let _ = self.tx.send(InternalEvent::DropSpawnerClaim {
            subagent_id: self.subagent_id.clone(),
        });
    }

    /// Resolve an in-memory resume source without sharing coordinator state.
    pub async fn resume_source(
        &self,
        source_id: &str,
        parent_session_id: &str,
    ) -> SubagentResumeLookup {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(InternalEvent::ResumeSource {
                source_id: source_id.to_owned(),
                parent_session_id: parent_session_id.to_owned(),
                respond_to,
            })
            .is_err()
        {
            return SubagentResumeLookup::Missing;
        }
        response_rx.await.unwrap_or(SubagentResumeLookup::Missing)
    }
}

pub(super) enum InternalEvent<C> {
    Started {
        subagent_id: String,
        child: StartedChild<C>,
        defer_admission: bool,
        respond_to: oneshot::Sender<bool>,
    },
    SettleDeferredStart {
        subagent_id: String,
        is_committed: bool,
        respond_to: oneshot::Sender<bool>,
    },
    Finalizing {
        subagent_id: String,
        respond_to: oneshot::Sender<bool>,
    },
    ResumeSource {
        source_id: String,
        parent_session_id: String,
        respond_to: oneshot::Sender<SubagentResumeLookup>,
    },
    DropSpawnerClaim {
        subagent_id: String,
    },
}

/// Prior terminal record removed while a wake is pending or queued.
/// The pending/queued wake owns restoration on every exit before `Started`;
/// promotion permanently replaces this record, so restoration is no longer legal.
pub(super) struct DisplacedCompletedChild {
    pub(super) completed: Box<CompletedChild>,
}

/// Set at the cancel sites; a wake rollback reads it because host rejections cancel the token too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PendingDisposition {
    #[default]
    Live,
    /// A user or owner cancel: the prior completed record must not be re-woken.
    Cancelled,
}

pub(super) struct PendingChild {
    pub(super) request: SubagentRequest,
    pub(super) started_at: std::time::Instant,
    pub(super) cancellation: CancellationToken,
    pub(super) spawn_reply: Option<oneshot::Sender<SubagentResult>>,
    pub(super) foreground_deadline: Option<tokio::time::Instant>,
    pub(super) handle_only: bool,
    pub(super) explicitly_killed: bool,
    pub(super) disposition: PendingDisposition,
    /// False when the record was synthesized for a spawn that never reached
    /// the runner (admission reject, cancelled while queued).
    pub(super) launched: bool,
    pub(super) attempt_id: distill_message_delivery_core::AttemptId,
    pub(super) generation: ActiveChildGeneration,
    pub(super) agent_address: Option<AgentAddress>,
    /// Pre-reparent spawner session for a nested spawn. Human sends from it
    /// stay owned only while that session can be given the live address.
    pub(super) spawner_session_id: Option<String>,
    pub(super) wake_of: Option<DisplacedCompletedChild>,
}

pub(super) struct ActiveChild<C> {
    pub(super) request: SubagentRequest,
    pub(super) started_at: std::time::Instant,
    pub(super) cancellation: CancellationToken,
    pub(super) spawn_reply: Option<oneshot::Sender<SubagentResult>>,
    pub(super) foreground_deadline: Option<tokio::time::Instant>,
    pub(super) handle_only: bool,
    /// Definition-declared background (see [`StartedChild`]): background for
    /// `Outstanding` accounting even while the spawn caller block-awaits.
    pub(super) definition_background: bool,
    pub(super) explicitly_killed: bool,
    pub(super) disposition: PendingDisposition,
    pub(super) child_session_id: String,
    pub(super) persona: Option<String>,
    pub(super) resumed_from: Option<String>,
    pub(super) child_cwd: String,
    pub(super) worktree_path: Option<String>,
    pub(super) effective_model_id: String,
    pub(super) attempt_id: distill_message_delivery_core::AttemptId,
    pub(super) generation: ActiveChildGeneration,
    pub(super) agent_address: Option<AgentAddress>,
    /// See [`PendingChild::spawner_session_id`].
    pub(super) spawner_session_id: Option<String>,
    pub(super) active_messages: ActiveMessageLifecycle,
    pub(super) deferred_wake: Option<DisplacedCompletedChild>,
    pub(super) control: C,
}

pub(super) struct CompletedChild {
    pub(super) completion_age: u64,
    pub(super) terminal_published: Arc<std::sync::atomic::AtomicBool>,
    pub(super) request: SubagentRequest,
    pub(super) started_at: std::time::Instant,
    pub(super) child_session_id: String,
    pub(super) persona: Option<String>,
    pub(super) resumed_from: Option<String>,
    pub(super) child_cwd: String,
    pub(super) worktree_path: Option<String>,
    pub(super) snapshot_ref: Option<String>,
    pub(super) persisted_output_ref: Option<String>,
    pub(super) effective_model_id: String,
    pub(super) agent_address: Option<AgentAddress>,
    pub(super) spawner_session_id: Option<String>,
    pub(super) wake_eligible: bool,
    pub(super) result: SubagentResult,
}

pub(super) struct BlockingWaiter {
    pub(super) deadline: tokio::time::Instant,
    pub(super) respond_to: oneshot::Sender<Option<SubagentSnapshot>>,
}

pub(super) struct BufferedCompletion {
    pub(super) parent_session_id: String,
    pub(super) summary: SubagentCompletionSummary,
}

impl BufferedCompletion {
    pub(super) fn is_owned_by(&self, parent_session_id: Option<&str>) -> bool {
        parent_session_id.is_none_or(|id| self.parent_session_id == id)
    }
}

pub(super) struct TaggedFuture<F> {
    pub(super) subagent_id: String,
    pub(super) future: Pin<Box<F>>,
}

impl<F: Future> Future for TaggedFuture<F> {
    type Output = (String, F::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.future
            .as_mut()
            .poll(cx)
            .map(|output| (this.subagent_id.clone(), output))
    }
}

pub(super) struct ReplyFuture<F, T> {
    pub(super) future: Pin<Box<F>>,
    pub(super) respond_to: Option<oneshot::Sender<T>>,
}

impl<F, T> Future for ReplyFuture<F, T>
where
    F: Future<Output = T>,
{
    type Output = (oneshot::Sender<T>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.future.as_mut().poll(cx).map(|output| {
            let respond_to = match this.respond_to.take() {
                Some(respond_to) => respond_to,
                None => unreachable!("reply future polled after completion"),
            };
            (respond_to, output)
        })
    }
}

#[derive(Clone)]
pub(super) struct RunningSeed {
    pub(super) subagent_id: String,
    pub(super) description: String,
    pub(super) subagent_type: String,
    pub(super) started_at_epoch_ms: u64,
    pub(super) duration_ms: u64,
    pub(super) persona: Option<String>,
    pub(super) parent_session_id: String,
    pub(super) child_session_id: String,
    pub(super) fork_parent_prompt_id: Option<String>,
    pub(super) resumed_from: Option<String>,
}

pub(super) enum ProgressTarget {
    Query(oneshot::Sender<Option<SubagentSnapshot>>),
    Inspect(oneshot::Sender<Option<SubagentInspection>>),
    List { request_id: u64, index: usize },
}

pub(super) struct ProgressFuture<F> {
    pub(super) future: Pin<Box<F>>,
    pub(super) seed: Option<RunningSeed>,
    pub(super) target: Option<ProgressTarget>,
}

impl<F> Future for ProgressFuture<F>
where
    F: Future<Output = SubagentProgress>,
{
    type Output = (RunningSeed, ProgressTarget, SubagentProgress);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.future.as_mut().poll(cx).map(|progress| {
            let seed = match this.seed.take() {
                Some(seed) => seed,
                None => unreachable!("progress future polled without a seed"),
            };
            let target = match this.target.take() {
                Some(target) => target,
                None => unreachable!("progress future polled without a target"),
            };
            (seed, target, progress)
        })
    }
}

pub(super) struct ListRequest {
    pub(super) slots: Vec<Option<SubagentInspection>>,
    pub(super) remaining: usize,
    pub(super) respond_to: oneshot::Sender<Vec<SubagentInspection>>,
}

// Both variants already hold a full child record; boxing Active would
// allocate on every promotion without shrinking the Pending case.
#[allow(clippy::large_enum_variant)]
pub(super) enum ChildRecord<C> {
    Pending(PendingChild),
    Active(ActiveChild<C>),
}

impl<C> ChildRecord<C> {
    pub(super) fn request(&self) -> &SubagentRequest {
        match self {
            Self::Pending(child) => &child.request,
            Self::Active(child) => &child.request,
        }
    }

    pub(super) fn explicitly_killed(&self) -> bool {
        match self {
            Self::Pending(child) => child.explicitly_killed,
            Self::Active(child) => child.explicitly_killed,
        }
    }

    pub(super) fn attempt_id(&self) -> &distill_message_delivery_core::AttemptId {
        match self {
            Self::Pending(child) => &child.attempt_id,
            Self::Active(child) => &child.attempt_id,
        }
    }

    pub(super) fn take_failed_pre_start_wake(
        &mut self,
        success: bool,
    ) -> Option<DisplacedCompletedChild> {
        if success {
            return None;
        }
        match self {
            Self::Pending(child) => child.wake_of.take(),
            Self::Active(child) => child.deferred_wake.take(),
        }
    }
}

pub(super) trait ForegroundChild {
    fn id(&self) -> &str;
    fn child_session_id(&self) -> &str;
    fn deadline(&self) -> Option<tokio::time::Instant>;
    /// True when the spawn caller dropped its result receiver while this
    /// child was still treated as turn-blocking (old shell `ParentGone`).
    fn caller_gone(&self) -> bool;
    fn is_workflow(&self) -> bool;
    fn take_reply(&mut self) -> Option<oneshot::Sender<SubagentResult>>;
    fn mark_backgrounded(&mut self);
    /// Cancel the child's execution (token + active control where present).
    fn cancel(&mut self);
}

impl ForegroundChild for PendingChild {
    fn id(&self) -> &str {
        &self.request.id
    }

    fn child_session_id(&self) -> &str {
        &self.request.id
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.foreground_deadline
    }

    fn caller_gone(&self) -> bool {
        !self.handle_only && self.spawn_reply.as_ref().is_some_and(|tx| tx.is_closed())
    }

    fn is_workflow(&self) -> bool {
        self.request.owner.is_workflow()
    }

    fn take_reply(&mut self) -> Option<oneshot::Sender<SubagentResult>> {
        self.spawn_reply.take()
    }

    fn mark_backgrounded(&mut self) {
        self.handle_only = true;
        self.foreground_deadline = None;
    }

    fn cancel(&mut self) {
        self.cancellation.cancel();
    }
}

impl<C: ChildControl> ForegroundChild for ActiveChild<C> {
    fn id(&self) -> &str {
        &self.request.id
    }

    fn child_session_id(&self) -> &str {
        &self.child_session_id
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.foreground_deadline
    }

    fn caller_gone(&self) -> bool {
        !self.handle_only && self.spawn_reply.as_ref().is_some_and(|tx| tx.is_closed())
    }

    fn is_workflow(&self) -> bool {
        self.request.owner.is_workflow()
    }

    fn take_reply(&mut self) -> Option<oneshot::Sender<SubagentResult>> {
        self.spawn_reply.take()
    }

    fn mark_backgrounded(&mut self) {
        self.handle_only = true;
        self.foreground_deadline = None;
    }

    fn cancel(&mut self) {
        self.cancellation.cancel();
        self.control.cancel();
    }
}

pub(super) fn background_at_deadline(
    child: &mut impl ForegroundChild,
    now: tokio::time::Instant,
    budget: std::time::Duration,
) {
    if child.deadline().is_none_or(|deadline| deadline > now) {
        return;
    }
    tracing::warn!(
        subagent_id = child.id(),
        budget_ms = budget.as_millis() as u64,
        "foreground subagent exceeded await budget; auto-backgrounding (child keeps running)",
    );
    if let Some(respond_to) = child.take_reply() {
        // Interim handoff, not a completion: keep `success: false` (default)
        // so `SubagentResult::status()` consumers cannot record a completed
        // status for a still-running child. Callers branch on `backgrounded`.
        let _ = respond_to.send(SubagentResult::backgrounded(
            child.id(),
            child.child_session_id(),
        ));
    }
    child.mark_backgrounded();
}

/// Handle a foreground child whose spawn caller dropped the result channel (parent turn stop /
/// cancelled await). Task-owned children keep running and just leave the turn-blocking
/// `Outstanding` set — shell `ParentGone` parity.
pub(super) fn background_if_caller_gone(child: &mut impl ForegroundChild) {
    if !child.caller_gone() {
        return;
    }
    let _ = child.take_reply();
    if child.is_workflow() {
        tracing::debug!(
            subagent_id = child.id(),
            "workflow subagent caller gone; cancelling child",
        );
        child.cancel();
        return;
    }
    tracing::debug!(
        subagent_id = child.id(),
        "foreground subagent caller gone; auto-backgrounding (child keeps running)",
    );
    child.mark_backgrounded();
}

pub(super) async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn instant_to_epoch_ms(instant: std::time::Instant) -> u64 {
    let now_instant = std::time::Instant::now();
    let now_system = std::time::SystemTime::now();
    let elapsed = now_instant.saturating_duration_since(instant);
    now_system
        .checked_sub(elapsed)
        .unwrap_or(now_system)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn active_summary<C>(child: &ActiveChild<C>) -> ActiveSubagentSummary {
    ActiveSubagentSummary {
        subagent_id: child.request.id.clone(),
        subagent_type: child.request.subagent_type.clone(),
        description: child.request.description.clone(),
        elapsed_ms: child.started_at.elapsed().as_millis() as u64,
    }
}

pub(super) fn running_seed<C>(child: &ActiveChild<C>) -> RunningSeed {
    RunningSeed {
        subagent_id: child.request.id.clone(),
        description: child.request.description.clone(),
        subagent_type: child.request.subagent_type.clone(),
        started_at_epoch_ms: instant_to_epoch_ms(child.started_at),
        duration_ms: child.started_at.elapsed().as_millis() as u64,
        persona: child.persona.clone(),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: child.child_session_id.clone(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.resumed_from.clone(),
    }
}

pub(super) fn running_inspection(
    seed: RunningSeed,
    progress: SubagentProgress,
) -> SubagentInspection {
    SubagentInspection {
        snapshot: SubagentSnapshot {
            subagent_id: seed.subagent_id,
            description: seed.description,
            subagent_type: seed.subagent_type,
            status: SubagentSnapshotStatus::Running {
                turn_count: progress.turn_count,
                tool_call_count: progress.tool_call_count,
                tokens_used: progress.tokens_used,
                context_window_tokens: progress.context_window_tokens,
                context_usage_pct: progress.context_usage_pct,
                tools_used: progress.tools_used,
                error_count: progress.error_count,
            },
            started_at_epoch_ms: seed.started_at_epoch_ms,
            duration_ms: seed.duration_ms,
            persona: seed.persona,
        },
        parent_session_id: seed.parent_session_id,
        child_session_id: seed.child_session_id,
        fork_parent_prompt_id: seed.fork_parent_prompt_id,
        resumed_from: seed.resumed_from,
    }
}

pub(super) fn pending_snapshot(child: &PendingChild) -> SubagentSnapshot {
    SubagentSnapshot {
        subagent_id: child.request.id.clone(),
        description: child.request.description.clone(),
        subagent_type: child.request.subagent_type.clone(),
        status: SubagentSnapshotStatus::Initializing,
        started_at_epoch_ms: instant_to_epoch_ms(child.started_at),
        duration_ms: child.started_at.elapsed().as_millis() as u64,
        persona: child.request.runtime_overrides.persona.clone(),
    }
}

pub(super) fn pending_inspection(child: &PendingChild) -> SubagentInspection {
    SubagentInspection {
        snapshot: pending_snapshot(child),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: String::new(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.request.resume_from.clone(),
    }
}

/// A spawn parked at the session's concurrent limit reads as initializing
/// rather than "not found".
pub(super) fn queued_snapshot(
    request: &SubagentRequest,
    queued_at: std::time::Instant,
) -> SubagentSnapshot {
    SubagentSnapshot {
        subagent_id: request.id.clone(),
        description: request.description.clone(),
        subagent_type: request.subagent_type.clone(),
        status: SubagentSnapshotStatus::Initializing,
        // Stable across polls: the enqueue time, with the duration showing
        // how long the spawn has waited for a slot.
        started_at_epoch_ms: instant_to_epoch_ms(queued_at),
        duration_ms: queued_at.elapsed().as_millis() as u64,
        persona: request.runtime_overrides.persona.clone(),
    }
}

pub(super) fn queued_inspection(
    request: &SubagentRequest,
    queued_at: std::time::Instant,
) -> SubagentInspection {
    SubagentInspection {
        snapshot: queued_snapshot(request, queued_at),
        parent_session_id: request.parent_session_id.clone(),
        child_session_id: String::new(),
        fork_parent_prompt_id: request.parent_prompt_id.clone(),
        resumed_from: request.resume_from.clone(),
    }
}

/// `persona` must be the resolved one; the tool and the notices render this same snapshot.
pub fn terminal_snapshot(
    request: &SubagentRequest,
    result: &SubagentResult,
    persisted_output: Option<&str>,
    persona: Option<String>,
    started_at_epoch_ms: u64,
) -> SubagentSnapshot {
    let status = if result.cancelled {
        SubagentSnapshotStatus::Cancelled {
            reason: result.error.clone(),
        }
    } else if result.success {
        SubagentSnapshotStatus::Completed {
            output: persisted_output
                .map(str::to_owned)
                .unwrap_or_else(|| result.output.to_string()),
            tool_calls: result.tool_calls,
            turns: result.turns,
            worktree_path: result.worktree_path.clone(),
        }
    } else {
        SubagentSnapshotStatus::Failed {
            error: result
                .error
                .clone()
                .unwrap_or_else(|| "Unknown error".to_owned()),
        }
    };
    SubagentSnapshot {
        subagent_id: request.id.clone(),
        description: request.description.clone(),
        subagent_type: request.subagent_type.clone(),
        status,
        started_at_epoch_ms,
        duration_ms: result.duration_ms,
        persona,
    }
}

pub(super) fn completed_snapshot(
    child: &CompletedChild,
    persisted_output: Option<&str>,
) -> SubagentSnapshot {
    terminal_snapshot(
        &child.request,
        &child.result,
        persisted_output,
        child.persona.clone(),
        instant_to_epoch_ms(child.started_at),
    )
}

pub(super) fn completed_inspection(
    child: &CompletedChild,
    persisted_output: Option<&str>,
) -> SubagentInspection {
    SubagentInspection {
        snapshot: completed_snapshot(child, persisted_output),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: child.child_session_id.clone(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.resumed_from.clone(),
    }
}

/// No marker here: the notice renders the cut from `full_output_bytes`.
pub fn cap_completion_output(output: &Arc<str>, cap: usize) -> Arc<str> {
    let head = crate::util::truncate::truncate_str(output, cap);
    if head.len() == output.len() {
        output.clone()
    } else {
        Arc::from(head)
    }
}

/// Model-facing summary for a finished child, honoring the request's
/// `completion_output_cap`. Shared by the coordinator's buffered reminder
/// path and the shell's auto-wake synthetic prompt.
pub fn completion_summary(
    request: &SubagentRequest,
    result: &SubagentResult,
    snapshot: &SubagentSnapshot,
) -> SubagentCompletionSummary {
    let output = match request.runtime_overrides.completion_output_cap {
        Some(cap) => cap_completion_output(&result.output, cap),
        None => result.output.clone(),
    };
    let status = match &snapshot.status {
        // Buffered entries must not pin a second copy of the text.
        SubagentSnapshotStatus::Completed {
            tool_calls,
            turns,
            worktree_path,
            ..
        } => SubagentSnapshotStatus::Completed {
            output: String::new(),
            tool_calls: *tool_calls,
            turns: *turns,
            worktree_path: worktree_path.clone(),
        },
        status => status.clone(),
    };
    SubagentCompletionSummary {
        snapshot: SubagentSnapshot {
            subagent_id: snapshot.subagent_id.clone(),
            description: snapshot.description.clone(),
            subagent_type: snapshot.subagent_type.clone(),
            status,
            started_at_epoch_ms: snapshot.started_at_epoch_ms,
            duration_ms: snapshot.duration_ms,
            persona: snapshot.persona.clone(),
        },
        loop_task_id: request.runtime_overrides.loop_task_id.clone(),
        tool_calls: result.tool_calls,
        output,
        full_output_bytes: result.output.len(),
    }
}

pub(super) fn workflow_outstanding<C>(
    pending: &HashMap<String, PendingChild>,
    active: &HashMap<String, ActiveChild<C>>,
    run_id: &str,
) -> usize {
    pending
        .values()
        .filter(|child| child.request.owner.workflow_run_id() == Some(run_id))
        .count()
        + active
            .values()
            .filter(|child| child.request.owner.workflow_run_id() == Some(run_id))
            .count()
}
