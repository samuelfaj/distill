// Modified for Distill by Samuel Fajreldines, 2026.
use super::super::types::{ActiveAgentMessageOutcome, ActiveAgentMessageRequest};
use super::*;
use std::sync::Arc;
use tokio::sync::mpsc;

const TEST_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

async fn await_with_timeout<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(TEST_WAIT, future)
        .await
        .expect("backend test wait timed out")
}

/// Helper: receive the next event, match the expected variant, or panic.
macro_rules! recv_event {
    ($rx:expr, Spawn) => {{
        let event = $rx.recv().await.unwrap();
        match event {
            SubagentEvent::Spawn(inner) => inner,
            _ => panic!("Expected SubagentEvent::Spawn, got different variant"),
        }
    }};
    ($rx:expr, $variant:ident) => {{
        let event = $rx.recv().await.unwrap();
        match event {
            SubagentEvent::$variant(inner) => inner,
            _ => panic!(
                "Expected SubagentEvent::{}, got different variant",
                stringify!($variant)
            ),
        }
    }};
}

struct BackendWithoutActiveMessages;

struct BackendTestControl;

impl super::super::coordinator::ChildControl for BackendTestControl {
    type ProgressFuture = std::future::Ready<super::super::coordinator::SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(super::super::coordinator::SubagentProgress::default())
    }

    fn cancel(&self) {}
}

struct BackendTestRunner;

impl super::super::coordinator::ChildRunner for BackendTestRunner {
    type Control = BackendTestControl;
    type RootControl = crate::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = ();
    type RunFuture = super::super::coordinator::SendBoxFuture<
        super::super::coordinator::ChildRunOutput<Self::CompletionData>,
    >;
    type ValidateFuture = super::super::coordinator::SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = super::super::coordinator::SendBoxFuture<SubagentDescribeOutcome>;

