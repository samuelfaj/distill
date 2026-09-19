// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::implementations::distill::task::admission::{LimitBehavior, SubagentLimits};
use crate::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageDelivery, ActiveAgentMessageOperation, ActiveAgentMessageOutcome,
    ActiveAgentMessageRequest, ActiveAgentMessageSource, ActiveMessageTarget, AgentMessageSender,
    SubagentCancelRequest, SubagentClearUsageNotAppliedRequest, SubagentCompletionsRequest,
    SubagentListActiveRequest, SubagentLoopUnitActiveRequest, SubagentMarkUsageNotAppliedRequest,
    SubagentOutstandingReply, SubagentOutstandingRequest, SubagentOwner, SubagentRegistryCounts,
    SubagentRequest, SubagentSnapshotStatus, SubagentWaitPromptDrainedRequest,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AdmissionGate {
    entered: mpsc::UnboundedSender<()>,
    release: tokio::sync::broadcast::Sender<()>,
}

#[derive(Clone)]
struct TestControl {
    cancellation: CancellationToken,
    admission_gate: Option<AdmissionGate>,
    admitted_messages: Option<mpsc::UnboundedSender<(ActiveAgentMessageOperation, String)>>,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(SubagentProgress {
            turn_count: 2,
            tool_call_count: 3,
            tokens_used: 100,
            context_window_tokens: 1_000,
            context_usage_pct: 10,
            tools_used: vec!["read_file".to_owned()],
            error_count: 0,
        })
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        if let Some(admitted_messages) = &self.admitted_messages {
            let operation = delivery.operation();
            let text = delivery.message().text.to_string();
            let admission = delivery
                .commit_admission(|| admitted_messages.send((operation, text)))
                .is_some_and(|result| result.is_ok());
            return Box::pin(std::future::ready(if admission {
                ActiveMessageAdmission::Admitted
            } else {
                ActiveMessageAdmission::Rejected
            }));
        }
        match self.admission_gate.clone() {
            Some(gate) => {
                let mut release = gate.release.subscribe();
                let _ = gate.entered.send(());
                Box::pin(async move {
                    let _ = release.recv().await;
                    ActiveMessageAdmission::Unsupported
                })
            }
            None => Box::pin(std::future::ready(ActiveMessageAdmission::Unsupported)),
        }
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Default)]
struct TestCompletionData;

type WakeRun = (
    String,
    distill_message_delivery_core::AttemptId,
    Option<String>,
    String,
    Option<ActiveAgentMessageSource>,
    Option<String>,
    Option<AgentMessageSender>,
);

#[derive(Clone, Copy, Default)]
pub(in crate::implementations::distill::task::coordinator) struct RunnerBehavior {
    pub(in crate::implementations::distill::task::coordinator) wait_before_start: bool,
    pub(in crate::implementations::distill::task::coordinator) fail_wake_before_start: bool,
    pub(in crate::implementations::distill::task::coordinator) reject_wake_after_deferred_start:
        bool,
    pub(in crate::implementations::distill::task::coordinator) wait_after_cancel: bool,
    pub(in crate::implementations::distill::task::coordinator) hold_terminal_publication: bool,
    pub(in crate::implementations::distill::task::coordinator) hold_failed_wake_teardown: bool,
}

struct TestRunner {
    behavior: RunnerBehavior,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    /// Per-id counterpart of `finish`.
    finish_one: tokio::sync::broadcast::Sender<String>,
    completions: mpsc::UnboundedSender<CompletionDisposition>,
    failed_wake_teardown_ready: mpsc::UnboundedSender<()>,
    terminal_publications: mpsc::UnboundedSender<Box<dyn FnOnce() + Send>>,
    requests: mpsc::UnboundedSender<SubagentRequest>,
    started: mpsc::UnboundedSender<String>,
    resume_sources: mpsc::UnboundedSender<SubagentResumeLookup>,
    queue_waits: mpsc::UnboundedSender<(String, Option<std::time::Duration>, usize)>,
    /// `(id, spawner_session_id)` as handed to the runner.
    advertise_targets: mpsc::UnboundedSender<(String, Option<String>)>,
    wake_runs: mpsc::UnboundedSender<WakeRun>,
    admitted_messages: Option<mpsc::UnboundedSender<(ActiveAgentMessageOperation, String)>>,
    admission_gate: Option<AdmissionGate>,
}

impl ChildRunner for TestRunner {
    type Control = TestControl;
    type RootControl = crate::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = TestCompletionData;
    type RunFuture = SendBoxFuture<ChildRunOutput<TestCompletionData>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let RunnerBehavior {
            wait_before_start,
            fail_wake_before_start,
            reject_wake_after_deferred_start,
            wait_after_cancel,
            hold_failed_wake_teardown,
            ..
        } = self.behavior;
        let mut start = self.start.subscribe();
        let mut finish = self.finish.subscribe();
        let mut finish_one = self.finish_one.subscribe();
        let requests = self.requests.clone();
        let started = self.started.clone();
        let resume_sources = self.resume_sources.clone();
        let queue_waits = self.queue_waits.clone();
        let advertise_targets = self.advertise_targets.clone();
        let wake_runs = self.wake_runs.clone();
        let admitted_messages = self.admitted_messages.clone();
        let admission_gate = self.admission_gate.clone();
        let failed_wake_teardown_ready = self.failed_wake_teardown_ready.clone();
        Box::pin(async move {
            let ChildRunRequest {
                request,
                cancellation,
                reporter,
                attempt_id,
                generation: _,
                agent_message_sender,
                wake_origin,
                queued_for,
                session_running,
                agent_address: _,
                spawner_session_id,
            } = run;
            let (wake_agent_id, wake_message_source, wake_message_id) = match wake_origin {
                Some(origin) => (
                    Some(origin.agent_id),
                    Some(origin.source),
                    Some(origin.message_id),
                ),
                None => (None, None, None),
            };
            let _ = wake_runs.send((
                request.id.clone(),
                attempt_id,
                wake_agent_id.clone(),
                request.prompt.clone(),
                wake_message_source,
                wake_message_id,
                agent_message_sender,
            ));
            if request.id == "identity-resume" {
                let source_id = request
                    .resume_from
                    .clone()
                    .unwrap_or_else(|| "identity-source".to_owned());
                let source = reporter
                    .resume_source(&source_id, &request.parent_session_id)
                    .await;
                let _ = resume_sources.send(source);
            }
            if request.id == "identity-panic" {
                panic!("test panic after identity allocation");
            }
            let _ = queue_waits.send((request.id.clone(), queued_for, session_running));
            let _ = advertise_targets.send((request.id.clone(), spawner_session_id));
            let _ = requests.send(request.clone());
            if fail_wake_before_start && wake_agent_id.is_some() {
                let _ = start.recv().await;
                if hold_failed_wake_teardown {
                    let _ = finish.recv().await;
                }
                return ChildRunOutput {
                    result: SubagentResult {
                        success: false,
                        error: Some("wake setup failed".to_owned()),
                        subagent_id: request.id.clone(),
                        child_session_id: request.id.clone(),
                        ..Default::default()
                    },
                    completion_data: TestCompletionData,
                    snapshot_ref: None,
                };
            }
            if wait_before_start {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        if wait_after_cancel {
                            let _ = finish.recv().await;
                        }
                        return ChildRunOutput {
                            result: cancelled_result(&request),
                            completion_data: TestCompletionData,
                            snapshot_ref: None,
                        };
                    }
                    _ = start.recv() => {}
                }
            }
            let child = StartedChild {
                child_session_id: if request.subagent_type == "divergent-session" {
                    format!("{}-session", request.id)
                } else {
                    request.id.clone()
                },
                persona: None,
                resumed_from: request.resume_from.clone(),
                child_cwd: request.cwd.clone().unwrap_or_default(),
                worktree_path: None,
                effective_model_id: "test-model".to_owned(),
                // Mock definition resolution: this type declares background.
                definition_background: request.subagent_type == "background-default",
                control: TestControl {
                    cancellation: cancellation.clone(),
                    admission_gate: admission_gate.clone(),
                    admitted_messages: if wake_agent_id.is_some() {
                        admitted_messages
                    } else {
                        None
                    },
                },
            };
            let rejects_deferred_start = reject_wake_after_deferred_start
                && wake_agent_id.is_some()
                && request.prompt == "first wake";
            let promoted = if rejects_deferred_start {
                reporter.started_deferred(child).await
            } else {
                reporter.started(child).await
            };
            if !promoted {
                return ChildRunOutput {
                    result: cancelled_result(&request),
                    completion_data: TestCompletionData,
                    snapshot_ref: None,
                };
            }
            if rejects_deferred_start {
                if hold_failed_wake_teardown {
                    let mut finish_after_signal = finish.resubscribe();
                    let _ = reporter.settle_deferred_start(false).await;
                    let _ = failed_wake_teardown_ready.send(());
                    let _ = finish_after_signal.recv().await;
                } else {
                    let _ = reporter.settle_deferred_start(false).await;
                }
                return ChildRunOutput {
                    result: cancelled_result(&request),
                    completion_data: TestCompletionData,
                    snapshot_ref: None,
                };
            }
            let _ = started.send(request.id.clone());
            let result = tokio::select! {
                _ = cancellation.cancelled() => {
                    if wait_after_cancel {
                        let _ = finish.recv().await;
                    }
                    cancelled_result(&request)
                },
                _ = finish.recv() => finished_result(&request),
                _ = finish_one_for(&mut finish_one, &request.id) => finished_result(&request),
            };
            ChildRunOutput {
                result,
                completion_data: TestCompletionData,
                snapshot_ref: None,
            }
        })
    }

    fn validate_type(
        &self,
        _subagent_type: String,
        _parent_session_id: String,
    ) -> Self::ValidateFuture {
        Box::pin(std::future::ready(SubagentValidateTypeOutcome::Ok))
    }

    fn describe_type(
        &self,
        _subagent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        Box::pin(std::future::ready(SubagentDescribeOutcome::Unavailable))
    }

    fn supports_wake(&self) -> bool {
        true
    }

    fn supports_agent_message_sender(&self) -> bool {
        true
    }

    fn on_completed(
        &self,
        completion: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        let _ = self.completions.send(completion.disposition);
        if self.behavior.hold_terminal_publication {
            let _ = self.terminal_publications.send(terminal_published);
        } else {
            terminal_published();
        }
    }
}

fn finished_result(request: &SubagentRequest) -> SubagentResult {
    SubagentResult {
        success: true,
        output: request.prompt.clone().into(),
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        tool_calls: 3,
        turns: 2,
        ..Default::default()
    }
}

