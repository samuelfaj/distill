// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use distill_tools::implementations::distill::task::types::ActiveAgentMessage;
use std::sync::Arc;

#[expect(
    clippy::unwrap_used,
    reason = "test assertions require live response senders"
)]
fn admission_response(
    response: Result<ActiveMessageAdmission, oneshot::error::RecvError>,
) -> ActiveMessageAdmission {
    response.unwrap()
}

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

use distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation;

fn message(id: &str) -> ActiveAgentMessage {
    message_with_text(id, "parent update")
}

pub(super) fn message_with_text(id: &str, text: &str) -> ActiveAgentMessage {
    ActiveAgentMessage {
        message_id: id.into(),
        sender_session_id: "root-session".into(),
        text: Arc::from(text),
    }
}

/// Fixture for the lane-order tests, which never go through admission.
pub(in crate::session::acp_session) fn delivery_message(
    id: &str,
    operation: ActiveAgentMessageOperation,
) -> DeliveryMessage<String, ParentMessageOrigin, PendingParentAgentMessage> {
    let message = message(id);
    let telemetry = crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry::new(
        std::time::Instant::now(),
        distill_telemetry::TelemetryCtx::new(
            "parent".to_owned(),
            Arc::new(tokio::sync::Mutex::new(0)),
        ),
        operation,
        operation,
        None,
    );
    DeliveryMessage::new(
        message.message_id.clone(),
        ParentMessageOrigin {
            sender_session_id: message.sender_session_id,
            source: ActiveAgentMessageSource::Agent,
        },
        PendingParentAgentMessage {
            prompt_id: format!("parent-message-{id}"),
            message_id: message.message_id,
            text: message.text,
            operation,
            telemetry,
        },
    )
}

pub(super) async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("parent-message test timed out")
}

pub(super) async fn admit(
    actor: &Arc<SessionActor>,
    message: ActiveAgentMessage,
    operation: ActiveAgentMessageOperation,
) -> crate::agent::subagent::PromptTurnReceipt {
    let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
    let (respond_to, response_rx) = oneshot::channel();
    let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
    actor
        .admit_parent_agent_message_for_test(
            message,
            ActiveAgentMessageSource::Agent,
            operation,
            receipt_sink,
            respond_to,
            completion_tx,
        )
        .await;
    assert_eq!(
        admission_response(await_with_timeout(response_rx).await),
        ActiveMessageAdmission::Admitted
    );
    receipt_rx.recv().await.expect("typed receipt handed off")
}

async fn admit_steer(
    actor: &Arc<SessionActor>,
    id: &str,
) -> crate::agent::subagent::PromptTurnReceipt {
    admit(actor, message(id), ActiveAgentMessageOperation::Steer).await
}

/// Starts `prompt_id` as the running turn with a fresh epoch, so two turns are distinguishable.
pub(super) async fn start_turn(actor: &SessionActor, prompt_id: &str) -> TurnEpoch {
    let epoch = actor.turn_report.start_next_turn();
    let handle = tokio::task::spawn_local(std::future::pending::<()>()).abort_handle();
    let mut state = actor.state.lock().await;
    state
        .pending_inputs
        .push_back(super::super::support::user_item(prompt_id, "owner"));
    state.running_task = Some(AgentTask::new_at_epoch(prompt_id, epoch, handle));
    epoch
}

pub(super) async fn set_running(actor: &SessionActor, prompt_id: &str) {
    start_turn(actor, prompt_id).await;
}