    fn run(
        &self,
        run: super::super::coordinator::ChildRunRequest<Self::Control>,
    ) -> Self::RunFuture {
        debug_assert!(run.agent_message_sender.is_none());
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

    fn on_completed(
        &self,
        _: super::super::coordinator::ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        terminal_published();
    }
}

#[async_trait::async_trait]
impl SubagentBackend for BackendWithoutActiveMessages {
    async fn spawn(
        &self,
        _request: SubagentRequest,
        _registered_tx: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<SubagentResult, ToolError> {
        Err(ToolError::custom("unsupported", "spawn unsupported"))
    }

    async fn query(
        &self,
        _id: &str,
        _block: bool,
        _timeout_ms: Option<u64>,
    ) -> Option<SubagentSnapshot> {
        None
    }

    async fn cancel(&self, _id: &str) -> SubagentCancelOutcome {
        SubagentCancelOutcome::NotFound
    }

    async fn validate_type(
        &self,
        _subagent_type: &str,
        _parent_session_id: &str,
    ) -> SubagentValidateTypeOutcome {
        SubagentValidateTypeOutcome::ValidationUnavailable
    }

    async fn describe_subagent_type(
        &self,
        _subagent_type: &str,
        _harness_agent_type: Option<&str>,
        _parent_session_id: &str,
    ) -> SubagentDescribeOutcome {
        SubagentDescribeOutcome::Unavailable
    }
}

#[tokio::test]
async fn backend_without_active_message_override_is_unsupported() {
    let resource = SubagentBackendResource(Arc::new(BackendWithoutActiveMessages));
    let request = ActiveAgentMessageRequest::try_new("sub-1", "follow up").unwrap();

    assert_eq!(
        resource.backend().send_active_message(request).await,
        ActiveAgentMessageOutcome::Unsupported
    );
}

#[tokio::test]
async fn channel_backend_spawn_success() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Spawn);
        assert_eq!(req.request.id, "test-id");
        assert_eq!(req.request.prompt, "do something");
        req.result_tx
            .send(SubagentResult {
                success: true,
                output: Arc::from("done"),
                subagent_id: "test-id".to_string(),
                child_session_id: "test-id".to_string(),
                tool_calls: 3,
                turns: 1,
                duration_ms: 500,
                ..Default::default()
            })
            .unwrap();
    });

    let request = SubagentRequest {
        id: "test-id".to_string(),
        prompt: "do something".to_string(),
        description: "test".to_string(),
        subagent_type: "general-purpose".to_string(),
        parent_session_id: "parent".to_string(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: super::super::types::SubagentOwner::Task,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        spawn_root: Default::default(),
    };

    let result = backend.spawn(request, None).await.unwrap();
    assert!(result.success);
    assert_eq!(result.subagent_id, "test-id");
    assert_eq!(result.tool_calls, 3);

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_spawn_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<SubagentEvent>();
    drop(rx);

    let backend = ChannelBackend::new(tx);

    let request = SubagentRequest {
        id: "test-id".to_string(),
        prompt: "do something".to_string(),
        description: "test".to_string(),
        subagent_type: "general-purpose".to_string(),
        parent_session_id: "parent".to_string(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: super::super::types::SubagentOwner::Task,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        spawn_root: Default::default(),
    };

    let err = backend.spawn(request, None).await.unwrap_err();
    assert!(err.to_string().contains("channel closed"));
}

#[tokio::test]
async fn channel_backend_query_found() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Query);
        assert_eq!(req.subagent_id, "sub-1");
        assert!(req.block);
        assert_eq!(req.timeout_ms, Some(5000));
        req.respond_to
            .send(Some(SubagentSnapshot {
                subagent_id: "sub-1".to_string(),
                description: "find bugs".to_string(),
                subagent_type: "explore".to_string(),
                status: super::super::types::SubagentSnapshotStatus::Completed {
                    output: "result".to_string(),
                    tool_calls: 2,
                    turns: 1,
                    worktree_path: None,
                },
                started_at_epoch_ms: 1000,
                duration_ms: 200,
                persona: Some("reviewer".to_string()),
            }))
            .unwrap();
    });

    let snap = backend.query("sub-1", true, Some(5000)).await;
    let snap = snap.expect("snapshot should be present");
    assert_eq!(snap.subagent_id, "sub-1");
    assert_eq!(snap.description, "find bugs");
    assert_eq!(snap.subagent_type, "explore");
    assert_eq!(snap.started_at_epoch_ms, 1000);
    assert_eq!(snap.duration_ms, 200);
    assert_eq!(snap.persona.as_deref(), Some("reviewer"));
    match &snap.status {
        super::super::types::SubagentSnapshotStatus::Completed {
            output,
            tool_calls,
            turns,
            worktree_path,
        } => {
            assert_eq!(output, "result");
            assert_eq!(*tool_calls, 2);
            assert_eq!(*turns, 1);
            assert!(worktree_path.is_none());
        }
        other => panic!("Expected Completed, got {:?}", other),
    }

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_non_blocking_passes_through() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Query);
        assert_eq!(req.subagent_id, "sub-nb");
        assert!(!req.block, "block should be false");
        assert_eq!(req.timeout_ms, None, "timeout_ms should be None");
        req.respond_to.send(None).unwrap();
    });

    let snap = backend.query("sub-nb", false, None).await;
    assert!(snap.is_none());

    handle.await.unwrap();
}