/// Parks forever once the gate is closed.
async fn finish_one_for(gate: &mut tokio::sync::broadcast::Receiver<String>, id: &str) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match gate.recv().await {
            Ok(finished) if finished == id => return,
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

fn cancelled_result(request: &SubagentRequest) -> SubagentResult {
    SubagentResult {
        success: false,
        cancelled: true,
        error: Some("cancelled".to_owned()),
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        ..Default::default()
    }
}

pub(in crate::implementations::distill::task::coordinator) fn request(
    id: &str,
    background: bool,
) -> SubagentRequest {
    SubagentRequest {
        id: id.to_owned(),
        prompt: "work".to_owned(),
        description: "test child".to_owned(),
        subagent_type: "explore".to_owned(),
        parent_session_id: "parent".to_owned(),
        parent_prompt_id: Some("prompt".to_owned()),
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: background,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
        spawn_root: Default::default(),
    }
}

pub(in crate::implementations::distill::task::coordinator) struct Harness {
    pub(in crate::implementations::distill::task::coordinator) backend: ChannelBackend,
    pub(in crate::implementations::distill::task::coordinator) start:
        tokio::sync::broadcast::Sender<()>,
    pub(in crate::implementations::distill::task::coordinator) finish:
        tokio::sync::broadcast::Sender<()>,
    pub(in crate::implementations::distill::task::coordinator) finish_one:
        tokio::sync::broadcast::Sender<String>,
    pub(in crate::implementations::distill::task::coordinator) completions:
        mpsc::UnboundedReceiver<CompletionDisposition>,
    pub(in crate::implementations::distill::task::coordinator) failed_wake_teardown_ready:
        mpsc::UnboundedReceiver<()>,
    pub(in crate::implementations::distill::task::coordinator) terminal_publications:
        mpsc::UnboundedReceiver<Box<dyn FnOnce() + Send>>,
    pub(in crate::implementations::distill::task::coordinator) requests:
        mpsc::UnboundedReceiver<SubagentRequest>,
    pub(in crate::implementations::distill::task::coordinator) started:
        mpsc::UnboundedReceiver<String>,
    pub(in crate::implementations::distill::task::coordinator) resume_sources:
        mpsc::UnboundedReceiver<SubagentResumeLookup>,
    pub(in crate::implementations::distill::task::coordinator) queue_waits:
        mpsc::UnboundedReceiver<(String, Option<std::time::Duration>, usize)>,
    pub(in crate::implementations::distill::task::coordinator) advertise_targets:
        mpsc::UnboundedReceiver<(String, Option<String>)>,
    pub(in crate::implementations::distill::task::coordinator) wake_runs:
        mpsc::UnboundedReceiver<WakeRun>,
    pub(in crate::implementations::distill::task::coordinator) admitted_messages:
        mpsc::UnboundedReceiver<(ActiveAgentMessageOperation, String)>,
    pub(in crate::implementations::distill::task::coordinator) actor: tokio::task::JoinHandle<()>,
}

pub(in crate::implementations::distill::task::coordinator) fn harness(
    wait_before_start: bool,
    foreground_budget: std::time::Duration,
) -> Harness {
    harness_with_config(
        wait_before_start,
        CoordinatorConfig {
            foreground_budget,
            ..CoordinatorConfig::default()
        },
    )
}

pub(in crate::implementations::distill::task::coordinator) fn harness_with_config(
    wait_before_start: bool,
    config: CoordinatorConfig,
) -> Harness {
    harness_with_options(
        RunnerBehavior {
            wait_before_start,
            ..Default::default()
        },
        config,
    )
}

pub(in crate::implementations::distill::task::coordinator) fn harness_with_options(
    behavior: RunnerBehavior,
    config: CoordinatorConfig,
) -> Harness {
    let (command_tx, command_rx) = SubagentCoordinator::<TestRunner>::channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (finish_one, _) = tokio::sync::broadcast::channel(4);
    let (completion_tx, completions) = mpsc::unbounded_channel();
    let (failed_wake_teardown_ready_tx, failed_wake_teardown_ready) = mpsc::unbounded_channel();
    let (terminal_publication_tx, terminal_publications) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let (resume_source_tx, resume_sources) = mpsc::unbounded_channel();
    let (queue_wait_tx, queue_waits) = mpsc::unbounded_channel();
    let (advertise_tx, advertise_targets) = mpsc::unbounded_channel();
    let (wake_run_tx, wake_runs) = mpsc::unbounded_channel();
    let (admitted_message_tx, admitted_messages) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        SubagentCoordinator::from_channel(
            command_rx,
            TestRunner {
                behavior,
                start: start.clone(),
                finish: finish.clone(),
                finish_one: finish_one.clone(),
                completions: completion_tx,
                failed_wake_teardown_ready: failed_wake_teardown_ready_tx,
                terminal_publications: terminal_publication_tx,
                requests: request_tx,
                started: started_tx,
                resume_sources: resume_source_tx,
                queue_waits: queue_wait_tx,
                advertise_targets: advertise_tx,
                wake_runs: wake_run_tx,
                admitted_messages: Some(admitted_message_tx),
                admission_gate: None,
            },
            config,
        )
        .run(),
    );
    Harness {
        // Unbound by default so tests can set request.parent_session_id
        // freely (e.g. nested reparent). ParentSession APIs must use
        // `parent_backend` so they stay session-scoped.
        backend: ChannelBackend::from_coordinator(command_tx),
        start,
        finish,
        finish_one,
        completions,
        failed_wake_teardown_ready,
        terminal_publications,
        requests,
        started,
        resume_sources,
        queue_waits,
        advertise_targets,
        wake_runs,
        admitted_messages,
        actor,
    }
}

struct AdmissionHarness {
    harness: Harness,
    admission_entered: mpsc::UnboundedReceiver<()>,
    admission_release: tokio::sync::broadcast::Sender<()>,
}

fn harness_with_admission_gate(
    wait_before_start: bool,
    config: CoordinatorConfig,
) -> AdmissionHarness {
    let (command_tx, command_rx) = SubagentCoordinator::<TestRunner>::channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (finish_one, _) = tokio::sync::broadcast::channel(4);
    let (completion_tx, completions) = mpsc::unbounded_channel();
    let (failed_wake_teardown_ready_tx, failed_wake_teardown_ready) = mpsc::unbounded_channel();
    let (terminal_publication_tx, terminal_publications) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let (resume_source_tx, resume_sources) = mpsc::unbounded_channel();
    let (queue_wait_tx, queue_waits) = mpsc::unbounded_channel();
    let (advertise_tx, advertise_targets) = mpsc::unbounded_channel();
    let (wake_run_tx, wake_runs) = mpsc::unbounded_channel();
    let (admitted_message_tx, admitted_messages) = mpsc::unbounded_channel();
    let (entered_tx, admission_entered) = mpsc::unbounded_channel();
    let (admission_release, _) = tokio::sync::broadcast::channel(4);
    let gate = AdmissionGate {
        entered: entered_tx,
        release: admission_release.clone(),
    };
    let actor = tokio::spawn(
        SubagentCoordinator::from_channel(
            command_rx,
            TestRunner {
                behavior: RunnerBehavior {
                    wait_before_start,
                    ..Default::default()
                },
                start: start.clone(),
                finish: finish.clone(),
                finish_one: finish_one.clone(),
                completions: completion_tx,
                failed_wake_teardown_ready: failed_wake_teardown_ready_tx,
                terminal_publications: terminal_publication_tx,
                requests: request_tx,
                started: started_tx,
                resume_sources: resume_source_tx,
                queue_waits: queue_wait_tx,
                advertise_targets: advertise_tx,
                wake_runs: wake_run_tx,
                admitted_messages: Some(admitted_message_tx),
                admission_gate: Some(gate),
            },
            config,
        )
        .run(),
    );
    AdmissionHarness {
        harness: Harness {
            backend: ChannelBackend::from_coordinator(command_tx),
            start,
            finish,
            finish_one,
            completions,
            failed_wake_teardown_ready,
            terminal_publications,
            requests,
            started,
            resume_sources,
            queue_waits,
            advertise_targets,
            wake_runs,
            admitted_messages,
            actor,
        },
        admission_entered,
        admission_release,
    }
}

/// Session-bound backend for ParentSession cancel / admission on the default
/// test parent (`"parent"`). Required because unbound cancel is rejected.
fn spawn_noting_registration(
    backend: ChannelBackend,
    request: SubagentRequest,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<Result<SubagentResult, distill_tool_runtime::ToolError>>,
) {
    let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        SubagentBackend::spawn(&backend, request, Some(registered_tx)).await
    });
    (registered_rx, handle)
}

pub(in crate::implementations::distill::task::coordinator) fn parent_backend(
    harness: &Harness,
) -> ChannelBackend {
    session_backend(harness, "parent")
}

#[tokio::test]
async fn active_message_on_owned_pending_admits_after_promotion() {
    let mut harness = harness(true, std::time::Duration::from_secs(5));
    let backend = parent_backend(&harness);
    let (registered, _spawn) =
        spawn_noting_registration(backend.clone(), request("starting", true));
    registered.await.expect("background registration");
    harness
        .requests
        .recv()
        .await
        .expect("runner received child");

    let send = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("starting", "update").unwrap(),
                )
                .await
        }
    });
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("starting"));
    assert_eq!(
        send.await.unwrap(),
        ActiveAgentMessageOutcome::Unsupported,
        "promotion must run the active admission path"
    );
    harness.actor.abort();
}

async fn loop_unit_active(backend: &ChannelBackend, task_id: &str) -> bool {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(SubagentEvent::LoopUnitActive(
            SubagentLoopUnitActiveRequest {
                task_id: task_id.to_owned(),
                respond_to,
            },
        ))
        .expect("actor command channel open");
    response_rx.await.expect("loop activity response")
}

async fn outstanding(backend: &ChannelBackend, prompt_id: &str) -> SubagentOutstandingReply {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(SubagentEvent::Outstanding(SubagentOutstandingRequest {
            parent_session_id: "parent".to_owned(),
            prompt_id: prompt_id.to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    response_rx.await.expect("outstanding response")
}

fn wait_prompt_drained(
    backend: &ChannelBackend,
    prompt_id: &str,
) -> oneshot::Receiver<SubagentOutstandingReply> {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(SubagentEvent::WaitPromptDrained(
            SubagentWaitPromptDrainedRequest {
                parent_session_id: "parent".to_owned(),
                prompt_id: prompt_id.to_owned(),
                respond_to,
            },
        ))
        .expect("actor command channel open");
    response_rx
}

#[tokio::test]
async fn background_spawn_acks_when_pending_before_session_start() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let (registered, _spawn) =
        spawn_noting_registration(harness.backend.clone(), request("bg-start", true));
    registered.await.expect("background registration");
    harness.requests.recv().await.expect("runner saw the child");
    let snapshot = harness
        .backend
        .query("bg-start", false, None)
        .await
        .expect("pending child is queryable");
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Initializing
    ));

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg-start"));
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded && disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn foreground_completion_is_delivered_inline() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("inline", false), None).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.foreground_delivered);
    assert!(!disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn foreground_deadline_hands_off_without_stopping_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("slow", false), None).await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let interim = spawn.await.unwrap().unwrap();
    assert!(interim.backgrounded);
    // Interim handoff must not read as a completion (status() contract).
    assert!(!interim.success);
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );
    assert_eq!(
        harness.backend.registry_counts().await,
        SubagentRegistryCounts {
            pending: 0,
            active: 1,
            completed: 0,
            queued: 0,
        }
    );

    let running = harness.backend.query("slow", false, None).await.unwrap();
    assert!(running.is_running());
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn live_blocking_waiter_suppresses_async_surface() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("waited", true), None).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("waited", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    assert!(wait.await.unwrap().unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    let started = spawn.await.unwrap().unwrap();
    assert!(started.success, "{started:?}");
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn timed_out_waiter_does_not_suppress_later_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("timeout", true), None).await }
    });
    tokio::task::yield_now().await;
    let snapshot = harness
        .backend
        .query("timeout", true, Some(1_000))
        .await
        .unwrap();
    assert!(snapshot.is_running());

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    let started = spawn.await.unwrap().unwrap();
    assert!(started.success, "{started:?}");
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn surviving_waiter_suppresses_after_peer_times_out() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("two-waiters", true), None).await }
    });
    tokio::task::yield_now().await;
    let short = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(1_000))
                .await
                .unwrap()
        }
    });
    let long = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(60_000))
                .await
                .unwrap()
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(short.await.unwrap().is_running());

    let _ = harness.finish.send(());
    assert!(long.await.unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    let started = spawn.await.unwrap().unwrap();
    assert!(started.success, "{started:?}");
    harness.actor.abort();
}

#[tokio::test]
async fn dropped_waiter_does_not_suppress_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("dropped-wait", true), None).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("dropped-wait", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    wait.abort();
    let _ = wait.await;

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    let started = spawn.await.unwrap().unwrap();
    assert!(started.success, "{started:?}");
    harness.actor.abort();
}

#[tokio::test]
async fn pending_cancel_delivers_waiter_once() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("pending-cancel", true), None).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("pending-cancel", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        harness.backend.cancel("pending-cancel").await,
        SubagentCancelOutcome::Cancelled
    ));
    let snapshot = wait.await.unwrap().unwrap();
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(disposition.explicitly_killed);
    assert!(!disposition.should_surface);
    let started = spawn.await.unwrap().unwrap();
    assert!(started.cancelled, "{started:?}");
    harness.actor.abort();
}

