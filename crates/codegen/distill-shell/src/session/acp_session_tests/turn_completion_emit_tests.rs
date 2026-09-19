// Modified for Distill by Samuel Fajreldines, 2026.
//! Leader-side emission of the durable `TurnCompleted` terminal.
//!
//! These drive the SHIPPED handlers (`handle_completion` for normal and error completions, `cancel_running_task` for cancellation).
//! They assert on the notification the real `send_xai_notification` persists.
//! The terminal is the persisted and replayed twin of the fire-and-forget `prompt_complete`, so a re-attaching viewer can finalize from replay.

use super::support::*;
use super::turn_end_reporting_tests::RecordingLifecycle;
use super::*;
use tokio::sync::mpsc;

/// Drain every persistence message queued so far.
fn drain_persistence(rx: &mut mpsc::UnboundedReceiver<PersistenceMsg>) -> Vec<PersistenceMsg> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        out.push(msg);
    }
    out
}

fn is_durable_turn_completed(m: &PersistenceMsg) -> bool {
    matches!(
        m,
        PersistenceMsg::AppendUpdateDurablyAndAck {
            update: crate::session::storage::SessionUpdate::Xai(n),
            ..
        } if matches!(n.update, XaiSessionUpdate::TurnCompleted { .. })
    )
}

fn is_buffered_turn_completed(m: &PersistenceMsg) -> bool {
    matches!(
        m,
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
            if matches!(n.update, XaiSessionUpdate::TurnCompleted { .. })
    )
}

fn is_agent_message_delta(m: &PersistenceMsg) -> bool {
    matches!(
        m,
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n))
            if matches!(n.update, acp::SessionUpdate::AgentMessageChunk(_))
    )
}

/// Pull the `(prompt_id, stop_reason, agent_result, elapsed_ms)` of the first persisted `TurnCompleted` (buffered or durable rail), if any.
fn turn_completed_fields(
    msgs: &[PersistenceMsg],
) -> Option<(String, String, Option<String>, Option<u64>)> {
    msgs.iter().find_map(|m| {
        let (PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
        | PersistenceMsg::AppendUpdateDurablyAndAck {
            update: crate::session::storage::SessionUpdate::Xai(n),
            ..
        }) = m
        else {
            return None;
        };
        match &n.update {
            XaiSessionUpdate::TurnCompleted {
                prompt_id,
                stop_reason,
                agent_result,
                elapsed_ms,
                ..
            } => Some((
                prompt_id.clone(),
                stop_reason.clone(),
                agent_result.clone(),
                *elapsed_ms,
            )),
            _ => None,
        }
    })
}

/// A minimal front pending input matching `prompt_id`.
/// `queue_meta` is `None` so `handle_completion` does not also broadcast a `queue/changed`.
/// The completion receiver is returned so the caller keeps it alive.
pub(super) fn pending_input(prompt_id: &str) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let item = InputItem {
        prompt_id: prompt_id.to_string(),
        prompt_blocks: vec![],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: None,
        screen_mode: None,
        verbatim: false,
        json_schema: None,
        input_origin: InputOrigin::new(crate::session::PromptOrigin::User),
        task_wake_fallback: None,
        tool_overrides_update: None,
        respond_to,
        persist_ack: None,
        parsed_prompt_tx: None,
        initial_child_prompt_ready: None,
        queue_meta: None,
        queue_mutation_policy: QueueMutationPolicy::hidden(),
        send_now: false,
        traceparent: None,
    };
    (item, rx)
}

fn agent_msg_update(text: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(text.to_string()),
    )))
}