#[tokio::test]
async fn coordinator_exits_after_external_senders_drop() {
    let (sender, receiver) =
        super::super::coordinator::SubagentCoordinator::<BackendTestRunner>::channel();
    let actor = tokio::spawn(
        super::super::coordinator::SubagentCoordinator::from_channel(
            receiver,
            BackendTestRunner,
            super::super::coordinator::CoordinatorConfig::default(),
        )
        .run(),
    );
    drop(sender);
    await_with_timeout(actor).await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_not_found() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Query);
        req.respond_to.send(None).unwrap();
    });

    let snap = backend.query("nonexistent", false, None).await;
    assert!(snap.is_none());

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_active_message_binds_parent_and_round_trips() {
    let (sender, mut receiver) =
        super::super::coordinator::SubagentCoordinator::<BackendTestRunner>::channel();
    let backend = ChannelBackend::for_coordinator_session(sender, "bound-parent");
    let send = tokio::spawn(async move {
        backend
            .send_active_message(ActiveAgentMessageRequest::try_new("sub-1", "follow up").unwrap())
            .await
    });

    let ingress = await_with_timeout(receiver.active_messages.recv())
        .await
        .expect("active-message ingress closed");
    let super::super::active_message::ActiveMessageIngress { request, permit } = ingress;
    assert!(matches!(
        request.sender_context,
        super::super::types::ActiveMessageSenderContext::RootSession { ref session_id }
            if session_id.as_ref() == "bound-parent"
    ));
    assert!(matches!(
        request.request.target(),
        crate::implementations::distill::task::active_message::ActiveMessageTarget::ChildId(
            id
        ) if id == "sub-1"
    ));
    assert_eq!(request.request.text().as_ref(), "follow up");
    request
        .respond_to
        .send(ActiveAgentMessageOutcome::Accepted {
            message_id: "message-1".to_owned(),
        })
        .unwrap();
    drop(permit);

    assert_eq!(
        ActiveAgentMessageOutcome::Accepted {
            message_id: "message-1".to_owned()
        },
        await_with_timeout(send).await.unwrap()
    );
}

#[tokio::test]
async fn inert_targets_do_not_enter_backend_ingress() {
    let (sender, mut receiver) =
        super::super::coordinator::SubagentCoordinator::<BackendTestRunner>::channel();
    let backend = ChannelBackend::for_coordinator_session(sender, "parent");
    for target in [
        super::super::types::ActiveMessageTarget::Parent,
        super::super::types::ActiveMessageTarget::Agent {
            agent_id: distill_message_delivery_core::AgentId::mint(7),
        },
    ] {
        let request = ActiveAgentMessageRequest::try_from_parts(
            target,
            "follow up",
            super::super::types::ActiveAgentMessageOperation::Queue,
        )
        .unwrap();
        assert_eq!(
            ActiveAgentMessageOutcome::Unsupported,
            backend.send_active_message(request).await
        );
        assert_eq!(
            super::super::coordinator::MAX_ACTIVE_MESSAGE_ADMISSIONS,
            receiver.active_message_available_permits()
        );
        assert!(receiver.active_messages.try_recv().is_err());
    }
}

#[tokio::test]
async fn root_capable_backend_maps_agent_target_to_root_sender() {
    let (sender, mut receiver) =
        super::super::coordinator::SubagentCoordinator::<BackendTestRunner>::channel();
    let backend = ChannelBackend::for_coordinator_session(sender, "root").with_root_targets();
    let send = tokio::spawn(async move {
        backend
            .send_active_message(
                ActiveAgentMessageRequest::try_from_parts(
                    super::super::types::ActiveMessageTarget::Agent {
                        agent_id: distill_message_delivery_core::AgentId::mint(7),
                    },
                    "follow up",
                    super::super::types::ActiveAgentMessageOperation::Queue,
                )
                .unwrap(),
            )
            .await
    });
    let ingress = receiver
        .active_messages
        .recv()
        .await
        .expect("agent ingress");
    assert!(matches!(
        ingress.request.sender_context,
        super::super::types::ActiveMessageSenderContext::RootSession { .. }
    ));
    ingress
        .request
        .respond_to
        .send(ActiveAgentMessageOutcome::Unsupported)
        .unwrap();
    assert_eq!(send.await.unwrap(), ActiveAgentMessageOutcome::Unsupported);
}