#[tokio::test]
async fn caller_drop_during_initialization_does_not_drop_owned_run() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("owned", false), None).await }
    });
    tokio::task::yield_now().await;
    spawn.abort();
    let _ = spawn.await;

    let initializing = harness.backend.query("owned", false, None).await.unwrap();
    assert!(matches!(
        initializing.status,
        SubagentSnapshotStatus::Initializing
    ));
    let _ = harness.start.send(());
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(
        disposition.should_surface,
        "dropped foreground receiver becomes handle-only"
    );
    let terminal = harness.backend.query("owned", false, None).await.unwrap();
    assert!(terminal.status.is_terminal());
    harness.actor.abort();
}

#[tokio::test]
async fn a_delivered_foreground_result_empties_the_live_set() {
    let harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("settled", false), None).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["settled".to_owned()],
        "a running turn-blocking child is live"
    );

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["settled".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a live turn-blocking child keeps the drain parked"
    );

    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success && !result.backgrounded);
    assert_eq!(
        drained.await.expect("drain fires when the set empties"),
        SubagentOutstandingReply::default(),
        "finish_child wakes the parked drain exactly when the result is delivered"
    );
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply::default(),
        "a delivered foreground result leaves the live set empty"
    );
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn drain_resolves_when_child_backgrounds_at_deadline() {
    let harness = harness(false, std::time::Duration::from_secs(1));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("slow", false), None).await }
    });
    tokio::task::yield_now().await;

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["slow".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a live turn-blocking child keeps the drain parked"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let interim = spawn.await.unwrap().unwrap();
    assert!(interim.backgrounded);
    assert_eq!(
        drained
            .await
            .expect("deadline handoff wakes the parked drain"),
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        },
        "an auto-backgrounded child leaves the turn-blocking set and is background_live"
    );
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn drain_resolves_when_caller_goes_away() {
    // The foreground deadline (600s) sits far past the shell drain budget (120s), so a parked drain must resolve on the
    // abandonment itself rather than by the clock advancing to that deadline. No command is sent after the caller drops,
    // so only the abandonment wake can trigger the reap that resolves the drain.
    let harness = harness(false, std::time::Duration::from_secs(600));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("abandoned", false), None).await }
    });
    tokio::task::yield_now().await;

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["abandoned".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a live turn-blocking child keeps the drain parked"
    );

    // Abandon the caller: aborting the spawn task drops its result receiver.
    spawn.abort();
    let _ = spawn.await;

    let before = tokio::time::Instant::now();
    assert_eq!(
        drained
            .await
            .expect("caller abandonment wakes the parked drain"),
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        },
        "the parked drain resolves via the abandonment reap, not a completion"
    );
    assert_eq!(
        tokio::time::Instant::now(),
        before,
        "the drain resolves on abandonment, never advancing to the foreground deadline"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn drain_resolves_when_queued_spawn_is_cancelled() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let mut filler = request("filler", true);
    filler.parent_prompt_id = Some("other".to_owned());
    let _filler = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(filler, None).await }
    });
    harness
        .requests
        .recv()
        .await
        .expect("filler occupies the slot");

    let queued = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("queued", false), None).await }
    });
    await_queued(&harness.backend, 1).await;

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["queued".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a queued turn-blocking spawn keeps the drain parked"
    );

    harness.backend.cancel_parent_prompt("prompt").await;
    assert_eq!(
        drained.await.expect("remove_queued wakes the parked drain"),
        SubagentOutstandingReply::default(),
        "a cancelled queued spawn leaves the scope empty"
    );
    let result = queued.await.expect("join").expect("spawn round-trips");
    assert!(
        result.cancelled,
        "the queued spawn was cancelled: {result:?}"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn drain_resolves_when_pending_child_starts_background() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut blocking = request("bg-def", false);
    blocking.subagent_type = "background-default".to_owned();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(blocking, None).await }
    });
    harness.requests.recv().await.expect("bg-def admitted");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["bg-def".to_owned()],
    );

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["bg-def".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a pending turn-blocking child keeps the drain parked"
    );

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg-def"));
    assert_eq!(
        drained
            .await
            .expect("Started handoff wakes the parked drain"),
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        },
        "a definition_background child is background_live once started"
    );

    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success && !result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn drain_waits_for_deferred_terminalization_to_finalize() {
    let AdmissionHarness {
        mut harness,
        mut admission_entered,
        admission_release,
    } = harness_with_admission_gate(
        false,
        CoordinatorConfig {
            foreground_budget: std::time::Duration::from_secs(60),
            ..CoordinatorConfig::default()
        },
    );
    let backend = parent_backend(&harness);
    let spawn = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("settling", false), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("settling"));

    let mut drained = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["settling".to_owned()],
    );
    assert!(
        drained.try_recv().is_err(),
        "a running turn-blocking child keeps the drain parked"
    );

    let send = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("settling", "ping").unwrap(),
                )
                .await
        }
    });
    admission_entered
        .recv()
        .await
        .expect("child admitted the active message");

    let _ = harness.finish.send(());
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["settling".to_owned()],
        "a buffered terminalizing child is still counted live in self.active"
    );
    assert!(
        drained.try_recv().is_err(),
        "the drain stays parked while the child is buffered in terminal_outputs"
    );

    let _ = admission_release.send(());
    assert_eq!(
        drained
            .await
            .expect("deferred terminalization finalization wakes the parked drain"),
        SubagentOutstandingReply::default(),
        "finalized terminalization leaves the scope empty"
    );
    let _ = send.await.expect("active-message task joins");
    let _ = spawn.await.expect("spawn task joins");
    harness.actor.abort();
}

#[tokio::test]
async fn two_parked_drains_on_one_scope_both_fire() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("fanout", false), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("fanout"));

    let mut first = wait_prompt_drained(&harness.backend, "prompt");
    let mut second = wait_prompt_drained(&harness.backend, "prompt");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["fanout".to_owned()],
    );
    assert!(
        first.try_recv().is_err() && second.try_recv().is_err(),
        "a live child keeps both drains parked"
    );

    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    assert_eq!(
        first.await.expect("first waiter fires"),
        SubagentOutstandingReply::default(),
    );
    assert_eq!(
        second.await.expect("second waiter fires"),
        SubagentOutstandingReply::default(),
        "the second waiter on the scope must also fire"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn abandoned_foreground_caller_clears_outstanding() {
    // ParentGone parity: dropping the spawn await must leave Outstanding
    // (turn-freeze) without waiting for the foreground budget.
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("abandoned", false), None).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["abandoned".to_owned()],
        "live foreground child blocks the turn"
    );

    spawn.abort();
    let _ = spawn.await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        },
        "caller-gone foreground is handle-only for Outstanding"
    );
    let running = harness
        .backend
        .query("abandoned", false, None)
        .await
        .unwrap();
    assert!(running.is_running(), "child keeps running after ParentGone");

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn duplicate_subagent_id_is_rejected_without_replacing_live_child() {
    let harness = harness(false, std::time::Duration::from_secs(60));
    let first = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("duplicate", true), None).await }
    });
    tokio::task::yield_now().await;

    let duplicate = harness
        .backend
        .spawn(request("duplicate", false), None)
        .await
        .expect("duplicate rejection is a lifecycle result");
    assert!(!duplicate.success);
    assert!(
        duplicate
            .error
            .as_deref()
            .is_some_and(|error| error.contains("already exists"))
    );

    let running = harness
        .backend
        .query("duplicate", false, None)
        .await
        .expect("original child remains queryable");
    assert!(running.is_running());
    let _ = harness.finish.send(());
    assert!(first.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test]
async fn external_cancel_token_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let request = request("external-cancel", false);
    let cancel_token = request.cancel_token.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request, None).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("external-cancel")
    );

    cancel_token.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), spawn)
        .await
        .expect("external cancellation should finish")
        .unwrap()
        .unwrap();
    assert!(result.cancelled);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.explicitly_killed);
    harness.actor.abort();
}

#[tokio::test]
async fn dropping_coordinator_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let cancellation = CancellationToken::new();
    let mut request = request("owner-drop", true);
    request.cancel_token = cancellation.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request, None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("owner-drop"));

    harness.actor.abort();
    tokio::time::timeout(std::time::Duration::from_secs(1), cancellation.cancelled())
        .await
        .expect("coordinator drop should cancel child");
    assert!(spawn.await.unwrap().is_err());
}

#[tokio::test(start_paused = true)]
async fn await_to_completion_has_no_foreground_deadline() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let mut request = request("await-completion", false);
    request.await_to_completion = true;
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request, None).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("await-completion")
    );

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert!(!spawn.is_finished());
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    assert!(!result.backgrounded);
    harness.actor.abort();
}

// A mintable UUIDv7 id, so only the workflow-owner exclusion withholds the sender.
const WORKFLOW_CHILD_ID: &str = "019b0000-0000-7000-8000-0000000000a1";

#[tokio::test]
async fn workflow_cancel_waits_for_drain_and_hides_owned_children() {
    let mut harness = harness_with_options(
        RunnerBehavior {
            wait_before_start: true,
            wait_after_cancel: true,
            ..Default::default()
        },
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );

    let mut active_request = request(WORKFLOW_CHILD_ID, false);
    active_request.await_to_completion = true;
    active_request.owner = SubagentOwner::workflow("workflow-run");
    let active_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(active_request, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some(WORKFLOW_CHILD_ID)
    );
    assert!(
        harness
            .wake_runs
            .recv()
            .await
            .expect("workflow run")
            .6
            .is_none()
    );
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some(WORKFLOW_CHILD_ID)
    );

    let mut pending_request = request("workflow-pending", false);
    pending_request.await_to_completion = true;
    pending_request.owner = SubagentOwner::workflow("workflow-run");
    let pending_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(pending_request, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("workflow-pending")
    );

    assert!(
        harness
            .backend
            .query(WORKFLOW_CHILD_ID, false, None)
            .await
            .is_none()
    );
    assert!(
        harness
            .backend
            .query("workflow-pending", false, None)
            .await
            .is_none()
    );
    assert!(harness.backend.inspect(WORKFLOW_CHILD_ID).await.is_some());
    assert!(harness.backend.inspect("workflow-pending").await.is_some());
    assert!(harness.backend.list_running("parent").await.is_empty());
    let (list_respond_to, list_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::ListActive(SubagentListActiveRequest {
            parent_session_id: "parent".to_owned(),
            respond_to: list_respond_to,
        }))
        .expect("actor command channel open");
    assert!(list_response_rx.await.unwrap().is_empty());

    let (cancel_respond_to, mut cancel_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Cancel(SubagentCancelRequest {
            parent_session_id: Some("parent".to_owned()),
            target: SubagentCancelTarget::WorkflowRunId("workflow-run".to_owned()),
            respond_to: cancel_respond_to,
        }))
        .expect("actor command channel open");
    assert!(harness.backend.inspect(WORKFLOW_CHILD_ID).await.is_some());
    assert!(matches!(
        cancel_response_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let _ = harness.finish.send(());
    assert!(matches!(
        cancel_response_rx.await.unwrap(),
        SubagentCancelOutcome::Cancelled
    ));
    assert!(active_spawn.await.unwrap().unwrap().cancelled);
    assert!(pending_spawn.await.unwrap().unwrap().cancelled);
    assert!(
        harness
            .backend
            .query(WORKFLOW_CHILD_ID, false, None)
            .await
            .is_none()
    );
    assert!(harness.backend.inspect(WORKFLOW_CHILD_ID).await.is_some());

    let (completions_respond_to, completions_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to: completions_respond_to,
        }))
        .expect("actor command channel open");
    assert!(completions_response_rx.await.unwrap().is_empty());
    harness.actor.abort();
}

/// Spawn an await-to-completion child under `session` and consume its request
/// event, returning the join handle for the in-flight spawn.
async fn spawn_session_child(
    harness: &mut Harness,
    id: &str,
    session: &str,
) -> tokio::task::JoinHandle<Result<SubagentResult, distill_tool_runtime::ToolError>> {
    let mut req = request(id, false);
    req.await_to_completion = true;
    req.parent_session_id = session.to_owned();
    let backend = harness.backend.clone();
    let handle = tokio::spawn(async move { backend.spawn(req, None).await });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some(id)
    );
    handle
}