#[tokio::test(flavor = "current_thread")]
async fn completion_and_cancel_arbitrate_during_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for completion_wins in [true, false] {
                let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
                let (gateway_tx, mut gateway_rx) = mpsc::unbounded_channel();
                let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                let lifecycle = std::rc::Rc::new(RecordingLifecycle::default());
                let mut extensions =
                    distill_agent_lifecycle::LocalExtensionRegistryBuilder::default();
                extensions.turn_lifecycle_contributor(lifecycle.clone());
                actor.extension_registry = extensions.build();
                let actor = std::sync::Arc::new(actor);
                let report = actor.turn_report.claim_for_gate().expect("report claim");
                let resources = actor.agent.borrow().tool_bridge().shared_resources().await;
                let resources_guard = resources.lock().await;
                let (front, mut front_rx) = pending_input("front");
                let (next, _) = pending_input("next");
                {
                    let mut state = actor.state.lock().await;
                    state.pending_inputs.extend([front, next]);
                    state.running_task = Some(running_task_stub("front"));
                }
                let identity = completion_identity(&actor);
                let winner_identity = identity.clone();
                let winner = actor.clone();
                let finalizing = tokio::task::spawn_local(async move {
                    if completion_wins {
                        winner
                            .handle_completion(
                                "front".into(),
                                TurnEpoch::default(),
                                &winner_identity,
                                crate::session::commands::ok_end_turn(0, None),
                                Some(0),
                            )
                            .await
                    } else {
                        winner
                            .cancel_running_task(crate::session::CancelOptions {
                                trigger: Some(crate::session::CancelTrigger::CtrlC),
                                ..Default::default()
                            })
                            .await
                            .settled
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let state = actor.state.lock().await;
                        if state.finalization_gate.is_active() && state.running_task.is_none() {
                            break;
                        }
                        drop(state);
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("winner reaches gate-only cleanup");
                let (completion_tx, _) = mpsc::unbounded_channel();
                actor
                    .clone()
                    .maybe_start_running_task(completion_tx.clone())
                    .await;
                let winner_suppressed = actor.state.lock().await.notifications_suppressed;
                assert_eq!(winner_suppressed, !completion_wins);
                let winner_aborts = lifecycle.aborts.get();
                assert_eq!(winner_aborts, usize::from(!completion_wins));
                let loser_owned = if completion_wins {
                    actor
                        .cancel_running_task(crate::session::CancelOptions {
                            trigger: Some(crate::session::CancelTrigger::CtrlC),
                            ..Default::default()
                        })
                        .await
                        .settled
                } else {
                    actor
                        .handle_completion(
                            "front".into(),
                            TurnEpoch::default(),
                            &identity,
                            crate::session::commands::ok_end_turn(0, None),
                            Some(0),
                        )
                        .await
                };
                assert!(!loser_owned);
                assert_eq!(
                    actor.state.lock().await.notifications_suppressed,
                    winner_suppressed
                );
                assert_eq!(lifecycle.aborts.get(), winner_aborts);
                let response = front_rx.try_recv().expect("winner resolves front").unwrap();
                assert_eq!(
                    matches!(response.completion_kind, PromptCompletionKind::Completed),
                    completion_wins
                );
                let persisted = drain_persistence(&mut persistence_rx);
                assert_eq!(
                    persisted
                        .iter()
                        .filter(|m| is_durable_turn_completed(m))
                        .count(),
                    1
                );
                assert_eq!(
                    turn_completed_fields(&persisted).unwrap().1,
                    if completion_wins {
                        "end_turn"
                    } else {
                        "cancelled"
                    }
                );
                let expected_report = if completion_wins {
                    super::turn_report_slot::CommitOutcome::Reported
                } else {
                    super::turn_report_slot::CommitOutcome::LostToAnotherReporter
                };
                assert_eq!(report.commit(), expected_report);
                assert_eq!(std::iter::from_fn(|| gateway_rx.try_recv().ok()).count(), 1);
                drop(resources_guard);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(5), finalizing)
                        .await
                        .expect("winner finishes")
                        .expect("winner joins")
                );
                actor.clone().maybe_start_running_task(completion_tx).await;
                assert_eq!(actor.state.lock().await.running_prompt_id(), Some("next"));
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn normal_completion_persists_turn_completed_after_buffered_delta_flush() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            // Buffering is enabled with a long window so a streamed delta is HELD in the replay buffer until an explicit flush
            // That is the exact state the actor loop is in when a turn's completion arrives
            let (mut actor, mut event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.buffering_settings = Some(BufferingSettings {
                max_items: 100,
                max_bytes: 1_000_000,
                max_duration_ms: 3_600_000,
            });

            // A running turn with its prompt queued at the front.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p1".to_string());
            let (item, response_rx) = pending_input("p1");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p1"));
                state.pending_inputs.push_back(item);
            }

            // Stream the turn's last delta through the BUFFERED path: `send_update` enqueues it on `event_tx`
            // The replay buffer merges and HOLDS it; it is NOT persisted yet
            actor
                .send_update(agent_msg_update("last delta"), Some(1))
                .await;

            // Mirror the actor-owned replay buffer: drain the queued event(s) into it
            // The delta is then stranded exactly as it is when a completion reaches `run_session`'s completion branch
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(notification) = event {
                    let _ = replay_buffer.consume_chunk(notification);
                }
            }
            assert!(
                persistence_rx.try_recv().is_err(),
                "the buffered delta must not be persisted before the flush"
            );

            // This is the exact flush `run_session`'s completion branch performs before calling `handle_completion` The.
            // Cancel/Shutdown arms perform the same flush.
            // Removing it leaves the held delta stranded, so the terminal would be the only persisted update.
            if let Some(notification) = replay_buffer.flush() {
                actor.emit_buffered(notification).await;
            }

            let owned = actor
                .handle_completion(
                    "p1".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    Some(0),
                )
                .await;

            assert!(owned);
            assert!(
                response_rx
                    .await
                    .expect("completion resolves the front")
                    .is_ok()
            );
            let state = actor.state.lock().await;
            assert!(state.pending_inputs.is_empty());
            assert!(state.running_task.is_none());
            drop(state);

            let msgs = drain_persistence(&mut persistence_rx);

            // The terminal is persisted with the right fields
            let (prompt_id, stop_reason, agent_result, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a normal completion must persist a TurnCompleted");
            assert_eq!(prompt_id, "p1");
            assert_eq!(stop_reason, "end_turn");
            assert_eq!(agent_result, None);
            assert_eq!(elapsed_ms, Some(0));

            // The terminal lands after the flushed buffered delta on the same persistence stream
            let delta_idx = msgs
                .iter()
                .position(is_agent_message_delta)
                .expect("the flushed buffered delta must be persisted");
            let terminal_idx = msgs
                .iter()
                .position(is_durable_turn_completed)
                .expect("the terminal must be persisted via the durable append path");
            assert!(
                delta_idx < terminal_idx,
                "TurnCompleted must land in updates.jsonl after the flushed buffered delta"
            );
            assert!(
                !msgs.iter().any(is_buffered_turn_completed),
                "the terminal must not ride the buffered Update rail, or a power loss \
                 after the turn's flush barrier could keep the content but drop the terminal"
            );

            // Limitation: this calls `handle_completion` and mirrors the completion-branch flush; it does not drive the full `run_session` loop.
            // Injecting a real completion would need a mock-model turn.
            // The negative case is a buffered delta NEVER reaching persistence without the flush.
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn same_prompt_and_epoch_wrong_allocation_does_not_settle_successor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let live_epoch = TurnEpoch::default();
            let wrong_allocation = std::rc::Rc::new(());
            let (item, mut response_rx) = pending_input("same");
            let handle = tokio::task::spawn_local(std::future::pending::<()>()).abort_handle();
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
                state.running_task = Some(AgentTask::new_at_epoch("same", live_epoch, handle));
            }

            let owned = actor
                .handle_completion(
                    "same".to_string(),
                    live_epoch,
                    &wrong_allocation,
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    Some(0),
                )
                .await;

            assert!(!owned);
            assert!(matches!(
                response_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            let state = actor.state.lock().await;
            assert_eq!(
                state
                    .pending_inputs
                    .front()
                    .map(|item| item.prompt_id.as_str()),
                Some("same")
            );
            let task = state
                .running_task
                .as_ref()
                .expect("successor task remains installed");
            assert_eq!(task.prompt_id, "same");
            assert_eq!(task.epoch, live_epoch);
            drop(state);
            assert!(turn_completed_fields(&drain_persistence(&mut persistence_rx)).is_none());
            assert!(gateway_rx.try_recv().is_err());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn error_completion_persists_turn_completed_with_error_detail() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-err".to_string());
            let (item, _rx) = pending_input("p-err");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-err"));
                state.pending_inputs.push_back(item);
            }

            actor
                .handle_completion(
                    "p-err".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Err(acp::Error::internal_error().data("boom")),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, agent_result, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a failed completion must persist a TurnCompleted");
            assert_eq!(prompt_id, "p-err");
            assert_eq!(stop_reason, "error");
            assert_eq!(agent_result.as_deref(), Some("boom"));
            assert_eq!(elapsed_ms, Some(0));
        })
        .await;
}