#[tokio::test]
async fn stalled_actor_flood_is_rejected_at_ingress_capacity() {
    const CAPACITY: usize = 2;
    let (sender, mut receiver) =
        super::super::coordinator::SubagentCoordinatorReceiver::with_capacity(CAPACITY);
    let backend = ChannelBackend::for_coordinator_session(sender, "parent");
    let first = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("first", "follow up").unwrap(),
                )
                .await
        }
    });
    let first_queued = await_with_timeout(receiver.active_messages.recv())
        .await
        .expect("first active message queued");
    let second = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("second", "follow up").unwrap(),
                )
                .await
        }
    });
    let second_queued = await_with_timeout(receiver.active_messages.recv())
        .await
        .expect("second active message queued");

    assert_eq!(
        ActiveAgentMessageOutcome::Saturated {
            max_in_flight: CAPACITY,
        },
        await_with_timeout(backend.send_active_message(
            ActiveAgentMessageRequest::try_new("overflow", "follow up").unwrap(),
        ))
        .await
    );
    assert!(receiver.active_messages.try_recv().is_err());
    drop([first_queued, second_queued]);
    assert_eq!(
        ActiveAgentMessageOutcome::ChannelClosed,
        await_with_timeout(first).await.unwrap()
    );
    assert_eq!(
        ActiveAgentMessageOutcome::ChannelClosed,
        await_with_timeout(second).await.unwrap()
    );
}

#[tokio::test]
async fn legacy_raw_sender_clones_do_not_create_active_message_budgets() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let first = ChannelBackend::for_session(tx.clone(), "parent");
    let second = ChannelBackend::for_session(tx.clone(), "parent");

    for backend in [first, second] {
        assert_eq!(
            ActiveAgentMessageOutcome::Unsupported,
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("child", "follow up").unwrap(),
                )
                .await
        );
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn channel_backend_active_message_unbound_fails_closed() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);
    let outcome = backend
        .send_active_message(ActiveAgentMessageRequest::try_new("sub-1", "follow up").unwrap())
        .await;
    assert_eq!(outcome, ActiveAgentMessageOutcome::NotFoundOrNotOwned);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn channel_backend_cancel_success() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Cancel);
        match &req.target {
            SubagentCancelTarget::SubagentId(id) => assert_eq!(id, "sub-cancel"),
            other => panic!("Expected SubagentId, got {:?}", other),
        }
        req.respond_to
            .send(SubagentCancelOutcome::Cancelled)
            .unwrap();
    });

    let outcome = backend.cancel("sub-cancel").await;
    assert!(matches!(outcome, SubagentCancelOutcome::Cancelled));

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_cancel_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<SubagentEvent>();
    drop(rx);

    let backend = ChannelBackend::new(tx);

    let outcome = backend.cancel("sub-cancel").await;
    assert!(matches!(outcome, SubagentCancelOutcome::NotFound));
}

#[tokio::test]
async fn workflow_spawn_future_drop_cancels_but_task_drop_does_not() {
    fn request_for(owner: super::super::types::SubagentOwner) -> SubagentRequest {
        SubagentRequest {
            id: "drop-owner-test".to_string(),
            prompt: "test".to_string(),
            description: "test".to_string(),
            subagent_type: "general-purpose".to_string(),
            parent_session_id: "parent".to_string(),
            parent_prompt_id: None,
            resume_from: None,
            cwd: None,
            runtime_overrides: Default::default(),
            run_in_background: false,
            surface_completion: false,
            await_to_completion: true,
            fork_context: false,
            owner,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            spawn_root: Default::default(),
        }
    }

    for (owner, should_cancel) in [
        (super::super::types::SubagentOwner::Task, false),
        (super::super::types::SubagentOwner::workflow("wf-1"), true),
    ] {
        let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
        let backend = Arc::new(ChannelBackend::new(tx));
        let request = request_for(owner);
        let cancel_token = request.cancel_token.clone();
        let task = tokio::spawn({
            let backend = backend.clone();
            async move { backend.spawn(request, None).await }
        });
        let spawned = recv_event!(rx, Spawn);
        task.abort();
        let _ = task.await;
        assert_eq!(
            cancel_token.is_cancelled(),
            should_cancel,
            "only workflow receiver drop owns cancellation"
        );
        drop(spawned.result_tx);
    }
}