#[tokio::test]
async fn teardown_session_children_spares_other_sessions() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));

    // Two children under "parent" (one active, one pending) plus one under a
    // different session that must survive.
    let keep = spawn_session_child(&mut harness, "keep-active", "other").await;
    let kill_active = spawn_session_child(&mut harness, "kill-active", "parent").await;

    // Start the children spawned so far; kill-pending subscribes after start, so
    // it never receives it and stays pending.
    let _ = harness.start.send(());
    let mut started = std::collections::HashSet::new();
    started.insert(harness.started.recv().await.unwrap());
    started.insert(harness.started.recv().await.unwrap());
    assert!(started.contains("keep-active") && started.contains("kill-active"));

    let kill_pending = spawn_session_child(&mut harness, "kill-pending", "parent").await;

    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: None,
        })
        .expect("actor command channel open");

    assert!(kill_active.await.unwrap().unwrap().cancelled);
    assert!(kill_pending.await.unwrap().unwrap().cancelled);

    assert!(
        !keep.is_finished(),
        "a different session's child must not be cancelled"
    );
    let _ = harness.finish.send(());
    let keep = keep.await.unwrap().unwrap();
    assert!(keep.success && !keep.cancelled);
    harness.actor.abort();
}

#[tokio::test]
async fn teardown_holds_admission_until_children_drain_then_reopens() {
    // The child stays active after cancellation, so admission remains blocked until teardown finishes.
    let mut harness = harness_with_options(
        RunnerBehavior {
            wait_before_start: true,
            wait_after_cancel: true,
            ..Default::default()
        },
        CoordinatorConfig::default(),
    );
    let child = spawn_session_child(&mut harness, "slow", "parent").await;
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("slow"));

    let (tx, rx) = tokio::sync::oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: Some(tx),
        })
        .expect("actor command channel open");

    // Same channel as TeardownSession: FIFO guarantees the block is set first.
    let mut late = request("late", false);
    late.parent_session_id = "parent".to_owned();
    assert!(
        harness.backend.spawn(late, None).await.unwrap().cancelled,
        "spawn must be refused while the teardown drains"
    );

    // OpenSpawnAdmission must not reopen mid-drain (CancelTurn → next-prompt
    // race during /delete).
    harness
        .backend
        .sender()
        .send(SubagentEvent::OpenSpawnAdmission {
            parent_session_id: "parent".to_owned(),
        })
        .expect("actor command channel open");
    let mut still_blocked = request("still-blocked", false);
    still_blocked.parent_session_id = "parent".to_owned();
    assert!(
        harness
            .backend
            .spawn(still_blocked, None)
            .await
            .unwrap()
            .cancelled,
        "OpenSpawnAdmission must not reopen during teardown drain"
    );

    // Finishing the cancelled child drains the session and resolves the ack.
    let _ = harness.finish.send(());
    assert!(child.await.unwrap().unwrap().cancelled);
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("drain ack")
        .expect("drain channel");

    // A later spawn on the same session is admitted and runs to completion.
    let admitted = tokio::spawn({
        let backend = harness.backend.clone();
        let mut req = request("after-drain", false);
        req.parent_session_id = "parent".to_owned();
        async move { backend.spawn(req, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("after-drain")
    );
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("after-drain"));
    let _ = harness.finish.send(());
    let result = admitted.await.unwrap().unwrap();
    assert!(
        result.success && !result.cancelled,
        "spawn must be admitted once the teardown drain completes: {result:?}"
    );
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn teardown_drain_deadline_reopens_spawns() {
    // A cancelled child that never finishes must not block spawns for the
    // process lifetime: the coordinator's backstop deadline force-reopens.
    let mut harness = harness_with_options(
        RunnerBehavior {
            wait_before_start: true,
            wait_after_cancel: true,
            ..Default::default()
        },
        CoordinatorConfig::default(),
    );
    let child = spawn_session_child(&mut harness, "stuck", "parent").await;
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("stuck"));

    let (tx, rx) = tokio::sync::oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: Some(tx),
        })
        .expect("actor command channel open");

    // Blocked: the cancelled child stays in `active` (finish never fired).
    let mut blocked = request("blocked", false);
    blocked.parent_session_id = "parent".to_owned();
    assert!(
        harness
            .backend
            .spawn(blocked, None)
            .await
            .unwrap()
            .cancelled,
        "spawn must be refused while the teardown drains"
    );

    // Past the backstop: the hold force-clears and the ack resolves even
    // though the child is still stuck.
    tokio::time::advance(TEARDOWN_DRAIN_MAX + std::time::Duration::from_secs(1)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("drain ack after deadline")
        .expect("drain channel");

    // Admission reopened: a later spawn on the same session starts.
    let _admitted = tokio::spawn({
        let backend = harness.backend.clone();
        let mut req = request("after-deadline", false);
        req.parent_session_id = "parent".to_owned();
        async move { backend.spawn(req, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("after-deadline"),
        "spawn must be admitted once the drain deadline force-reopens admission"
    );

    let _ = harness.finish.send(());
    let _ = child.await;
    harness.actor.abort();
}

#[tokio::test]
async fn panic_keeps_request_uuid_as_resume_identity() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let result = harness
        .backend
        .spawn(request("identity-panic", false), None)
        .await
        .unwrap();
    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("Subagent runtime panicked"));

    let mut resume = request("identity-resume", false);
    resume.resume_from = Some("identity-panic".to_owned());
    let resume_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(resume, None).await }
    });
    let SubagentResumeLookup::Completed(source) = harness.resume_sources.recv().await.unwrap()
    else {
        panic!("expected completed resume source")
    };
    assert_eq!(source.subagent_id, "identity-panic");

    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("identity-resume")
    );
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("identity-resume")
    );
    let _ = harness.finish.send(());
    resume_spawn.await.unwrap().unwrap();
    harness.actor.abort();
}

#[tokio::test]
async fn divergent_child_session_id_disables_sender_authority() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let id = uuid::Uuid::now_v7().to_string();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        let mut request = request(&id, false);
        request.subagent_type = "divergent-session".to_owned();
        async move { backend.spawn(request, None).await }
    });
    let run = harness.wake_runs.recv().await.expect("child run");
    let sender = run.6.expect("coordinator minted sender");
    assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
    assert_eq!(
        sender
            .send(
                ActiveAgentMessageRequest::try_from_parts(
                    ActiveMessageTarget::Parent,
                    "hello",
                    ActiveAgentMessageOperation::Steer,
                )
                .unwrap()
            )
            .await,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
    );
    let _ = harness.finish.send(());
    let _ = spawn.await;
    harness.actor.abort();
}

#[tokio::test]
async fn completed_resume_source_uses_request_uuid_as_agent_id() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("identity-source", false), None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("identity-source")
    );
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("identity-source")
    );
    let _ = harness.finish.send(());
    spawn.await.unwrap().unwrap();

    let mut resume = request("identity-resume", false);
    resume.resume_from = Some("identity-source".to_owned());
    let resume_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(resume, None).await }
    });
    let SubagentResumeLookup::Completed(source) = harness.resume_sources.recv().await.unwrap()
    else {
        panic!("expected completed resume source")
    };
    assert_eq!(source.subagent_id, "identity-source");
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("identity-resume")
    );
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("identity-resume")
    );
    let _ = harness.finish.send(());
    resume_spawn.await.unwrap().unwrap();

    harness.actor.abort();
}

#[tokio::test]
async fn teardown_cancels_background_child_without_rebuffering() {
    let mut harness = harness_with_config(
        true,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );

    // A background subagent that outlives its parent is the production case that
    // rebuffers a completion for a later resume of the same session id.
    let mut req = request("bg", true);
    req.parent_session_id = "parent".to_owned();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(req, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("bg")
    );
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg"));

    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: None,
        })
        .expect("actor command channel open");

    // Wait for the cancelled child to finish, then assert it buffered nothing.
    let _ = harness.completions.recv().await;
    let (tx, rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to: tx,
        }))
        .expect("actor command channel open");
    assert!(
        rx.await.unwrap().is_empty(),
        "torn-down background child must not rebuffer a completion"
    );
    let _ = spawn.await;
    harness.actor.abort();
}

#[tokio::test]
async fn teardown_rejects_spawn_from_cancelled_parent() {
    // wait_after_cancel keeps the cancelled parent in `active`, so its late
    // nested Spawn still finds it.
    let mut harness = harness_with_options(
        RunnerBehavior {
            wait_before_start: true,
            wait_after_cancel: true,
            ..Default::default()
        },
        CoordinatorConfig::default(),
    );

    // A parent subagent whose child_session_id is "A".
    let parent = spawn_session_child(&mut harness, "A", "parent").await;
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("A"));

    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: None,
        })
        .expect("actor command channel open");

    // A nested Spawn from the now-cancelled parent (parent_session_id = its
    // child_session_id) must be rejected, not reparented and left running.
    let mut nested = request("B", false);
    nested.await_to_completion = true;
    nested.parent_session_id = "A".to_owned();
    let outcome = harness.backend.spawn(nested, None).await.unwrap();
    assert!(outcome.cancelled && !outcome.success);

    let _ = harness.finish.send(());
    let _ = parent.await;
    harness.actor.abort();
}

#[tokio::test]
async fn usage_events_feed_sorted_outstanding_reply() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut spawns = Vec::new();
    for (id, is_background) in [
        ("z-foreground", false),
        ("a-foreground", false),
        ("background", true),
    ] {
        spawns.push(tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request(id, is_background), None).await }
        }));
        assert_eq!(
            harness
                .requests
                .recv()
                .await
                .as_ref()
                .map(|request| request.id.as_str()),
            Some(id)
        );
    }

    let (foreign_respond_to, foreign_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::MarkUsageNotApplied(
            SubagentMarkUsageNotAppliedRequest {
                parent_session_id: "foreign".to_owned(),
                prompt_id: "prompt".to_owned(),
                respond_to: foreign_respond_to,
            },
        ))
        .expect("actor command channel open");
    foreign_response_rx.await.expect("mark acknowledgement");
    assert!(
        !outstanding(&harness.backend, "prompt")
            .await
            .subagent_usage_not_applied
    );

    let (mark_respond_to, mark_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::MarkUsageNotApplied(
            SubagentMarkUsageNotAppliedRequest {
                parent_session_id: "parent".to_owned(),
                prompt_id: "prompt".to_owned(),
                respond_to: mark_respond_to,
            },
        ))
        .expect("actor command channel open");
    mark_response_rx.await.expect("mark acknowledgement");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: vec!["a-foreground".to_owned(), "z-foreground".to_owned()],
            background_live: true,
            subagent_usage_not_applied: true,
        }
    );

    harness
        .backend
        .sender()
        .send(SubagentEvent::ClearUsageNotApplied(
            SubagentClearUsageNotAppliedRequest {
                parent_session_id: "parent".to_owned(),
                prompt_id: "prompt".to_owned(),
            },
        ))
        .expect("actor command channel open");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: vec!["a-foreground".to_owned(), "z-foreground".to_owned()],
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    assert!(matches!(
        harness.backend.cancel_parent_prompt("prompt").await,
        SubagentCancelOutcome::Cancelled
    ));
    for id in ["z-foreground", "a-foreground", "background"] {
        let snapshot = harness
            .backend
            .query(id, true, Some(5_000))
            .await
            .unwrap_or_else(|| panic!("{id} never finished"));
        assert!(
            matches!(snapshot.status, SubagentSnapshotStatus::Cancelled { .. }),
            "{id} should be cancelled, got {:?}",
            snapshot.status
        );
    }
    for spawn in spawns {
        let result = spawn.await.expect("spawn join").expect("spawn result");
        assert!(
            result.cancelled || !result.success,
            "cancel must yield a terminal failure, got {result:?}"
        );
    }
    harness.actor.abort();
}

