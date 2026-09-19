// Modified for Distill by Samuel Fajreldines, 2026.
use super::active_message::tests::*;
use super::tests::*;
use super::*;
use crate::implementations::distill::task::admission::{LimitBehavior, SubagentLimits};
use crate::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
use crate::implementations::distill::task::types::*;
use tokio::sync::oneshot;

const CHILD_ID: &str = "019b0000-0000-7000-8000-000000000001";

#[tokio::test]
async fn outer_ancestor_wakes_completed_descendant_by_raw_id_and_address() {
    for use_address in [false, true] {
        let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
        insert_child_with(
            &mut coordinator,
            admission_tx.clone(),
            "child",
            "root",
            None,
            SubagentOwner::Task,
        );
        insert_child_with(
            &mut coordinator,
            admission_tx.clone(),
            "grandchild",
            "root",
            Some("child"),
            SubagentOwner::Task,
        );
        let address = insert_child_with(
            &mut coordinator,
            admission_tx,
            "great-grandchild",
            "root",
            Some("grandchild"),
            SubagentOwner::Task,
        );
        finish_child(&mut coordinator, "great-grandchild");

        let mut response = if use_address {
            begin_human_send(&mut coordinator, &command_tx, &address, "child")
        } else {
            begin_send(&mut coordinator, &command_tx, "great-grandchild", "child")
        };
        assert!(response.try_recv().is_err());
        assert!(coordinator.pending.contains_key("great-grandchild"));
        assert!(!coordinator.completed.contains_key("great-grandchild"));
    }
}

#[tokio::test]
async fn dropping_coordinator_refuses_parked_wake() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    finish_child(&mut coordinator, "child");
    let Some(child) = coordinator.completed.get_mut("child") else {
        panic!("expected completed child");
    };
    child
        .terminal_published
        .store(false, std::sync::atomic::Ordering::Release);
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert_eq!(
        coordinator.pending_wakes.get("child").map(Vec::len),
        Some(1)
    );

    drop(coordinator);

    assert_eq!(
        response_outcome(response).await,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
}

#[tokio::test]
async fn evicted_unpublished_completion_refuses_parked_wake() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "evicted", "parent");
    finish_child(&mut coordinator, "evicted");
    let Some(evicted) = coordinator.completed.get_mut("evicted") else {
        panic!("expected completed evicted child");
    };
    evicted
        .terminal_published
        .store(false, std::sync::atomic::Ordering::Release);
    let response = begin_send(&mut coordinator, &command_tx, "evicted", "parent");
    assert_eq!(
        coordinator.pending_wakes.get("evicted").map(Vec::len),
        Some(1)
    );

    for index in 0..MAX_COMPLETED_ENTRIES {
        let id = format!("retained-{index:04}");
        insert_child(&mut coordinator, admission_tx.clone(), &id, "parent");
        finish_child(&mut coordinator, &id);
    }
    coordinator.evict_completed_overflow();

    assert_eq!(
        response_outcome(response).await,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
    assert!(!coordinator.completed.contains_key("evicted"));
    assert!(!coordinator.pending_wakes.contains_key("evicted"));
}

#[tokio::test]
async fn stopped_root_blocks_completed_wake_from_authorized_spawner() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    let address = insert_child_with(
        &mut coordinator,
        admission_tx,
        "child",
        "root",
        Some("spawner"),
        SubagentOwner::Task,
    );
    finish_child(&mut coordinator, "child");
    coordinator.spawn_blocked_sessions.insert("root".to_owned());

    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_human_send(
            &mut coordinator,
            &command_tx,
            &address,
            "spawner",
        ))
        .await
    );
    assert!(!coordinator.pending.contains_key("child"));
    assert!(coordinator.completed.contains_key("child"));
}

#[tokio::test]
async fn human_completed_address_rejects_foreign_parent() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    let address = insert_child(&mut coordinator, admission_tx, "child", "parent");
    finish_child(&mut coordinator, "child");

    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(begin_human_send(
            &mut coordinator,
            &command_tx,
            &address,
            "foreign",
        ))
        .await
    );
    assert!(!coordinator.pending.contains_key("child"));
    assert!(coordinator.completed.contains_key("child"));
}