fn turn_completed_error_kind(msgs: &[PersistenceMsg]) -> Option<String> {
    msgs.iter().find_map(|m| {
        let (PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
        | PersistenceMsg::AppendUpdateDurablyAndAck {
            update: crate::session::storage::SessionUpdate::Xai(n),
            ..
        }) = m
        else {
            return None;
        };
        match &n.update {
            XaiSessionUpdate::TurnCompleted { error_kind, .. } => error_kind.clone(),
            _ => None,
        }
    })
}

/// A max-tokens truncation failure stamps its typed kind in the durable terminal's `error_kind` field.
/// Replay/wake rails pick the truncation copy from it.
/// A failure without a kind marker stamps none.
#[tokio::test(flavor = "current_thread")]
async fn truncation_completion_stamps_error_kind_on_turn_completed_field() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-trunc".to_string());
            let (item, _rx) = pending_input("p-trunc");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-trunc"));
                state.pending_inputs.push_back(item);
            }

            actor
                .handle_completion(
                    "p-trunc".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Err(crate::sampling::error::map_sampling_err_to_acp(
                        crate::sampling::error::SamplingError::MaxTokensTruncation,
                    )),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, agent_result, _) = turn_completed_fields(&msgs)
                .expect("a truncation failure must persist a TurnCompleted");
            assert_eq!(prompt_id, "p-trunc");
            assert_eq!(stop_reason, "error");
            assert_eq!(
                agent_result.as_deref(),
                Some(crate::sampling::error::MAX_TOKENS_TRUNCATION_MESSAGE)
            );
            assert_eq!(
                turn_completed_error_kind(&msgs).as_deref(),
                Some("max_tokens_truncation"),
                "the terminal must carry the typed error kind"
            );
        })
        .await;
}

