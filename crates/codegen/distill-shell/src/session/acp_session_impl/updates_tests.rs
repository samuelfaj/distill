// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::{
    create_test_actor, create_test_actor_with_chat_persistence, set_goal_harness_for_tests,
};
use super::*;
use crate::session::storage::prepare_replay_lines;
use crate::test_support::lsp_runtime::test_gateway;
use distill_tools::implementations::distill::task::{
    backend::ChannelBackend, types::SubagentEvent,
};

const SUBAGENT_ID: &str = "subagent-reactivated";
const RESUME_PARENT_ID: &str = "resume-parent";
const GOAL_ID: &str = "goal-reactivation";

fn attempt(entropy: u128) -> String {
    distill_message_delivery_core::AttemptId::mint(entropy).to_string()
}

fn spawned(
    subagent_id: &str,
    attempt_id: Option<&str>,
    resumed_from: Option<&str>,
) -> XaiSessionUpdate {
    XaiSessionUpdate::SubagentSpawned {
        subagent_id: subagent_id.to_owned(),
        attempt_id: attempt_id.map(str::to_owned),
        parent_session_id: "test-actor".to_owned(),
        parent_prompt_id: None,
        child_session_id: subagent_id.to_owned(),
        subagent_type: "general-purpose".to_owned(),
        description: "continue work".to_owned(),
        effective_context_source: Some("resumed".to_owned()),
        context_normalized: false,
        capability_mode: None,
        persona: None,
        role: None,
        model: Some("test-model".to_owned()),
        resumed_from: resumed_from.map(str::to_owned),
        workflow_run_id: None,
        agent_address: None,
    }
}

fn finished_for(
    subagent_id: &str,
    attempt_id: Option<&str>,
    tokens_used: u64,
    output: Option<&str>,
) -> XaiSessionUpdate {
    XaiSessionUpdate::SubagentFinished {
        subagent_id: subagent_id.to_owned(),
        attempt_id: attempt_id.map(str::to_owned),
        child_session_id: subagent_id.to_owned(),
        status: "completed".to_owned(),
        error: None,
        tool_calls: 1,
        turns: 1,
        duration_ms: 1,
        tokens_used,
        output: output.map(str::to_owned),
        will_wake: false,
    }
}

fn finished(attempt_id: Option<&str>, tokens_used: u64) -> XaiSessionUpdate {
    finished_for(SUBAGENT_ID, attempt_id, tokens_used, None)
}

fn progress(subagent_id: &str, attempt_id: Option<&str>, tokens_used: u64) -> XaiSessionUpdate {
    XaiSessionUpdate::SubagentProgress {
        subagent_id: subagent_id.to_owned(),
        attempt_id: attempt_id.map(str::to_owned),
        parent_session_id: "test-actor".to_owned(),
        child_session_id: subagent_id.to_owned(),
        duration_ms: 2,
        turn_count: 2,
        tool_call_count: 2,
        tokens_used,
        context_window_tokens: 256_000,
        context_usage_pct: 1,
        tools_used: vec!["read_file".to_owned()],
        error_count: 0,
        active_model: None,
        active_reasoning_effort: None,
    }
}

async fn notify(actor: &SessionActor, update: XaiSessionUpdate) {
    actor
        .handle_xai_session_notification(XaiSessionNotification {
            session_id: acp::SessionId::new("test-actor"),
            update,
            meta: None,
        })
        .await;
}

fn persisted_updates(
    persistence_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) -> Vec<XaiSessionUpdate> {
    std::iter::from_fn(|| persistence_rx.try_recv().ok())
        .filter_map(|message| match message {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notification)) => {
                Some(notification.update)
            }
            _ => None,
        })
        .collect()
}

fn persisted_finishes(
    persistence_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) -> Vec<XaiSessionUpdate> {
    persisted_updates(persistence_rx)
        .into_iter()
        .filter(|update| matches!(update, XaiSessionUpdate::SubagentFinished { .. }))
        .collect()
}

fn test_persistence_sampling_client() -> crate::sampling::Client {
    crate::sampling::Client::new(distill_sampler::SamplerConfig {
        api_key: Some("test-key".to_owned()),
        base_url: "http://localhost".to_owned(),
        model: "test-model".to_owned(),
        context_window: 256_000,
        ..Default::default()
    })
    .expect("sampling client")
}

async fn actor_with_goal() -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    set_goal_harness_for_tests(&actor);
    actor.goal_tracker.lock().create_goal(
        GOAL_ID.to_owned(),
        "second attempt".to_owned(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_owned(),
        None,
    );
    (actor, persistence_rx)
}