/// Prior-turn background + current-turn children all die on ParentSession cancel (GBT-4942).
#[tokio::test]
async fn cancel_parent_session_kills_prior_turn_background() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut prior = request("prior-bg", true);
    prior.parent_prompt_id = Some("turn-1".into());
    let mut current = request("current", false);
    current.parent_prompt_id = Some("turn-2".into());
    let mut spawns = Vec::new();
    for req in [prior, current] {
        let id = req.id.clone();
        spawns.push(tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(req, None).await }
        }));
        assert_eq!(
            harness
                .requests
                .recv()
                .await
                .as_ref()
                .map(|r| r.id.as_str()),
            Some(id.as_str())
        );
        let _ = harness.start.send(());
        assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
    }
    assert!(matches!(
        parent_backend(&harness).cancel_parent_session().await,
        SubagentCancelOutcome::Cancelled
    ));
    for id in ["prior-bg", "current"] {
        let snapshot = harness
            .backend
            .query(id, true, Some(5_000))
            .await
            .unwrap_or_else(|| panic!("{id} never finished"));
        assert!(
            matches!(snapshot.status, SubagentSnapshotStatus::Cancelled { .. }),
            "ParentSession cancel must kill {id}, got {:?}",
            snapshot.status
        );
    }
    for spawn in spawns {
        let result = spawn.await.expect("spawn join").expect("spawn result");
        assert!(
            result.cancelled || !result.success,
            "ParentSession cancel must yield a terminal failure, got {result:?}"
        );
    }
    harness.actor.abort();
}

/// A foreign session's children must not die when this session Stop fires.
#[tokio::test]
async fn cancel_parent_session_does_not_touch_foreign_session() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut foreign = request("foreign-child", true);
    foreign.parent_session_id = "other-session".into();
    let foreign_spawn = tokio::spawn({
        let backend = ChannelBackend::for_coordinator_session(
            harness
                .backend
                .coordinator_sender()
                .expect("coordinator sender"),
            "other-session",
        );
        async move { backend.spawn(foreign, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("foreign-child")
    );
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("foreign-child")
    );

    assert!(matches!(
        parent_backend(&harness).cancel_parent_session().await,
        SubagentCancelOutcome::Cancelled
    ));
    // Foreign child still running — finish it successfully.
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.should_surface);
    let result = foreign_spawn.await.unwrap().unwrap();
    assert!(result.success, "{result:?}");
    harness.actor.abort();
}

/// Unbound backend must not wildcard-cancel (rejects before send).
#[tokio::test]
async fn cancel_parent_session_unbound_backend_is_not_found() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let unbound = ChannelBackend::new(tx);
    assert!(matches!(
        unbound.cancel_parent_session().await,
        SubagentCancelOutcome::NotFound
    ));
}

/// Late Task spawn after ParentSession cancel is rejected until admission reopens.
#[tokio::test]
async fn cancel_parent_session_rejects_late_spawn_until_admission_reopens() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let bound = parent_backend(&harness);
    let prior = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("prior", true), None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("prior")
    );
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("prior"));

    assert!(matches!(
        bound.cancel_parent_session().await,
        SubagentCancelOutcome::Cancelled
    ));
    let prior_snapshot = harness
        .backend
        .query("prior", true, Some(5_000))
        .await
        .expect("prior child finishes");
    assert!(matches!(
        prior_snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));
    let _ = prior.await;

    // Late Task spawn is rejected by the coordinator gate (request still carries
    // parent="parent" via unbound backend + request default).
    let late = harness
        .backend
        .spawn(request("late-after-stop", true), None)
        .await
        .unwrap();
    assert!(
        late.cancelled && !late.success,
        "late Task spawn after ParentSession must be rejected"
    );

    assert!(bound.open_spawn_admission());
    let allowed = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("after-reopen", true), None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("after-reopen")
    );
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("after-reopen")
    );
    let _ = harness.finish.send(());
    assert!(allowed.await.unwrap().unwrap().success);
    harness.actor.abort();
}

/// Nested children of a workflow subagent keep workflow ownership after reparent
/// and survive ParentSession cancel (active + pending).
#[tokio::test]
async fn cancel_parent_session_spares_nested_workflow_children() {
    // wait_before_start only: keep one child in pending through ParentSession.
    // (wait_after_cancel not needed — workflow lineage is not cancelled.)
    let mut harness = harness(true, std::time::Duration::from_secs(60));

    // Workflow-owned parent child (child_session_id = "wf-child").
    let mut wf_parent = request("wf-child", true);
    wf_parent.owner = SubagentOwner::workflow("run-1");
    let wf_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(wf_parent, None).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("wf-child")
    );
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("wf-child"));

    // Nested spawns use a backend bound to the workflow child's session id
    // (production binds ChannelBackend::for_session to the child session).
    let child_backend = ChannelBackend::for_coordinator_session(
        harness
            .backend
            .coordinator_sender()
            .expect("coordinator sender"),
        "wf-child",
    );

    // Nested Task-owned spawn from the workflow child (reparented to root parent).
    let nested_active = request("nested-active", true);
    let nested_active_spawn = tokio::spawn({
        let backend = child_backend.clone();
        async move { backend.spawn(nested_active, None).await }
    });
    let observed = harness
        .requests
        .recv()
        .await
        .expect("nested active observed");
    assert_eq!(observed.parent_session_id, "parent");
    assert_eq!(
        observed.owner.workflow_run_id(),
        Some("run-1"),
        "reparent must copy workflow lineage"
    );
    // Workflow-owned children answer `inspect` but not `query`.
    assert!(child_backend.inspect("nested-active").await.is_some());
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("nested-active")
    );

    // Nested pending (not started yet) under the same workflow child.
    let nested_pending = request("nested-pending", true);
    let nested_pending_spawn = tokio::spawn({
        let backend = child_backend.clone();
        async move { backend.spawn(nested_pending, None).await }
    });
    let observed_pending = harness
        .requests
        .recv()
        .await
        .expect("nested pending observed");
    assert_eq!(observed_pending.owner.workflow_run_id(), Some("run-1"));

    assert!(matches!(
        parent_backend(&harness).cancel_parent_session().await,
        SubagentCancelOutcome::Cancelled
    ));

    // Promote pending → active, then finish all three (must wait until each is
    // subscribed on finish; broadcast does not buffer for late receivers).
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("nested-pending")
    );
    let _ = harness.finish.send(());
    assert!(
        nested_active_spawn.await.unwrap().unwrap().success,
        "active nested workflow child must survive ParentSession"
    );
    assert!(
        nested_pending_spawn.await.unwrap().unwrap().success,
        "pending nested workflow child must survive ParentSession"
    );
    assert!(
        wf_spawn.await.unwrap().unwrap().success,
        "workflow parent must survive ParentSession"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn loop_tracking_covers_pending_active_and_nested_reparenting() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut outer_request = request("outer", true);
    outer_request.runtime_overrides.loop_task_id = Some("loop-task".to_owned());
    let outer_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(outer_request, None).await }
    });
    let observed_outer = harness.requests.recv().await.unwrap();
    assert_eq!(observed_outer.parent_session_id, "parent");
    assert!(loop_unit_active(&harness.backend, "loop-task").await);

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("outer"));
    let refs = harness
        .backend
        .spawned_refs_for_prompt("parent", "prompt")
        .await;
    let [first] = refs.as_slice() else {
        panic!("expected exactly one spawned ref, got {}", refs.len());
    };
    assert_eq!(first.description, "test child");

    let mut nested_request = request("nested", true);
    nested_request.parent_session_id = "outer".to_owned();
    let (nested_registered, _nested_spawn) =
        spawn_noting_registration(harness.backend.clone(), nested_request);
    let observed_nested = harness.requests.recv().await.unwrap();
    assert_eq!(observed_nested.parent_session_id, "parent");
    assert!(!observed_nested.surface_completion);
    assert_eq!(
        observed_nested.runtime_overrides.loop_task_id.as_deref(),
        Some("loop-task")
    );
    assert!(loop_unit_active(&harness.backend, "loop-task").await);
    nested_registered
        .await
        .expect("nested Task registers even after loop_task_id is copied");
    assert!(
        session_backend(&harness, "outer")
            .query("nested", false, None)
            .await
            .is_some(),
        "the spawner reaches its re-keyed child"
    );

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("nested"));
    let _ = harness.finish.send(());
    let outer_result = outer_spawn.await.unwrap().unwrap();
    assert!(outer_result.success && !outer_result.backgrounded);
    let _ = harness.completions.recv().await;
    assert!(!loop_unit_active(&harness.backend, "loop-task").await);
    harness.actor.abort();
}

#[tokio::test]
async fn runner_receives_direct_spawner_as_advertise_target() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let _child_spawn = tokio::spawn({
        let backend = parent_backend(&harness);
        async move { backend.spawn(request("child", true), None).await }
    });
    assert_eq!(
        harness.advertise_targets.recv().await,
        Some(("child".to_owned(), None))
    );
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("child"));

    let child_backend = ChannelBackend::for_coordinator_session(
        harness
            .backend
            .coordinator_sender()
            .expect("coordinator sender"),
        "child",
    );
    let _grandchild_spawn = tokio::spawn({
        let backend = child_backend.clone();
        async move { backend.spawn(request("grandchild", true), None).await }
    });
    assert_eq!(
        harness.advertise_targets.recv().await,
        Some(("grandchild".to_owned(), Some("child".to_owned())))
    );
    harness.actor.abort();
}

fn session_backend(harness: &Harness, session_id: &str) -> ChannelBackend {
    ChannelBackend::for_coordinator_session(
        harness
            .backend
            .coordinator_sender()
            .expect("coordinator sender"),
        session_id,
    )
}

/// Returns the backend the child's own tools would use.
async fn spawn_child(harness: &mut Harness, wait_before_start: bool) -> ChannelBackend {
    spawn_child_with(harness, wait_before_start, request("child", true)).await
}

async fn spawn_child_with(
    harness: &mut Harness,
    wait_before_start: bool,
    child: SubagentRequest,
) -> ChannelBackend {
    let backend = parent_backend(harness);
    // Background: resolves at completion, which no caller of this helper awaits.
    let _child = tokio::spawn(async move { backend.spawn(child, None).await });
    harness.requests.recv().await.expect("child observed");
    if wait_before_start {
        let _ = harness.start.send(());
    }
    assert_eq!(harness.started.recv().await.as_deref(), Some("child"));
    session_backend(harness, "child")
}

async fn spawn_nested(
    harness: &mut Harness,
    backend: &ChannelBackend,
    nested: SubagentRequest,
) -> tokio::task::JoinHandle<Result<SubagentResult, distill_tool_runtime::ToolError>> {
    let join = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(nested, None).await }
    });
    let observed = harness.requests.recv().await.expect("nested observed");
    assert_eq!(observed.parent_session_id, "parent");
    assert!(!observed.surface_completion);
    join
}

#[tokio::test]
async fn nested_spawner_can_query_inspect_and_cancel_background_grandchild() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let child_backend = spawn_child(&mut harness, true).await;
    let grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert!(
        child_backend
            .query("grandchild", false, None)
            .await
            .is_some()
    );
    assert!(child_backend.inspect("grandchild").await.is_some());

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));
    let running = child_backend.query("grandchild", false, None).await;
    assert!(matches!(
        running.map(|snapshot| snapshot.status),
        Some(SubagentSnapshotStatus::Running { .. })
    ));
    assert!(child_backend.inspect("grandchild").await.is_some());
    let root = parent_backend(&harness);
    assert!(root.query("grandchild", false, None).await.is_some());

    assert!(matches!(
        child_backend.cancel("grandchild").await,
        SubagentCancelOutcome::Cancelled
    ));
    assert!(grandchild.await.unwrap().unwrap().cancelled);
    for backend in [&child_backend, &root] {
        let snapshot = backend
            .query("grandchild", false, None)
            .await
            .expect("completed snapshot");
        assert!(matches!(
            snapshot.status,
            SubagentSnapshotStatus::Cancelled { .. }
        ));
    }
    assert!(matches!(
        child_backend.cancel("grandchild").await,
        SubagentCancelOutcome::AlreadyFinished { .. }
    ));
    harness.actor.abort();
}

#[tokio::test]
async fn foreign_session_cannot_reach_nested_grandchild() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let child_backend = spawn_child(&mut harness, false).await;
    let _grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));

    let foreign = session_backend(&harness, "foreign");
    assert!(foreign.query("grandchild", false, None).await.is_none());
    assert!(foreign.inspect("grandchild").await.is_none());
    assert!(matches!(
        foreign.cancel("grandchild").await,
        SubagentCancelOutcome::NotFound
    ));
    assert!(foreign.list_running("foreign").await.is_empty());
    harness.actor.abort();
}