#[tokio::test]
async fn channel_backend_spawn_result_dropped() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let req = recv_event!(rx, Spawn);
        drop(req.result_tx);
    });

    let request = SubagentRequest {
        id: "drop-test".to_string(),
        prompt: "test".to_string(),
        description: "test".to_string(),
        subagent_type: "general-purpose".to_string(),
        parent_session_id: "parent".to_string(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: super::super::types::SubagentOwner::Task,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        spawn_root: Default::default(),
    };

    let err = backend.spawn(request, None).await.unwrap_err();
    assert!(
        err.to_string().contains("result channel dropped"),
        "error: {err}"
    );

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<SubagentEvent>();
    drop(rx);

    let backend = ChannelBackend::new(tx);

    let snap = backend.query("sub-1", false, None).await;
    assert!(snap.is_none());
}

// ── validate_type ────────────────────────────────────────────────

#[tokio::test]
async fn channel_backend_validate_type_round_trips_outcome() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let event = rx.recv().await.unwrap();
        match event {
            SubagentEvent::ValidateType(req) => {
                assert_eq!(req.subagent_type, "explore");
                assert_eq!(req.parent_session_id, "parent-1");
                req.respond_to
                    .send(SubagentValidateTypeOutcome::Ok)
                    .unwrap();
            }
            _ => panic!("Expected ValidateType event"),
        }
    });

    let outcome = backend.validate_type("explore", "parent-1").await;
    assert!(matches!(outcome, SubagentValidateTypeOutcome::Ok));
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_validate_type_propagates_unknown_outcome() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        if let Some(SubagentEvent::ValidateType(req)) = rx.recv().await {
            req.respond_to
                .send(SubagentValidateTypeOutcome::Unknown {
                    available: vec!["explore".into(), "plan".into()],
                })
                .unwrap();
        }
    });

    let outcome = backend.validate_type("invented", "p").await;
    match outcome {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert_eq!(available, vec!["explore".to_string(), "plan".to_string()]);
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_validate_type_returns_coordinator_gone_when_channel_closed() {
    let (tx, rx) = mpsc::unbounded_channel::<SubagentEvent>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    let outcome = backend.validate_type("explore", "p").await;
    assert!(matches!(
        outcome,
        SubagentValidateTypeOutcome::CoordinatorGone
    ));
}

#[tokio::test]
async fn channel_backend_validate_type_returns_validation_unavailable_when_responder_dropped() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);
    let handle = tokio::spawn(async move {
        if let Some(SubagentEvent::ValidateType(req)) = rx.recv().await {
            drop(req.respond_to);
        }
    });
    let outcome = backend.validate_type("explore", "p").await;
    assert!(matches!(
        outcome,
        SubagentValidateTypeOutcome::ValidationUnavailable,
    ));
    handle.await.unwrap();
}

use super::super::types::test_capture;

#[tokio::test(start_paused = true)]
async fn channel_backend_validate_type_logs_warn_on_timeout() {
    let captured = test_capture::capture();
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    // Coordinator receives but never replies; keeps the responder
    // alive so the timeout arm fires (not responder-dropped).
    let holder = tokio::spawn(async move {
        if let Some(SubagentEvent::ValidateType(req)) = rx.recv().await {
            std::mem::forget(req.respond_to);
            std::future::pending::<()>().await;
        }
    });

    let timeout = validate_type_timeout();
    let validate = tokio::spawn(async move { backend.validate_type("explore", "p").await });
    tokio::time::advance(timeout + std::time::Duration::from_millis(1)).await;
    let outcome = validate.await.unwrap();
    assert!(matches!(
        outcome,
        SubagentValidateTypeOutcome::ValidationUnavailable
    ));

    let mut events_rx = captured.events_rx;
    let mut saw_timeout_warn = false;
    while let Ok(event) = events_rx.try_recv() {
        if event.level == tracing::Level::WARN
            && event.fields.contains("coordinator validation timed out")
            && event.fields.contains("subagent_type=explore")
            && event
                .fields
                .contains(&format!("timeout_ms={}", timeout.as_millis()))
        {
            saw_timeout_warn = true;
            break;
        }
    }
    assert!(
        saw_timeout_warn,
        "must emit WARN with the resolved timeout_ms value"
    );

    holder.abort();
}