#[tokio::test]
async fn orphan_finish_without_token_record_persists_once_and_stops_reheal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            use crate::session::storage::StorageAdapter;

            let session_dir = tempfile::TempDir::new().expect("session dir");
            let info = crate::session::info::Info {
                id: acp::SessionId::new("test-actor"),
                cwd: session_dir.path().to_string_lossy().into_owned(),
            };
            let persistence = crate::session::persistence::new_with_explicit_dir(
                &info,
                session_dir.path().to_path_buf(),
                acp::ModelId::new("test-model"),
                test_persistence_sampling_client(),
                "test-model".to_owned(),
                crate::session::persistence::ExplicitSessionOpen::New {
                    identity: None,
                    next_trace_turn: None,
                },
            )
            .await
            .expect("persistence actor");
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (mut actor, _) = create_test_actor_with_chat_persistence(
                0,
                256_000,
                85,
                gateway_tx,
                persistence.tx.clone(),
                Box::new(
                    crate::session::chat_persistence::ChannelChatPersistence::new(
                        persistence.tx.clone(),
                    ),
                ),
            )
            .await;
            actor.session_info = info.clone();
            assert!(actor.subagent_token_records.lock().is_empty());

            let orphan_id = "crash-resume-orphan";
            let raw_spawn = XaiSessionNotification {
                session_id: info.id.clone(),
                update: spawned(orphan_id, None, None),
                meta: None,
            };
            let adapter =
                crate::session::storage::jsonl::JsonlStorageAdapter::with_explicit_session_dir(
                    session_dir.path().to_path_buf(),
                );
            adapter
                .append_update(
                    &info,
                    &crate::session::storage::SessionUpdate::Xai(Box::new(raw_spawn)),
                )
                .await
                .expect("persist orphan spawn");
            let finish = finished_for(orphan_id, None, 0, None);
            notify(&actor, finish.clone()).await;
            let (respond_to, response_rx) = tokio::sync::oneshot::channel();
            persistence
                .tx
                .send(PersistenceMsg::FlushAndAck { respond_to })
                .expect("flush dispatch");
            response_rx.await.expect("flush reply").expect("flush");

            let updates = std::fs::read_to_string(session_dir.path().join("updates.jsonl"))
                .expect("updates jsonl");
            assert_eq!(updates.matches("subagent_finished").count(), 1);
            assert!(
                prepare_replay_lines(&updates, None)
                    .unfinished_subagents
                    .is_empty()
            );

            let before = std::fs::read(session_dir.path().join("updates.jsonl"))
                .expect("updates before second load");
            let prepared =
                prepare_replay_lines(std::str::from_utf8(&before).expect("updates utf8"), None);
            crate::agent::subagent::reconcile_orphaned_subagents_with_backend(
                &prepared.unfinished_subagents,
                &ChannelBackend::new(tokio::sync::mpsc::unbounded_channel().0),
                session_dir.path(),
                info.id.0.as_ref(),
                &test_gateway(),
                None,
                crate::agent::subagent::ORPHAN_RECONCILE_REASON,
                Arc::new(tokio::sync::Mutex::new(())),
            )
            .await;
            assert_eq!(
                std::fs::read(session_dir.path().join("updates.jsonl"))
                    .expect("updates after second load"),
                before
            );
        })
        .await;
}