/// A failure without a kind marker carries no `error_kind` on its terminal.
#[tokio::test(flavor = "current_thread")]
async fn generic_error_completion_omits_error_kind_on_turn_completed_field() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-generic".to_string());
            let (item, _rx) = pending_input("p-generic");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-generic"));
                state.pending_inputs.push_back(item);
            }

            actor
                .handle_completion(
                    "p-generic".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Err(acp::Error::internal_error().data("boom")),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_some(),
                "a failed completion must persist a TurnCompleted"
            );
            assert_eq!(
                turn_completed_error_kind(&msgs),
                None,
                "a failure without a kind marker must not stamp error_kind"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn completion_without_elapsed_persists_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-none".to_string());
            let (item, _rx) = pending_input("p-none");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-none"));
                state.pending_inputs.push_back(item);
            }

            actor
                .handle_completion(
                    "p-none".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    None,
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (_, _, _, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a completion with no elapsed must persist a TurnCompleted");
            assert_eq!(elapsed_ms, None);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_persists_turn_completed_cancelled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // A running turn in flight.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (item, _rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(item);
            }

            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, agent_result, elapsed_ms) =
                turn_completed_fields(&msgs).expect("a cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            assert_eq!(agent_result, None);
            assert!(elapsed_ms.is_some(), "cancel must persist elapsed_ms");
            // Ctrl+C also stamps a trigger (informational; only send_now changes client behavior).
            assert_eq!(
                turn_completed_meta(&msgs).and_then(|m| m
                    .get("cancelTrigger")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)),
                Some("ctrl_c".to_string()),
                "a Ctrl+C cancel stamps its trigger on the terminal meta"
            );
        })
        .await;
}

/// A cancel that races ahead of promote has a pin but no running task.
/// Persist `elapsed_ms: None` so resume can tell unknown duration from a 0ms turn.
#[tokio::test(flavor = "current_thread")]
async fn cancel_without_running_task_persists_none_elapsed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (item, _rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
            }
            let goal_was_active = actor
                .tool_context
                .goal_loop_active_gate
                .load(std::sync::atomic::Ordering::Relaxed);
            let report = actor.turn_report.claim_for_gate().expect("report claim");
            let outcome = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(outcome.settled);

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a pin-only cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            assert_eq!(elapsed_ms, None);
            assert_eq!(
                report.commit(),
                super::turn_report_slot::CommitOutcome::Reported
            );
            assert_eq!(
                actor
                    .tool_context
                    .goal_loop_active_gate
                    .load(std::sync::atomic::Ordering::Relaxed),
                goal_was_active
            );
            let state = actor.state.lock().await;
            assert!(state.running_task.is_none());
            assert!(state.pending_inputs.is_empty());
        })
        .await;
}

/// Pull the first persisted `TurnCompleted`'s notification `_meta`, if any.
fn turn_completed_meta(msgs: &[PersistenceMsg]) -> Option<serde_json::Value> {
    msgs.iter().find_map(|m| {
        let (PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
        | PersistenceMsg::AppendUpdateDurablyAndAck {
            update: crate::session::storage::SessionUpdate::Xai(n),
            ..
        }) = m
        else {
            return None;
        };
        matches!(n.update, XaiSessionUpdate::TurnCompleted { .. })
            .then(|| n.meta.clone())
            .flatten()
    })
}

/// The completion-race window: `current_prompt_id` is already cleared while the finished front and its task slot are still queued.
/// The cancel identity must come from `running_task.prompt_id`, so the durable `TurnCompleted` (with `cancelTrigger=send_now`) is still persisted.
/// Without it viewers strand on "Waiting…" with no terminal.
#[tokio::test(flavor = "current_thread")]
async fn send_now_cancel_in_completion_race_window_still_persists_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // The pin is already cleared; only the task slot knows the turn
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;
            let (item, _rpc_rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(item);
            }

            let mut replay_buffer = ReplayBuffer::new(None);
            let _ = actor.cancel_turn_for_send_now(&mut replay_buffer).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a send-now cancel with a cleared pin must still persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            assert!(elapsed_ms.is_some());
            let meta = turn_completed_meta(&msgs).expect("terminal must carry _meta");
            assert_eq!(
                meta.get("cancelTrigger").and_then(|v| v.as_str()),
                Some("send_now"),
            );
        })
        .await;
}

