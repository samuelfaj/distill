// Modified for Distill by Samuel Fajreldines, 2026.
use std::future::Future;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;
use crate::implementations::distill::task::active_message::SubagentActiveMessageRequest;
use crate::implementations::distill::task::coordinator::graph::NestedSpawner;
use crate::implementations::distill::task::coordinator_state::{
    ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT, InternalEvent, PendingChild,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageRequest, ActiveAgentMessageSource,
    ActiveMessageRoute, ActiveMessageSenderContext, AgentMessageSender, SubagentOwner,
    SubagentResult,
};

const TEST_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

pub(in super::super) struct AdmissionCall {
    pub(in super::super) message_id: String,
    pub(in super::super) late_delivery: ActiveAgentMessageDelivery,
    pub(in crate::implementations::distill::task::coordinator) release:
        oneshot::Sender<ActiveMessageAdmission>,
}

pub(in crate::implementations::distill::task::coordinator) struct TestControl {
    pub(in super::super) admissions: mpsc::UnboundedSender<AdmissionCall>,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(SubagentProgress::default())
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let (release, released) = oneshot::channel();
        let _ = self.admissions.send(AdmissionCall {
            message_id: delivery.message().message_id.clone(),
            late_delivery: delivery.clone(),
            release,
        });
        Box::pin(async move {
            let admission = released
                .await
                .unwrap_or(ActiveMessageAdmission::ChannelClosed);
            if admission == ActiveMessageAdmission::Admitted
                && delivery.commit_admission(|| ()).is_none()
            {
                return ActiveMessageAdmission::Rejected;
            }
            admission
        })
    }

    fn cancel(&self) {}
}

pub(in crate::implementations::distill::task::coordinator) struct TestRunner;

struct PanickingRunner {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    started: mpsc::UnboundedSender<()>,
    panic: std::sync::Arc<tokio::sync::Notify>,
    completions: mpsc::UnboundedSender<SubagentResult>,
    reporters: mpsc::UnboundedSender<ChildReporter<TestControl>>,
}

impl ChildRunner for PanickingRunner {
    type Control = TestControl;
    type RootControl = crate::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let admissions = self.admissions.clone();
        let started = self.started.clone();
        let panic = self.panic.clone();
        let reporters = self.reporters.clone();
        Box::pin(async move {
            let request = run.request;
            let reporter = run.reporter;
            assert!(
                reporter
                    .started(StartedChild {
                        child_session_id: request.id.clone(),
                        persona: None,
                        resumed_from: None,
                        child_cwd: String::new(),
                        worktree_path: None,
                        effective_model_id: "test-model".to_owned(),
                        definition_background: false,
                        control: TestControl { admissions },
                    })
                    .await
            );
            let _ = reporters.send(reporter);
            let _ = started.send(());
            panic.notified().await;
            panic!("runner panic after promotion");
        })
    }

    fn validate_type(&self, _: String, _: String) -> Self::ValidateFuture {
        Box::pin(std::future::pending())
    }

    fn describe_type(&self, _: String, _: Option<String>, _: String) -> Self::DescribeFuture {
        Box::pin(std::future::pending())
    }

    fn supports_wake(&self) -> bool {
        true
    }

    fn on_completed(
        &self,
        completion: ChildCompletion<()>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        terminal_published();
        let _ = self.completions.send(completion.result);
    }
}

impl ChildRunner for TestRunner {
    type Control = TestControl;
    type RootControl = super::super::root_targets::tests::TestRootControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, _: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        Box::pin(std::future::pending())
    }

    fn validate_type(&self, _: String, _: String) -> Self::ValidateFuture {
        Box::pin(std::future::pending())
    }

    fn describe_type(&self, _: String, _: Option<String>, _: String) -> Self::DescribeFuture {
        Box::pin(std::future::pending())
    }

    fn supports_wake(&self) -> bool {
        true
    }

    fn resolve_root(
        &self,
        agent_id: &distill_message_delivery_core::AgentId,
    ) -> Option<Self::RootControl> {
        super::super::root_targets::tests::resolve_root(agent_id)
    }

    fn resolve_root_session(&self, session_id: &str) -> Option<Self::RootControl> {
        super::super::root_targets::tests::resolve_root_session(session_id)
    }

    fn on_completed(&self, _: ChildCompletion<()>, terminal_published: Box<dyn FnOnce() + Send>) {
        terminal_published();
    }
}

pub(in crate::implementations::distill::task::coordinator) type TestCoordinator =
    SubagentCoordinator<TestRunner>;

fn fixture_with_capacity(
    active_message_capacity: usize,
) -> (
    TestCoordinator,
    crate::implementations::distill::task::backend::SubagentCoordinatorSender,
    mpsc::UnboundedSender<AdmissionCall>,
    mpsc::UnboundedReceiver<AdmissionCall>,
) {
    let config = CoordinatorConfig::default();
    let (command_tx, command_rx) =
        SubagentCoordinatorReceiver::with_capacity(active_message_capacity);
    let (admission_tx, admissions) = mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::from_channel(command_rx, TestRunner, config);
    (coordinator, command_tx, admission_tx, admissions)
}