#[tokio::test]
async fn completed_wake_waits_for_terminal_publication() {
    let mut harness = harness_with_options(
        RunnerBehavior {
            hold_terminal_publication: true,
            ..Default::default()
        },
        CoordinatorConfig {
            foreground_budget: std::time::Duration::from_secs(60),
            ..CoordinatorConfig::default()
        },
    );
    let backend = parent_backend(&harness);
    let spawn = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("identity-source", true), None).await }
    });
    harness.wake_runs.recv().await.expect("initial run");
    harness.started.recv().await.expect("initial start");
    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().success);
    harness
        .completions
        .recv()
        .await
        .expect("initial completion");

    let send = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("identity-source", "continue").unwrap(),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(harness.wake_runs.try_recv().is_err());
    assert!(!send.is_finished());

    harness
        .terminal_publications
        .recv()
        .await
        .expect("publication callback")();
    harness.wake_runs.recv().await.expect("wake run");
    harness.started.recv().await.expect("wake start");
    assert!(matches!(
        send.await.unwrap(),
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
    harness.actor.abort();
}

async fn park_wake_until_terminal_publication(
    harness: &mut Harness,
    backend: &ChannelBackend,
) -> tokio::task::JoinHandle<ActiveAgentMessageOutcome> {
    complete_child(harness, backend, "identity-source").await;
    let send = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("identity-source", "continue").unwrap(),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!send.is_finished());
    assert!(harness.wake_runs.try_recv().is_err());
    send
}

#[tokio::test]
async fn stopped_session_refuses_parked_wake_before_publication() {
    let mut harness = harness_with_options(
        RunnerBehavior {
            hold_terminal_publication: true,
            ..Default::default()
        },
        CoordinatorConfig::default(),
    );
    let backend = parent_backend(&harness);
    let send = park_wake_until_terminal_publication(&mut harness, &backend).await;

    assert_eq!(
        backend.cancel_parent_session().await,
        SubagentCancelOutcome::Cancelled
    );
    assert_eq!(
        send.await.unwrap(),
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
    harness
        .terminal_publications
        .recv()
        .await
        .expect("publication callback")();
    tokio::task::yield_now().await;
    assert!(harness.wake_runs.try_recv().is_err());
    harness.actor.abort();
}

#[tokio::test]
async fn deleted_session_refuses_parked_wake_before_publication() {
    let mut harness = harness_with_options(
        RunnerBehavior {
            hold_terminal_publication: true,
            ..Default::default()
        },
        CoordinatorConfig::default(),
    );
    let backend = parent_backend(&harness);
    let send = park_wake_until_terminal_publication(&mut harness, &backend).await;
    let (respond_to, response) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::TeardownSession {
            parent_session_id: "parent".to_owned(),
            respond_to: Some(respond_to),
        })
        .expect("teardown queued");

    response.await.expect("delete drain response");
    assert_eq!(
        send.await.unwrap(),
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
    harness
        .terminal_publications
        .recv()
        .await
        .expect("publication callback")();
    tokio::task::yield_now().await;
    assert!(harness.wake_runs.try_recv().is_err());
    harness.actor.abort();
}

#[tokio::test]
async fn completed_agent_message_wakes_same_id_and_queues_next_turn() {
    for requested_operation in [
        ActiveAgentMessageOperation::Queue,
        ActiveAgentMessageOperation::Steer,
        ActiveAgentMessageOperation::Interject,
    ] {
        let mut harness = harness(false, std::time::Duration::from_secs(60));
        let backend = parent_backend(&harness);
        let spawn = tokio::spawn({
            let backend = backend.clone();
            async move { backend.spawn(request(CHILD_ID, true), None).await }
        });
        let first_run = harness.wake_runs.recv().await.expect("initial run");
        assert_eq!(
            (
                &first_run.0,
                &first_run.2,
                &first_run.3,
                first_run.4,
                &first_run.5,
            ),
            (&CHILD_ID.to_owned(), &None, &"work".to_owned(), None, &None,)
        );
        assert!(distill_message_delivery_core::AttemptId::parse(first_run.1.as_str()).is_some());
        let first_sender = first_run.6.as_ref().expect("ordinary child sender");
        assert_eq!(&first_run.1, first_sender.holder().attempt_id());
        assert_eq!(harness.started.recv().await.as_deref(), Some(CHILD_ID));
        let _ = harness.finish.send(());
        spawn.await.unwrap().unwrap();
        harness.completions.recv().await.unwrap();

        let send = tokio::spawn({
            let backend = backend.clone();
            async move {
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new_with_operation(
                            CHILD_ID,
                            "continue",
                            requested_operation,
                        )
                        .unwrap(),
                    )
                    .await
            }
        });
        let wake_run = harness.wake_runs.recv().await.expect("wake run");
        assert_eq!(
            (&wake_run.0, &wake_run.2, &wake_run.3, wake_run.4),
            (
                &CHILD_ID.to_owned(),
                &Some(CHILD_ID.to_owned()),
                &"continue".to_owned(),
                Some(ActiveAgentMessageSource::Agent),
            )
        );
        assert!(distill_message_delivery_core::AttemptId::parse(wake_run.1.as_str()).is_some());
        let wake_sender = wake_run.6.as_ref().expect("wake sender");
        assert_eq!(&wake_run.1, wake_sender.holder().attempt_id());
        assert_ne!(first_run.1, wake_run.1);
        assert_ne!(
            first_sender.holder().generation(),
            wake_sender.holder().generation()
        );
        let wake_message_id = wake_run.5.expect("wake message id");
        assert_eq!(harness.started.recv().await.as_deref(), Some(CHILD_ID));
        assert!(harness.admitted_messages.try_recv().is_err());
        assert_eq!(
            send.await.unwrap(),
            ActiveAgentMessageOutcome::Accepted {
                message_id: wake_message_id,
            }
        );

        let _ = harness.finish.send(());
        harness.completions.recv().await.unwrap();
        harness.actor.abort();
    }
}