/// Pins that the timeout WARN reports the duration actually raced, so an
/// env override shows its real (post-override) value, not the default.
#[tokio::test(start_paused = true)]
async fn validate_reply_timeout_warn_reports_the_raced_duration() {
    let captured = test_capture::capture();
    let override_timeout = std::time::Duration::from_millis(250);
    let (_respond_to, response_rx) = tokio::sync::oneshot::channel();

    let raced = tokio::spawn(await_validate_reply(
        "explore",
        override_timeout,
        response_rx,
    ));
    tokio::time::advance(override_timeout + std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        raced.await.unwrap(),
        SubagentValidateTypeOutcome::ValidationUnavailable
    ));

    let mut events_rx = captured.events_rx;
    let mut saw_timeout_warn = false;
    while let Ok(event) = events_rx.try_recv() {
        if event.level == tracing::Level::WARN
            && event.fields.contains("coordinator validation timed out")
            && event.fields.contains("timeout_ms=250")
        {
            saw_timeout_warn = true;
            break;
        }
    }
    assert!(saw_timeout_warn, "timeout WARN must carry the raced value");
}

/// Pins the raised default: a coordinator busy past the old 2s default (e.g. pegged by turn-end
/// trace packaging) but inside [`VALIDATE_TYPE_TIMEOUT`] must still get its verdict through instead
/// of a spurious `ValidationUnavailable`.
#[tokio::test(start_paused = true)]
async fn channel_backend_validate_type_waits_out_a_busy_coordinator() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let responder = tokio::spawn(async move {
        if let Some(SubagentEvent::ValidateType(req)) = rx.recv().await {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = req.respond_to.send(SubagentValidateTypeOutcome::Ok);
        }
    });

    let outcome = backend.validate_type("explore", "p").await;
    assert!(matches!(outcome, SubagentValidateTypeOutcome::Ok));
    responder.await.unwrap();
}

// ── describe_subagent_type ───────────────────────────────────────