pub(in crate::implementations::distill::task::coordinator) fn fixture() -> (
    TestCoordinator,
    crate::implementations::distill::task::backend::SubagentCoordinatorSender,
    mpsc::UnboundedSender<AdmissionCall>,
    mpsc::UnboundedReceiver<AdmissionCall>,
) {
    fixture_with_capacity(MAX_ACTIVE_MESSAGE_ADMISSIONS)
}

pub(in crate::implementations::distill::task::coordinator) fn insert_child(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
    parent: &str,
) -> String {
    insert_child_with(
        coordinator,
        admissions,
        id,
        parent,
        None,
        SubagentOwner::Task,
    )
}

pub(in crate::implementations::distill::task::coordinator) fn insert_child_with(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
    parent: &str,
    spawner: Option<&str>,
    owner: SubagentOwner,
) -> String {
    let mut request =
        crate::implementations::distill::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.owner = owner;
    request.surface_completion = false;
    let address = crate::implementations::distill::task::types::AgentAddress::mint(
        uuid::Uuid::new_v4().as_u128(),
    );
    let address_text = address.to_string();
    // In this harness a spawner's session id is its child id.
    match spawner {
        Some(spawner) => {
            if !coordinator.graph.is_reachable_from(spawner, parent) {
                coordinator.graph.insert_root_child(spawner, parent);
            }
            coordinator
                .graph
                .insert_nested(
                    id,
                    parent,
                    NestedSpawner {
                        child_id: spawner.to_owned(),
                        session_id: spawner.to_owned(),
                        surface_completion: false,
                    },
                )
                .expect("spawner node inserted");
        }
        None => coordinator.graph.insert_root_child(id, parent),
    }
    coordinator.active.insert(
        id.to_owned(),
        ActiveChild {
            request,
            started_at: std::time::Instant::now(),
            cancellation: CancellationToken::new(),
            spawn_reply: None,
            foreground_deadline: None,
            handle_only: true,
            definition_background: false,
            explicitly_killed: false,
            disposition: Default::default(),
            child_session_id: id.to_owned(),
            persona: None,
            resumed_from: None,
            child_cwd: String::new(),
            worktree_path: None,
            effective_model_id: "test-model".to_owned(),
            attempt_id: distill_message_delivery_core::AttemptId::mint(1),
            generation: ActiveChildGeneration::new(),
            agent_address: Some(address),
            spawner_session_id: None,
            active_messages: ActiveMessageLifecycle::default(),
            deferred_wake: None,
            control: TestControl { admissions },
        },
    );
    address_text
}

pub(in super::super) fn insert_pending(coordinator: &mut TestCoordinator, id: &str, parent: &str) {
    let mut request =
        crate::implementations::distill::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.surface_completion = false;
    coordinator.graph.insert_root_child(id, parent);
    coordinator.pending.insert(
        id.to_owned(),
        PendingChild {
            request,
            started_at: std::time::Instant::now(),
            cancellation: CancellationToken::new(),
            spawn_reply: None,
            foreground_deadline: None,
            handle_only: true,
            explicitly_killed: false,
            disposition: Default::default(),
            launched: true,
            attempt_id: distill_message_delivery_core::AttemptId::mint(1),
            generation: ActiveChildGeneration::new(),
            agent_address: None,
            spawner_session_id: None,
            wake_of: None,
        },
    );
}

pub(in super::super) fn promote_pending(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
) {
    let (respond_to, _response) = oneshot::channel();
    coordinator.handle_internal(InternalEvent::Started {
        subagent_id: id.to_owned(),
        child: StartedChild {
            child_session_id: id.to_owned(),
            persona: None,
            resumed_from: None,
            child_cwd: String::new(),
            worktree_path: None,
            effective_model_id: "test-model".to_owned(),
            definition_background: false,
            control: TestControl { admissions },
        },
        defer_admission: false,
        respond_to,
    });
}

pub(in crate::implementations::distill::task::coordinator) fn begin_send(
    coordinator: &mut TestCoordinator,
    command_tx: &crate::implementations::distill::task::backend::SubagentCoordinatorSender,
    id: &str,
    parent: &str,
) -> oneshot::Receiver<ActiveAgentMessageOutcome> {
    let (respond_to, response_rx) = oneshot::channel();
    let request = SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_new(id, "follow up").unwrap(),
        sender_context: ActiveMessageSenderContext::RootSession {
            session_id: Arc::from(parent),
        },
        respond_to,
    };
    command_tx
        .try_send_active_message(request)
        .expect("active-message ingress open");
    let ingress = coordinator
        .active_message_ingress
        .as_mut()
        .expect("paired active-message ingress")
        .try_recv()
        .expect("active-message command queued");
    coordinator.handle_send_active_message(ingress);
    response_rx
}