#[tokio::test]
async fn duplicate_spawn_after_finish_is_not_persisted_or_rehealed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let session_dir = tempfile::TempDir::new().expect("session dir");
            let info = crate::session::info::Info {
                id: acp::SessionId::new("test-actor"),
                cwd: session_dir.path().to_string_lossy().into_owned(),
            };
            let persistence = crate::session::persistence::new_with_explicit_dir(
                &info,
                session_dir.path().to_path_buf(),
                acp::ModelId::new("test-model"),
                test_persistence_sampling_client(),
                "test-model".to_owned(),
                crate::session::persistence::ExplicitSessionOpen::New {
                    identity: None,
                    next_trace_turn: None,
                },
            )
            .await
            .expect("persistence actor");
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (mut actor, _session_event_rx) = create_test_actor_with_chat_persistence(
                0,
                256_000,
                85,
                gateway_tx,
                persistence.tx.clone(),
                Box::new(
                    crate::session::chat_persistence::ChannelChatPersistence::new(
                        persistence.tx.clone(),
                    ),
                ),
            )
            .await;
            actor.session_info = info.clone();
            let child_attempt = attempt(0x10);
            let spawn = spawned(SUBAGENT_ID, Some(&child_attempt), None);
            let finish = finished_for(
                SUBAGENT_ID,
                Some(&child_attempt),
                20,
                Some("completed payload"),
            );

            notify(&actor, spawn).await;
            notify(&actor, finish).await;
            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            let (respond_to, response_rx) = tokio::sync::oneshot::channel();
            persistence
                .tx
                .send(PersistenceMsg::FlushAndAck { respond_to })
                .expect("flush dispatch");
            response_rx.await.expect("flush reply").expect("flush");
            let before = std::fs::read(session_dir.path().join("updates.jsonl"))
                .expect("updates before load");
            let prepared = prepare_replay_lines(std::str::from_utf8(&before).unwrap(), None);
            assert!(prepared.unfinished_subagents.is_empty());
            crate::agent::subagent::reconcile_orphaned_subagents_with_backend(
                &prepared.unfinished_subagents,
                &ChannelBackend::new(tokio::sync::mpsc::unbounded_channel().0),
                session_dir.path(),
                info.id.0.as_ref(),
                &test_gateway(),
                None,
                crate::agent::subagent::ORPHAN_RECONCILE_REASON,
                Arc::new(tokio::sync::Mutex::new(())),
            )
            .await;
            assert_eq!(
                std::fs::read(session_dir.path().join("updates.jsonl")).unwrap(),
                before
            );
            let outcome = actor
                .subagent_token_records
                .lock()
                .get_mut(SUBAGENT_ID)
                .expect("subagent record")
                .finish(
                    Some(&child_attempt),
                    SubagentFinishPayload {
                        child_session_id: SUBAGENT_ID.to_owned(),
                        status: "completed".to_owned(),
                        error: None,
                        tool_calls: 1,
                        turns: 1,
                        duration_ms: 1,
                        tokens_used: 20,
                        output: Some("completed payload".to_owned()),
                        will_wake: false,
                    },
                );
            assert_eq!(outcome, SubagentFinishOutcome::Duplicate);
        })
        .await;
}

#[tokio::test]
async fn reactivation_preserves_tokens_and_rejects_previous_attempt_updates() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let first_attempt = attempt(0x11);
            let second_attempt = attempt(0x22);

            notify(&actor, spawned(SUBAGENT_ID, Some(&first_attempt), None)).await;
            notify(&actor, finished(Some(&first_attempt), 120)).await;
            notify(&actor, spawned(SUBAGENT_ID, Some(&second_attempt), None)).await;
            notify(&actor, progress(SUBAGENT_ID, Some(&second_attempt), 180)).await;
            assert_eq!(actor.goal_tokens(0), (180, 120));
            while persistence_rx.try_recv().is_ok() {}

            notify(&actor, spawned(SUBAGENT_ID, Some(&first_attempt), None)).await;
            notify(&actor, progress(SUBAGENT_ID, Some(&first_attempt), 500)).await;
            notify(&actor, finished(None, 500)).await;
            notify(&actor, finished(Some(&first_attempt), 500)).await;

            assert_eq!(actor.goal_tokens(0), (180, 120));
            assert!(persistence_rx.try_recv().is_err());
        })
        .await;
}

#[tokio::test]
async fn same_attempt_finish_corrects_synthetic_completion_and_persists_output() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let child_attempt = attempt(0x44);

            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            notify(&actor, progress(SUBAGENT_ID, Some(&child_attempt), 90)).await;
            let synthetic = finished(Some(&child_attempt), 0);
            notify(&actor, synthetic.clone()).await;
            let real = finished_for(
                SUBAGENT_ID,
                Some(&child_attempt),
                120,
                Some("persisted result"),
            );
            notify(&actor, real.clone()).await;

            assert_eq!(actor.goal_tokens(0), (120, 120));
            assert_eq!(
                persisted_finishes(&mut persistence_rx),
                vec![synthetic, real]
            );
        })
        .await;
}

#[tokio::test]
async fn same_token_finish_with_output_corrects_healed_completion() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let child_attempt = attempt(0x45);

            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            let healed = finished(Some(&child_attempt), 0);
            notify(&actor, healed.clone()).await;
            let real = finished_for(
                SUBAGENT_ID,
                Some(&child_attempt),
                0,
                Some("persisted result"),
            );
            notify(&actor, real.clone()).await;

            assert_eq!(persisted_finishes(&mut persistence_rx), vec![healed, real]);
        })
        .await;
}

#[tokio::test]
async fn byte_identical_finish_is_dropped() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let child_attempt = attempt(0x46);

            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            let finish = finished_for(
                SUBAGENT_ID,
                Some(&child_attempt),
                0,
                Some("persisted result"),
            );
            notify(&actor, finish.clone()).await;
            notify(&actor, finish.clone()).await;

            assert_eq!(persisted_finishes(&mut persistence_rx), vec![finish]);
        })
        .await;
}