async fn complete_child(harness: &mut Harness, backend: &ChannelBackend, id: &str) {
    let spawn = tokio::spawn({
        let backend = backend.clone();
        let request = request(id, true);
        async move { backend.spawn(request, None).await }
    });
    harness.wake_runs.recv().await.expect("run observed");
    assert_eq!(harness.started.recv().await.as_deref(), Some(id));
    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().success);
    harness
        .completions
        .recv()
        .await
        .expect("completion observed");
}

#[derive(Clone, Copy)]
enum WakeAdmissionOrigin {
    Direct,
    Dequeued,
}

#[derive(Clone, Copy)]
enum PreStartExit {
    Failure,
    Cancellation,
}

async fn run_pre_start_wake_restore_scenario(origin: WakeAdmissionOrigin, exit: PreStartExit) {
    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    tokio::time::timeout(TEST_TIMEOUT, async move {
        let is_failure = matches!(exit, PreStartExit::Failure);
        let config = match origin {
            WakeAdmissionOrigin::Direct => buffering(),
            WakeAdmissionOrigin::Dequeued => CoordinatorConfig {
                limits: SubagentLimits {
                    max_concurrent: 1,
                    behavior: LimitBehavior::Queue,
                },
                buffer_completions: true,
                ..CoordinatorConfig::default()
            },
        };
        let mut harness = harness_with_options(
            RunnerBehavior {
                wait_before_start: !is_failure,
                fail_wake_before_start: is_failure,
                ..Default::default()
            },
            config,
        );
        let backend = parent_backend(&harness);
        if is_failure {
            complete_child(&mut harness, &backend, CHILD_ID).await;
        } else {
            let source = tokio::spawn({
                let backend = backend.clone();
                async move { backend.spawn(request(CHILD_ID, true), None).await }
            });
            harness.wake_runs.recv().await.expect("source run observed");
            let _ = harness.start.send(());
            harness.started.recv().await.expect("source started");
            let _ = harness.finish.send(());
            assert!(source.await.unwrap().unwrap().success);
            harness.completions.recv().await.expect("source completion");
        }
        let _ = buffered_completions(&harness, Some("parent")).await;

        let mut held = None;
        if matches!(origin, WakeAdmissionOrigin::Dequeued) {
            let spawn = tokio::spawn({
                let backend = backend.clone();
                async move { backend.spawn(request("held", true), None).await }
            });
            harness.wake_runs.recv().await.expect("held run observed");
            if !is_failure {
                let _ = harness.start.send(());
            }
            harness.started.recv().await.expect("held started");
            held = Some(spawn);
        }

        let wake = tokio::spawn({
            let backend = backend.clone();
            async move {
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new(CHILD_ID, "continue").unwrap(),
                    )
                    .await
            }
        });
        match origin {
            WakeAdmissionOrigin::Direct => {
                harness.wake_runs.recv().await.expect("wake run observed");
            }
            WakeAdmissionOrigin::Dequeued => {
                await_queued(&harness.backend, 1).await;
                let _ = harness.finish.send(());
                assert!(
                    held.take()
                        .expect("held spawn")
                        .await
                        .unwrap()
                        .unwrap()
                        .success
                );
                harness.completions.recv().await.expect("held completion");
                harness
                    .wake_runs
                    .recv()
                    .await
                    .expect("dequeued wake observed");
            }
        }

        let (respond_to, mut response) = oneshot::channel();
        harness
            .backend
            .sender()
            .send(SubagentEvent::Query(SubagentQueryRequest {
                subagent_id: CHILD_ID.to_owned(),
                parent_session_id: Some("parent".to_owned()),
                block: true,
                timeout_ms: Some(1_000),
                respond_to,
            }))
            .expect("query queued");
        let barrier = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.registry_counts().await }
        });
        let _ = barrier.await.expect("actor barrier");
        assert!(response.try_recv().is_err(), "waiter must be parked");

        match exit {
            PreStartExit::Failure => {
                let _ = harness.start.send(());
            }
            PreStartExit::Cancellation => {
                assert_eq!(
                    backend.cancel(CHILD_ID).await,
                    SubagentCancelOutcome::Cancelled
                );
            }
        }

        let snapshot = response
            .await
            .expect("waiter response")
            .expect("restored snapshot");
        let SubagentSnapshotStatus::Completed { output, .. } = snapshot.status else {
            panic!("prior completed snapshot was not restored")
        };
        assert_eq!(output, "work");
        assert_eq!(
            wake.await.unwrap(),
            ActiveAgentMessageOutcome::NotActiveOrFinalizing
        );
        assert!(harness.completions.try_recv().is_err());
        let buffered = buffered_completions(&harness, Some("parent")).await;
        if matches!(origin, WakeAdmissionOrigin::Dequeued) {
            assert_eq!(buffered.len(), 1);
            let Some(first) = buffered.first() else {
                panic!("expected buffered completion: {buffered:?}");
            };
            assert_eq!(first.subagent_id(), "held");
        } else {
            assert!(buffered.is_empty());
        }
        harness.actor.abort();
    })
    .await
    .expect("pre-start wake scenario timed out");
}