async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_WAIT, future)
        .await
        .expect("active-message test wait timed out")
}

pub(in super::super) async fn recv_with_timeout<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    await_with_timeout(receiver.recv())
        .await
        .expect("active-message test channel closed")
}

async fn finish_next_active_message(coordinator: &mut TestCoordinator) {
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("active-message completion stream ended");
    coordinator.finish_active_message(completion);
}

pub(in super::super) async fn release_admission(
    coordinator: &mut TestCoordinator,
    call: AdmissionCall,
    admission: ActiveMessageAdmission,
) {
    call.release
        .send(admission)
        .expect("active-message admission future dropped");
    finish_next_active_message(coordinator).await;
}

pub(in crate::implementations::distill::task::coordinator) async fn response_outcome(
    response: oneshot::Receiver<ActiveAgentMessageOutcome>,
) -> ActiveAgentMessageOutcome {
    await_with_timeout(response)
        .await
        .expect("active-message response dropped")
}

async fn response_outcome_result(response: oneshot::Receiver<SubagentResult>) -> SubagentResult {
    await_with_timeout(response)
        .await
        .expect("spawn result dropped")
}

async fn finalization_outcome(response: oneshot::Receiver<bool>) -> bool {
    await_with_timeout(response)
        .await
        .expect("active-message finalization response dropped")
}

pub(in crate::implementations::distill::task::coordinator) fn finish_child(
    coordinator: &mut TestCoordinator,
    id: &str,
) {
    finish_child_with_result(
        coordinator,
        id,
        SubagentResult {
            success: true,
            subagent_id: id.to_owned(),
            child_session_id: id.to_owned(),
            ..Default::default()
        },
    );
}

fn finish_child_with_result(coordinator: &mut TestCoordinator, id: &str, result: SubagentResult) {
    coordinator.begin_terminalization(
        id,
        ChildRunOutput {
            result,
            completion_data: (),
            snapshot_ref: None,
        },
    );
}

#[tokio::test]
async fn unregistered_granted_child_is_not_active_and_releases_capacity() {
    let (mut coordinator, _command_tx, _admission_tx, mut admissions) = fixture();
    let factory = coordinator.agent_message_sender_factory.as_ref().unwrap();
    let child_id = uuid::Uuid::now_v7().to_string();
    let sender = AgentMessageSender::mint_for_child(Some(factory), &child_id, true)
        .sender
        .unwrap();
    let send = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .send(ActiveAgentMessageRequest::try_new("child", "follow up").unwrap())
                .await
        }
    });
    let ingress = coordinator
        .active_message_ingress
        .as_mut()
        .unwrap()
        .recv()
        .await
        .unwrap();
    coordinator.handle_send_active_message(ingress);
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        send.await.unwrap(),
    );
    assert_eq!(MAX_ACTIVE_MESSAGE_ADMISSIONS, sender.available_permits());
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn two_sequential_admissions_keep_child_open() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    for _ in 0..2 {
        let (respond_to, response) = oneshot::channel();
        let request = SubagentActiveMessageRequest {
            request: ActiveAgentMessageRequest::try_new_with_operation(
                "child",
                "follow up",
                ActiveAgentMessageOperation::Steer,
            )
            .unwrap(),
            sender_context: ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("parent"),
            },
            respond_to,
        };
        command_tx
            .try_send_active_message(request)
            .expect("active-message ingress open");
        let ingress = coordinator
            .active_message_ingress
            .as_mut()
            .expect("paired active-message ingress")
            .try_recv()
            .expect("active-message command queued");
        coordinator.handle_send_active_message(ingress);

        let call = recv_with_timeout(&mut admissions).await;
        let message_id = call.message_id.clone();
        assert_eq!(
            call.late_delivery.operation(),
            ActiveAgentMessageOperation::Steer
        );
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
        assert_eq!(
            ActiveAgentMessageOutcome::Accepted { message_id },
            response_outcome(response).await
        );
    }
}