#[tokio::test]
async fn nested_spawner_can_query_and_cancel_queued_grandchild() {
    let (config, mut notices) = limited_with_sink(1, LimitBehavior::Queue);
    let mut harness = harness_with_config(false, config);
    let child_backend = spawn_child(&mut harness, false).await;
    let grandchild = tokio::spawn({
        let backend = child_backend.clone();
        async move { backend.spawn(request("grandchild", true), None).await }
    });
    // Admission is keyed to the root: its one slot is held by the spawner.
    let notice = notices.recv().await.expect("queued notice");
    assert_eq!(
        (notice.parent_session_id.as_str(), notice.running),
        ("parent", 1)
    );
    assert!(matches!(
        notice.decision,
        SubagentLimitDecision::QueuedAtConcurrentLimit { limit: 1 }
    ));
    await_queued(&harness.backend, 1).await;

    let queued = child_backend.query("grandchild", false, None).await;
    assert!(matches!(
        queued.map(|snapshot| snapshot.status),
        Some(SubagentSnapshotStatus::Initializing)
    ));
    assert!(matches!(
        child_backend.cancel("grandchild").await,
        SubagentCancelOutcome::Cancelled
    ));
    assert!(grandchild.await.unwrap().unwrap().cancelled);
    harness.actor.abort();
}

#[tokio::test]
async fn nested_spawner_lists_grandchild_in_list_running_and_list_active() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let child_backend = spawn_child(&mut harness, false).await;
    let _grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));

    let from_child: Vec<_> = child_backend
        .list_running("child")
        .await
        .into_iter()
        .map(|inspection| inspection.snapshot.subagent_id)
        .collect();
    assert_eq!(from_child, ["grandchild"]);
    let mut from_root: Vec<_> = parent_backend(&harness)
        .list_running("parent")
        .await
        .into_iter()
        .map(|inspection| inspection.snapshot.subagent_id)
        .collect();
    from_root.sort();
    assert_eq!(from_root, ["child", "grandchild"]);

    let (respond_to, response) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::ListActive(SubagentListActiveRequest {
            parent_session_id: "child".to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    let active: Vec<_> = response
        .await
        .unwrap()
        .into_iter()
        .map(|summary| summary.subagent_id)
        .collect();
    assert_eq!(active, ["grandchild"]);
    harness.actor.abort();
}

#[tokio::test]
async fn ancestor_chain_reaches_great_grandchild() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let child_backend = spawn_child(&mut harness, false).await;
    let _grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));
    let grandchild_backend = session_backend(&harness, "grandchild");
    let _great = spawn_nested(&mut harness, &grandchild_backend, request("great", true)).await;

    for session in ["child", "grandchild", "parent"] {
        let backend = session_backend(&harness, session);
        assert!(
            backend.query("great", false, None).await.is_some(),
            "{session} must reach its descendant"
        );
    }
    let foreign = session_backend(&harness, "foreign");
    assert!(foreign.query("great", false, None).await.is_none());
    harness.actor.abort();
}

#[tokio::test]
async fn nested_spawner_resume_of_grandchild_is_reparented_and_reachable() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let child_backend = spawn_child(&mut harness, false).await;
    let grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));
    // `finish` is broadcast to every runner and would end the spawner too.
    assert!(matches!(
        child_backend.cancel("grandchild").await,
        SubagentCancelOutcome::Cancelled
    ));
    assert!(grandchild.await.unwrap().unwrap().cancelled);

    let mut resume = request("g2", true);
    resume.resume_from = Some("grandchild".to_owned());
    let _g2 = spawn_nested(&mut harness, &child_backend, resume).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("g2"));
    let inspection = child_backend
        .inspect("g2")
        .await
        .expect("spawner reaches g2");
    assert_eq!(inspection.resumed_from.as_deref(), Some("grandchild"));
    harness.actor.abort();
}

pub(in crate::implementations::distill::task::coordinator) fn buffering() -> CoordinatorConfig {
    CoordinatorConfig {
        buffer_completions: true,
        ..CoordinatorConfig::default()
    }
}

async fn buffered_completion_ids(harness: &Harness, session: Option<&str>) -> Vec<String> {
    completion_ids_via(harness, session, SubagentEvent::Completions).await
}

async fn peeked_completion_ids(harness: &Harness, session: Option<&str>) -> Vec<String> {
    completion_ids_via(harness, session, SubagentEvent::PeekCompletions).await
}

async fn completion_ids_via(
    harness: &Harness,
    session: Option<&str>,
    event: fn(SubagentCompletionsRequest) -> SubagentEvent,
) -> Vec<String> {
    completions_via(harness, session, event)
        .await
        .into_iter()
        .map(|summary| summary.snapshot.subagent_id)
        .collect()
}

pub(in crate::implementations::distill::task::coordinator) async fn buffered_completions(
    harness: &Harness,
    session: Option<&str>,
) -> Vec<crate::implementations::distill::task::types::SubagentCompletionSummary> {
    completions_via(harness, session, SubagentEvent::Completions).await
}

async fn completions_via(
    harness: &Harness,
    session: Option<&str>,
    event: fn(SubagentCompletionsRequest) -> SubagentEvent,
) -> Vec<crate::implementations::distill::task::types::SubagentCompletionSummary> {
    let (respond_to, response) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(event(SubagentCompletionsRequest {
            parent_session_id: session.map(str::to_owned),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    response.await.expect("completions response")
}

async fn running_nested_pair(
    nested: SubagentRequest,
) -> (
    Harness,
    tokio::task::JoinHandle<Result<SubagentResult, distill_tool_runtime::ToolError>>,
) {
    let mut harness = harness_with_config(false, buffering());
    let child_backend = spawn_child(&mut harness, false).await;
    let grandchild = spawn_nested(&mut harness, &child_backend, nested).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));
    (harness, grandchild)
}

#[tokio::test]
async fn nested_grandchild_completion_buffers_for_spawner_not_root() {
    let (mut harness, grandchild) = running_nested_pair(request("grandchild", true)).await;
    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    let disposition = harness.completions.recv().await.expect("grandchild done");
    assert!(!disposition.should_surface, "the root is not woken");
    assert_eq!(
        buffered_completion_ids(&harness, Some("child")).await,
        ["grandchild"]
    );
    assert!(
        buffered_completion_ids(&harness, Some("parent"))
            .await
            .is_empty()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn nested_grandchild_completion_not_buffered_when_spawner_finished_first() {
    let (mut harness, grandchild) = running_nested_pair(request("grandchild", true)).await;
    let _ = harness.finish_one.send("child".to_owned());
    harness.completions.recv().await.expect("child done");
    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    harness.completions.recv().await.expect("grandchild done");
    assert!(
        buffered_completion_ids(&harness, Some("child"))
            .await
            .is_empty()
    );
    // Only the root's own direct child is left anywhere in the buffer.
    assert_eq!(buffered_completion_ids(&harness, None).await, ["child"]);
    harness.actor.abort();
}

#[tokio::test]
async fn spawner_finish_purges_buffered_grandchild_completion() {
    let (mut harness, grandchild) = running_nested_pair(request("grandchild", true)).await;
    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    harness.completions.recv().await.expect("grandchild done");
    let _ = harness.finish_one.send("child".to_owned());
    harness.completions.recv().await.expect("child done");
    assert!(
        buffered_completion_ids(&harness, Some("child"))
            .await
            .is_empty()
    );
    assert_eq!(buffered_completion_ids(&harness, None).await, ["child"]);
    harness.actor.abort();
}

#[tokio::test]
async fn cancelled_spawner_does_not_receive_grandchild_completion() {
    // wait_after_cancel parks the cancelled spawner in `active` until `finish`.
    let mut harness = harness_with_options(
        RunnerBehavior {
            wait_after_cancel: true,
            ..Default::default()
        },
        buffering(),
    );
    let child_backend = spawn_child(&mut harness, false).await;
    let grandchild = spawn_nested(&mut harness, &child_backend, request("grandchild", true)).await;
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));
    assert!(matches!(
        parent_backend(&harness).cancel("child").await,
        SubagentCancelOutcome::Cancelled
    ));

    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    harness.completions.recv().await.expect("grandchild done");
    assert!(
        buffered_completion_ids(&harness, Some("child"))
            .await
            .is_empty()
    );

    let _ = harness.finish.send(());
    harness.completions.recv().await.expect("spawner done");
    harness.actor.abort();
}

#[tokio::test]
async fn harness_internal_nested_spawn_does_not_surface_to_spawner() {
    let mut quiet = request("grandchild", true);
    quiet.surface_completion = false;
    let (mut harness, grandchild) = running_nested_pair(quiet).await;
    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    harness.completions.recv().await.expect("grandchild done");
    assert!(buffered_completion_ids(&harness, None).await.is_empty());
    harness.actor.abort();
}

#[tokio::test]
async fn workflow_nested_grandchild_completion_still_buffers_for_spawner() {
    let mut harness = harness_with_config(false, buffering());
    let mut child = request("child", true);
    child.owner = SubagentOwner::workflow("wf-1");
    let child_backend = spawn_child_with(&mut harness, false, child).await;
    let grandchild = tokio::spawn({
        let backend = child_backend.clone();
        async move { backend.spawn(request("grandchild", true), None).await }
    });
    let observed = harness.requests.recv().await.expect("grandchild observed");
    assert!(
        observed.owner.is_workflow(),
        "reparent inherits workflow ownership"
    );
    assert_eq!(harness.started.recv().await.as_deref(), Some("grandchild"));

    let _ = harness.finish_one.send("grandchild".to_owned());
    assert!(grandchild.await.unwrap().unwrap().success);
    harness.completions.recv().await.expect("grandchild done");
    assert_eq!(
        buffered_completion_ids(&harness, Some("child")).await,
        ["grandchild"]
    );
    assert!(
        buffered_completion_ids(&harness, Some("parent"))
            .await
            .is_empty()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn direct_workflow_child_completion_is_not_buffered() {
    let mut harness = harness_with_config(false, buffering());
    let mut child = request("child", true);
    child.owner = SubagentOwner::workflow("wf-1");
    let _child_backend = spawn_child_with(&mut harness, false, child).await;
    let _ = harness.finish_one.send("child".to_owned());
    harness.completions.recv().await.expect("child done");
    assert!(buffered_completion_ids(&harness, None).await.is_empty());
    harness.actor.abort();
}

#[tokio::test]
async fn completion_buffer_caps_summary_without_mutating_result() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("buffered", true);
    request.prompt = "aéb".to_owned();
    request.runtime_overrides.completion_output_cap = Some(2);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request, None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("buffered"));
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.as_ref(), "aéb");
    let _ = harness.completions.recv().await;
    let snapshot = harness
        .backend
        .query("buffered", false, None)
        .await
        .unwrap();
    let SubagentSnapshotStatus::Completed { output, .. } = snapshot.status else {
        panic!("expected completed snapshot");
    };
    assert_eq!(output, "aéb");

    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    let buffered = response_rx.await.expect("completion response");
    let [first] = buffered.as_slice() else {
        panic!(
            "expected exactly one buffered completion, got {}",
            buffered.len()
        );
    };
    assert_eq!(first.subagent_id(), "buffered");
    assert_eq!(first.output.as_ref(), "a");
    assert_eq!(first.full_output_bytes, 4);
    assert!(matches!(
        &first.snapshot.status,
        SubagentSnapshotStatus::Completed { output, .. } if output.is_empty()
    ));
    harness.actor.abort();
}