/// A send-now cancel stamps `_meta.cancelTrigger == "send_now"` on both the durable `TurnCompleted` terminal and the cancelled turn's resolved RPC.
#[tokio::test(flavor = "current_thread")]
async fn send_now_cancel_stamps_cancel_trigger_on_turn_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (item, rpc_rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(item);
            }

            // The shipped send-now cancel path.
            let mut replay_buffer = ReplayBuffer::new(None);
            let _ = actor.cancel_turn_for_send_now(&mut replay_buffer).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a send-now cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            assert!(elapsed_ms.is_some());
            let meta = turn_completed_meta(&msgs).expect("terminal must carry _meta");
            assert_eq!(
                meta.get("cancelTrigger").and_then(|v| v.as_str()),
                Some("send_now"),
                "the terminal `_meta` must carry cancelTrigger=send_now"
            );

            let mut wire_meta = None;
            while let Ok(msg) = gateway_rx.try_recv() {
                if let distill_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref() == "x.ai/session_notification"
                    && let Ok(v) =
                        serde_json::from_str::<serde_json::Value>(args.request.params.get())
                    && j(j(&v, "update"), "sessionUpdate") == "turn_completed"
                {
                    wire_meta = Some(j(&v, "_meta").clone());
                }
            }
            let wire_meta = wire_meta.expect("the TurnCompleted terminal must reach the wire");
            assert_eq!(
                j(&wire_meta, "cancelTrigger"),
                "send_now",
                "wire `_meta.cancelTrigger` must be send_now"
            );

            let result = rpc_rx.await.expect("running turn RPC must resolve");
            match result {
                Ok(PromptTurnOk {
                    stop_reason: acp::StopReason::Cancelled,
                    completion_kind: PromptCompletionKind::Cancelled { context, .. },
                    ..
                }) => {
                    assert_eq!(
                        context.and_then(|c| c.trigger).as_deref(),
                        Some("send_now"),
                        "the running turn's completion context must carry the send-now trigger"
                    );
                }
                other => panic!("expected a Cancelled completion, got {other:?}"),
            }
        })
        .await;
}

/// A hook-denied cancel stamps `_meta.cancellationCategory == "HookDenied"` on the durable `TurnCompleted` terminal.
/// Shipped clients match that wire value to render the blocked-by-a-hook copy.
#[tokio::test(flavor = "current_thread")]
async fn hook_denied_cancel_stamps_cancellation_category_on_turn_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // A running turn with its prompt queued at the front (owned).
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-hook".to_string());
            let (item, _rx) = pending_input("p-hook");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-hook"));
                state.pending_inputs.push_back(item);
            }

            // The completion a `UserPromptSubmit` hook denial resolves the turn with (`TurnOutcome::Cancelled { category: HookDenied }`)
            actor
                .handle_completion(
                    "p-hook".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Cancelled {
                            category: Some(
                                crate::session::events::CancellationCategory::HookDenied,
                            ),
                            context: None,
                        },
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _, elapsed_ms) = turn_completed_fields(&msgs)
                .expect("a hook-denied cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "p-hook");
            assert_eq!(stop_reason, "cancelled");
            assert_eq!(elapsed_ms, Some(0));
            let meta = turn_completed_meta(&msgs).expect("terminal must carry _meta");
            assert_eq!(
                meta.get("cancellationCategory").and_then(|v| v.as_str()),
                Some("HookDenied"),
                "the terminal `_meta` must pin the hook-denied category encoding"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn no_output_rewind_cancel_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // An in-flight turn with no output yet at the front of the queue.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("rw".to_string());
            let (item, _rx) = pending_input("rw");
            {
                let mut state = actor.state.lock().await;
                state.rewindable = true;
                state.running_task = Some(running_task_stub("rw"));
                state.pending_inputs.push_back(item);
            }

            // A RewindIfNoOutput cancel on a rewindable turn takes the rewind path: the turn is treated as UNSENT
            // Matching the legacy emit_turn_ended, NO durable terminal is emitted, else replay would finalize a turn that was rewound
            let _ = actor.cancel_running_task(crate::session::CancelOptions { history: crate::session::CancelHistoryDisposition::RewindIfNoOutput { prompt_id: None }, user_initiated: true, ..Default::default() }).await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a rewind cancel before any output treats the turn as unsent and must persist no TurnCompleted"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn removed_from_queue_completion_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-removed".to_string());
            let (item, _rx) = pending_input("p-removed");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-removed"));
                state.pending_inputs.push_back(item);
            }

            // A removed queued prompt never started a turn, so it must emit no durable terminal even though it resolves with `Cancelled`
            actor
                .handle_completion(
                    "p-removed".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a RemovedFromQueue completion never ran a turn and must emit no terminal"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_prompt_completion_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Reproduce the cancel race: the Cancel path already finalized and dequeued the turn (current_prompt_id cleared, queue empty)
            // A stale `(prompt, EndTurn)` completion now lands on the unknown-prompt branch of handle_completion
            // It must NOT emit a second terminal; the Cancel path already emitted TurnCompleted{cancelled} for it
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;

            actor
                .handle_completion(
                    "already-finalized".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    Some(0),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a stale completion for a prompt the Cancel path already finalized must NOT \
                 emit a second TurnCompleted (the double-emit bug)"
            );
        })
        .await;
}

// ── Analytics turn delta ──────────────────────────────────────────────────────.
// The turn delta posts once per turn from `emit_turn_completed`, behind the finalization lease.
// These drive the actor's own install path (`maybe_start_running_task` → `AgentTask::new_prompt`) and settle the turn the way the run loop does, so the turn-open, cancel and completion order is the shipped one.