#[tokio::test]
async fn finalizing_first_and_send_first_are_deterministic() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "closed", "parent");
    let (closed_tx, closed_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("closed".to_owned(), closed_tx);
    assert!(finalization_outcome(closed_rx).await);
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "closed",
            "parent"
        ))
        .await
    );
    assert!(admissions.try_recv().is_err());

    insert_child(&mut coordinator, admission_tx, "held", "parent");
    let admitted = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("held".to_owned(), finalize_tx);
    assert!(finalize_rx.try_recv().is_err());
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "held", "parent")).await
    );
    assert!(admissions.try_recv().is_err());
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert!(matches!(
        response_outcome(admitted).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
    assert!(finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn finalization_waits_for_both_admissions_on_one_child() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "held", "parent");

    let first_response = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let first_call = recv_with_timeout(&mut admissions).await;
    let second_response = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let second_call = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("held".to_owned(), finalize_tx);

    release_admission(
        &mut coordinator,
        first_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(first_response).await
    );
    assert!(finalize_rx.try_recv().is_err());

    release_admission(
        &mut coordinator,
        second_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(second_response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn admission_results_release_lifecycle_accounting() {
    let results = [
        (
            ActiveMessageAdmission::Unsupported,
            ActiveAgentMessageOutcome::Unsupported,
        ),
        (
            ActiveMessageAdmission::ChannelClosed,
            ActiveAgentMessageOutcome::ChannelClosed,
        ),
        (
            ActiveMessageAdmission::Rejected,
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        ),
    ];
    for (admission, expected) in results {
        let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
        insert_child(&mut coordinator, admission_tx, "child", "parent");
        let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
        let call = recv_with_timeout(&mut admissions).await;
        let (finalize_tx, finalize_rx) = oneshot::channel();
        coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);

        release_admission(&mut coordinator, call, admission).await;
        assert_eq!(expected, response_outcome(response).await);
        assert!(finalization_outcome(finalize_rx).await);
    }
}

#[tokio::test]
async fn per_child_admission_cap_rejects_before_invoking_host() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");

    let mut responses = Vec::new();
    let mut calls = Vec::new();
    for _ in 0..MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD {
        responses.push(begin_send(&mut coordinator, &command_tx, "child", "parent"));
        calls.push(recv_with_timeout(&mut admissions).await);
    }

    assert_eq!(
        ActiveAgentMessageOutcome::Saturated {
            max_in_flight: MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
        },
        response_outcome(begin_send(&mut coordinator, &command_tx, "child", "parent")).await
    );
    assert!(admissions.try_recv().is_err());

    for (response, call) in responses.into_iter().zip(calls) {
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(response).await
        );
    }
}

#[tokio::test]
async fn global_ingress_cap_rejects_before_enqueue_and_drains() {
    const CAPACITY: usize = 3;
    let (mut coordinator, command_tx, admission_tx, mut admissions) =
        fixture_with_capacity(CAPACITY);
    for id in ["first", "second", "third", "overflow"] {
        insert_child(&mut coordinator, admission_tx.clone(), id, "parent");
    }

    let mut responses = Vec::new();
    let mut calls = Vec::new();
    for id in ["first", "second", "third"] {
        responses.push(begin_send(&mut coordinator, &command_tx, id, "parent"));
        calls.push(recv_with_timeout(&mut admissions).await);
    }
    let (respond_to, overflow_response) = oneshot::channel();
    assert_eq!(
        Err(CAPACITY),
        command_tx.try_send_active_message(SubagentActiveMessageRequest {
            request: ActiveAgentMessageRequest::try_new("overflow", "follow up").unwrap(),
            sender_context: ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("parent"),
            },
            respond_to,
        })
    );
    assert!(await_with_timeout(overflow_response).await.is_err());
    assert!(admissions.try_recv().is_err());

    for (response, call) in responses.into_iter().zip(calls) {
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(response).await
        );
    }
    let (respond_to, response) = oneshot::channel();
    assert!(
        command_tx
            .try_send_active_message(SubagentActiveMessageRequest {
                request: ActiveAgentMessageRequest::try_new("overflow", "follow up").unwrap(),
                sender_context: ActiveMessageSenderContext::RootSession {
                    session_id: Arc::from("parent"),
                },
                respond_to,
            })
            .is_ok()
    );
    drop(coordinator);
    assert!(await_with_timeout(response).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn admission_deadline_revocation_is_definite_and_unblocks_finalization() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    assert!(finalize_rx.try_recv().is_err());

    tokio::time::advance(ACTIVE_MESSAGE_ADMISSION_TIMEOUT).await;
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
    assert!(held.release.send(ActiveMessageAdmission::Admitted).is_err());
    assert!(held.late_delivery.commit_admission(|| ()).is_none());
    assert_eq!(coordinator.active_messages.len(), 0);
}

#[tokio::test]
async fn cancellation_revocation_is_definite_and_settles_finalization() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();

    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
    assert!(held.release.send(ActiveMessageAdmission::Admitted).is_err());
    assert!(held.late_delivery.commit_admission(|| ()).is_none());
    assert_eq!(coordinator.active_messages.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn claimed_deadline_race_remains_uncertain_and_finalization_unclean() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    held.late_delivery.mark_admission_uncertain();
    let (finalize_tx, finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);

    tokio::time::advance(ACTIVE_MESSAGE_ADMISSION_TIMEOUT).await;
    finish_next_active_message(&mut coordinator).await;

    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    assert!(!finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn unsettled_completion_before_runner_output_marks_terminal_result_failed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    finish_child(&mut coordinator, "child");
    let result = &coordinator
        .completed
        .get("child")
        .expect("completed")
        .result;
    assert!(!result.success);
    assert!(result.cancelled);
}

#[tokio::test]
async fn runner_output_parked_before_unsettled_completion_is_failed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();

    finish_child(&mut coordinator, "child");
    assert!(coordinator.terminal_outputs.contains_key("child"));
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    let result = &coordinator
        .completed
        .get("child")
        .expect("completed")
        .result;
    assert!(!result.success);
    assert!(result.cancelled);
}