#[tokio::test]
async fn channel_backend_describe_round_trips_summary() {
    use super::super::types::{SubagentDescribeOutcome, SubagentTypeSummary};
    use crate::types::tool::ToolKind;

    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        match rx.recv().await.unwrap() {
            SubagentEvent::DescribeType(req) => {
                assert_eq!(req.subagent_type, "explore");
                assert_eq!(req.harness_agent_type.as_deref(), Some("cursor"));
                assert_eq!(req.parent_session_id, "parent-1");
                let mut summary = SubagentTypeSummary {
                    can_read: true,
                    can_search: true,
                    ..Default::default()
                };
                summary
                    .tool_names
                    .insert(ToolKind::Read, "read_file".to_string());
                req.respond_to
                    .send(SubagentDescribeOutcome::Ok(summary))
                    .unwrap();
            }
            _ => panic!("Expected DescribeType event"),
        }
    });

    let outcome = backend
        .describe_subagent_type("explore", Some("cursor"), "parent-1")
        .await;
    match outcome {
        SubagentDescribeOutcome::Ok(summary) => {
            assert!(summary.can_read && summary.can_search && !summary.can_execute);
            assert_eq!(
                summary.tool_names.get(&ToolKind::Read).unwrap(),
                "read_file"
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_describe_propagates_not_allowed_outcome() {
    use super::super::types::SubagentDescribeOutcome;

    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        if let Some(SubagentEvent::DescribeType(req)) = rx.recv().await {
            req.respond_to
                .send(SubagentDescribeOutcome::NotAllowed {
                    allowed: vec!["explore".into()],
                })
                .unwrap();
        }
    });

    match backend.describe_subagent_type("plan", None, "p").await {
        SubagentDescribeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["explore".to_string()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_describe_returns_unavailable_when_channel_closed() {
    use super::super::types::SubagentDescribeOutcome;
    let (tx, rx) = mpsc::unbounded_channel::<SubagentEvent>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    assert!(matches!(
        backend.describe_subagent_type("explore", None, "p").await,
        SubagentDescribeOutcome::Unavailable
    ));
}

#[tokio::test]
async fn channel_backend_describe_returns_unavailable_when_responder_dropped() {
    use super::super::types::SubagentDescribeOutcome;
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);
    let handle = tokio::spawn(async move {
        if let Some(SubagentEvent::DescribeType(req)) = rx.recv().await {
            drop(req.respond_to);
        }
    });
    assert!(matches!(
        backend.describe_subagent_type("explore", None, "p").await,
        SubagentDescribeOutcome::Unavailable
    ));
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn channel_backend_describe_returns_unavailable_on_timeout() {
    use super::super::types::SubagentDescribeOutcome;
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let holder = tokio::spawn(async move {
        if let Some(SubagentEvent::DescribeType(req)) = rx.recv().await {
            std::mem::forget(req.respond_to);
            std::future::pending::<()>().await;
        }
    });

    let describe =
        tokio::spawn(async move { backend.describe_subagent_type("explore", None, "p").await });
    tokio::time::advance(DESCRIBE_TYPE_TIMEOUT + std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        describe.await.unwrap(),
        SubagentDescribeOutcome::Unavailable
    ));
    holder.abort();
}

/// Pins that describe did NOT inherit the spawn-validation timeout raise: a reply past
/// [`DESCRIBE_TYPE_TIMEOUT`] but inside [`VALIDATE_TYPE_TIMEOUT`] must already have timed out (the
/// /goal gate awaits describe serially per agent type, so its budget stays short).
#[tokio::test(start_paused = true)]
async fn channel_backend_describe_times_out_before_validate_default() {
    use super::super::types::{SubagentDescribeOutcome, SubagentTypeSummary};
    let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
    let backend = ChannelBackend::new(tx);

    let responder = tokio::spawn(async move {
        if let Some(SubagentEvent::DescribeType(req)) = rx.recv().await {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = req
                .respond_to
                .send(SubagentDescribeOutcome::Ok(SubagentTypeSummary::default()));
        }
    });

    assert!(matches!(
        backend.describe_subagent_type("explore", None, "p").await,
        SubagentDescribeOutcome::Unavailable
    ));
    responder.abort();
}

#[test]
fn duration_or_reads_the_override_as_milliseconds() {
    assert_eq!(
        duration_or(Some("250"), VALIDATE_TYPE_TIMEOUT),
        std::time::Duration::from_millis(250)
    );
}

/// Pins that validate and describe read DIFFERENT env vars: one ops knob
/// must not silently undo the 10s/2s budget split (a legacy 2000 pin on the
/// validate var must not cap describe, and vice versa).
#[test]
fn validate_and_describe_timeouts_have_separate_overrides() {
    assert_ne!(VALIDATE_TYPE_TIMEOUT_ENV_VAR, DESCRIBE_TYPE_TIMEOUT_ENV_VAR);
    // Same composition the getters use, with only the validate override set.
    assert_eq!(
        duration_or(Some("9000"), VALIDATE_TYPE_TIMEOUT),
        std::time::Duration::from_millis(9000)
    );
    assert_eq!(
        duration_or(None, DESCRIBE_TYPE_TIMEOUT),
        std::time::Duration::from_secs(2),
        "validate override must not leak into describe"
    );
    // And only the describe override set.
    assert_eq!(
        duration_or(Some("500"), DESCRIBE_TYPE_TIMEOUT),
        std::time::Duration::from_millis(500)
    );
    assert_eq!(
        duration_or(None, VALIDATE_TYPE_TIMEOUT),
        std::time::Duration::from_secs(10),
        "describe override must not leak into validate"
    );
}

/// Pins the documented defaults through the same composition
/// `validate_type_timeout` / `describe_type_timeout` use.
#[test]
fn duration_or_falls_back_to_the_default() {
    for bad in [
        None,
        Some(""),
        Some("0"),
        Some("-100"),
        Some("3.14"),
        Some("not-a-number"),
    ] {
        assert_eq!(
            duration_or(bad, VALIDATE_TYPE_TIMEOUT),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            duration_or(bad, DESCRIBE_TYPE_TIMEOUT),
            std::time::Duration::from_secs(2)
        );
    }
}