/// Regression (review): an agent definition with `background: true` spawned with a BLOCKING tool
/// call (`run_in_background: false`) is background for Outstanding/freeze accounting — not
/// turn-blocking — while the spawn caller still receives the result inline.
#[tokio::test]
async fn definition_background_counts_as_background_for_outstanding() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let mut blocking_request = request("bg-def", false);
    blocking_request.subagent_type = "background-default".to_owned();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(blocking_request, None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg-def"));

    // Started with definition background: live for the child itself but not
    // turn-blocking; the prompt sees it as background work.
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    // The blocking caller still gets the completed result inline.
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    assert!(!result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn buffered_completion_output_cap_bounds_buffered_summary() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            buffered_completion_output_cap: Some(8),
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("capped", true);
    request.prompt = "x".repeat(64);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request, None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("capped"));
    let _ = harness.finish.send(());
    // Spawn result and queryable snapshot keep the full output…
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.len(), 64);
    let _ = harness.completions.recv().await;

    // …only the buffered reminder copy is truncated.
    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    let buffered = response_rx.await.expect("completion response");
    let [first] = buffered.as_slice() else {
        panic!(
            "expected exactly one buffered completion, got {}",
            buffered.len()
        );
    };
    assert_eq!(first.output.len(), 8, "buffered output must be capped");
    assert_eq!(first.full_output_bytes, 64);
    let notice = crate::reminders::task_completion::format_subagent_completion(
        first,
        Some("get_task_output"),
        None,
        None,
    );
    assert!(
        notice.contains(
            "\n[output truncated: 8 of 64 bytes shown]\n\
             Use get_task_output(\"capped\") to see the full output.\n"
        ),
        "{notice}"
    );
    assert_eq!(notice.matches("[output truncated:").count(), 1);
    harness.actor.abort();
}

#[tokio::test]
async fn teardown_session_drops_only_that_sessions_buffer() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request, None).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        let _ = harness.completions.recv().await;
        let _ = spawn.await;
    }

    // Tearing down parent-a discards its buffered completion...
    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent-a".to_owned(),
            respond_to: None,
        })
        .expect("actor command channel open");

    let drain = |parent: &str| {
        let sender = harness.backend.sender();
        let parent = parent.to_owned();
        async move {
            let (respond_to, response_rx) = oneshot::channel();
            sender
                .send(SubagentEvent::Completions(SubagentCompletionsRequest {
                    parent_session_id: Some(parent),
                    suppress_ids: Vec::new(),
                    respond_to,
                }))
                .expect("actor command channel open");
            response_rx.await.expect("completion response")
        }
    };
    assert!(drain("parent-a").await.is_empty());
    // ...while parent-b's completion stays buffered for its own drain.
    let b = drain("parent-b").await;
    let [first] = b.as_slice() else {
        panic!("expected exactly one completion, got {}", b.len());
    };
    assert_eq!(first.subagent_id(), "child-b");
    harness.actor.abort();
}

/// A suppressed id's wake may still be dropped, so its copy must outlive the suppressing request.
#[tokio::test]
async fn suppressed_completion_stays_buffered_for_a_later_drain() {
    let mut harness = harness_with_config(false, buffering());
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("child", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("child"));
    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let _ = spawn.await;

    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: vec!["child".to_owned()],
            respond_to,
        }))
        .expect("actor command channel open");
    assert!(response_rx.await.expect("completion response").is_empty());

    assert_eq!(
        buffered_completion_ids(&harness, Some("parent")).await,
        vec!["child"],
        "the next unsuppressed drain hands the kept copy back"
    );
    assert!(
        buffered_completion_ids(&harness, Some("parent"))
            .await
            .is_empty(),
        "the drain that hands the copy back also consumes it"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn peek_completions_returns_owned_without_draining() {
    let mut harness = harness_with_config(false, buffering());
    for id in ["child-a", "child-b"] {
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request(id, true), None).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        let _ = harness.completions.recv().await;
        let _ = spawn.await;
    }

    let peeked = peeked_completion_ids(&harness, Some("parent")).await;
    assert_eq!(peeked, ["child-a", "child-b"]);
    assert_eq!(
        peeked_completion_ids(&harness, Some("parent")).await,
        peeked,
        "a peek leaves the buffer intact"
    );
    assert_eq!(
        buffered_completion_ids(&harness, Some("parent")).await,
        peeked,
        "the destructive drain still hands every peeked copy back"
    );
    assert!(
        peeked_completion_ids(&harness, Some("parent"))
            .await
            .is_empty()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn peek_is_scoped_to_parent_session() {
    let mut harness = harness_with_config(false, buffering());
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request, None).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        let _ = harness.completions.recv().await;
        let _ = spawn.await;
    }

    assert_eq!(
        peeked_completion_ids(&harness, Some("parent-a")).await,
        ["child-a"]
    );
    assert_eq!(
        peeked_completion_ids(&harness, Some("parent-b")).await,
        ["child-b"]
    );
    assert_eq!(
        buffered_completion_ids(&harness, None).await,
        ["child-a", "child-b"]
    );
    harness.actor.abort();
}

#[tokio::test]
async fn completion_drain_is_scoped_to_parent_session() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request, None).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        let _ = harness.completions.recv().await;
        let started = spawn.await.unwrap().unwrap();
        assert!(started.success, "{started:?}");
    }

    for (parent, expected_id) in [("parent-a", "child-a"), ("parent-b", "child-b")] {
        let (respond_to, response_rx) = oneshot::channel();
        harness
            .backend
            .sender()
            .send(SubagentEvent::Completions(SubagentCompletionsRequest {
                parent_session_id: Some(parent.to_owned()),
                suppress_ids: Vec::new(),
                respond_to,
            }))
            .expect("actor command channel open");
        let completions = response_rx.await.expect("completion response");
        let [first] = completions.as_slice() else {
            panic!("expected exactly one completion, got {}", completions.len());
        };
        assert_eq!(first.subagent_id(), expected_id);
    }
    harness.actor.abort();
}

#[tokio::test]
async fn blocking_query_of_completed_child_returns_immediately() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("already-done", true), None).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let started = spawn.await.unwrap().unwrap();
    assert!(started.success, "{started:?}");

    let started = std::time::Instant::now();
    let snapshot = harness
        .backend
        .query("already-done", true, Some(600_000))
        .await
        .expect("completed child");
    assert!(snapshot.status.is_terminal());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "already-completed query must not burn the 600s cap; elapsed {:?}",
        started.elapsed()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn blocking_query_of_cancelled_child_returns_immediately() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("already-killed", true), None).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("already-killed")
    );
    assert!(matches!(
        harness.backend.cancel("already-killed").await,
        SubagentCancelOutcome::Cancelled
    ));
    assert!(spawn.await.unwrap().unwrap().cancelled);
    let _ = harness.completions.recv().await;

    let started = std::time::Instant::now();
    let snapshot = harness
        .backend
        .query("already-killed", true, Some(600_000))
        .await
        .expect("cancelled child");
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "already-cancelled query must not burn the 600s cap; elapsed {:?}",
        started.elapsed()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn blocking_query_of_unknown_id_returns_immediately() {
    let harness = harness(false, std::time::Duration::from_secs(60));
    let started = std::time::Instant::now();
    let snapshot = harness
        .backend
        .query("never-existed", true, Some(600_000))
        .await;
    assert!(snapshot.is_none());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "not-found query must not burn the 600s cap; elapsed {:?}",
        started.elapsed()
    );
    harness.actor.abort();
}

#[tokio::test]
async fn session_backend_cannot_query_or_cancel_foreign_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("scoped", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("scoped"));

    let foreign = ChannelBackend::for_coordinator_session(
        harness
            .backend
            .coordinator_sender()
            .expect("coordinator sender"),
        "foreign-parent",
    );
    assert!(foreign.query("scoped", false, None).await.is_none());
    assert!(foreign.inspect("scoped").await.is_none());
    assert!(matches!(
        foreign.cancel("scoped").await,
        SubagentCancelOutcome::NotFound
    ));

    assert!(matches!(
        harness.backend.cancel("scoped").await,
        SubagentCancelOutcome::Cancelled
    ));
    let snapshot = harness
        .backend
        .query("scoped", true, Some(5_000))
        .await
        .expect("scoped child finishes");
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));
    let _ = spawn.await;
    let _ = harness.completions.recv().await;
    harness.actor.abort();
}

#[tokio::test]
async fn completed_cache_evicts_oldest_entry_at_cap() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            foreground_budget: std::time::Duration::from_secs(60),
            ..CoordinatorConfig::default()
        },
    );
    for index in 0..=MAX_COMPLETED_ENTRIES {
        let id = format!("cache-{index:04}");
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            let request = request(&id, true);
            async move { backend.spawn(request, None).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
        let _ = harness.finish.send(());
        let _ = harness.completions.recv().await;
        let _ = spawn.await;
    }

    assert!(
        harness
            .backend
            .query("cache-0000", false, None)
            .await
            .is_none()
    );
    assert!(
        harness
            .backend
            .query("cache-0001", false, None)
            .await
            .is_some()
    );
    assert!(
        harness
            .backend
            .query(&format!("cache-{MAX_COMPLETED_ENTRIES:04}"), false, None,)
            .await
            .is_some()
    );
    harness.actor.abort();
}

pub(in crate::implementations::distill::task::coordinator) fn limited(
    max_concurrent: usize,
    behavior: LimitBehavior,
) -> CoordinatorConfig {
    CoordinatorConfig {
        limits: SubagentLimits {
            max_concurrent,
            behavior,
        },
        ..CoordinatorConfig::default()
    }
}

/// `limited`, plus a recording sink so tests can assert the notice payloads.
fn limited_with_sink(
    max_concurrent: usize,
    behavior: LimitBehavior,
) -> (
    CoordinatorConfig,
    mpsc::UnboundedReceiver<SubagentLimitNotice>,
) {
    let (notice_tx, notices) = mpsc::unbounded_channel();
    let config = CoordinatorConfig {
        limit_sink: Some(std::sync::Arc::new(move |notice| {
            let _ = notice_tx.send(notice);
        })),
        ..limited(max_concurrent, behavior)
    };
    (config, notices)
}

pub(in crate::implementations::distill::task::coordinator) async fn await_queued(
    backend: &ChannelBackend,
    queued: usize,
) {
    for _ in 0..400 {
        if backend.registry_counts().await.queued == queued {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("queued count never reached {queued}");
}

#[tokio::test]
async fn spawns_past_the_concurrent_limit_queue_until_a_slot_frees() {
    let mut harness = harness_with_config(false, limited(2, LimitBehavior::Queue));
    let spawns: Vec<_> = (0..4)
        .map(|i| {
            tokio::spawn({
                let backend = harness.backend.clone();
                async move {
                    backend
                        .spawn(request(&format!("wave-{i}"), true), None)
                        .await
                }
            })
        })
        .collect();

    for _ in 0..2 {
        harness.requests.recv().await.expect("child started");
    }
    await_queued(&harness.backend, 2).await;
    assert!(
        harness.requests.try_recv().is_err(),
        "a child started past the concurrent limit"
    );
    for i in 0..4 {
        assert!(
            harness
                .backend
                .query(&format!("wave-{i}"), false, None)
                .await
                .is_some(),
            "a queued or running spawn must stay queryable"
        );
    }

    let _ = harness.finish.send(());
    for _ in 0..2 {
        harness.requests.recv().await.expect("queued child started");
    }
    let _ = harness.finish.send(());
    for _ in 0..4 {
        let _ = harness.completions.recv().await;
    }
    for spawn in spawns {
        let result = spawn.await.expect("join").expect("spawn round-trips");
        assert!(result.success, "terminal spawn result: {result:?}");
    }
    // The launch-time concurrency count never exceeds the limit.
    for _ in 0..4 {
        let (id, _, session_running) = harness.queue_waits.recv().await.expect("ran");
        assert!(
            (1..=2).contains(&session_running),
            "{id} launched with session_running={session_running}, limit is 2"
        );
    }
    harness.actor.abort();
}

#[tokio::test]
async fn fail_mode_rejects_at_the_limit_and_recovers_when_a_slot_frees() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Fail));
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.requests.recv().await.expect("first child started");

    let rejected = harness
        .backend
        .spawn(request("rejected", true), None)
        .await
        .expect("spawn round-trips");
    assert!(!rejected.success);
    assert!(
        rejected
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Concurrent subagent limit reached"),
        "unexpected error: {:?}",
        rejected.error
    );
    // The rejection surfaces like any failed background child and leaves a
    // failed record, so the id the model holds does not vanish.
    let disposition = harness.completions.recv().await.expect("disposition");
    assert!(disposition.should_surface);
    let snapshot = harness
        .backend
        .query("rejected", false, None)
        .await
        .expect("rejected id resolves");
    assert!(
        matches!(snapshot.status, SubagentSnapshotStatus::Failed { .. }),
        "expected a failed record, got {:?}",
        snapshot.status
    );

    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let held = held.await.expect("join").expect("spawn round-trips");
    assert!(held.success, "{held:?}");

    let next = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("next", true), None).await }
    });
    harness
        .requests
        .recv()
        .await
        .expect("spawning succeeds again once a slot frees");
    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let next = next.await.expect("join").expect("spawn round-trips");
    assert!(next.success, "{next:?}");
    harness.actor.abort();
}