#[tokio::test]
async fn runner_panic_parks_until_uncertain_admission_terminalizes_failed() {
    let (command_tx, command_rx) = SubagentCoordinatorReceiver::with_capacity(1);
    let (admission_tx, mut admissions) = mpsc::unbounded_channel();
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let panic = std::sync::Arc::new(tokio::sync::Notify::new());
    let (completion_tx, mut completions) = mpsc::unbounded_channel();
    let (reporter_tx, mut reporters) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        SubagentCoordinator::from_channel(
            command_rx,
            PanickingRunner {
                admissions: admission_tx,
                started: started_tx,
                panic: panic.clone(),
                completions: completion_tx,
                reporters: reporter_tx,
            },
            CoordinatorConfig::default(),
        )
        .run(),
    );
    let backend =
        crate::implementations::distill::task::backend::ChannelBackend::for_coordinator_session(
            command_tx, "parent",
        );
    let mut spawn = tokio::spawn({
        let backend = backend.clone();
        async move {
            let mut request = crate::implementations::distill::task::coordinator::tests::request(
                "panic-child",
                false,
            );
            request.parent_session_id = "parent".to_owned();
            crate::implementations::distill::task::backend::SubagentBackend::spawn(
                &backend, request, None,
            )
            .await
        }
    });
    recv_with_timeout(&mut started).await;
    let reporter = recv_with_timeout(&mut reporters).await;
    let response = tokio::spawn({
        let backend = backend.clone();
        async move {
            crate::implementations::distill::task::backend::SubagentBackend::send_active_message(
                &backend,
                ActiveAgentMessageRequest::try_new("panic-child", "follow up").unwrap(),
            )
            .await
        }
    });
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();

    panic.notify_one();
    let short_wait = std::time::Duration::from_millis(100);
    assert!(
        tokio::time::timeout(short_wait, completions.recv())
            .await
            .is_err()
    );
    assert!(tokio::time::timeout(short_wait, &mut spawn).await.is_err());

    call.release
        .send(ActiveMessageAdmission::Rejected)
        .expect("selected admission future alive");
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        await_with_timeout(response).await.unwrap()
    );
    let result = recv_with_timeout(&mut completions).await;
    assert!(!result.success);
    assert!(result.cancelled);
    assert_eq!(Some("Subagent runtime panicked"), result.error.as_deref());
    let spawned_result = await_with_timeout(&mut spawn).await.unwrap().unwrap();
    assert!(!spawned_result.success);
    assert!(spawned_result.cancelled);
    let SubagentResumeLookup::Completed(source) =
        reporter.resume_source("panic-child", "parent").await
    else {
        panic!("expected completed panic source")
    };
    assert_eq!(source.subagent_id, "panic-child");
    drop(backend);
    actor.abort();
    let _ = await_with_timeout(actor).await;
}

#[tokio::test]
async fn coordinator_terminalization_parks_output_until_admission_settles() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;

    finish_child(&mut coordinator, "child");
    assert!(coordinator.active.contains_key("child"));
    assert!(coordinator.terminal_outputs.contains_key("child"));
    assert!(!coordinator.completed.contains_key("child"));

    release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    assert!(!coordinator.active.contains_key("child"));
    assert!(!coordinator.terminal_outputs.contains_key("child"));
    assert!(coordinator.completed.contains_key("child"));
}

#[tokio::test]
async fn stale_settled_rejection_remains_definite() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "reused", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    finish_child(&mut coordinator, "reused");
    coordinator.completed.remove("reused");
    coordinator.completed_order.clear();
    coordinator.graph.remove("reused");
    insert_child(&mut coordinator, admission_tx, "reused", "parent");

    release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;

    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn stale_admission_and_completed_lookup_preserve_identity_authority() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "reused", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    finish_child(&mut coordinator, "reused");
    coordinator.completed.remove("reused");
    coordinator.completed_order.clear();
    coordinator.graph.remove("reused");
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let late_delivery = call.late_delivery.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    assert!(late_delivery.commit_admission(|| ()).is_none());

    insert_child(&mut coordinator, admission_tx, "completed", "parent");
    finish_child(&mut coordinator, "completed");
    for id in ["completed", "missing"] {
        assert_eq!(
            ActiveAgentMessageOutcome::NotFoundOrNotOwned,
            response_outcome(begin_send(&mut coordinator, &command_tx, id, "foreign")).await
        );
    }
    let mut response = begin_send(&mut coordinator, &command_tx, "completed", "parent");
    assert!(response.try_recv().is_err());
    assert!(coordinator.pending.contains_key("completed"));
}