#[tokio::test]
async fn legacy_lifecycle_remains_single_attempt() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _persistence_rx) = actor_with_goal().await;

            notify(&actor, spawned(SUBAGENT_ID, None, None)).await;
            notify(&actor, progress(SUBAGENT_ID, None, 90)).await;
            assert_eq!(actor.goal_tokens(0), (90, 0));

            notify(&actor, finished(None, 100)).await;
            assert_eq!(actor.goal_tokens(0), (100, 100));
        })
        .await;
}

#[tokio::test]
async fn replayed_attempt_orphan_heal_closes_record_and_persists_finish() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let child_attempt = attempt(0x33);
            let raw = serde_json::to_string(&XaiSessionNotification {
                session_id: acp::SessionId::new("test-actor"),
                update: spawned(SUBAGENT_ID, Some(&child_attempt), None),
                meta: None,
            })
            .expect("spawn serializes");
            let prepared = prepare_replay_lines(&raw, None);
            let replayed_spawn = prepared
                .unfinished_subagents
                .first()
                .expect("spawn is unfinished");
            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            notify(&actor, progress(SUBAGENT_ID, Some(&child_attempt), 90)).await;

            let session_dir = tempfile::TempDir::new().expect("session dir");
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let backend = ChannelBackend::new(event_tx);
            let respond = tokio::task::spawn_local(async move {
                let Some(SubagentEvent::Inspect(request)) = event_rx.recv().await else {
                    panic!("expected inspect request");
                };
                let _ = request.respond_to.send(None);
            });
            let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::agent::subagent::reconcile_orphaned_subagents_with_backend(
                std::slice::from_ref(replayed_spawn),
                &backend,
                session_dir.path(),
                "test-actor",
                &test_gateway(),
                Some(&cmd_tx),
                crate::agent::subagent::ORPHAN_RECONCILE_REASON,
                Arc::new(tokio::sync::Mutex::new(())),
            )
            .await;
            respond.await.expect("inspect responder");
            let SessionCommand::XaiSessionNotification { notification } =
                cmd_rx.recv().await.expect("orphan finish")
            else {
                panic!("expected subagent notification");
            };
            let finish = notification.update;
            assert!(matches!(
                &finish,
                XaiSessionUpdate::SubagentFinished { attempt_id, .. }
                    if attempt_id.as_deref() == Some(child_attempt.as_str())
            ));
            notify(&actor, finish.clone()).await;

            assert_eq!(actor.goal_tokens(0), (90, 90));
            assert!(
                actor
                    .subagent_token_records
                    .lock()
                    .get(SUBAGENT_ID)
                    .expect("subagent token record")
                    .active_attempt
                    .is_none()
            );
            assert!(actor.goal_tracker.lock().snapshot().is_some_and(|goal| {
                goal.live_subagent_tokens == 0
                    && goal.live_turn_count == 0
                    && goal.live_tool_call_count == 0
            }));
            assert_eq!(persisted_finishes(&mut persistence_rx), vec![finish]);
        })
        .await;
}

#[tokio::test]
async fn legacy_finish_closes_current_attempt_and_persists() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut persistence_rx) = actor_with_goal().await;
            let child_attempt = attempt(0x33);

            notify(&actor, spawned(SUBAGENT_ID, Some(&child_attempt), None)).await;
            notify(&actor, progress(SUBAGENT_ID, Some(&child_attempt), 90)).await;
            notify(&actor, finished(None, 100)).await;

            assert_eq!(actor.goal_tokens(0), (100, 100));
            assert!(
                actor
                    .subagent_token_records
                    .lock()
                    .get(SUBAGENT_ID)
                    .expect("subagent token record")
                    .active_attempt
                    .is_none()
            );
            assert!(actor.goal_tracker.lock().snapshot().is_some_and(|goal| {
                goal.live_subagent_tokens == 0
                    && goal.live_turn_count == 0
                    && goal.live_tool_call_count == 0
            }));
            assert_eq!(
                persisted_finishes(&mut persistence_rx),
                vec![finished(None, 100)]
            );
        })
        .await;
}

#[tokio::test]
async fn first_resumed_activation_preserves_cumulative_live_value() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _persistence_rx) = actor_with_goal().await;
            let parent_attempt = attempt(0x10);
            let child_attempt = attempt(0x20);

            notify(
                &actor,
                spawned(RESUME_PARENT_ID, Some(&parent_attempt), None),
            )
            .await;
            notify(
                &actor,
                progress(RESUME_PARENT_ID, Some(&parent_attempt), 500),
            )
            .await;
            notify(
                &actor,
                spawned(SUBAGENT_ID, Some(&child_attempt), Some(RESUME_PARENT_ID)),
            )
            .await;
            notify(&actor, progress(SUBAGENT_ID, Some(&child_attempt), 700)).await;

            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .expect("active goal")
                    .live_subagent_tokens,
                700
            );
        })
        .await;
}