/// Localhost turn-deltas endpoint; every posted body is forwarded on the returned channel.
async fn turn_delta_sink() -> (
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    use axum::{Json, Router, routing::post};
    let (tx, rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let router = Router::new().route(
        "/v1/sessions/{id}/turn-deltas",
        post(move |Json(body): Json<serde_json::Value>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(body);
                Json(serde_json::json!({
                    "sessionId": "test-actor",
                    "turnNumber": 1,
                    "recordedAt": chrono::Utc::now(),
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, rx)
}

/// A telemetry-enabled feedback manager that posts turn deltas to `addr`.
#[allow(clippy::disallowed_methods)] // test client hits a localhost mock
fn turn_delta_feedback_manager(addr: std::net::SocketAddr) -> Arc<FeedbackManager> {
    Arc::new(FeedbackManager::new(
        "test-actor",
        Some(crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            None,
        )),
        FeedbackManagerConfig {
            telemetry_enabled: true,
            ..Default::default()
        },
    ))
}

async fn next_turn_delta(rx: &mut mpsc::UnboundedReceiver<serde_json::Value>) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("a turn delta must be posted")
        .expect("delta channel open")
}

async fn assert_no_turn_delta(rx: &mut mpsc::UnboundedReceiver<serde_json::Value>, why: &str) {
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "{why}"
    );
}

/// Queue `prompt_id` as a user row and promote it through the actor's own install path.
/// Returns the prompt's RPC receiver; the promoted task is running (or about to) when this returns.
async fn promote_prompt(
    actor: &Arc<SessionActor>,
    prompt_id: &str,
    completion_tx: &mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
) -> oneshot::Receiver<PromptTurnResult> {
    let (item, rx) = user_item_with_rx(prompt_id, "owner");
    actor.state.lock().await.pending_inputs.push_back(item);
    actor
        .clone()
        .maybe_start_running_task(completion_tx.clone())
        .await;
    assert_eq!(
        actor.state.lock().await.running_prompt_id(),
        Some(prompt_id),
        "the row must have promoted"
    );
    rx
}

/// Await the turn task's completion message, as the run loop does.
async fn next_completion(
    completion_rx: &mut mpsc::UnboundedReceiver<super::turn_task::TurnCompletionMsg>,
) -> super::turn_task::TurnCompletionMsg {
    tokio::time::timeout(std::time::Duration::from_secs(60), completion_rx.recv())
        .await
        .expect("the turn task must finish")
        .expect("completion channel open")
}

/// Settle a completion the way the run loop does; returns whether it owned the turn.
async fn settle(actor: &SessionActor, msg: super::turn_task::TurnCompletionMsg) -> bool {
    actor
        .handle_completion(
            msg.prompt_id,
            msg.epoch,
            &msg.task_identity,
            msg.result,
            msg.elapsed_ms,
        )
        .await
}

fn esc() -> crate::session::CancelOptions {
    crate::session::CancelOptions {
        cancel_subagents: true,
        trigger: Some(crate::session::CancelTrigger::Esc),
        user_initiated: true,
        ..Default::default()
    }
}

/// A cancel that wins the finalization lease before the turn task ever runs still posts the.
/// turn's own row: the turn opens at install (`AgentTask::new_prompt`), not inside the spawned.
/// future, so the row carries this turn's number and the cancellation. An idle Esc afterwards.
#[test]
fn cancel_before_the_turn_task_runs_posts_this_turns_cancelled_delta() {
    use super::disk_full_tests::{
        actor_with_mock_sampler_configured, block_on_session, current_thread_local,
    };
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (addr, mut delta_rx) = turn_delta_sink().await;
            let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            super::support::drain_persistence(persistence_rx);
            let actor = actor_with_mock_sampler_configured(
                &server,
                persistence_tx,
                gateway_tx,
                None,
                |actor| actor.feedback_manager = turn_delta_feedback_manager(addr),
            )
            .await;
            let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
            // A previous turn's mode is still in the trackers; install must replace it with
            // this request's mode before the future resolves the final one.
            *actor.turn_start_prompt_mode.lock() = PromptMode::Plan;
            *actor.turn_prompt_mode.lock() = PromptMode::Plan;

            // Turn 1: Esc lands before the promoted task's future is first polled.
            let rx = promote_prompt(&actor, "p1", &completion_tx).await;
            let outcome = actor.cancel_running_task(esc()).await;
            assert!(outcome.settled);
            assert_eq!(
                rx.await
                    .expect("cancel resolves the RPC")
                    .map(|ok| ok.stop_reason),
                Ok(acp::StopReason::Cancelled)
            );
            let delta = next_turn_delta(&mut delta_rx).await;
            assert_eq!(j(&delta, "turnNumber"), 1, "{delta}");
            assert_eq!(j(&delta, "requestId"), "p1");
            assert_eq!(j(&delta, "turnOutcome"), "cancelled");
            assert_eq!(j(&delta, "deltaCancellations"), 1);
            assert!(j(&delta, "turnDurationMs").is_number(), "{delta}");
            assert_eq!(
                j(j(&delta, "metadata"), "startPromptMode"),
                "agent",
                "the row carries this request's mode, not the previous turn's: {delta}"
            );

            // Idle Esc: no turn to end, so nothing is recorded or posted.
            let outcome = actor.cancel_running_task(esc()).await;
            assert!(!outcome.turn_stopped);
            assert_no_turn_delta(&mut delta_rx, "an idle cancel has no turn to report").await;

            // Turn 2 completes through the mock sampler.
            let rx = promote_prompt(&actor, "p2", &completion_tx).await;
            let msg = next_completion(&mut completion_rx).await;
            assert!(settle(&actor, msg).await);
            let ok = rx
                .await
                .expect("completion resolves the RPC")
                .expect("the turn completes");
            assert_eq!(ok.stop_reason, acp::StopReason::EndTurn);
            let snapshot = ok
                .turn_snapshot
                .expect("a completed turn carries its snapshot");
            assert_eq!(snapshot.turn_input_tokens, 10);
            assert_eq!(snapshot.turn_output_tokens, 5);
            let delta = next_turn_delta(&mut delta_rx).await;
            assert_eq!(j(&delta, "turnNumber"), 2, "{delta}");
            assert_eq!(j(&delta, "requestId"), "p2");
            assert_eq!(j(&delta, "turnOutcome"), "completed");
            assert_eq!(
                j(&delta, "deltaCancellations"),
                0,
                "the idle Esc must not land on this turn's row"
            );
            assert!(j(&delta, "turnDurationMs").is_number(), "{delta}");
            assert_no_turn_delta(&mut delta_rx, "one row per turn").await;
        });
    });
}