#[tokio::test]
async fn actor_drop_classifies_channel_closure_from_lease_proof() {
    for (is_claimed, expected) in [
        (false, ActiveAgentMessageOutcome::ChannelClosed),
        (true, ActiveAgentMessageOutcome::AdmissionUncertain),
    ] {
        let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
        insert_child(&mut coordinator, admission_tx, "child", "parent");
        let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
        let held = recv_with_timeout(&mut admissions).await;
        if is_claimed {
            held.late_delivery.mark_admission_uncertain();
        }

        drop(coordinator);
        assert_eq!(expected, response_outcome(response).await);
        assert!(held.late_delivery.commit_admission(|| ()).is_none());
    }
}

#[tokio::test]
async fn dropped_pre_commit_completion_is_channel_closed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.release
        .send(ActiveMessageAdmission::ChannelClosed)
        .expect("admission waiter");
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("completion");
    assert!(completion.is_settled);
    drop(completion);
    assert_eq!(
        ActiveAgentMessageOutcome::ChannelClosed,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn dropped_admitted_completion_is_uncertain_and_finalization_unclean() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.release
        .send(ActiveMessageAdmission::Admitted)
        .expect("admission waiter");
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("completion");
    assert!(completion.is_settled);
    drop(completion);
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    assert!(
        finalize_rx.try_recv().is_err(),
        "lost admitted completion must leave finalization in-flight"
    );
    drop(coordinator);
    assert!(!finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn held_admission_is_independent_per_child() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "first", "parent");
    insert_child(&mut coordinator, admission_tx, "second", "parent");
    let first_response = begin_send(&mut coordinator, &command_tx, "first", "parent");
    let first_call = recv_with_timeout(&mut admissions).await;
    let (first_tx, mut first_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("first".to_owned(), first_tx);
    let (second_tx, second_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("second".to_owned(), second_tx);
    assert!(finalization_outcome(second_rx).await);
    assert!(first_rx.try_recv().is_err());
    release_admission(
        &mut coordinator,
        first_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(first_response).await
    );
    assert!(finalization_outcome(first_rx).await);
}

#[tokio::test]
async fn send_to_owned_pending_waits_until_started_then_admits() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());

    promote_pending(&mut coordinator, admission_tx, "child");
    let call = recv_with_timeout(&mut admissions).await;
    let message_id = call.message_id.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::Accepted { message_id },
        response_outcome(response).await
    );
}

#[tokio::test]
async fn send_to_owned_pending_that_fails_is_not_active() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(admissions.try_recv().is_err());

    finish_child(&mut coordinator, "child");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn send_to_completed_workflow_child_does_not_start_replacement() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    insert_child_with(
        &mut coordinator,
        admission_tx,
        "child",
        "parent",
        None,
        SubagentOwner::workflow("run"),
    );
    finish_child(&mut coordinator, "child");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "child", "parent")).await
    );
    assert!(!coordinator.pending.contains_key("child"));
    assert!(coordinator.completed.contains_key("child"));
}

#[tokio::test]
async fn parent_session_cancel_unblocks_parked_send() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    let spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());

    assert!(matches!(
        coordinator.cancel_parent_session(Some("parent")),
        crate::implementations::distill::task::types::SubagentCancelOutcome::Cancelled
    ));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    let terminal = response_outcome_result(spawn_result).await;
    assert!(
        terminal.cancelled && !terminal.success,
        "session cancel must resolve a terminal result: {terminal:?}"
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn send_to_owned_pending_hits_spawn_ready_backstop() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    tokio::time::advance(ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT).await;
    coordinator.expire_spawn_ready_messages(tokio::time::Instant::now());
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(admissions.try_recv().is_err());
}

fn insert_queued(
    coordinator: &mut TestCoordinator,
    id: &str,
    parent: &str,
) -> oneshot::Receiver<SubagentResult> {
    let mut request =
        crate::implementations::distill::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.surface_completion = false;
    let (result_tx, result_rx) = oneshot::channel();
    coordinator.graph.insert_root_child(id, parent);
    coordinator
        .queued
        .push_back(super::super::queue::QueuedSpawn {
            request: Box::new(request),
            queued_at: tokio::time::Instant::now(),
            caller: super::super::queue::QueuedCaller::Awaiting {
                result_tx,
                deadline: None,
            },
            agent_address: None,
            spawner_session_id: None,
            wake_origin: None,
            wake: None,
        });
    result_rx
}