#[tokio::test]
async fn failed_wake_refuses_sends_until_runner_teardown_completes() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut harness = harness_with_options(
            RunnerBehavior {
                reject_wake_after_deferred_start: true,
                hold_failed_wake_teardown: true,
                ..Default::default()
            },
            buffering(),
        );
        let backend = parent_backend(&harness);
        complete_child(&mut harness, &backend, "identity-source").await;
        let _ = buffered_completions(&harness, Some("parent")).await;

        let failed = tokio::spawn({
            let backend = backend.clone();
            async move {
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new("identity-source", "first wake")
                            .unwrap(),
                    )
                    .await
            }
        });
        harness.wake_runs.recv().await.expect("failed wake run");
        harness
            .failed_wake_teardown_ready
            .recv()
            .await
            .expect("failed wake teardown held");
        assert_eq!(
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("identity-source", "too early").unwrap(),
                )
                .await,
            ActiveAgentMessageOutcome::NotActiveOrFinalizing
        );
        assert!(harness.wake_runs.try_recv().is_err());
        assert_eq!(harness.backend.registry_counts().await.active, 1);

        let _ = harness.finish.send(());
        assert_eq!(
            failed.await.unwrap(),
            ActiveAgentMessageOutcome::NotActiveOrFinalizing
        );
        for _ in 0..400 {
            if harness.backend.registry_counts().await.completed == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(harness.backend.registry_counts().await.active, 0);

        let retry = tokio::spawn({
            let backend = backend.clone();
            async move {
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new("identity-source", "after teardown")
                            .unwrap(),
                    )
                    .await
            }
        });
        harness.wake_runs.recv().await.expect("retry wake run");
        harness.started.recv().await.expect("retry wake started");
        assert!(matches!(
            retry.await.unwrap(),
            ActiveAgentMessageOutcome::Accepted { .. }
        ));
        assert_eq!(harness.backend.registry_counts().await.active, 1);
        let _ = harness.finish.send(());
        harness.completions.recv().await.expect("retry completion");
        harness.actor.abort();
    })
    .await
    .expect("failed wake teardown scenario timed out");
}