#[tokio::test(flavor = "current_thread")]
async fn admission_rejects_closed_receipt_sink_without_queueing() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (receipt_sink, receipt_rx) = mpsc::channel(1);
        drop(receipt_rx);
        let (respond_to, response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("closed"),
            ActiveAgentMessageSource::Agent,
            ActiveAgentMessageOperation::Queue,
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::ChannelClosed
        );
        assert!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .is_empty()
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn receipt_backpressure_waits_before_queue_commit() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (_occupied_tx, occupied_rx) = oneshot::channel();
        receipt_sink
            .send(crate::agent::subagent::PromptTurnReceipt {
                prompt_id: "occupied".into(),
                result: occupied_rx,
                telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry::new(
                    std::time::Instant::now(),
                    distill_telemetry::TelemetryCtx::new(
                        "parent".to_owned(),
                        Arc::new(tokio::sync::Mutex::new(0)),
                    ),
                    ActiveAgentMessageOperation::Queue,
                    ActiveAgentMessageOperation::Queue,
                    None,
                ),
            })
            .await
            .expect("occupy receipt capacity");
        let (respond_to, mut response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let admission = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move {
                actor
                    .admit_parent_agent_message_for_test(
                        message("backpressured"),
                        ActiveAgentMessageSource::Agent,
                        ActiveAgentMessageOperation::Queue,
                        receipt_sink,
                        respond_to,
                        completion_tx,
                    )
                    .await;
            }
        });

        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut response_rx)
                .await
                .is_err(),
            "receipt backpressure must keep admission pending"
        );
        assert!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .is_empty(),
            "no queue row exists before receipt capacity is reserved"
        );
        receipt_rx.recv().await.expect("occupied receipt");

        await_with_timeout(admission).await.expect("admission task");
        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        assert_eq!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .len(),
            1
        );
        assert_eq!(
            receipt_rx
                .recv()
                .await
                .expect("committed receipt")
                .prompt_id,
            "parent-message-backpressured"
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn busy_state_lock_waits_before_commit_and_receipt_handoff() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let state = await_with_timeout(actor.state.lock()).await;
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (respond_to, mut response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let admission = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move {
                actor
                    .admit_parent_agent_message_for_test(
                        message("busy"),
                        ActiveAgentMessageSource::Agent,
                        ActiveAgentMessageOperation::Queue,
                        receipt_sink,
                        respond_to,
                        completion_tx,
                    )
                    .await;
            }
        });

        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut response_rx)
                .await
                .is_err(),
            "lock contention must keep admission pending"
        );
        assert!(state.pending_inputs.is_empty());
        assert!(receipt_rx.try_recv().is_err());
        drop(state);

        await_with_timeout(admission).await.expect("admission task");
        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        assert!(receipt_rx.try_recv().is_ok());
        assert_eq!(
            await_with_timeout(actor.state.lock())
                .await
                .running_prompt_id(),
            Some("parent-message-busy")
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_live_identity_rejects_without_second_receiver() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        {
            let mut state = await_with_timeout(actor.state.lock()).await;
            state
                .pending_inputs
                .push_back(super::super::support::user_item("running", "owner"));
            state.running_task = Some(super::super::support::running_task_stub("running"));
        }
        let (receipt_sink, mut receipt_rx) = mpsc::channel(2);
        for expected in [
            ActiveMessageAdmission::Admitted,
            ActiveMessageAdmission::Rejected,
        ] {
            let (respond_to, response_rx) = oneshot::channel();
            await_with_timeout(actor.admit_parent_agent_message_for_test(
                message("duplicate"),
                ActiveAgentMessageSource::Agent,
                ActiveAgentMessageOperation::Steer,
                receipt_sink.clone(),
                respond_to,
                completion_tx.clone(),
            ))
            .await;
            assert_eq!(
                admission_response(await_with_timeout(response_rx).await),
                expected
            );
        }
        assert!(receipt_rx.try_recv().is_ok());
        assert!(receipt_rx.try_recv().is_err());
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn idle_human_steer_queues_one_verbatim_protected_row() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        actor.deferred_prefix.arm(
            tokio::task::spawn_local(async { "PARENT_PREFIX_READY".to_owned() }),
            false,
        );
        actor.state.lock().await.arm_hook_block_hold();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let (receipt_sink, _receipt_rx) = mpsc::channel(1);
        let (respond_to, response_rx) = oneshot::channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("idle-steer"),
            ActiveAgentMessageSource::Human,
            ActiveAgentMessageOperation::Steer,
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        let state = await_with_timeout(actor.state.lock()).await;
        assert_eq!(state.pending_inputs.len(), 1);
        let queued = state.pending_inputs.front().expect("one queued row");
        assert!(queued.verbatim);
        assert!(queued.is_queue_protected());
        assert!(matches!(
            queued.input_origin.as_prompt_origin(),
            PromptOrigin::ParentHumanMessage {
                message_id,
                sender_session_id,
            } if message_id == "idle-steer" && sender_session_id == "root-session"
        ));
        assert!(state.message_delivery.is_empty());
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn running_steer_projects_at_safe_point_with_agent_provenance() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        {
            let mut state = await_with_timeout(actor.state.lock()).await;
            state
                .pending_inputs
                .push_back(super::super::support::user_item("running", "owner"));
            state.running_task = Some(super::super::support::running_task_stub("running"));
        }
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (respond_to, response_rx) = oneshot::channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("steered"),
            ActiveAgentMessageSource::Agent,
            ActiveAgentMessageOperation::Steer,
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        assert_eq!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .len(),
            1,
        );
        let receipt = receipt_rx.recv().await.expect("typed receipt handed off");
        assert!(actor.drain_parent_messages_at_safe_point().await);
        let conversation = await_with_timeout(actor.chat_state_handle.get_conversation()).await;
        assert!(matches!(
            conversation.as_slice(),
            [item @ ConversationItem::User(user)]
                if user.synthetic_reason == SyntheticReason::AgentMessage
                    && item.text_content() == "parent update"
        ));
        let completions = {
            let mut state = await_with_timeout(actor.state.lock()).await;
            let task = state.running_task.as_ref().expect("running task");
            let binding =
                distill_message_delivery_core::TurnBinding::new(task.prompt_id.clone(), task.epoch);
            let (completions, _) = actor.transition_parent_messages(
                &mut state,
                distill_message_delivery_core::TerminalTarget::Turn(&binding),
                distill_message_delivery_core::TerminalCause::Completion,
            );
            let (second, _) = actor.transition_parent_messages(
                &mut state,
                distill_message_delivery_core::TerminalTarget::Turn(&binding),
                distill_message_delivery_core::TerminalCause::Completion,
            );
            assert!(second.is_empty());
            completions
        };
        let result = crate::session::commands::ok_end_turn(7, None);
        SessionActor::settle_parent_message_completions(completions, &result);
        assert_eq!(
            await_with_timeout(receipt.result)
                .await
                .expect("receipt settles")
                .expect("successful turn")
                .total_tokens,
            7
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn completion_fallback_appends_after_retained_queue() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let task = super::super::support::running_task_stub("running");
        let binding =
            distill_message_delivery_core::TurnBinding::new("running".to_owned(), task.epoch);
        {
            let mut state = await_with_timeout(actor.state.lock()).await;
            state
                .pending_inputs
                .push_back(super::super::support::user_item("running", "owner"));
            state
                .pending_inputs
                .push_back(super::super::support::user_item("retained", "owner"));
            state.running_task = Some(task);
        }
        // Both lanes fall back the same way: one protected row each, in admission order
        for (id, operation) in [
            ("steer", ActiveAgentMessageOperation::Steer),
            ("interject", ActiveAgentMessageOperation::Interject),
        ] {
            admit(&actor, message(id), operation).await;
        }

        let mut state = await_with_timeout(actor.state.lock()).await;
        let (completions, had_fallbacks) = actor.transition_parent_messages(
            &mut state,
            distill_message_delivery_core::TerminalTarget::Turn(&binding),
            distill_message_delivery_core::TerminalCause::Completion,
        );
        assert!(had_fallbacks);
        assert!(completions.is_empty());
        assert_eq!(
            state
                .pending_inputs
                .iter()
                .map(|input| input.prompt_id.as_str())
                .collect::<Vec<_>>(),
            [
                "running",
                "retained",
                "parent-message-steer",
                "parent-message-interject"
            ]
        );
        assert!(state.message_delivery.is_empty());
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_persistence_barrier_skips_delivery_without_fallback() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
        let (actor, _) = super::super::support::create_test_actor_with_chat_persistence(
            0,
            256_000,
            85,
            gateway_tx,
            persistence_tx.clone(),
            Box::new(crate::session::chat_persistence::ChannelChatPersistence::new(persistence_tx)),
        )
        .await;
        let actor = Arc::new(actor);
        set_running(&actor, "running").await;
        let mut receipt = admit_steer(&actor, "persist-failed").await;
        tokio::task::spawn_local(async move {
            while let Some(message) = persistence_rx.recv().await {
                if let PersistenceMsg::FlushAndAck { respond_to } = message {
                    drop(respond_to);
                }
            }
        });

        assert!(!actor.drain_parent_messages_at_safe_point().await);
        let _ = actor
            .cancel_running_task(crate::session::CancelOptions::default())
            .await;
        let state = actor.state.lock().await;
        assert!(state.message_delivery.is_empty());
        assert!(state.pending_inputs.is_empty());
        let conversation = actor.chat_state_handle.get_conversation().await;
        assert_eq!(
            conversation
                .iter()
                .filter(|item| item.text_content() == "parent update")
                .count(),
            0,
            "a failed barrier must keep the undelivered text out of chat state"
        );
        drop(state);
        let settled = receipt
            .result
            .try_recv()
            .expect("soft cancel settles the in-flight steer receipt");
        assert!(
            matches!(settled, Ok(ok) if ok.stop_reason == acp::StopReason::Cancelled),
            "soft cancel must not fallback and must settle Cancelled"
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_running_turn_shutdown_drains_after_stale_rewind_cancel() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (gateway_tx, mut gateway_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(async move {
            while let Some(message) = gateway_rx.recv().await {
                if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = message {
                    let _ = args.response_tx.send(Ok(()));
                }
            }
        });
        let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(async move {
            while let Some(message) = persistence_rx.recv().await {
                if let PersistenceMsg::FlushAndAck { respond_to } = message {
                    let _ = respond_to.send(Ok(()));
                }
            }
        });
        let (actor, event_rx) =
            super::super::support::create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
        let actor = Arc::new(actor);
        set_running(&actor, "running").await;
        let receipt = admit_steer(&actor, "shutdown").await;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_chat_tx, chat_rx) = mpsc::unbounded_channel();
        let loop_task = tokio::task::spawn_local(super::super::run_session(
            Arc::clone(&actor),
            cmd_rx,
            chat_rx,
            event_rx,
            None,
            Arc::new(parking_lot::Mutex::new(
                distill_workspace::file_system::CodebaseIndexManager::new(),
            )),
            std::path::PathBuf::from("/tmp"),
            crate::session::fs_watch::FsWatchCapabilities::none(),
        ));
        cmd_tx
            .send(SessionCommand::Cancel(crate::session::CancelOptions {
                cancel_subagents: true,
                kill_background_tasks: true,
                history: crate::session::CancelHistoryDisposition::RewindIfNoOutput {
                    prompt_id: Some("stale".to_owned()),
                },
                trigger: Some(crate::session::CancelTrigger::Shutdown),
                user_initiated: false,
            }))
            .expect("loop running");
        cmd_tx
            .send(SessionCommand::Shutdown(
                crate::session::ShutdownKind::CancelRunningTurn,
            ))
            .expect("loop running");
        await_with_timeout(loop_task).await.expect("loop exits");

        assert!(actor.state.lock().await.message_delivery.is_empty());
        let settled = await_with_timeout(receipt.result)
            .await
            .expect("shutdown settles the steer receipt");
        assert!(
            matches!(settled, Ok(ok) if ok.stop_reason == acp::StopReason::Cancelled),
            "shutdown cancel settles Cancelled rather than dropping the sender"
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unresolved_persistence_barrier_does_not_block_hard_teardown() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
        let (actor, _) = super::super::support::create_test_actor_with_chat_persistence(
            0,
            256_000,
            85,
            gateway_tx,
            persistence_tx.clone(),
            Box::new(crate::session::chat_persistence::ChannelChatPersistence::new(persistence_tx)),
        )
        .await;
        let actor = Arc::new(actor);
        set_running(&actor, "running").await;
        let receipt = admit_steer(&actor, "stalled").await;
        let (barrier_seen_tx, barrier_seen_rx) = oneshot::channel();
        let held_barrier = Arc::new(tokio::sync::Mutex::new(None));
        let held_by_drain = Arc::clone(&held_barrier);
        tokio::task::spawn_local(async move {
            let mut barrier_seen_tx = Some(barrier_seen_tx);
            while let Some(message) = persistence_rx.recv().await {
                if let PersistenceMsg::FlushAndAck { respond_to } = message {
                    *held_by_drain.lock().await = Some(respond_to);
                    if let Some(barrier_seen_tx) = barrier_seen_tx.take() {
                        let _ = barrier_seen_tx.send(());
                    }
                }
            }
        });
        let drain = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move { actor.drain_parent_messages_at_safe_point().await }
        });
        await_with_timeout(barrier_seen_rx)
            .await
            .expect("barrier was enqueued");

        await_with_timeout(actor.settle_all_parent_messages(
            distill_message_delivery_core::TerminalCause::HardTeardown,
        ))
        .await;
        let state = actor.state.lock().await;
        assert!(state.message_delivery.is_empty());
        assert!(
            state
                .pending_inputs
                .iter()
                .all(|input| input.prompt_id == "running"),
            "hard teardown must not requeue the in-flight steer"
        );
        drop(state);
        assert_eq!(
            actor
                .chat_state_handle
                .get_conversation()
                .await
                .iter()
                .filter(|item| item.text_content() == "parent update")
                .count(),
            0,
            "hard teardown must keep the settled steer text out of chat state"
        );
        let settled = await_with_timeout(receipt.result)
            .await
            .expect("hard teardown settles the steer receipt");
        assert!(settled.is_err(), "hard teardown settles with an error");
        held_barrier.lock().await.take();
        assert!(!await_with_timeout(drain).await.expect("drain task"));
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn teardown_settlement_during_barrier_skips_persist_and_push() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
        let (actor, _) = super::super::support::create_test_actor_with_chat_persistence(
            0,
            256_000,
            85,
            gateway_tx,
            persistence_tx.clone(),
            Box::new(crate::session::chat_persistence::ChannelChatPersistence::new(persistence_tx)),
        )
        .await;
        let actor = Arc::new(actor);
        set_running(&actor, "running").await;
        let receipt = admit_steer(&actor, "settled-mid-drain").await;
        let (barrier_seen_tx, barrier_seen_rx) = oneshot::channel();
        let held_barrier = Arc::new(tokio::sync::Mutex::new(None));
        let steer_text_persisted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held_by_drain = Arc::clone(&held_barrier);
        let persisted_flag = Arc::clone(&steer_text_persisted);
        tokio::task::spawn_local(async move {
            let mut barrier_seen_tx = Some(barrier_seen_tx);
            while let Some(message) = persistence_rx.recv().await {
                match message {
                    PersistenceMsg::FlushAndAck { respond_to } => {
                        *held_by_drain.lock().await = Some(respond_to);
                        if let Some(barrier_seen_tx) = barrier_seen_tx.take() {
                            let _ = barrier_seen_tx.send(());
                        }
                    }
                    PersistenceMsg::Update(update) => {
                        if serde_json::to_string(&update)
                            .is_ok_and(|json| json.contains("parent update"))
                        {
                            persisted_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    PersistenceMsg::Chat(item) => {
                        if item.text_content() == "parent update" {
                            persisted_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    _ => {}
                }
            }
        });
        let drain = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move { actor.drain_parent_messages_at_safe_point().await }
        });
        await_with_timeout(barrier_seen_rx)
            .await
            .expect("barrier was enqueued");

        // The command channel closing settles all receipts with `ActorDrop`
        // while the drain is suspended on its persistence barrier.
        await_with_timeout(
            actor.settle_all_parent_messages(
                distill_message_delivery_core::TerminalCause::ActorDrop,
            ),
        )
        .await;
        let settled = await_with_timeout(receipt.result)
            .await
            .expect("teardown settles the steer receipt");
        assert!(settled.is_err(), "teardown settles with an error");

        let released = held_barrier
            .lock()
            .await
            .take()
            .expect("drain enqueued the barrier");
        let _ = released.send(Ok(()));
        assert!(
            !await_with_timeout(drain).await.expect("drain task"),
            "a drain resumed after settlement must not report a delivery"
        );
        assert!(actor.state.lock().await.message_delivery.is_empty());
        assert_eq!(
            actor
                .chat_state_handle
                .get_conversation()
                .await
                .iter()
                .filter(|item| item.text_content() == "parent update")
                .count(),
            0,
            "settled steer text must not be pushed into chat state"
        );
        assert!(
            !steer_text_persisted.load(std::sync::atomic::Ordering::SeqCst),
            "settled steer text must not be persisted"
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn delivered_message_is_durable_in_updates_and_chat_history_before_shutdown() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let session_dir = tempfile::tempdir().expect("session dir");
        let sampling_client =
            crate::sampling::Client::new(distill_sampler::SamplerConfig::default())
                .expect("sampling client");
        let info = crate::session::info::Info {
            id: acp::SessionId::new("parent-message-durable"),
            cwd: "/tmp".to_owned(),
        };
        let persistence = crate::session::persistence::new_with_explicit_dir(
            &info,
            session_dir.path().to_path_buf(),
            acp::ModelId::new("test-model"),
            sampling_client,
            "test-model".to_owned(),
            crate::session::persistence::ExplicitSessionOpen::New {
                identity: None,
                next_trace_turn: None,
            },
        )
        .await
        .expect("persistence actor");
        let persistence_tx = persistence.tx.clone();
        let (gateway_tx, mut gateway_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(async move {
            while let Some(message) = gateway_rx.recv().await {
                if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = message {
                    let _ = args.response_tx.send(Ok(()));
                }
            }
        });
        let (actor, event_rx) = super::super::support::create_test_actor_with_chat_persistence(
            0,
            256_000,
            85,
            gateway_tx,
            persistence_tx.clone(),
            Box::new(crate::session::chat_persistence::ChannelChatPersistence::new(persistence_tx)),
        )
        .await;
        let actor = Arc::new(actor);
        set_running(&actor, "running").await;
        let _receipt = admit_steer(&actor, "durable").await;

        assert!(actor.drain_parent_messages_at_safe_point().await);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_chat_tx, chat_rx) = mpsc::unbounded_channel();
        let loop_task = tokio::task::spawn_local(super::super::run_session(
            Arc::clone(&actor),
            cmd_rx,
            chat_rx,
            event_rx,
            None,
            Arc::new(parking_lot::Mutex::new(
                distill_workspace::file_system::CodebaseIndexManager::new(),
            )),
            std::path::PathBuf::from("/tmp"),
            crate::session::fs_watch::FsWatchCapabilities::none(),
        ));
        cmd_tx
            .send(SessionCommand::Shutdown(
                crate::session::ShutdownKind::Graceful,
            ))
            .expect("loop running");
        await_with_timeout(loop_task).await.expect("loop exits");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        persistence
            .tx
            .send(PersistenceMsg::FlushAndAck {
                respond_to: shutdown_tx,
            })
            .expect("persistence actor alive");
        await_with_timeout(shutdown_rx)
            .await
            .expect("shutdown barrier reply")
            .expect("shutdown barrier");

        for file in ["updates.jsonl", "chat_history.jsonl"] {
            let contents = std::fs::read_to_string(session_dir.path().join(file))
                .unwrap_or_else(|error| panic!("read {file}: {error}"));
            assert!(
                contents.contains("parent update"),
                "missing delivery in {file}"
            );
        }
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn committed_delivery_queues_protected_fifo_row_with_typed_receipt_identity() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        actor.deferred_prefix.arm(
            tokio::task::spawn_local(async { "PARENT_PREFIX_READY".to_string() }),
            false,
        );
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        {
            let mut state = await_with_timeout(actor.state.lock()).await;
            state
                .pending_inputs
                .push_back(super::super::support::user_item("running", "owner"));
            state.running_task = Some(super::super::support::running_task_stub("running"));
        }
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (respond_to, response_rx) = oneshot::channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("queued"),
            ActiveAgentMessageSource::Agent,
            ActiveAgentMessageOperation::Queue,
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        let receipt = receipt_rx.recv().await.expect("typed receipt handed off");
        assert_eq!(receipt.prompt_id, "parent-message-queued");
        assert_eq!(receipt.telemetry.parent_ctx.session_id, "test-parent");
        assert_eq!(
            *await_with_timeout(receipt.telemetry.parent_ctx.prompt_index.lock()).await,
            0,
        );
        let state = await_with_timeout(actor.state.lock()).await;
        assert_eq!(state.pending_inputs.len(), 2);
        let queued = state.pending_inputs.back().expect("queued input");
        assert_eq!(queued.prompt_id, receipt.prompt_id);
        assert!(queued.is_queue_protected());
        assert!(matches!(
            queued.input_origin.as_prompt_origin(),
            PromptOrigin::ParentAgentMessage {
                message_id,
                sender_session_id,
                ..
            } if message_id == "queued" && sender_session_id == "root-session"
        ));
        drop(state);
        let conversation = await_with_timeout(actor.chat_state_handle.get_conversation()).await;
        assert!(matches!(
            conversation.first(),
            Some(ConversationItem::User(user))
                if matches!(user.content.first(), Some(ContentPart::Text { text }) if text.as_ref() == "PARENT_PREFIX_READY")
        ));
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn safe_point_slots_reject_past_named_cap() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        set_running(&actor, "running").await;
        let lanes = [
            ActiveAgentMessageOperation::Steer,
            ActiveAgentMessageOperation::Interject,
        ];
        // One bound for both lanes: they fill it together and either one overflows it
        for (i, lane) in lanes
            .iter()
            .cycle()
            .take(super::MAX_PARENT_SAFE_POINT_SLOTS)
            .enumerate()
        {
            admit(&actor, message(&format!("cap-{i}")), *lane).await;
        }
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let (receipt_sink, _receipt_rx) = mpsc::channel(1);
        for (i, operation) in lanes.into_iter().enumerate() {
            let (respond_to, response_rx) = oneshot::channel();
            await_with_timeout(actor.admit_parent_agent_message_for_test(
                message(&format!("cap-overflow-{i}")),
                ActiveAgentMessageSource::Agent,
                operation,
                receipt_sink.clone(),
                respond_to,
                completion_tx.clone(),
            ))
            .await;
            assert_eq!(
                ActiveMessageAdmission::Rejected,
                admission_response(await_with_timeout(response_rx).await)
            );
        }
    }))
    .await;
}