#[tokio::test]
async fn send_to_owned_queued_child_parks_until_started() {
    replay_owned_queued_child_promotion().await;
}

pub(in super::super) async fn replay_owned_queued_child_promotion() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let mut spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());
    assert!(admissions.try_recv().is_err());

    // Dequeue into pending, then promote — the parked send must admit.
    let queued = coordinator.queued.pop_front().expect("queued child");
    coordinator.start_child(
        *queued.request,
        queued.caller.into_spawn_reply(),
        None,
        super::super::queue::StartOrigin::Dequeued {
            queued_for: std::time::Duration::ZERO,
            deadline: None,
        },
        queued.agent_address,
        queued.spawner_session_id,
        queued.wake_origin,
        queued.wake,
    );
    promote_pending(&mut coordinator, admission_tx, "child");
    let call = recv_with_timeout(&mut admissions).await;
    let message_id = call.message_id.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::Accepted { message_id },
        response_outcome(response).await
    );
    assert!(spawn_result.try_recv().is_err(), "child still running");
}

#[tokio::test]
async fn parked_send_holds_ingress_permit_until_admit() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture_with_capacity(1);
    insert_pending(&mut coordinator, "spawning", "parent");
    let mut parked = begin_send(&mut coordinator, &command_tx, "spawning", "parent");
    assert!(parked.try_recv().is_err());

    // The parked send reserved the only ingress slot, so another enqueue is saturated.
    let (respond_to, _saturated) = oneshot::channel();
    assert_eq!(
        command_tx.try_send_active_message(SubagentActiveMessageRequest {
            request: ActiveAgentMessageRequest::try_new("holder", "follow up").unwrap(),
            sender_context: ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("parent"),
            },
            respond_to,
        }),
        Err(1),
    );

    promote_pending(&mut coordinator, admission_tx, "spawning");
    let call = recv_with_timeout(&mut admissions).await;
    let message_id = call.message_id.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::Accepted { message_id },
        response_outcome(parked).await
    );
}

#[tokio::test]
async fn send_to_cancelled_or_workflow_spawning_child_fails_fast() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "cancelled", "parent");
    coordinator
        .pending
        .get("cancelled")
        .expect("pending")
        .cancellation
        .cancel();
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "cancelled",
            "parent",
        ))
        .await
    );

    insert_pending(&mut coordinator, "wf", "parent");
    coordinator
        .pending
        .get_mut("wf")
        .expect("pending")
        .request
        .owner = crate::implementations::distill::task::types::SubagentOwner::workflow("run-1");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "wf", "parent")).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn cancel_parent_prompt_rejects_parked_send() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    let spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    coordinator.cancel_parent_prompt("prompt", Some("parent"));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    let terminal = response_outcome_result(spawn_result).await;
    assert!(
        terminal.cancelled && !terminal.success,
        "queued cancel must resolve a terminal result: {terminal:?}"
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn cancel_workflow_run_rejects_parked_send() {
    let (mut coordinator, _command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    coordinator
        .pending
        .get_mut("child")
        .expect("pending")
        .request
        .owner = crate::implementations::distill::task::types::SubagentOwner::workflow("run-1");
    let (tx, rx) = oneshot::channel();
    coordinator
        .spawn_ready
        .push(super::ParkedSpawnReadyMessage {
            subagent_id: "child".to_owned(),
            sender_context: ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("parent"),
            },
            route: ActiveMessageRoute::ParentToOwnedDescendant,
            request: ActiveAgentMessageRequest::try_new("child", "hello").unwrap(),
            respond_to: Some(tx),
            deadline: Some(tokio::time::Instant::now() + ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT),
            initial_message_id: None,
            quota_admission: None,
            // Cancel replies without admitting, so no ingress permit is held.
            permit: None,
        });
    coordinator.cancel_workflow_children("run-1", Some("parent"));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(rx).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn cancel_pending_rejects_parked_human_as_terminal() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    let address = crate::implementations::distill::task::types::AgentAddress::mint(
        uuid::Uuid::new_v4().as_u128(),
    );
    let address_text = address.to_string();
    insert_pending(&mut coordinator, "child", "parent");
    coordinator
        .pending
        .get_mut("child")
        .expect("pending")
        .agent_address = Some(address);
    let mut response = begin_human_send(&mut coordinator, &command_tx, &address_text, "parent");
    assert!(
        response.try_recv().is_err(),
        "human send parks while starting"
    );
    assert!(matches!(
        coordinator.cancel_one("child", Some("parent"), true),
        crate::implementations::distill::task::types::SubagentCancelOutcome::Cancelled
    ));
    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(response).await
    );
    assert!(admissions.try_recv().is_err());
}