#[tokio::test]
async fn limit_notices_report_running_count_queue_depth_and_origin() {
    let (config, mut notices) = limited_with_sink(1, LimitBehavior::Queue);
    let mut harness = harness_with_config(false, config);
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.requests.recv().await.expect("first child started");
    assert!(
        notices.try_recv().is_err(),
        "an admitted spawn must not notify the sink"
    );

    let parked = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("parked", true), None).await }
    });
    await_queued(&harness.backend, 1).await;
    let notice = notices.recv().await.expect("queued notice");
    assert_eq!(notice.parent_session_id, "parent");
    assert_eq!(
        notice.decision,
        SubagentLimitDecision::QueuedAtConcurrentLimit { limit: 1 }
    );
    assert_eq!(
        (notice.running, notice.queue_depth),
        (1, 1),
        "a queued spawn counts itself in queue_depth"
    );
    assert_eq!(notice.origin, LimitedSpawnOrigin::Task);

    let mut loop_request = request("loop-fire", true);
    loop_request.runtime_overrides.loop_task_id = Some("loop-1".to_owned());
    let looped = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(loop_request, None).await }
    });
    await_queued(&harness.backend, 2).await;
    let notice = notices.recv().await.expect("loop-fire notice");
    assert_eq!((notice.running, notice.queue_depth), (1, 2));
    assert_eq!(notice.origin, LimitedSpawnOrigin::SchedulerLoop);

    let _ = harness.finish.send(());
    harness.requests.recv().await.expect("parked started");
    let _ = harness.finish.send(());
    harness.requests.recv().await.expect("loop-fire started");
    let _ = harness.finish.send(());
    for spawn in [held, parked, looped] {
        assert!(spawn.await.expect("join").expect("round-trips").success);
    }
    harness.actor.abort();
}

#[tokio::test]
async fn a_rejected_spawn_notice_excludes_itself_from_queue_depth() {
    let (config, mut notices) = limited_with_sink(1, LimitBehavior::Fail);
    let mut harness = harness_with_config(false, config);
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.requests.recv().await.expect("first child started");

    let rejected = harness
        .backend
        .spawn(request("rejected", true), None)
        .await
        .expect("spawn round-trips");
    assert!(!rejected.success);
    let notice = notices.recv().await.expect("rejected notice");
    assert_eq!(
        notice.decision,
        SubagentLimitDecision::RejectedAtConcurrentLimit { limit: 1 }
    );
    assert_eq!(
        (notice.running, notice.queue_depth),
        (1, 0),
        "a rejected spawn never enters the queue"
    );

    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let held_ack = held.await.expect("join").expect("round-trips");
    assert!(held_ack.success, "{held_ack:?}");
    harness.actor.abort();
}

#[tokio::test]
async fn teardown_purges_a_queued_spawn_without_rebuffering() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..limited(1, LimitBehavior::Queue)
        },
    );
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));
    let parked = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("parked", true), None).await }
    });
    await_queued(&harness.backend, 1).await;

    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: None,
        })
        .expect("actor command channel open");

    for id in ["parked", "held"] {
        let snapshot = harness
            .backend
            .query(id, true, Some(5_000))
            .await
            .unwrap_or_else(|| panic!("{id} never finished after teardown"));
        assert!(
            matches!(snapshot.status, SubagentSnapshotStatus::Cancelled { .. }),
            "teardown must cancel {id}, got {:?}",
            snapshot.status
        );
    }
    let _ = parked.await;
    let _ = held.await;
    // Both completions processed; neither may rebuffer for a later resume
    // of the torn-down session id.
    for _ in 0..2 {
        let _ = harness.completions.recv().await;
    }
    let (tx, rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to: tx,
        }))
        .expect("actor command channel open");
    assert!(
        rx.await.unwrap().is_empty(),
        "a spawn parked at teardown must not rebuffer a completion"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn dropping_the_actor_resolves_queued_callers_without_host_callbacks() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    // Foreground so spawn_reply is still held when the actor drops.
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", false), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));
    let parked = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("parked", false), None).await }
    });
    await_queued(&harness.backend, 1).await;

    harness.actor.abort();
    let parked = parked.await.expect("join").expect("queued caller resolves");
    assert!(
        parked.cancelled,
        "the destructor must resolve a queued caller: {parked:?}"
    );
    assert!(held.await.expect("join").is_err());
    assert!(
        harness.completions.try_recv().is_err(),
        "the destructor must not run host completion callbacks"
    );
}

#[tokio::test]
async fn a_spawn_cancelled_while_queued_never_starts() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.requests.recv().await.expect("first child started");

    let mut queued = request("queued", true);
    let cancel = CancellationToken::new();
    queued.cancel_token = cancel.clone();
    let queued = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(queued, None).await }
    });
    await_queued(&harness.backend, 1).await;

    cancel.cancel();
    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let ack = queued.await.expect("join").expect("spawn round-trips");
    assert!(
        ack.cancelled,
        "queued background spawn must resolve cancelled: {ack:?}"
    );
    let held_ack = held.await.expect("join").expect("spawn round-trips");
    assert!(held_ack.success, "{held_ack:?}");
    assert!(
        harness.requests.try_recv().is_err(),
        "the cancelled spawn reached the runner"
    );
    let snapshot = harness
        .backend
        .query("queued", false, None)
        .await
        .expect("a spawn cancelled while queued leaves a completed record");
    assert!(
        matches!(snapshot.status, SubagentSnapshotStatus::Cancelled { .. }),
        "queued-cancelled id must read as cancelled: {:?}",
        snapshot.status
    );
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn a_queued_spawn_auto_backgrounds_its_caller_at_the_await_budget() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            foreground_budget: std::time::Duration::from_secs(1),
            limits: SubagentLimits {
                max_concurrent: 1,
                behavior: LimitBehavior::Queue,
            },
            ..CoordinatorConfig::default()
        },
    );
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    let parked = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("parked", false), None).await }
    });
    await_queued(&harness.backend, 1).await;

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let interim = parked.await.expect("join").expect("spawn round-trips");
    assert!(
        interim.backgrounded,
        "queued caller must be handed off at the await budget: {interim:?}"
    );
    assert!(
        !interim.success,
        "interim handoff must not read as completed"
    );
    assert_eq!(
        harness.backend.registry_counts().await.queued,
        1,
        "the spawn itself stays queued for a slot"
    );
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    let _ = harness.finish.send(());
    let _ = harness.completions.recv().await;
    let held_ack = held.await.expect("join").expect("spawn round-trips");
    assert!(held_ack.success, "{held_ack:?}");
    // The freed slot starts the parked spawn as a background child.
    assert_eq!(harness.started.recv().await.as_deref(), Some("parked"));
    let _ = harness.finish.send(());
    harness.actor.abort();
}

#[tokio::test]
async fn an_abandoned_queued_caller_stops_turn_blocking() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    // No await budget: only the caller-gone reap can unblock the turn.
    let mut abandoned = request("abandoned", false);
    abandoned.await_to_completion = true;
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(abandoned, None).await }
    });
    await_queued(&harness.backend, 1).await;
    spawn.abort();
    let _ = spawn.await;

    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    let _ = harness.finish.send(());
    assert!(
        held.await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn a_dequeued_spawn_keeps_spending_its_enqueue_await_budget() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            foreground_budget: std::time::Duration::from_secs(2),
            limits: SubagentLimits {
                max_concurrent: 1,
                behavior: LimitBehavior::Queue,
            },
            ..CoordinatorConfig::default()
        },
    );
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    let parked = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("parked", false), None).await }
    });
    await_queued(&harness.backend, 1).await;
    assert!(
        outstanding(&harness.backend, "prompt")
            .await
            .live_ids
            .contains(&"parked".to_owned()),
        "a queued foreground spawn blocks the turn until the budget handoff"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let _ = harness.finish.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("parked"));

    let (id, queued_for, _) = harness.queue_waits.recv().await.expect("held ran");
    assert_eq!((id.as_str(), queued_for), ("held", None));
    let (id, queued_for, _) = harness.queue_waits.recv().await.expect("parked ran");
    assert_eq!(id, "parked");
    let queued_for = queued_for.expect("a dequeued spawn reports its time parked");
    assert!(
        queued_for >= std::time::Duration::from_secs(1)
            && queued_for < std::time::Duration::from_secs(2),
        "queued_for must measure the paused-clock park time, got {queued_for:?}"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        parked.is_finished(),
        "the caller must be handed off at the enqueue-time deadline, \
         not a restarted budget"
    );
    let interim = parked.await.expect("join").expect("spawn round-trips");
    assert!(interim.backgrounded);
    assert!(
        held.await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    let _ = harness.finish.send(());
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn an_out_of_band_token_cancel_resolves_without_other_actor_traffic() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    let mut queued = request("queued", true);
    let cancel = CancellationToken::new();
    queued.cancel_token = cancel.clone();
    let queued = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(queued, None).await }
    });
    await_queued(&harness.backend, 1).await;

    // Send nothing after the cancel: the periodic queue sweep must resolve
    // it alone.
    cancel.cancel();
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        queued.is_finished(),
        "a token-cancelled queued spawn must resolve without other traffic"
    );
    let result = queued.await.expect("join").expect("spawn round-trips");
    assert!(result.cancelled);

    let _ = harness.finish.send(());
    assert!(
        held.await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    harness.actor.abort();
}

#[tokio::test]
async fn a_cancel_command_by_id_resolves_a_queued_spawn() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    let queued = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("queued", true), None).await }
    });
    await_queued(&harness.backend, 1).await;

    let (respond_to, outcome) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Cancel(SubagentCancelRequest {
            parent_session_id: Some("parent".to_owned()),
            target: SubagentCancelTarget::SubagentId("queued".to_owned()),
            respond_to,
        }))
        .expect("actor command channel open");
    assert!(matches!(
        outcome.await.unwrap(),
        SubagentCancelOutcome::Cancelled
    ));
    let result = queued.await.expect("join").expect("spawn round-trips");
    assert!(
        result.cancelled,
        "cancel by id must resolve the spawn caller"
    );
    let snapshot = harness
        .backend
        .query("queued", false, None)
        .await
        .expect("cancelled queued id stays queryable");
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));

    let _ = harness.finish.send(());
    assert!(
        held.await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    harness.actor.abort();
}

#[tokio::test]
async fn a_saturated_session_does_not_block_another_sessions_spawns() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let a_held = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("a-held", true), None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("a-held"));

    let a_queued = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("a-queued", true), None).await }
    });
    await_queued(&harness.backend, 1).await;

    // Session B must start despite sitting behind session A's parked entry.
    let mut for_b = request("b-first", true);
    for_b.parent_session_id = "other".to_owned();
    let b_first = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(for_b, None).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("b-first"));

    let _ = harness.finish.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("a-queued"));
    let _ = harness.finish.send(());
    for spawn in [a_held, a_queued, b_first] {
        assert!(
            spawn
                .await
                .expect("join")
                .expect("spawn round-trips")
                .success
        );
    }
    harness.actor.abort();
}

#[tokio::test]
async fn workflow_spawns_bypass_the_session_concurrent_limit() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Fail));
    let task_child = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("task-child", true), None).await }
    });
    harness.requests.recv().await.expect("task child started");

    let mut workflow = request("wf-child", true);
    workflow.owner = SubagentOwner::workflow("run-1");
    let workflow = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(workflow, None).await }
    });
    harness
        .requests
        .recv()
        .await
        .expect("workflow child started with the session at the concurrent limit");

    let _ = harness.finish.send(());
    assert!(
        task_child
            .await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    assert!(
        workflow
            .await
            .expect("join")
            .expect("spawn round-trips")
            .success
    );
    harness.actor.abort();
}