/// Whichever path settles the turn posts its one row with the settled outcome: a completed
/// turn's terminal posts the task's snapshot; max-turns (the turn future's own cancelled ending)
/// and a failed sampler take theirs at the terminal, with no user cancellation counted.
#[test]
fn every_terminal_posts_one_delta_with_the_settled_outcome() {
    use super::disk_full_tests::{
        TODO_ARGS, actor_with_mock_sampler_configured, block_on_session, current_thread_local,
    };
    use distill_test_support::sse::{
        responses_api_reasoning_then_tool_call_events, responses_api_script_exact,
    };
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_reasoning_then_tool_call_events(
                    "poll",
                    "max-turns-call",
                    "todo_write",
                    TODO_ARGS,
                    "test",
                )),
            );
            // A 400 is terminal for the turn; a 5xx would be retried against an empty script queue.
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::json(
                    400,
                    serde_json::json!({ "error": { "message": "bad request", "type": "invalid_request_error" } }),
                ),
            );
            let (addr, mut delta_rx) = turn_delta_sink().await;
            let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            super::support::drain_persistence(persistence_rx);
            let actor = actor_with_mock_sampler_configured(
                &server,
                persistence_tx,
                gateway_tx,
                Some(1),
                |actor| actor.feedback_manager = turn_delta_feedback_manager(addr),
            )
            .await;
            let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

            let expected = [
                ("p-completed", "completed", 0),
                ("p-max-turns", "cancelled", 1),
                ("p-failed", "error", 0),
            ];
            for (turn, (prompt_id, outcome, tool_calls)) in (1..).zip(expected) {
                let rx = promote_prompt(&actor, prompt_id, &completion_tx).await;
                let msg = next_completion(&mut completion_rx).await;
                assert!(settle(&actor, msg).await, "{prompt_id}");
                let result = rx.await.expect("the terminal resolves the RPC");
                match outcome {
                    "completed" => assert!(
                        result.as_ref().is_ok_and(|ok| ok.turn_snapshot.is_some()),
                        "{prompt_id}: a completed turn carries its snapshot"
                    ),
                    "cancelled" => assert!(
                        result.as_ref().is_ok_and(|ok| matches!(
                            ok.completion_kind,
                            PromptCompletionKind::MaxTurnsReached { .. }
                        ) && ok.turn_snapshot.is_none()),
                        "{prompt_id}: {result:?}"
                    ),
                    _ => assert!(result.is_err(), "{prompt_id}: {result:?}"),
                }
                let delta = next_turn_delta(&mut delta_rx).await;
                assert_eq!(j(&delta, "turnNumber"), turn, "{prompt_id}: {delta}");
                assert_eq!(j(&delta, "requestId"), prompt_id);
                assert_eq!(j(&delta, "turnOutcome"), outcome, "{prompt_id}");
                assert_eq!(j(&delta, "deltaToolCalls"), tool_calls, "{prompt_id}");
                assert_eq!(
                    j(&delta, "deltaCancellations"),
                    0,
                    "{prompt_id}: not a user cancel"
                );
                assert!(
                    j(&delta, "turnDurationMs").is_number(),
                    "{prompt_id}: {delta}"
                );
            }
            assert_no_turn_delta(&mut delta_rx, "one row per turn").await;
        });
    });
}