pub(in crate::implementations::distill::task::coordinator) fn begin_human_send(
    coordinator: &mut TestCoordinator,
    command_tx: &crate::implementations::distill::task::backend::SubagentCoordinatorSender,
    address: &str,
    parent: &str,
) -> oneshot::Receiver<ActiveAgentMessageOutcome> {
    let (respond_to, response_rx) = oneshot::channel();
    let request = SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_new_from_human(
            address,
            "follow up",
            ActiveAgentMessageOperation::Steer,
        )
        .unwrap(),
        sender_context: ActiveMessageSenderContext::HumanRoot {
            session_id: Arc::from(parent),
        },
        respond_to,
    };
    command_tx
        .try_send_active_message(request)
        .expect("active-message ingress open");
    let ingress = coordinator
        .active_message_ingress
        .as_mut()
        .expect("paired active-message ingress")
        .try_recv()
        .expect("active-message command queued");
    coordinator.handle_send_active_message(ingress);
    response_rx
}

#[tokio::test]
async fn human_address_accepts_owned_child() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let address = insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_human_send(&mut coordinator, &command_tx, &address, "parent");
    let call = recv_with_timeout(&mut admissions).await;
    assert_eq!(call.late_delivery.source(), ActiveAgentMessageSource::Human);
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert!(matches!(
        response_outcome(response).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
}

#[tokio::test]
async fn agent_cancelled_or_workflow_child_stays_not_active() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    insert_child(
        &mut coordinator,
        admission_tx.clone(),
        "cancelled",
        "parent",
    );
    coordinator
        .active
        .get_mut("cancelled")
        .expect("cancelled child")
        .cancellation
        .cancel();
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "cancelled",
            "parent"
        ))
        .await
    );

    insert_child_with(
        &mut coordinator,
        admission_tx,
        "workflow",
        "parent",
        None,
        SubagentOwner::workflow("run-1"),
    );
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "workflow",
            "parent"
        ))
        .await
    );
}

#[tokio::test]
async fn human_pending_address_is_not_active() {
    let (mut coordinator, command_tx, _admission_tx, _admissions) = fixture();
    let address = crate::implementations::distill::task::types::AgentAddress::mint(
        uuid::Uuid::new_v4().as_u128(),
    );
    let address_text = address.to_string();
    let mut request =
        crate::implementations::distill::task::coordinator::tests::request("child", true);
    request.parent_session_id = "parent".to_owned();
    coordinator.graph.insert_root_child("child", "parent");
    coordinator.pending.insert(
        "child".to_owned(),
        crate::implementations::distill::task::coordinator_state::PendingChild {
            request,
            started_at: std::time::Instant::now(),
            cancellation: CancellationToken::new(),
            spawn_reply: None,
            foreground_deadline: None,
            handle_only: true,
            explicitly_killed: false,
            disposition: Default::default(),
            launched: true,
            attempt_id: distill_message_delivery_core::AttemptId::mint(1),
            generation: ActiveChildGeneration::new(),
            agent_address: Some(address),
            spawner_session_id: None,
            wake_of: None,
        },
    );
    let mut response = begin_human_send(&mut coordinator, &command_tx, &address_text, "parent");
    assert!(
        response.try_recv().is_err(),
        "owned starting address parks until the child is active"
    );
    coordinator.expire_spawn_ready_messages(
        tokio::time::Instant::now() + ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT,
    );
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn human_finalizing_address_is_rejected() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    let address = insert_child(&mut coordinator, admission_tx, "child", "parent");
    let (finalizing_tx, _finalizing_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalizing_tx);
    let response = begin_human_send(&mut coordinator, &command_tx, &address, "parent");
    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn human_cancelled_address_is_rejected() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    let address = insert_child(&mut coordinator, admission_tx, "child", "parent");
    coordinator
        .active
        .get("child")
        .unwrap()
        .cancellation
        .cancel();
    let response = begin_human_send(&mut coordinator, &command_tx, &address, "parent");
    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn human_reparented_nested_child_is_addressable_from_spawner_and_root() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let address = insert_child_with(
        &mut coordinator,
        admission_tx,
        "nested",
        "root",
        Some("spawner"),
        SubagentOwner::Task,
    );
    let from_spawner = begin_human_send(&mut coordinator, &command_tx, &address, "spawner");
    let spawner_call = recv_with_timeout(&mut admissions).await;
    release_admission(
        &mut coordinator,
        spawner_call,
        ActiveMessageAdmission::Admitted,
    )
    .await;
    assert!(matches!(
        response_outcome(from_spawner).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));

    let from_root = begin_human_send(&mut coordinator, &command_tx, &address, "root");
    let root_call = recv_with_timeout(&mut admissions).await;
    release_admission(
        &mut coordinator,
        root_call,
        ActiveMessageAdmission::Admitted,
    )
    .await;
    assert!(matches!(
        response_outcome(from_root).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));

    let foreign = begin_human_send(&mut coordinator, &command_tx, &address, "other");
    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(foreign).await
    );
}