#[tokio::test]
async fn pre_start_wake_exits_restore_prior_observers() {
    for origin in [WakeAdmissionOrigin::Direct, WakeAdmissionOrigin::Dequeued] {
        for exit in [PreStartExit::Failure, PreStartExit::Cancellation] {
            run_pre_start_wake_restore_scenario(origin, exit).await;
        }
    }
}

#[tokio::test]
async fn completed_wake_queues_at_concurrent_limit_until_slot_frees() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Queue));
    let backend = parent_backend(&harness);
    let source = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("identity-source", true), None).await }
    });
    harness.wake_runs.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let _ = harness.finish.send(());
    source.await.unwrap().unwrap();
    harness.completions.recv().await.unwrap();

    let held = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.wake_runs.recv().await.unwrap();
    harness.started.recv().await.unwrap();

    let wake = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new("identity-source", "continue").unwrap(),
                )
                .await
        }
    });
    await_queued(&harness.backend, 1).await;
    assert!(!wake.is_finished());
    assert!(harness.wake_runs.try_recv().is_err());
    assert_eq!(
        harness.backend.registry_counts().await,
        SubagentRegistryCounts {
            pending: 0,
            active: 1,
            completed: 0,
            queued: 1,
        }
    );

    let _ = harness.finish.send(());
    held.await.unwrap().unwrap();
    harness.completions.recv().await.unwrap();
    let wake_run = harness.wake_runs.recv().await.expect("wake run");
    assert_eq!(
        (&wake_run.0, &wake_run.2, &wake_run.3, wake_run.4),
        (
            &"identity-source".to_owned(),
            &Some("identity-source".to_owned()),
            &"continue".to_owned(),
            Some(ActiveAgentMessageSource::Agent),
        )
    );
    let wake_message_id = wake_run.5.expect("wake message id");
    harness.started.recv().await.unwrap();
    assert_eq!(
        wake.await.unwrap(),
        ActiveAgentMessageOutcome::Accepted {
            message_id: wake_message_id,
        }
    );

    let _ = harness.finish.send(());
    harness.completions.recv().await.unwrap();
    harness.actor.abort();
}

#[tokio::test]
async fn completed_wake_rejects_at_concurrent_limit_without_losing_terminal_record() {
    let mut harness = harness_with_config(false, limited(1, LimitBehavior::Fail));
    let backend = parent_backend(&harness);
    let source = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("identity-source", true), None).await }
    });
    harness.wake_runs.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let _ = harness.finish.send(());
    source.await.unwrap().unwrap();
    harness.completions.recv().await.unwrap();

    let held = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request("held", true), None).await }
    });
    harness.wake_runs.recv().await.unwrap();
    harness.started.recv().await.unwrap();

    assert_eq!(
        backend
            .send_active_message(
                ActiveAgentMessageRequest::try_new("identity-source", "continue").unwrap(),
            )
            .await,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
    assert!(harness.wake_runs.try_recv().is_err());
    let terminal = backend
        .query("identity-source", false, None)
        .await
        .expect("completed record remains queryable");
    assert!(terminal.status.is_terminal());
    assert_eq!(
        harness.backend.registry_counts().await,
        SubagentRegistryCounts {
            pending: 0,
            active: 1,
            completed: 1,
            queued: 0,
        }
    );

    let _ = harness.finish.send(());
    held.await.unwrap().unwrap();
    harness.completions.recv().await.unwrap();
    harness.actor.abort();
}