/// A cancel that wins the lease against an already finished task settles the turn as cancelled
/// and posts that one row; the task's completion is then stale and posts nothing, so the backend
/// never sees two rows for one turn.
#[test]
fn cancel_racing_a_finished_task_posts_one_cancelled_delta() {
    use super::disk_full_tests::{
        actor_with_mock_sampler_configured, block_on_session, current_thread_local,
    };
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (addr, mut delta_rx) = turn_delta_sink().await;
            let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            super::support::drain_persistence(persistence_rx);
            let actor = actor_with_mock_sampler_configured(
                &server,
                persistence_tx,
                gateway_tx,
                None,
                |actor| actor.feedback_manager = turn_delta_feedback_manager(addr),
            )
            .await;
            let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

            let rx = promote_prompt(&actor, "p-race", &completion_tx).await;
            // The task finished (its completion is queued) but the run loop has not settled it.
            let msg = next_completion(&mut completion_rx).await;
            let outcome = actor.cancel_running_task(esc()).await;
            assert!(outcome.settled);
            assert_eq!(
                rx.await
                    .expect("cancel resolves the RPC")
                    .map(|ok| ok.stop_reason),
                Ok(acp::StopReason::Cancelled)
            );
            let delta = next_turn_delta(&mut delta_rx).await;
            assert_eq!(j(&delta, "turnNumber"), 1, "{delta}");
            assert_eq!(j(&delta, "turnOutcome"), "cancelled");
            assert_eq!(j(&delta, "deltaCancellations"), 1);

            assert!(!settle(&actor, msg).await, "the completion lost the lease");
            assert_no_turn_delta(&mut delta_rx, "a stale completion posts no second row").await;
        });
    });
}

/// A multi-round turn (a Stop hook keeps the agent working once) posts one row spanning every
/// round: the duration runs from install to terminal (it includes the gate wait) and the token
/// sums cover both rounds.
#[test]
fn multi_round_turn_posts_one_delta_spanning_every_round() {
    use super::disk_full_tests::{
        actor_with_mock_sampler_configured, block_on_session, current_thread_local,
    };
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    const GATE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            for text in ["first", "second"] {
                server.enqueue_response(
                    "/v1/responses",
                    ScriptedResponse::sse(responses_api_script_exact(text, "test")),
                );
            }
            let (addr, mut delta_rx) = turn_delta_sink().await;
            let (gateway_tx, mut gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            super::support::drain_persistence(persistence_rx);

            // The Stop hook keeps the agent working once, after `GATE_WAIT`, then lets it stop.
            tokio::task::spawn_local(async move {
                let mut kept_working = false;
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let reply = if kept_working {
                                serde_json::json!({})
                            } else {
                                kept_working = true;
                                tokio::time::sleep(GATE_WAIT).await;
                                serde_json::json!({ "decision": "deny", "systemMessage": "more" })
                            };
                            let body: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&reply).unwrap().into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(body)));
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let actor = actor_with_mock_sampler_configured(
                &server,
                persistence_tx,
                gateway_tx,
                None,
                |actor| {
                    actor.feedback_manager = turn_delta_feedback_manager(addr);
                    let mut hooks = crate::extensions::hooks::ClientHooks::new();
                    hooks.insert(
                        distill_hooks::event::HookEventName::Stop,
                        vec![crate::extensions::hooks::ClientHookGroup {
                            matcher: None,
                            callback_ids: vec!["cb".to_string()],
                            timeout: None,
                        }],
                    );
                    *actor.client_hooks.borrow_mut() = hooks;
                },
            )
            .await;
            let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

            let rx = promote_prompt(&actor, "p-two-rounds", &completion_tx).await;
            let msg = next_completion(&mut completion_rx).await;
            assert!(settle(&actor, msg).await);
            let ok = rx
                .await
                .expect("completion resolves the RPC")
                .expect("the turn completes after the hook lets it stop");
            assert_eq!(ok.stop_reason, acp::StopReason::EndTurn);
            let snapshot = ok
                .turn_snapshot
                .expect("a completed turn carries its snapshot");
            assert_eq!(snapshot.turn_input_tokens, 20, "both rounds' prompts");
            assert_eq!(snapshot.turn_output_tokens, 10, "both rounds' completions");

            let delta = next_turn_delta(&mut delta_rx).await;
            assert_eq!(j(&delta, "turnNumber"), 1, "{delta}");
            assert_eq!(j(&delta, "turnOutcome"), "completed");
            assert_eq!(j(&delta, "deltaAssistantMessages"), 2, "{delta}");
            let duration_ms = j(&delta, "turnDurationMs")
                .as_u64()
                .expect("rows carry a duration");
            assert!(
                duration_ms >= GATE_WAIT.as_millis() as u64,
                "the row spans the whole turn, gate wait included: {duration_ms}ms"
            );
            assert_no_turn_delta(&mut delta_rx, "one row per turn, not one per round").await;
        });
    });
}
