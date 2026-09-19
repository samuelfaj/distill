// Modified for Distill by Samuel Fajreldines, 2026.
//! Parent/subagent boundary: every parent-side lifecycle call site, in order.
//!
//! 1. `MvpAgent::start_subagent_coordinator` (parent thread, in `mvp_agent`) hands the event receiver and concurrency limit here.
//!    `spawn_subagent_coordinator` drives the coordinator (living in `distill-tools`).
//! 2. `ShellChildRunner::run` (parent thread) gathers what a child needs from the parent via `MvpAgent::try_build_subagent_spawn_context`.
//!    That is the parent-to-child snapshot, built by the owner in `mvp_agent`.
//!    It then runs the spawn work on the worker pool (`worker_runtime()`, built on first use).
//! 3. `run_shell_child` (worker pool, in `handle_request.rs`) prepares the child (toolset, optional worktree, context).
//!    It starts the child on its own thread via `spawn_session_on_thread`.
//! 4. `on_completed` then `present_child_completion` (worker pool): reports the child finished, persists the result, and may wake the parent.
//!
//! Stage timings are recorded in `subagent_spawn::SubagentSpawnPhase`.
use super::ShellCompletionData;
use crate::agent::mvp_agent::{LocalRef, MvpAgent};
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::session::SessionCommand;
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_telemetry::region::Region;
use distill_tools::implementations::distill::task::coordinator::{self, ChildCompletion};
use distill_tools::implementations::distill::task::types::{
    SubagentRequest, SubagentResult, SubagentSnapshot,
};
use tokio::sync::mpsc;
/// Floor keeps the pool responsive when `available_parallelism` is tiny.
const MIN_WORKER_THREADS: usize = 2;
/// Four suffice for 32 children (each runs on its own OS thread); `GROK_SUBAGENT_WORKER_THREADS` overrides.
const MAX_WORKER_THREADS: usize = 4;
/// Pool for the per-child pipeline: a dedicated multi-thread runtime so concurrent subagents run in parallel.
/// It sits off the user-facing session's `LocalSet`.
pub(crate) fn worker_runtime() -> Result<&'static tokio::runtime::Handle, std::io::Error> {
    static WORKER: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if let Some(runtime) = WORKER.get() {
        return Ok(runtime.handle());
    }
    let _guard = INIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(runtime) = WORKER.get() {
        return Ok(runtime.handle());
    }
    let runtime = build_worker_runtime()?;
    Ok(WORKER.get_or_init(|| runtime).handle())
}
fn build_worker_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let workers = std::env::var("GROK_SUBAGENT_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(MAX_WORKER_THREADS)
                .clamp(MIN_WORKER_THREADS, MAX_WORKER_THREADS)
        });
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(workers)
        .thread_name("subagent-worker");
    distill_tty_utils::runtime::apply_blocking_pool(builder.enable_all()).build()
}
struct ShellChildRunner {
    agent_ref: LocalRef<MvpAgent>,
    /// Owned: panics are logged, coordinator teardown aborts stragglers.
    presentations: std::cell::RefCell<Vec<tokio_util::task::AbortOnDropHandle<()>>>,
}
pub(crate) fn spawn_pipeline_parent(
    root_span: Option<&tracing::Span>,
) -> Option<tracing::span::Id> {
    root_span
        .and_then(|span| span.id())
        .or_else(|| tracing::Span::current().id())
}
pub(crate) fn subagent_coordinator_channel() -> (
    distill_tools::implementations::distill::task::backend::SubagentCoordinatorSender,
    coordinator::SubagentCoordinatorReceiver,
) {
    coordinator::SubagentCoordinator::<ShellChildRunner>::channel()
}
/// Recovers a worker panic with caller-owned completion data; the handle aborts on drop.
pub(crate) async fn join_worker_task<T>(task: tokio::task::JoinHandle<T>, panic_output: T) -> T {
    let mut task = tokio_util::task::AbortOnDropHandle::new(task);
    match (&mut task).await {
        Ok(output) => output,
        Err(err) if err.is_panic() => panic_output,
        Err(_) => unreachable!("worker runtime is never shut down"),
    }
}
impl coordinator::ChildRunner for ShellChildRunner {
    type Control = crate::agent::subagent::ShellChildRuntime;
    type RootControl = distill_tools::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = crate::agent::subagent::ShellCompletionData;
    type RunFuture = coordinator::LocalBoxFuture<coordinator::ChildRunOutput<Self::CompletionData>>;
    type ValidateFuture = coordinator::LocalBoxFuture<
        distill_tools::implementations::distill::task::types::SubagentValidateTypeOutcome,
    >;
    type DescribeFuture = coordinator::LocalBoxFuture<
        distill_tools::implementations::distill::task::types::SubagentDescribeOutcome,
    >;
    fn run(&self, mut run: coordinator::ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let this = agent_ref.get();
            let parent_sid = run.request.parent_session_id.clone();
            let root_span = run.request.spawn_root.take_span();
            let root_parent = spawn_pipeline_parent(root_span.as_ref());
            let spawner_session_id = run
                .spawner_session_id
                .clone()
                .filter(|sid| sid != &parent_sid);
            let claim_reporter = run.reporter.clone();
            let ctx = {
                let _region = Region::from_span(tracing::info_span!(
                    parent: root_parent.clone(),
                    "subagent.spawn_context",
                    parent_session_id = %parent_sid,
                    subagent_id = %run.request.id,
                ));
                this.try_build_subagent_spawn_context(&parent_sid)
            };
            let Some(mut ctx) = ctx else {
                if spawner_session_id.is_some() {
                    claim_reporter.drop_spawner_claim();
                }
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    subagent_id = %run.request.id,
                    "Spawn for unknown or evicted parent session"
                );
                return coordinator::ChildRunOutput {
                    result: SubagentResult::failed(
                        run.request.id.clone(),
                        run.request.id,
                        "Parent session not found (evicted or torn down); cannot spawn subagent.",
                    ),
                    completion_data: Default::default(),
                    snapshot_ref: None,
                };
            };
            let parent_handle = this.resident_handle(&acp::SessionId::new(parent_sid.clone()));
            if let Some(handle) = parent_handle {
                let _region = Region::from_span(tracing::info_span!(
                    parent: root_parent.clone(),
                    "subagent.parent_snapshot",
                    parent_session_id = %parent_sid,
                ));
                let (pool, hooks, definitions) = tokio::join!(
                    handle.snapshot_mcp_pool(),
                    handle.snapshot_client_hooks(),
                    handle.snapshot_tool_definitions()
                );
                ctx.parent_mcp_pool = pool;
                ctx.client_hooks = hooks;
                ctx.parent_tool_definitions = (!definitions.is_empty()).then_some(definitions);
            }
            if let Some(spawner) = spawner_session_id.as_deref() {
                if this.is_resident(&acp::SessionId::new(spawner)) {
                    ctx.spawner_address_target = Some(super::SpawnerAddressTarget {
                        session_id: spawner.to_owned(),
                    });
                } else {
                    claim_reporter.drop_spawner_claim();
                }
            }
            let gateway = this.gateway.clone();
            let handle = match crate::agent::subagent::worker_runtime() {
                Ok(handle) => handle,
                Err(err) => {
                    if ctx.spawner_address_target.is_some() {
                        claim_reporter.drop_spawner_claim();
                    }
                    tracing::error!(
                        subagent_id = %run.request.id,
                        error = %err,
                        "subagent worker runtime failed to build"
                    );
                    return coordinator::ChildRunOutput {
                        result: SubagentResult::failed(
                            run.request.id.clone(),
                            run.request.id,
                            format!("Failed to start the subagent worker runtime: {err}"),
                        ),
                        completion_data: Default::default(),
                        snapshot_ref: None,
                    };
                }
            };
            let panic_request = run.request.clone();
            let child_session_id = acp::SessionId::new(run.request.id.clone());
            let turn_number = if run.wake_origin.is_some() {
                None
            } else {
                Some(this.allocate_turn_number(&child_session_id))
            };
            let completion_data =
                ShellCompletionData::from_context(&ctx, run.attempt_id.clone(), turn_number);
            let panic_completion_data = completion_data.clone();
            let task = {
                let _region = Region::from_span(tracing::info_span!(
                    parent: root_parent,
                    "subagent.worker_handoff",
                    parent_session_id = %parent_sid,
                    subagent_id = %run.request.id,
                ));
                handle.spawn(crate::agent::subagent::run_shell_child(
                    run,
                    ctx,
                    completion_data,
                    gateway,
                    root_span,
                ))
            };
            join_worker_task(
                task,
                coordinator::ChildRunOutput {
                    result: SubagentResult::failed(
                        panic_request.id.clone(),
                        panic_request.id,
                        "Subagent runtime panicked",
                    ),
                    completion_data: panic_completion_data,
                    snapshot_ref: None,
                },
            )
            .await
        })
    }
    fn validate_type(
        &self,
        subagent_type: String,
        parent_session_id: String,
    ) -> Self::ValidateFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let this = agent_ref.get();
            let ctx = this.build_subagent_validation_context(&parent_session_id);
            crate::agent::subagent::validate_subagent_type(&subagent_type, &ctx)
        })
    }
    fn describe_type(
        &self,
        subagent_type: String,
        harness_agent_type: Option<String>,
        parent_session_id: String,
    ) -> Self::DescribeFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let this = agent_ref.get();
            match this.try_build_subagent_spawn_context(&parent_session_id) {
                Some(ctx) => crate::agent::subagent::describe_subagent_type(
                    &subagent_type,
                    harness_agent_type.as_deref(),
                    &ctx,
                ),
                None => {
                    tracing::warn!(
                        parent_session_id,
                        subagent_type,
                        "DescribeType for unknown/evicted parent session, replying Unavailable",
                    );
                    distill_tools::implementations::distill::task::types::SubagentDescribeOutcome::Unavailable
                }
            }
        })
    }
    fn supports_wake(&self) -> bool {
        true
    }
    fn supports_agent_message_sender(&self) -> bool {
        true
    }
    fn on_completed(
        &self,
        completion: coordinator::ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        let child_session_id = acp::SessionId::new(completion.request.id.clone());
        self.agent_ref
            .get()
            .release_subagent_turn_number(&child_session_id);
        let gateway = self.agent_ref.get().gateway.clone();
        let will_wake = will_wake_for(&completion);
        let subagent_id = completion.request.id.clone();
        let present = move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                present_child_completion(completion, &gateway, will_wake)
            }))
            .is_err()
            {
                tracing::error!(subagent_id, "subagent completion presentation panicked");
            }
            terminal_published();
        };
        match worker_runtime() {
            Ok(handle) => {
                let task = handle.spawn(async move { present() });
                let mut tasks = self.presentations.borrow_mut();
                tasks.retain(|t| !t.is_finished());
                tasks.push(tokio_util::task::AbortOnDropHandle::new(task));
            }
            Err(_) => present(),
        }
    }
    fn running_count_changed(&self, running: usize) {
        self.agent_ref
            .get()
            .activity
            .subagent_gauge()
            .store(running, std::sync::atomic::Ordering::Relaxed);
    }
    fn persisted_output_ref(&self, completion_data: &Self::CompletionData) -> Option<String> {
        completion_data
            .persisted_output_dir()
            .map(|path| path.to_string_lossy().into_owned())
    }
    fn load_persisted_output(&self, reference: &str) -> Option<std::sync::Arc<str>> {
        crate::agent::subagent::read_subagent_output(std::path::Path::new(reference))
            .map(std::sync::Arc::from)
    }
}
/// Coordinator limit sink; the coordinator cannot link telemetry directly.
fn log_limit_notice(notice: coordinator::SubagentLimitNotice) {
    use coordinator::{LimitedSpawnOrigin, SubagentLimitDecision};
    use distill_telemetry::events::{
        SubagentLimitDisposition, SubagentLimitHit, SubagentOwnerKind,
    };
    let (disposition, limit) = match notice.decision {
        SubagentLimitDecision::QueuedAtConcurrentLimit { limit } => {
            (SubagentLimitDisposition::Queued, limit as u64)
        }
        SubagentLimitDecision::RejectedAtConcurrentLimit { limit } => {
            (SubagentLimitDisposition::Failed, limit as u64)
        }
    };
    distill_telemetry::session_ctx::log_event(SubagentLimitHit::session_concurrent(
        notice.parent_session_id,
        disposition,
        limit,
        u32::try_from(notice.running).unwrap_or(u32::MAX),
        u32::try_from(notice.queue_depth).unwrap_or(u32::MAX),
        match notice.origin {
            LimitedSpawnOrigin::SchedulerLoop => SubagentOwnerKind::SchedulerLoop,
            LimitedSpawnOrigin::Task => SubagentOwnerKind::Task,
        },
    ));
}
/// Wire the shared subagent coordinator actor onto the current `LocalSet`. Builds the `ShellChildRunner`, attaches the limit sink, and `spawn_local`s the `SubagentCoordinator` draining `rx`.
/// Coordinator/runner construction lives here in the boundary module. `MvpAgent::start_subagent_coordinator` owns the parent state (the event receiver and concurrency limits) it feeds in.
pub(crate) fn spawn_subagent_coordinator(
    agent_ref: LocalRef<MvpAgent>,
    rx: coordinator::SubagentCoordinatorReceiver,
    limits: distill_tools::implementations::distill::task::admission::SubagentLimits,
) {
    let runner = ShellChildRunner {
        agent_ref,
        presentations: Default::default(),
    };
    let limit_sink: coordinator::SubagentLimitSink = std::sync::Arc::new(log_limit_notice);
    let config = coordinator::CoordinatorConfig {
        foreground_budget: distill_tools::implementations::distill::task::backend::env_duration_or(
            "GROK_SUBAGENT_AWAIT_BUDGET_MS",
            std::time::Duration::from_secs(600),
        ),
        limits,
        limit_sink: Some(limit_sink),
        buffer_completions: true,
        buffered_completion_output_cap: None,
    };
    tokio::task::spawn_local(
        coordinator::SubagentCoordinator::from_channel(rx, runner, config).run(),
    );
}
/// Whether this completion will inject an auto-wake prompt; decided on the coordinator thread in `on_completed`.
pub(crate) fn will_wake_for(completion: &ChildCompletion<ShellCompletionData>) -> bool {
    should_auto_wake_subagent(AutoWakeInputs::from_completion(completion))
        && completion.disposition.should_surface
}
pub(crate) fn present_child_completion(
    completion: ChildCompletion<ShellCompletionData>,
    gateway: &GatewaySender,
    will_wake: bool,
) {
    let ChildCompletion {
        request,
        result,
        snapshot,
        completion_data,
        disposition: _,
    } = completion;
    if completion_data.has_emitted_spawned_notification() || request.run_in_background {
        emit_subagent_notification(
            gateway,
            &request.parent_session_id,
            SessionUpdate::SubagentFinished {
                subagent_id: request.id.clone(),
                attempt_id: completion_data.attempt_id.as_ref().map(ToString::to_string),
                child_session_id: result.child_session_id.clone(),
                status: result.status().to_owned(),
                error: result.error.clone(),
                tool_calls: result.tool_calls,
                turns: result.turns,
                duration_ms: result.duration_ms,
                tokens_used: completion_data.telemetry_tokens(),
                output: result.success.then(|| result.output.to_string()),
                will_wake,
            },
            completion_data.parent_cmd_tx.as_ref(),
        );
    }
    if will_wake {
        inject_subagent_completed_prompt(InjectParams {
            subagent_id: &request.id,
            result: &result,
            request: &request,
            snapshot: &snapshot,
            parent_cmd_tx: completion_data.parent_cmd_tx.as_ref(),
            task_output_tool_name: &completion_data.task_output_tool_name,
            scheduler_delete_tool_name: completion_data.scheduler_delete_tool_name.as_deref(),
            scheduler_create_tool_name: completion_data.scheduler_create_tool_name.as_deref(),
            synthetic_trace_tx: &completion_data.synthetic_trace_tx,
            goal_loop_active: &completion_data.goal_loop_active,
        });
    }
}
/// Inputs to the auto-wake gate, one field per suppression reason.
#[derive(Clone, Copy)]
pub(crate) struct AutoWakeInputs {
    pub run_in_background: bool,
    pub cancelled: bool,
    pub auto_wake_enabled: bool,
    pub block_waited: bool,
    pub explicitly_killed: bool,
    pub goal_loop_active: bool,
    pub parent_channel_open: bool,
}
impl AutoWakeInputs {
    pub(crate) fn from_completion(completion: &ChildCompletion<ShellCompletionData>) -> Self {
        Self {
            run_in_background: completion.disposition.backgrounded,
            cancelled: completion.result.cancelled,
            auto_wake_enabled: completion.completion_data.auto_wake_enabled,
            block_waited: completion.disposition.waiter_delivered,
            explicitly_killed: completion.disposition.explicitly_killed,
            goal_loop_active: completion
                .completion_data
                .goal_loop_active
                .load(std::sync::atomic::Ordering::Relaxed),
            parent_channel_open: completion
                .completion_data
                .parent_cmd_tx
                .as_ref()
                .is_some_and(|tx| !tx.is_closed()),
        }
    }
}
/// Auto-wake gate. `parent_channel_open` folds the inject's no-channel bail into the decision, so a stamped `will_wake` never promises a wake the inject won't do.
/// `cancelled` never wakes: the Ctrl+C race can background a foreground child moments before its cancel lands. Waking would prompt the model right after the user stopped everything.
pub(crate) fn should_auto_wake_subagent(inputs: AutoWakeInputs) -> bool {
    inputs.run_in_background
        && !inputs.cancelled
        && inputs.auto_wake_enabled
        && !inputs.block_waited
        && !inputs.explicitly_killed
        && !inputs.goal_loop_active
        && inputs.parent_channel_open
}
/// Inputs to [`inject_subagent_completed_prompt`], grouped so the call site names each field (mirrors [`AutoWakeInputs`]).
pub(crate) struct InjectParams<'a> {
    pub subagent_id: &'a str,
    pub result: &'a SubagentResult,
    pub request: &'a SubagentRequest,
    pub snapshot: &'a SubagentSnapshot,
    pub parent_cmd_tx: Option<&'a mpsc::UnboundedSender<SessionCommand>>,
    pub task_output_tool_name: &'a str,
    pub scheduler_delete_tool_name: Option<&'a str>,
    pub scheduler_create_tool_name: Option<&'a str>,
    pub synthetic_trace_tx:
        &'a Option<mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    pub goal_loop_active: &'a std::sync::atomic::AtomicBool,
}
/// Inject the auto-wake synthetic prompt for a completed background subagent.
/// Marks nothing reported: the ledger is written when the wake turn's message commits.
pub(crate) fn inject_subagent_completed_prompt(params: InjectParams) {
    let InjectParams {
        subagent_id,
        result,
        request,
        snapshot,
        parent_cmd_tx,
        task_output_tool_name,
        scheduler_delete_tool_name,
        scheduler_create_tool_name,
        synthetic_trace_tx,
        goal_loop_active,
    } = params;
    if goal_loop_active.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(cmd_tx) = parent_cmd_tx else {
        return;
    };
    let summary = distill_tools::implementations::distill::task::completion_summary(
        request, result, snapshot,
    );
    let message = distill_tools::reminders::task_completion::format_subagent_completion(
        &summary,
        Some(task_output_tool_name),
        scheduler_delete_tool_name,
        scheduler_create_tool_name,
    );
    let wrapped = distill_tools::reminders::wrap_reminder(&message);
    let prompt_id = format!("subagent-completed-{subagent_id}");
    let before_rx = if synthetic_trace_tx.is_some() {
        let (before_tx, before_rx) = tokio::sync::oneshot::channel();
        let _ = cmd_tx.send(SessionCommand::CopyFile {
            respond_to: before_tx,
        });
        Some(before_rx)
    } else {
        None
    };
    let (respond_to, completion_rx) = tokio::sync::oneshot::channel();
    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(wrapped))];
    if cmd_tx
        .send(SessionCommand::Prompt {
            prompt_id: prompt_id.clone(),
            prompt_blocks,
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            artifact_upload_ctx: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            traceparent: None,
            json_schema: None,
            send_now: false,
            admission: None,
            tool_overrides_update: None,
            respond_to,
            prompt_admitted: None,
            persist_ack: None,
            parsed_prompt_tx: None,
        })
        .is_err()
    {
        return;
    }
    if let Some(trace_tx) = synthetic_trace_tx {
        let _ = trace_tx.send(crate::upload::turn::SyntheticTurnTraceRequest {
            session_id: acp::SessionId::new(request.parent_session_id.clone()),
            prompt_id,
            completion_rx,
            before_session_copy_rx: before_rx
                .expect("before_rx set when synthetic_trace_tx is Some"),
        });
    }
}
pub(crate) fn emit_subagent_notification(
    gateway: &GatewaySender,
    parent_session_id: &str,
    update: SessionUpdate,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
) -> bool {
    let mut meta = None;
    crate::util::event_id::ensure_event_id_meta(parent_session_id, &mut meta);
    let notification = SessionNotification {
        session_id: acp::SessionId::new(parent_session_id),
        update,
        meta: meta.map(serde_json::Value::Object),
    };
    let persist_ok = match parent_cmd_tx {
        Some(cmd_tx) => cmd_tx
            .send(SessionCommand::XaiSessionNotification {
                notification: notification.clone(),
            })
            .is_ok(),
        None => true,
    };
    let params = serde_json::to_value(&notification)
        .and_then(|v| serde_json::value::to_raw_value(&v))
        .ok();
    if let Some(params) = params {
        let ext_notification =
            acp::ExtNotification::new("x.ai/session_notification", params.into());
        gateway.forward_fire_and_forget(ext_notification);
        return persist_ok;
    }
    false
}
#[cfg(test)]
mod address_tests {
    use super::emit_subagent_notification;
    use crate::extensions::notification::SessionUpdate;
    use crate::session::SessionCommand;
    use crate::test_support::lsp_runtime::test_gateway_with_receiver;
    use tokio::sync::mpsc;
    fn spawned(agent_address: Option<String>) -> SessionUpdate {
        SessionUpdate::SubagentSpawned {
            attempt_id: None,
            subagent_id: "child-1".into(),
            parent_session_id: "parent".into(),
            parent_prompt_id: None,
            child_session_id: "child-1".into(),
            subagent_type: "explore".into(),
            description: "d".into(),
            effective_context_source: None,
            context_normalized: false,
            capability_mode: None,
            persona: None,
            role: None,
            model: None,
            resumed_from: None,
            workflow_run_id: None,
            agent_address,
        }
    }
    #[tokio::test]
    async fn emit_keeps_typed_address_until_durable_write() {
        let (gateway, mut gateway_rx) = test_gateway_with_receiver();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let ok = emit_subagent_notification(
            &gateway,
            "parent",
            spawned(Some("opaque-address".into())),
            Some(&cmd_tx),
        );
        assert!(ok);
        match cmd_rx.try_recv().expect("persist hop must fire") {
            SessionCommand::XaiSessionNotification { notification } => {
                match &notification.update {
                    SessionUpdate::SubagentSpawned { agent_address, .. } => {
                        assert_eq!(agent_address.as_deref(), Some("opaque-address"));
                    }
                    other => panic!("expected SubagentSpawned, got {other:?}"),
                }
                let durable = notification.to_durable_value().unwrap();
                assert!(
                    durable
                        .get("update")
                        .and_then(|u| u.get("agentAddress"))
                        .is_none()
                );
            }
            _ => panic!("expected XaiSessionNotification"),
        }
        match gateway_rx.try_recv().expect("broadcast must fire") {
            distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                let params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).unwrap();
                assert_eq!(
                    params
                        .get("update")
                        .and_then(|u| u.get("agentAddress"))
                        .and_then(|v| v.as_str()),
                    Some("opaque-address")
                );
            }
            _ => panic!("expected ExtNotification"),
        }
    }
}
