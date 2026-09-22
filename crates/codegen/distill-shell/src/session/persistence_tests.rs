// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::session::storage::jsonl::AppendDurability;

struct ActorGuard {
    handle: PersistenceHandle,
    task: tokio::task::JoinHandle<()>,
    _title_recorder: distill_chat_state::ChatStateHandle,
}

impl ActorGuard {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn test_actor(info: Info, storage: Arc<dyn StorageAdapter>) -> ActorGuard {
    test_actor_with_remote_sync(info, storage, None)
}

fn test_actor_with_remote_sync(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
) -> ActorGuard {
    test_actor_inner(info, storage, remote_sync, false)
}

fn test_actor_inner(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
    mark_summary_done: bool,
) -> ActorGuard {
    let title_recorder = distill_chat_state::ChatStateHandle::noop();
    test_actor_inner_with_recorder(
        info,
        storage,
        remote_sync,
        mark_summary_done,
        title_recorder,
    )
}

fn test_actor_with_recorder(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    title_recorder: distill_chat_state::ChatStateHandle,
) -> ActorGuard {
    test_actor_inner_with_recorder(info, storage, None, false, title_recorder)
}

fn test_actor_inner_with_recorder(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
    mark_summary_done: bool,
    title_recorder: distill_chat_state::ChatStateHandle,
) -> ActorGuard {
    let (tx, rx) = mpsc::unbounded_channel();
    let (disk_full_tx, disk_full_rx) = tokio::sync::watch::channel(false);
    let sampling_client = OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap();
    let mut summary =
        crate::session::summary::SummaryGenerator::new(crate::session::summary::SummaryConfig {
            sampling_client,
            model: String::new(),
            persistence_tx: tx.downgrade(),
        });
    summary.register_initial_title_recorder(title_recorder.downgrade());
    if mark_summary_done {
        summary.mark_done();
    }
    let task = tokio::spawn(
        SessionPersistence {
            info,
            storage,
            pending_notification: None,
            rx,
            remote_sync,
            // These tests run the actor as resumed; the backfill on writeback upgrade only runs for a fresh session
            created_fresh: false,
            relay_sync: None,
            summary,
            registry_title_sync: None,
            gateway: None,
            search_index: crate::session::storage::search::SharedSearchIndex::never_indexed(),
            disk_full_tx,
            disk_full_notified: false,
            dirty_files: Default::default(),
            pending_write_error: None,
            last_usage_live: None,
            last_usage_turn: None,
            last_incoming_turn: None,
            pending_usage_live: None,
        }
        .run(),
    );
    ActorGuard {
        handle: PersistenceHandle::from_parts_for_test(tx, disk_full_rx),
        task,
        _title_recorder: title_recorder,
    }
}

fn notification(info: &Info, text: &str) -> acp::SessionNotification {
    acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )
}

fn neutral_update(info: &Info, text: &str) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(notification(info, text)))
}

#[tokio::test]
async fn late_title_refresh_clears_pending_persisted_completeness_once() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("late-title-usage"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let recorder = distill_chat_state::ChatStateActor::spawn(
        Vec::new(),
        distill_sampling_types::SamplingConfig::default(),
        Box::new(distill_chat_state::NullChatPersistence),
        event_tx,
        cancellation.clone(),
    );
    let actor = test_actor_with_recorder(info.clone(), storage.clone(), recorder.clone());

    recorder.record_usage_attribution(
        distill_chat_state::UsageAttribution {
            attempt_id: "main:foreground".to_owned(),
            task_id: Some("prompt-1".to_owned()),
            turn_id: Some("1".to_owned()),
            request_id: Some("main-request".to_owned()),
            role: "main".to_owned(),
            model_id: "main-model".to_owned(),
            endpoint: Some("mock://main".to_owned()),
            requested_effort: Some("low".to_owned()),
            applied_effort: Some("low".to_owned()),
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            usage_complete: true,
            api_duration_ms: Some(10),
            cost_usd_ticks: Some(10),
            cost_basis: distill_chat_state::UsageCostBasis::Reported,
        },
        false,
    );
    recorder
        .register_pending_usage_attempt("initial-title:delayed".to_owned(), false)
        .unwrap();
    let pending = recorder
        .try_get_session_usage()
        .await
        .expect("chat-state actor should answer the pending snapshot");
    assert!(pending.is_incomplete());
    let pending_live = crate::session::usage_file::UsageSummary::from_ledger(&pending);
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 1,
            live: pending_live.clone(),
        })
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let interim = loop {
        if let Some(file) = storage.read_usage(&info).await.unwrap()
            && file.session.model_calls == 1
            && file.session.usage_is_incomplete
        {
            break file;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pending usage was not persisted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(interim.session.cost_usd_ticks, Some(10));
    assert_eq!(interim.session.attributions.len(), 1);

    recorder.record_usage_attribution(
        distill_chat_state::UsageAttribution {
            attempt_id: "main:foreground-2".to_owned(),
            task_id: Some("prompt-2".to_owned()),
            turn_id: Some("2".to_owned()),
            request_id: Some("main-request-2".to_owned()),
            role: "main".to_owned(),
            model_id: "main-model".to_owned(),
            endpoint: Some("mock://main".to_owned()),
            requested_effort: Some("low".to_owned()),
            applied_effort: Some("low".to_owned()),
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            usage_complete: true,
            api_duration_ms: Some(8),
            cost_usd_ticks: Some(4),
            cost_basis: distill_chat_state::UsageCostBasis::Reported,
        },
        false,
    );
    let pending_turn2 = recorder
        .try_get_session_usage()
        .await
        .expect("chat-state actor should answer the second pending snapshot");
    assert!(pending_turn2.is_incomplete());
    assert_eq!(pending_turn2.totals.model_calls, 2);
    assert_eq!(pending_turn2.totals.cost_usd_ticks, Some(14));
    assert_eq!(pending_turn2.attributions.len(), 2);
    let pending_turn2_live = crate::session::usage_file::UsageSummary::from_ledger(&pending_turn2);
    let mut child_ledger = distill_chat_state::UsageLedger::default();
    child_ledger.record_auxiliary_call(
        "child-model",
        Some(&distill_sampling_types::TokenUsage {
            prompt_tokens: 5,
            completion_tokens: 1,
            total_tokens: 6,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        }),
        Some(5),
        Some(5),
        false,
    );
    let child_live = crate::session::usage_file::UsageSummary::from_ledger(&child_ledger);
    let mixed_turn2_live = pending_turn2_live.saturating_add(&child_live);
    assert_eq!(mixed_turn2_live.model_calls, 3);
    assert_eq!(mixed_turn2_live.cost_usd_ticks, Some(19));

    recorder.record_usage_attribution(
        distill_chat_state::UsageAttribution {
            attempt_id: "initial-title:delayed".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: Some("title-request".to_owned()),
            role: "auxiliary".to_owned(),
            model_id: "title-model".to_owned(),
            endpoint: Some("mock://title".to_owned()),
            requested_effort: None,
            applied_effort: Some("absent".to_owned()),
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 4,
                completion_tokens: 2,
                total_tokens: 6,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            usage_complete: true,
            api_duration_ms: Some(5),
            cost_usd_ticks: Some(7),
            cost_basis: distill_chat_state::UsageCostBasis::Reported,
        },
        false,
    );
    let terminal = recorder
        .try_get_session_usage()
        .await
        .expect("chat-state actor should answer the terminal snapshot");
    assert!(!terminal.is_incomplete());
    let terminal_live = crate::session::usage_file::UsageSummary::from_ledger(&terminal)
        .saturating_add(&child_live);
    actor
        .handle
        .tx
        .send(PersistenceMsg::RefreshUsage {
            recorder: recorder.downgrade(),
        })
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let late_refresh = loop {
        if let Some(file) = storage.read_usage(&info).await.unwrap()
            && file.turns.len() == 1
            && file.turns[0].usage.model_calls == 2
            && file.turns[0].usage.cost_usd_ticks == Some(17)
            && file.session.model_calls == 2
            && file.session.cost_usd_ticks == Some(17)
            && file.session.attributions.len() == 2
            && !file.session.usage_is_incomplete
        {
            break file;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "late title refresh consumed active turn usage"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(late_refresh.turns[0].usage.model_calls, 2);
    assert_eq!(late_refresh.turns[0].usage.cost_usd_ticks, Some(17));

    // The captured active-turn snapshot arrives before the terminal full
    // snapshot. It overlaps the precursor on main1 but omits the title, so
    // reconciliation must add only main2 and retain the title.
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 2,
            live: mixed_turn2_live.clone(),
        })
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let partial_overlap = loop {
        if let Some(file) = storage.read_usage(&info).await.unwrap()
            && file.session.model_calls == 4
            && file.session.cost_usd_ticks == Some(26)
            && file.session.attributions.len() == 3
            && file.turns.len() == 2
            && !file.session.usage_is_incomplete
        {
            break file;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "partial-overlap turn snapshot was rebilled or lost the title"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(partial_overlap.session.model_calls, 4);
    assert_eq!(partial_overlap.session.cost_usd_ticks, Some(26));
    assert!(partial_overlap.session.pending_attempt_ids.is_empty());
    assert_eq!(partial_overlap.turns[0].usage.cost_usd_ticks, Some(17));
    assert_eq!(partial_overlap.turns[1].usage.model_calls, 2);
    assert_eq!(partial_overlap.turns[1].usage.cost_usd_ticks, Some(9));

    // The terminal full snapshot arrives after the partial overlap and must
    // be an identity-only no-op, not another aggregate addition.
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 2,
            live: terminal_live,
        })
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let complete = loop {
        if let Some(file) = storage.read_usage(&info).await.unwrap()
            && file.session.model_calls == 4
            && file.session.cost_usd_ticks == Some(26)
            && file.session.attributions.len() == 3
            && file.turns.len() == 2
        {
            break file;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "terminal title usage was not persisted as complete"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(complete.turns.len(), 2);
    assert_eq!(complete.turns[0].usage.model_calls, 2);
    assert_eq!(complete.turns[0].usage.cost_usd_ticks, Some(17));
    assert_eq!(complete.turns[1].usage.model_calls, 2);
    assert_eq!(complete.turns[1].usage.cost_usd_ticks, Some(9));
    assert_eq!(complete.session.model_calls, 4);
    assert_eq!(complete.session.cost_usd_ticks, Some(26));
    assert_eq!(complete.session.attributions.len(), 3);
    assert!(!complete.session.usage_is_incomplete);
    assert!(!complete.turns[0].usage.usage_is_incomplete);
    assert!(!complete.turns[1].usage.usage_is_incomplete);
    assert!(complete.session.pending_attempt_ids.is_empty());
    assert!(complete.turns[0].usage.pending_attempt_ids.is_empty());
    assert!(complete.turns[1].usage.pending_attempt_ids.is_empty());
    assert_eq!(
        complete
            .session
            .attributions
            .iter()
            .filter(|row| row.attempt_id == "initial-title:delayed")
            .count(),
        1
    );

    // Two stale snapshots must both be rejected by the retained canonical
    // snapshot; the second one used to re-add the older aggregate.
    actor
        .handle
        .tx
        .send(PersistenceMsg::RefreshUsage {
            recorder: recorder.downgrade(),
        })
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 2,
            live: mixed_turn2_live.clone(),
        })
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 2,
            live: mixed_turn2_live,
        })
        .unwrap();

    // A second late notification is harmless: the cursor sees the same
    // canonical snapshot and must not add another title call or retain the
    // pending-only incomplete bit.
    actor
        .handle
        .tx
        .send(PersistenceMsg::RefreshUsage {
            recorder: recorder.downgrade(),
        })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let repeated = storage.read_usage(&info).await.unwrap().unwrap();
    assert_eq!(repeated.session.model_calls, complete.session.model_calls);
    assert_eq!(
        repeated.session.cost_usd_ticks,
        complete.session.cost_usd_ticks
    );
    assert!(!repeated.session.usage_is_incomplete);
    assert_eq!(
        repeated
            .session
            .attributions
            .iter()
            .filter(|row| row.attempt_id == "initial-title:delayed")
            .count(),
        1
    );

    actor.stop().await;
    cancellation.cancel();
}

#[tokio::test]
async fn title_refresh_before_first_usage_turn_is_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("late-title-before-cursor"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let recorder = distill_chat_state::ChatStateActor::spawn(
        Vec::new(),
        distill_sampling_types::SamplingConfig::default(),
        Box::new(distill_chat_state::NullChatPersistence),
        event_tx,
        cancellation.clone(),
    );
    let actor = test_actor_with_recorder(info.clone(), storage.clone(), recorder.clone());

    recorder.record_usage_attribution(
        distill_chat_state::UsageAttribution {
            attempt_id: "main:before-cursor".to_owned(),
            task_id: Some("prompt-before-cursor".to_owned()),
            turn_id: Some("1".to_owned()),
            request_id: Some("main-before-cursor".to_owned()),
            role: "main".to_owned(),
            model_id: "main-model".to_owned(),
            endpoint: Some("mock://main".to_owned()),
            requested_effort: Some("low".to_owned()),
            applied_effort: Some("low".to_owned()),
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            usage_complete: true,
            api_duration_ms: Some(10),
            cost_usd_ticks: Some(10),
            cost_basis: distill_chat_state::UsageCostBasis::Reported,
        },
        false,
    );
    recorder
        .register_pending_usage_attempt("initial-title:before-cursor".to_owned(), false)
        .unwrap();
    let pending = recorder
        .try_get_session_usage()
        .await
        .expect("chat-state actor should answer the pending first-turn snapshot");
    let pending_live = crate::session::usage_file::UsageSummary::from_ledger(&pending);

    recorder.record_usage_attribution(
        distill_chat_state::UsageAttribution {
            attempt_id: "initial-title:before-cursor".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: Some("title-before-cursor".to_owned()),
            role: "auxiliary".to_owned(),
            model_id: "title-model".to_owned(),
            endpoint: Some("mock://title".to_owned()),
            requested_effort: None,
            applied_effort: Some("absent".to_owned()),
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(distill_sampling_types::TokenUsage {
                prompt_tokens: 4,
                completion_tokens: 2,
                total_tokens: 6,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            usage_complete: true,
            api_duration_ms: Some(5),
            cost_usd_ticks: Some(7),
            cost_basis: distill_chat_state::UsageCostBasis::Reported,
        },
        false,
    );
    let terminal = recorder
        .try_get_session_usage()
        .await
        .expect("chat-state actor should answer the terminal first-turn snapshot");
    assert!(!terminal.is_incomplete());

    // The terminal refresh reaches persistence before the first foreground
    // UsageTurn. It must remain available for reconciliation with the stale
    // pending snapshot rather than being dropped for lack of a cursor.
    actor
        .handle
        .tx
        .send(PersistenceMsg::RefreshUsage {
            recorder: recorder.downgrade(),
        })
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::UsageTurn {
            turn_number: 1,
            live: pending_live,
        })
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let file = loop {
        if let Some(file) = storage.read_usage(&info).await.unwrap()
            && file.turns.len() == 1
            && file.session.model_calls == 2
            && file.session.cost_usd_ticks == Some(17)
            && file.session.attributions.len() == 2
            && !file.session.usage_is_incomplete
        {
            break file;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pre-cursor terminal refresh was discarded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(file.turns[0].usage.cost_usd_ticks, Some(17));
    assert!(file.session.pending_attempt_ids.is_empty());
    assert_eq!(
        file.session
            .attributions
            .iter()
            .filter(|row| row.attempt_id == "initial-title:before-cursor")
            .count(),
        1
    );

    actor.stop().await;
    cancellation.cancel();
}

#[tokio::test]
async fn writeback_backfill_is_fresh_only_and_acp_only() {
    let info = Info {
        id: acp::SessionId::new("wb-backfill"),
        cwd: "/test".into(),
    };

    // Fresh session: every ACP update is queued to the writeback sync.
    let (sync, mut observed) = RemoteSync::test_observer();
    let updates = vec![neutral_update(&info, "a"), neutral_update(&info, "b")];
    let n = backfill_updates_to_sync(true, updates, &sync);
    assert_eq!(n, 2, "a fresh session backfills its full local ACP history");
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
            .await
            .expect("backfilled notification not observed within 1s")
            .expect("observer channel closed unexpectedly");
    }

    // Resumed session: nothing is backfilled (prior history may already be synced).
    let (sync2, mut observed2) = RemoteSync::test_observer();
    let n2 = backfill_updates_to_sync(false, vec![neutral_update(&info, "a")], &sync2);
    assert_eq!(n2, 0, "a resumed session is forward-only, no backfill");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), observed2.recv())
            .await
            .is_err(),
        "resumed session must not re-send any prior history",
    );
}

#[tokio::test]
async fn winning_identity_stamp_seeds_remote_writeback() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("winner-identity"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (remote_sync, mut identities) = RemoteSync::test_identity_observer();
    let actor = test_actor_with_remote_sync(info, storage, Some(remote_sync));
    let identity = mint_next_session_identity(None, false);
    let expected = identity.agent_id.clone();
    let (respond_to, response) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::StampSessionIdentity {
            identity,
            respond_to,
        })
        .unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(identities.recv().await.as_deref(), Some(expected.as_str()));
    actor.stop().await;
}

fn break_summary_writes(dir: &std::path::Path) {
    let summary = dir.join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(summary).unwrap();
}

fn break_plan_writes(dir: &std::path::Path) {
    std::fs::create_dir(dir.join("plan.json")).unwrap();
}

async fn recv_observed(
    observed: &mut tokio::sync::mpsc::UnboundedReceiver<acp::SessionNotification>,
) -> acp::SessionNotification {
    tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
        .await
        .expect("remote sync timed out")
        .expect("remote sync observer closed")
}

#[test]
fn committed_error_returns_sync_disposition() {
    let info = Info {
        id: acp::SessionId::new("committed-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "committed");
    let PendingAppendOutcome::CommittedErr(sync_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::Committed(
                io::Error::other("summary patch failed"),
            )),
        )
    else {
        panic!("expected committed failure");
    };
    assert_eq!(sync_notification.session_id, info.id);
    assert_eq!(error.to_string(), "summary patch failed");
}

#[test]
fn uncommitted_error_returns_restore_disposition() {
    let info = Info {
        id: acp::SessionId::new("uncommitted-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "pending");
    let PendingAppendOutcome::NotCommittedErr(pending_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::NotCommitted(
                io::Error::other("append failed"),
            )),
        )
    else {
        panic!("expected uncommitted failure");
    };
    assert_eq!(pending_notification.session_id, info.id);
    assert_eq!(error.to_string(), "append failed");
}

#[tokio::test]
async fn noop_handle_rejects_durable_append() {
    let info = Info {
        id: acp::SessionId::new("noop-durable-update"),
        cwd: "/test".into(),
    };
    assert!(matches!(
        PersistenceHandle::noop()
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(error))
            if error.kind() == io::ErrorKind::Unsupported
    ));
}

#[tokio::test]
async fn pending_drain_disposition_controls_remote_sync() {
    let info = Info {
        id: acp::SessionId::new("pending-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = JsonlStorageAdapter::with_update_append_probe("/unused".into(), |_| {
        Err(io::Error::other("append failed"))
    });
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), Arc::new(storage), Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(_))
    ));
    assert!(observed.try_recv().is_err());
    actor.stop().await;

    let dir = tempfile::tempdir().unwrap();
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_attempts = attempts.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        move |durability| {
            observed_attempts.lock().unwrap().push(durability);
            Ok(())
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    break_summary_writes(dir.path());
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered, AppendDurability::Durable]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_committed_failure_is_synced() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    break_summary_writes(dir.path());
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    actor.stop().await;
}

#[tokio::test]
async fn failed_pending_drain_retains_record_and_skips_durable_update() {
    let info = Info {
        id: acp::SessionId::new("durable-drain-failure"),
        cwd: "/test".into(),
    };
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = attempts.clone();
    let storage =
        JsonlStorageAdapter::with_update_append_probe("/unused".into(), move |durability| {
            observed.lock().unwrap().push(durability);
            Err(io::Error::other("pending append failed"))
        });
    let actor = test_actor(info.clone(), Arc::new(storage));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            actor
                .handle
                .append_update_durably(neutral_update(&info, "durable"))
                .await
                .unwrap_err()
                .to_string(),
            "pending append failed"
        );
    }
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered, AppendDurability::Buffered]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn committed_pending_drain_still_writes_the_durable_update() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-after-committed-drain"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    break_summary_writes(dir.path());
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "terminal"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let jsonl = std::fs::read_to_string(dir.path().join("updates.jsonl")).unwrap();
    assert!(
        jsonl.contains("pending") && jsonl.contains("terminal"),
        "a committed drain must not drop the durable terminal: {jsonl}"
    );
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_drains_pending_update_in_fifo_order() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-update"),
        cwd: dir.path().to_string_lossy().into_owned(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "before")))
        .unwrap();
    actor
        .handle
        .append_update_durably(neutral_update(&info, "durable"))
        .await
        .unwrap();
    let summary = storage.load_summary(&info).await.unwrap();
    assert_eq!(summary.num_messages, 2);

    let updates = storage.load_session(&info).await.unwrap().updates;
    let texts = updates
        .iter()
        .filter_map(|update| {
            let SessionUpdate::Acp(notification) = update else {
                return None;
            };
            let acp::SessionUpdate::AgentMessageChunk(chunk) = &notification.update else {
                return None;
            };
            let acp::ContentBlock::Text(text) = &chunk.content else {
                return None;
            };
            Some(text.text.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["before", "durable"]);
    actor.stop().await;
}

async fn flush_ack(handle: &PersistenceHandle) -> io::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(PersistenceMsg::FlushAndAck { respond_to: tx })
        .unwrap();
    rx.await.unwrap()
}

#[tokio::test]
async fn aborted_wake_queued_behind_actor_delay_keeps_prior_summary() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("aborted-wake-delayed"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_wake_start(
            &info,
            WakeStart {
                prior: WakeSummaryState {
                    attempt_id: None,
                    next_trace_turn: 0,
                    current_model_id: default_model_id(),
                    agent_name: None,
                    reasoning_effort: None,
                    summary_bytes: std::fs::read(dir.path().join("summary.json")).unwrap(),
                },
                attempt_id: "at1.prior".into(),
                next_trace_turn: 7,
                model_id: default_model_id(),
                agent_name: None,
                reasoning_effort: None,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::HoldForTest {
            entered,
            release: release_rx,
        })
        .unwrap();
    entered_rx.await.unwrap();

    let abort = tokio_util::sync::CancellationToken::new();
    let (start_reply, start_rx) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::WakeStart {
            start: WakeStart {
                prior: WakeSummaryState {
                    attempt_id: Some("at1.prior".into()),
                    next_trace_turn: 7,
                    current_model_id: default_model_id(),
                    agent_name: None,
                    reasoning_effort: None,
                    summary_bytes: std::fs::read(dir.path().join("summary.json")).unwrap(),
                },
                attempt_id: "at1.aborted".into(),
                next_trace_turn: 8,
                model_id: default_model_id(),
                agent_name: None,
                reasoning_effort: None,
            },
            abort: abort.clone(),
            respond_to: start_reply,
        })
        .unwrap();
    abort.cancel();
    let (abort_reply, abort_rx) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::WakeAbort {
            prior: WakeSummaryState {
                attempt_id: Some("at1.prior".into()),
                next_trace_turn: 7,
                current_model_id: default_model_id(),
                agent_name: None,
                reasoning_effort: None,
                summary_bytes: std::fs::read(dir.path().join("summary.json")).unwrap(),
            },
            respond_to: abort_reply,
        })
        .unwrap();
    release.send(()).unwrap();

    assert_eq!(
        start_rx.await.unwrap().unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    abort_rx.await.unwrap().unwrap();
    let summary = storage.load_summary(&info).await.unwrap();
    assert_eq!(summary.attempt_id.as_deref(), Some("at1.prior"));
    assert_eq!(summary.next_trace_turn, 7);
    actor.stop().await;
}

#[tokio::test]
async fn acknowledged_wake_start_stamps_summary_once() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("acknowledged-wake-start"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_wake_start(
            &info,
            WakeStart {
                prior: WakeSummaryState {
                    attempt_id: None,
                    next_trace_turn: 0,
                    current_model_id: default_model_id(),
                    agent_name: None,
                    reasoning_effort: None,
                    summary_bytes: std::fs::read(dir.path().join("summary.json")).unwrap(),
                },
                attempt_id: "at1.prior".into(),
                next_trace_turn: 7,
                model_id: default_model_id(),
                agent_name: None,
                reasoning_effort: None,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());
    let (respond_to, response_rx) = tokio::sync::oneshot::channel();
    let wake_model = acp::ModelId::new("wake-model");
    actor
        .handle
        .tx
        .send(PersistenceMsg::WakeStart {
            start: WakeStart {
                prior: WakeSummaryState {
                    attempt_id: Some("at1.prior".into()),
                    next_trace_turn: 7,
                    current_model_id: default_model_id(),
                    agent_name: None,
                    reasoning_effort: None,
                    summary_bytes: std::fs::read(dir.path().join("summary.json")).unwrap(),
                },
                attempt_id: "at1.started".into(),
                next_trace_turn: 8,
                model_id: wake_model.clone(),
                agent_name: Some("wake-agent".into()),
                reasoning_effort: Some(Some(distill_sampling_types::ReasoningEffort::High)),
            },
            abort: tokio_util::sync::CancellationToken::new(),
            respond_to,
        })
        .unwrap();

    response_rx.await.unwrap().unwrap();
    let summary = storage.load_summary(&info).await.unwrap();
    assert_eq!(summary.attempt_id.as_deref(), Some("at1.started"));
    assert_eq!(summary.next_trace_turn, 8);
    assert_eq!(summary.current_model_id, wake_model);
    assert_eq!(summary.agent_name.as_deref(), Some("wake-agent"));
    assert_eq!(
        summary.reasoning_effort,
        Some(distill_sampling_types::ReasoningEffort::High)
    );
    actor.stop().await;
}

fn merge_boundary_update(info: &Info, text: &str) -> SessionUpdate {
    let mut chunk_meta = serde_json::Map::new();
    chunk_meta.insert("mergeBoundary".into(), serde_json::json!(true));
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)))
                .meta(Some(chunk_meta)),
        ),
    )))
}

struct SyncBarrierProbe {
    appends: Arc<std::sync::Mutex<Vec<AppendDurability>>>,
    syncs: Arc<std::sync::Mutex<Vec<crate::session::storage::SessionFileSet>>>,
}

fn actor_with_barrier_probes(dir: &std::path::Path, info: &Info) -> (ActorGuard, SyncBarrierProbe) {
    let appends = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_appends = appends.clone();
    let syncs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_syncs = syncs.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.to_path_buf(),
        move |durability| {
            observed_appends.lock().unwrap().push(durability);
            Ok(())
        },
        move |files| {
            observed_syncs.lock().unwrap().push(files);
            Ok(())
        },
    ));
    let actor = test_actor(info.clone(), storage);
    (actor, SyncBarrierProbe { appends, syncs })
}

#[tokio::test]
async fn flush_and_ack_syncs_only_dirty_files_once_and_keeps_streamed_appends_buffered() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-sync"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    for text in ["chunk-a", "chunk-b"] {
        actor
            .handle
            .tx
            .send(PersistenceMsg::Update(neutral_update(&info, text)))
            .unwrap();
    }
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(merge_boundary_update(
            &info, "boundary",
        )))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert!(
        matches!(
            probe.appends.lock().unwrap().as_slice(),
            [AppendDurability::Buffered, AppendDurability::Buffered]
        ),
        "every streamed-chunk append must stay buffered (no per-chunk syncs)"
    );
    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            updates: true,
            ..Default::default()
        }],
        "the barrier must sync exactly once, covering only the dirtied updates file"
    );

    flush_ack(&actor.handle).await.unwrap();
    assert_eq!(
        probe.syncs.lock().unwrap().len(),
        1,
        "a second barrier with nothing dirtied since the first must sync no files"
    );
    actor.stop().await;
}

#[tokio::test]
async fn idle_flush_and_ack_acks_without_syncing_any_files() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-idle"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    flush_ack(&actor.handle).await.unwrap();

    assert!(
        probe.syncs.lock().unwrap().is_empty(),
        "an idle barrier must not pay for untouched files"
    );
    actor.stop().await;
}

#[tokio::test]
async fn buffered_chat_plan_and_rewind_writes_dirty_exactly_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-dirty-set"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("hello")))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::PlanState(TodoState::default()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::RewindPoint(RewindPoint::new(0)))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            chat: true,
            rewind_points: true,
            ..Default::default()
        }],
        "buffered chat/rewind writes must dirty exactly their files; \
         atomic-rename writes (plan, summary bookkeeping) are durable at write time and stay out"
    );
    actor.stop().await;
}

#[tokio::test]
async fn copy_file_flush_syncs_every_file_regardless_of_dirtiness() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("copy-file-sync-all"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    let (one_shot, copied) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::CopyFile { one_shot })
        .unwrap();
    copied.await.unwrap().unwrap();

    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet::ALL],
        "a CopyFile snapshot must sync the full barrier file set even when clean"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_syncs_chat_when_summary_bookkeeping_fails_after_append() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-chat-committed"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    break_summary_writes(dir.path());
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("hello")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            chat: true,
            ..Default::default()
        }],
        "a chat append that reached the page cache must stay on the barrier dirty set even when summary bookkeeping fails"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("chat_history.jsonl"))
            .unwrap()
            .contains("hello"),
        "the chat JSONL record must survive the bookkeeping failure"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_succeeds_after_committed_pending_drain() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-committed-drain"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    break_summary_writes(dir.path());
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            updates: true,
            ..Default::default()
        }],
        "a Committed pending drain must not fail FlushAndAck after the prompt bytes are on the dirty set"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .contains("pending"),
        "the pending JSONL record must survive the bookkeeping failure"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_succeeds_after_not_committed_pending_drain() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-restored-drain"),
        cwd: "/test".into(),
    };
    let remaining_failures = std::sync::atomic::AtomicUsize::new(1);
    let syncs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_syncs = syncs.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.path().to_path_buf(),
        move |durability| {
            if matches!(durability, AppendDurability::Buffered)
                && remaining_failures.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
            {
                Err(io::Error::other("pending drain failed"))
            } else {
                Ok(())
            }
        },
        move |files| {
            observed_syncs.lock().unwrap().push(files);
            Ok(())
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    assert_eq!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "terminal"))
            .await
            .unwrap_err()
            .to_string(),
        "pending drain failed"
    );

    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("hello")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            chat: true,
            updates: true,
            ..Default::default()
        }],
        "a restored NotCommitted drain must not latch into the prompt barrier after a later successful redrain and chat append"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .contains("pending"),
        "the restored pending record must be written on the next FlushAndAck"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_succeeds_after_atomic_plan_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-plan-fail"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    break_plan_writes(dir.path());
    let (actor, probe) = actor_with_barrier_probes(dir.path(), &info);

    actor
        .handle
        .tx
        .send(PersistenceMsg::PlanState(TodoState::default()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("hello")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        probe.syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            chat: true,
            ..Default::default()
        }],
        "a failed atomic-rename plan write must not latch into the prompt barrier or skip a later successful chat append"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_succeeds_after_durable_append_never_reached_disk() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-durable-not-committed"),
        cwd: "/test".into(),
    };
    let syncs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_syncs = syncs.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.path().to_path_buf(),
        |durability| match durability {
            AppendDurability::Durable => Err(io::Error::other("durable append failed")),
            AppendDurability::Buffered => Ok(()),
        },
        move |files| {
            observed_syncs.lock().unwrap().push(files);
            Ok(())
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);

    assert_eq!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "terminal"))
            .await
            .unwrap_err()
            .to_string(),
        "durable append failed"
    );

    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("hello")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            chat: true,
            ..Default::default()
        }],
        "a NotCommitted durable append must not latch into the prompt barrier or skip a later successful chat append"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_retries_fsync_after_durable_append_file_barrier_failure() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-durable-barrier"),
        cwd: "/test".into(),
    };
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let syncs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_syncs = syncs.clone();
    let storage = Arc::new(
        JsonlStorageAdapter::with_probes(
            dir.path().to_path_buf(),
            |_| Ok(()),
            move |files| {
                observed_syncs.lock().unwrap().push(files);
                Ok(())
            },
        )
        .with_file_sync_probe(|| Err(io::Error::other("file barrier failed"))),
    );
    let actor = test_actor(info.clone(), storage);

    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "terminal"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    assert!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .contains("terminal"),
        "the durable JSONL record must survive the file-barrier failure"
    );

    flush_ack(&actor.handle).await.unwrap();

    assert_eq!(
        syncs.lock().unwrap().as_slice(),
        [crate::session::storage::SessionFileSet {
            updates: true,
            ..Default::default()
        }],
        "a durable append whose file barrier failed must stay on the dirty set so a later idle FlushAndAck retries the fsync"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_fails_after_copy_file_when_a_buffered_write_never_reached_disk() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-copy-file-latch"),
        cwd: "/test".into(),
    };
    let remaining_failures = std::sync::atomic::AtomicUsize::new(1);
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.path().to_path_buf(),
        move |_| {
            if remaining_failures.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err(io::Error::other("update append failed"))
            } else {
                Ok(())
            }
        },
        |_| Ok(()),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(merge_boundary_update(
            &info, "boundary",
        )))
        .unwrap();

    let (one_shot, copied) = tokio::sync::oneshot::channel();
    actor
        .handle
        .tx
        .send(PersistenceMsg::CopyFile { one_shot })
        .unwrap();
    copied.await.unwrap().unwrap();

    assert_eq!(
        flush_ack(&actor.handle).await.unwrap_err().to_string(),
        "update append failed",
        "CopyFile must not take the write-failure latch; FlushAndAck still withholds persist_ack"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_fails_when_a_buffered_update_write_never_reached_disk() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-lost-write"),
        cwd: "/test".into(),
    };
    let remaining_failures = std::sync::atomic::AtomicUsize::new(1);
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.path().to_path_buf(),
        move |_| {
            if remaining_failures.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err(io::Error::other("update append failed"))
            } else {
                Ok(())
            }
        },
        |_| Ok(()),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(merge_boundary_update(
            &info, "boundary",
        )))
        .unwrap();
    assert_eq!(
        flush_ack(&actor.handle).await.unwrap_err().to_string(),
        "update append failed"
    );
    actor.stop().await;
}

#[tokio::test]
async fn flush_and_ack_propagates_session_file_sync_error_through_the_ack() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("flush-ack-sync-error"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_probes(
        dir.path().to_path_buf(),
        |_| Ok(()),
        |_| Err(io::Error::other("session file sync failed")),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);

    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    assert_eq!(
        flush_ack(&actor.handle).await.unwrap_err().to_string(),
        "session file sync failed"
    );
    actor.stop().await;
}

/// Baselines on APFS (M-series laptop SSD), 50 iterations, medians:
/// prompt-send FlushAndAck round-trip ~26 ms (max ~48 ms);.
/// idle FlushAndAck ~30-40 us with zero file syncs (was ~5 ms for the fixed 5-file set before dirty tracking);.
#[tokio::test]
#[ignore = "manual durability-cost measurement; run with --ignored --nocapture and RUST_MIN_STACK=8388608"]
async fn measure_prompt_barrier_idle_barrier_and_summary_rewrite_cost() {
    fn median_and_max(mut samples: Vec<std::time::Duration>) -> (String, String) {
        samples.sort();
        let Some(mid) = samples.get(samples.len() / 2) else {
            panic!("expected samples for median: {samples:?}");
        };
        let Some(last) = samples.last() else {
            panic!("expected samples for max: {samples:?}");
        };
        (format!("{mid:?}"), format!("{last:?}"))
    }

    const N: usize = 50;
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("measure-barrier"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());

    actor
        .handle
        .tx
        .send(PersistenceMsg::Chat(ConversationItem::user("seed")))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::PlanState(TodoState::default()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::RewindPoint(RewindPoint::new(0)))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let mut prompt_shaped_barrier = Vec::with_capacity(N);
    for index in 0..N {
        actor
            .handle
            .tx
            .send(PersistenceMsg::Chat(ConversationItem::user(format!(
                "prompt {index}"
            ))))
            .unwrap();
        actor
            .handle
            .tx
            .send(PersistenceMsg::Update(neutral_update(&info, "user echo")))
            .unwrap();
        let start = std::time::Instant::now();
        flush_ack(&actor.handle).await.unwrap();
        prompt_shaped_barrier.push(start.elapsed());
    }

    let mut idle_barrier = Vec::with_capacity(N);
    for _ in 0..N {
        let start = std::time::Instant::now();
        flush_ack(&actor.handle).await.unwrap();
        idle_barrier.push(start.elapsed());
    }

    let dirty_two = crate::session::storage::SessionFileSet {
        updates: true,
        chat: true,
        ..Default::default()
    };
    let mut sync_two_files = Vec::with_capacity(N);
    for _ in 0..N {
        let start = std::time::Instant::now();
        storage
            .sync_session_files_selected(&info, dirty_two)
            .await
            .unwrap();
        sync_two_files.push(start.elapsed());
    }

    let mut sync_all_files = Vec::with_capacity(N);
    for _ in 0..N {
        let start = std::time::Instant::now();
        storage
            .sync_session_files_selected(&info, crate::session::storage::SessionFileSet::ALL)
            .await
            .unwrap();
        sync_all_files.push(start.elapsed());
    }

    let summary_path = dir.path().join("summary.json");
    let payload = std::fs::read(&summary_path).unwrap();
    let mut summary_rewrite = Vec::with_capacity(N);
    for _ in 0..N {
        let start = std::time::Instant::now();
        crate::session::storage::write_bytes_atomic(&summary_path, &payload).unwrap();
        summary_rewrite.push(start.elapsed());
    }

    for (label, samples) in [
        (
            "prompt-send FlushAndAck (chat+echo appends, their summary bookkeeping, dirty barrier)",
            prompt_shaped_barrier,
        ),
        ("idle FlushAndAck (nothing dirty)", idle_barrier),
        ("barrier sync of 2 dirty files + dir", sync_two_files),
        (
            "barrier sync of all 5 files + dir (pre-dirty-tracking shape)",
            sync_all_files,
        ),
        (
            "summary.json atomic rewrite (per-append bookkeeping cost)",
            summary_rewrite,
        ),
    ] {
        let (median, max) = median_and_max(samples);
        println!("{label}: median {median}, max {max}");
    }

    actor.stop().await;
}

async fn probe_writable(handle: &PersistenceHandle) -> io::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(PersistenceMsg::ProbeWritable { respond_to: tx })
        .unwrap();
    rx.await.unwrap()
}

/// Seeds `RemoteSync` with a pre-rename title (the cache at init), drives `PersistenceMsg::ManualTitleRenamed`, then queues an update and flushes.
/// The flush's `save_session_data` payload must carry the manual title.
#[tokio::test]
async fn manual_rename_next_flush_does_not_revert_backend_title() {
    use std::sync::Arc;

    use crate::remote::BackendClient;
    use crate::session::export::ExportedMetadata;
    use distill_login::{AuthManager, GrokAuth};
    use distill_test_support::MockInferenceServer;

    const OLD_TITLE: &str = "Auto first-prompt summary";
    const NEW_TITLE: &str = "Manual rename";
    const SESSION_ID: &str = "rename-writeback";

    let server = MockInferenceServer::start()
        .await
        .expect("start MockInferenceServer");
    let home = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(
        home.path(),
        distill_login::GrokComConfig::default(),
    ));
    auth.hot_swap(GrokAuth {
        key: "writeback-test-token".into(),
        ..GrokAuth::test_default()
    });

    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let metadata = ExportedMetadata {
        title: Some(OLD_TITLE.into()),
        cwd: info.cwd.clone(),
        model_id: Some("test-model".into()),
        created_at: None,
        updated_at: None,
        total_messages: None,
        parent_session_id: None,
        agent_id: None,
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: None,
    };
    let client = BackendClient::with_base_url(server.origin()).with_auth_manager(auth);
    let remote_sync = RemoteSync::new(SESSION_ID.to_owned(), metadata, client);
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));

    actor
        .handle
        .tx
        .send(PersistenceMsg::ManualTitleRenamed(NEW_TITLE.into()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(
            &info,
            "turn after rename",
        )))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let titles = wait_for_save_session_titles(&server, SESSION_ID).await;
    let last_nonempty = titles
        .iter()
        .rev()
        .find(|t| t.nonempty_messages)
        .map(|t| t.title.as_str());
    assert_eq!(
        last_nonempty,
        Some(NEW_TITLE),
        "next RemoteSync flush after ManualTitleRenamed must not revert to {OLD_TITLE:?}"
    );
    assert!(
        titles
            .iter()
            .filter(|t| t.nonempty_messages)
            .all(|t| t.title != OLD_TITLE),
        "no non-empty save_session_data may carry the pre-rename title"
    );
    assert!(
        titles
            .iter()
            .filter(|t| t.title == NEW_TITLE)
            .all(|t| t.title_is_manual == Some(true)),
        "every save of the manual title must stamp title_is_manual: {titles:?}"
    );
    let upsert_path = format!("/sessions/{SESSION_ID}");
    let upserted_title = server.requests().into_iter().rev().find_map(|r| {
        (r.method == "PUT" && r.path == upsert_path)
            .then(|| {
                r.body
                    .as_ref()?
                    .get("session")?
                    .get("title")?
                    .as_str()
                    .map(str::to_owned)
            })
            .flatten()
    });
    assert_eq!(
        upserted_title.as_deref(),
        Some(NEW_TITLE),
        "SetTitle must upsert the session-row title, not only the metadata blob; requests={:?}",
        request_path_summary(&server)
    );
    actor.stop().await;
}

#[derive(Debug)]
struct SaveTitle {
    title: String,
    title_present: bool,
    nonempty_messages: bool,
    title_is_manual: Option<bool>,
}

fn save_session_titles(
    server: &distill_test_support::MockInferenceServer,
    session_id: &str,
) -> Vec<SaveTitle> {
    let path = format!("/sessions/{session_id}/data");
    server
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path == path)
        .filter_map(|r| {
            let body = r.body.as_ref()?;
            let metadata = body.get("metadata");
            let title_field = metadata.and_then(|m| m.get("title"));
            let title_present = title_field.is_some();
            let title = title_field
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_owned();
            let title_is_manual = metadata
                .and_then(|m| m.get("title_is_manual"))
                .and_then(|v| v.as_bool());
            let nonempty_messages = body
                .get("messages")
                .and_then(|m| m.as_array())
                .is_some_and(|msgs| !msgs.is_empty());
            Some(SaveTitle {
                title,
                title_present,
                nonempty_messages,
                title_is_manual,
            })
        })
        .collect()
}

fn request_path_summary(server: &distill_test_support::MockInferenceServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

async fn wait_for_save_session_titles(
    server: &distill_test_support::MockInferenceServer,
    session_id: &str,
) -> Vec<SaveTitle> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let titles = save_session_titles(server, session_id);
        if titles.iter().any(|t| t.nonempty_messages) {
            return titles;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for flush save_session_data; requests={:?}",
                request_path_summary(server)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn manual_after_auto_last_flush_is_manual() {
    use std::sync::Arc;

    use crate::remote::BackendClient;
    use crate::session::export::ExportedMetadata;
    use distill_login::{AuthManager, GrokAuth};
    use distill_test_support::MockInferenceServer;

    const AUTO: &str = "Auto title";
    const MANUAL: &str = "Manual wins";
    const SESSION_ID: &str = "rename-fifo-auto-then-manual";

    let server = MockInferenceServer::start()
        .await
        .expect("start MockInferenceServer");
    let home = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(
        home.path(),
        distill_login::GrokComConfig::default(),
    ));
    auth.hot_swap(GrokAuth {
        key: "writeback-test-token".into(),
        ..GrokAuth::test_default()
    });

    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let metadata = ExportedMetadata {
        title: Some("pre".into()),
        cwd: info.cwd.clone(),
        model_id: Some("test-model".into()),
        created_at: None,
        updated_at: None,
        total_messages: None,
        parent_session_id: None,
        agent_id: None,
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: None,
    };
    let client = BackendClient::with_base_url(server.origin()).with_auth_manager(auth);
    let remote_sync = RemoteSync::new(SESSION_ID.to_owned(), metadata, client);
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));

    actor
        .handle
        .tx
        .send(PersistenceMsg::GeneratedTitle(AUTO.into()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::ManualTitleRenamed(MANUAL.into()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let titles = wait_for_save_session_titles(&server, SESSION_ID).await;
    let last = titles
        .iter()
        .rev()
        .find(|t| t.nonempty_messages)
        .map(|t| t.title.as_str());
    assert_eq!(
        last,
        Some(MANUAL),
        "auto-then-manual last flush must be manual"
    );
    assert!(
        titles
            .iter()
            .filter(|t| t.title == AUTO)
            .all(|t| t.title_is_manual.is_none()),
        "auto SetTitle must omit title_is_manual: {titles:?}"
    );
    assert!(
        titles
            .iter()
            .filter(|t| t.title == MANUAL)
            .all(|t| t.title_is_manual == Some(true)),
        "manual SetTitle/flush must stamp title_is_manual: {titles:?}"
    );
    actor.stop().await;
}

#[tokio::test]
async fn auto_after_committed_manual_emits_no_set_title() {
    use std::sync::Arc;

    use crate::remote::BackendClient;
    use crate::session::export::ExportedMetadata;
    use distill_login::{AuthManager, GrokAuth};
    use distill_test_support::MockInferenceServer;

    const AUTO: &str = "Rejected auto";
    const MANUAL: &str = "Pinned manual";
    const SESSION_ID: &str = "rename-fifo-manual-then-auto";

    let server = MockInferenceServer::start()
        .await
        .expect("start MockInferenceServer");
    let home = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(
        home.path(),
        distill_login::GrokComConfig::default(),
    ));
    auth.hot_swap(GrokAuth {
        key: "writeback-test-token".into(),
        ..GrokAuth::test_default()
    });

    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_session_title(&info, MANUAL.to_owned())
        .await
        .unwrap();

    let metadata = ExportedMetadata {
        title: Some("stale".into()),
        cwd: info.cwd.clone(),
        model_id: Some("test-model".into()),
        created_at: None,
        updated_at: None,
        total_messages: None,
        parent_session_id: None,
        agent_id: None,
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: None,
    };
    let client = BackendClient::with_base_url(server.origin()).with_auth_manager(auth);
    let remote_sync = RemoteSync::new(SESSION_ID.to_owned(), metadata, client);
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));

    actor
        .handle
        .tx
        .send(PersistenceMsg::ManualTitleRenamed(MANUAL.into()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::GeneratedTitle(AUTO.into()))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let titles = wait_for_save_session_titles(&server, SESSION_ID).await;
    assert!(
        titles.iter().all(|t| t.title != AUTO),
        "rejected auto title must not reach save_session_data"
    );
    let last = titles
        .iter()
        .rev()
        .find(|t| t.nonempty_messages)
        .map(|t| t.title.as_str());
    assert_eq!(last, Some(MANUAL));
    assert!(
        titles
            .iter()
            .filter(|t| t.title == MANUAL)
            .all(|t| t.title_is_manual == Some(true)),
        "manual stamp must survive rejected auto + flush: {titles:?}"
    );
    actor.stop().await;
}

#[tokio::test]
async fn manual_title_renamed_is_noop_without_remote_sync() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("rename-local-only"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ManualTitleRenamed("Local only".into()))
        .unwrap();
    flush_ack(&actor.handle)
        .await
        .expect("ManualTitleRenamed with remote_sync=None must not fail");
    actor.stop().await;
}

/// A ContentChunk before reset must not spawn.
/// ResetTitleToAuto must `reset()` and clear the remote pin.
/// A later ContentChunk must adopt via the fallback (empty model, no live LLM).
#[tokio::test]
async fn reset_title_to_auto_then_generated_title_is_adopted() {
    use std::sync::Arc;

    use crate::remote::BackendClient;
    use crate::session::export::ExportedMetadata;
    use crate::session::helpers::session_summary::title_fallback_from_user_text;
    use crate::session::persistence::PersistenceContentChunk;
    use distill_login::{AuthManager, GrokAuth};
    use distill_test_support::MockInferenceServer;

    const AUTO: &str = "Auto first title";
    const MANUAL: &str = "Pinned manual";
    const CHUNK: &str = "fresh auto title from next chunk please";
    const SESSION_ID: &str = "rename-reset-to-auto";
    let expected_fresh = title_fallback_from_user_text(CHUNK);

    let server = MockInferenceServer::start()
        .await
        .expect("start MockInferenceServer");
    let home = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(
        home.path(),
        distill_login::GrokComConfig::default(),
    ));
    auth.hot_swap(GrokAuth {
        key: "writeback-test-token".into(),
        ..GrokAuth::test_default()
    });

    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert!(
        storage
            .set_generated_title_if_absent(&info, AUTO.to_owned())
            .await
            .unwrap()
    );
    storage
        .update_session_title(&info, MANUAL.to_owned())
        .await
        .unwrap();
    assert!(
        !storage
            .set_generated_title_if_absent(&info, "Rejected auto".into())
            .await
            .unwrap(),
        "manual pin must reject auto title before reset"
    );

    let metadata = ExportedMetadata {
        title: Some(MANUAL.into()),
        cwd: info.cwd.clone(),
        model_id: Some("test-model".into()),
        created_at: None,
        updated_at: None,
        total_messages: None,
        parent_session_id: None,
        agent_id: None,
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: Some(true),
    };
    let client = BackendClient::with_base_url(server.origin()).with_auth_manager(auth);
    let remote_sync = RemoteSync::new(SESSION_ID.to_owned(), metadata, client);
    let actor = test_actor_inner(
        info.clone(),
        storage.clone(),
        Some(remote_sync),
        true, /* mark_summary_done: production load after a titled session */
    );

    let pre_reset_chunk = PersistenceContentChunk::new(vec![acp::ContentBlock::Text(
        acp::TextContent::new("should not generate while still manual and Done"),
    )]);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(pre_reset_chunk))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();
    let summary_path = dir.path().join("summary.json");
    let still_manual: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    assert_eq!(still_manual.display_title(), MANUAL);
    assert!(still_manual.title_is_manual);
    assert!(
        save_session_titles(&server, SESSION_ID)
            .iter()
            .filter(|t| t.title_present)
            .all(|t| t.title == MANUAL),
        "Done generator must not adopt an in-flight title before reset"
    );

    assert!(storage.reset_title_to_auto(&info).await.unwrap());
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after reset")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let titles = save_session_titles(&server, SESSION_ID);
        if titles
            .iter()
            .any(|t| t.title_present && t.title.is_empty() && t.title_is_manual == Some(false))
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "unpin must POST title:\"\" and title_is_manual:false (merge backends keep a prior true if omitted): {titles:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // ClearTitle is save_session_data then upsert_session on the remote-sync
    // task; the POST can become visible before the PUT is recorded under load.
    // The sync task sends the row PUT only after the data POST's response.
    let upsert_path = format!("/sessions/{SESSION_ID}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let upserted_title = loop {
        let found = server.requests().into_iter().rev().find_map(|r| {
            (r.method == "PUT" && r.path == upsert_path)
                .then(|| {
                    r.body
                        .as_ref()?
                        .get("session")?
                        .get("title")?
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten()
        });
        if found.as_deref() == Some("") {
            break found;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "ClearTitle must upsert the session-row title empty, not only the metadata blob; last={found:?} requests={:?}",
                request_path_summary(&server)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        upserted_title.as_deref(),
        Some(""),
        "ClearTitle must upsert the session-row title empty, not only the metadata blob; requests={:?}",
        request_path_summary(&server)
    );

    let post_reset: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    assert!(
        post_reset.display_title().trim().is_empty(),
        "display_title must be blank so if-absent can adopt"
    );

    let post_reset_chunk =
        PersistenceContentChunk::new(vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))]);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(post_reset_chunk))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after auto")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if on_disk.display_title() == expected_fresh && !on_disk.title_is_manual {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "ContentChunk after reset never adopted fallback {expected_fresh:?}; display={:?}",
                on_disk.display_title()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(on_disk.display_title(), expected_fresh);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let titles = save_session_titles(&server, SESSION_ID);
        if titles
            .iter()
            .any(|t| t.title == expected_fresh && t.title_is_manual.is_none())
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("adopted auto title after reset never reached save_session_data: {titles:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    actor.stop().await;
}

/// A title generation from the first chunk is still running when the unpin lands.
/// Disk is already blank when the stale `GeneratedTitle` arrives, so `set_generated_title_if_absent` adopts it as auto (never re-pins).
#[tokio::test]
async fn reset_title_to_auto_adopts_in_flight_generation_as_auto() {
    use std::sync::Arc;

    use crate::session::persistence::PersistenceContentChunk;

    const MANUAL: &str = "Pinned manual";
    const CHUNK: &str = "in flight chunk text for title fallback";

    let info = Info {
        id: acp::SessionId::new("rename-reset-inflight"),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_session_title(&info, MANUAL.to_owned())
        .await
        .unwrap();

    let actor = test_actor_inner(info.clone(), storage.clone(), None, false);

    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
            vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))],
        )))
        .unwrap();
    assert!(storage.reset_title_to_auto(&info).await.unwrap());
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let summary_path = dir.path().join("summary.json");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if !on_disk.display_title().trim().is_empty() {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("in-flight GeneratedTitle after unpin never adopted");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_ne!(on_disk.display_title(), MANUAL);
    assert!(
        !on_disk.title_is_manual,
        "in-flight adopt after unpin must stay auto"
    );
    actor.stop().await;
}

/// An unpin while the session is not resident only patches disk.
/// The next load sees a blank `display_title()` so the generator stays Idle and a ContentChunk adopts.
#[tokio::test]
async fn non_resident_reset_then_load_regenerates() {
    use std::sync::Arc;

    use crate::session::helpers::session_summary::title_fallback_from_user_text;
    use crate::session::persistence::PersistenceContentChunk;

    const CHUNK: &str = "dormant session next turn title text";
    let expected = title_fallback_from_user_text(CHUNK);

    let info = Info {
        id: acp::SessionId::new("rename-reset-dormant-load"),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert!(
        storage
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap()
    );
    storage
        .update_session_title(&info, "Manual Title".into())
        .await
        .unwrap();
    assert!(storage.reset_title_to_auto(&info).await.unwrap());

    let summary_path = dir.path().join("summary.json");
    let after_reset: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    let has_title = !after_reset.display_title().is_empty();
    assert!(
        !has_title,
        "production load would mark_done() if display_title stayed set"
    );

    let actor = test_actor_inner(info.clone(), storage, None, has_title);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
            vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))],
        )))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if on_disk.display_title() == expected && !on_disk.title_is_manual {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "load after non-resident unpin never adopted {expected:?}; display={:?}",
                on_disk.display_title()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(on_disk.display_title(), expected);
    actor.stop().await;
}

#[tokio::test]
async fn reset_title_to_auto_is_noop_without_remote_sync() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("reset-local-only"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_session_title(&info, "Manual".into())
        .await
        .unwrap();
    storage.reset_title_to_auto(&info).await.unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    flush_ack(&actor.handle)
        .await
        .expect("ResetTitleToAuto with remote_sync=None must not fail");
    actor.stop().await;
}

#[tokio::test]
async fn successful_append_clears_disk_full_latch() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("disk-full-clear"),
        cwd: "/test".into(),
    };
    let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fail_flag = fail.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        move |_| {
            if fail_flag.load(std::sync::atomic::Ordering::SeqCst) {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            } else {
                Ok(())
            }
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_err());
    assert!(actor.handle.is_disk_full());

    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "recovered")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_ok());
    assert!(!actor.handle.is_disk_full());
    actor.stop().await;
}

#[tokio::test]
async fn successful_probe_writable_clears_disk_full_latch() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("disk-full-probe"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        |_| Err(io::Error::from(io::ErrorKind::StorageFull)),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_err());
    assert!(actor.handle.is_disk_full());

    assert!(probe_writable(&actor.handle).await.is_ok());
    assert!(!actor.handle.is_disk_full());
    actor.stop().await;
}

#[cfg(unix)]
mod prompt_file_tests {
    use super::*;
    use crate::test_support::unix_mode;

    #[test]
    fn prompt_file_dir_chain_is_owner_only() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("prompt-perm-test"),
            cwd: "/some/project".to_string(),
        };

        let path = get_prompt_file_path_in(home.path(), &info, 0);

        // The chain below prompts/ is ensure_owner_only_session_dir_in's job, pinned by ensure_owner_only_session_dir_tightens_chain
        // Only the prompts/ level is this path's own creation
        let prompts_dir = path.parent().unwrap();
        assert_eq!(unix_mode(prompts_dir), 0o700, "prompts dir must be 0700");
    }

    /// ensure_owner_only_session_dir_in is the dir creator for chat-kind (noop-persistence) writers.
    #[test]
    fn ensure_owner_only_session_dir_tightens_chain() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("chat-kind-perm-test"),
            cwd: "/some/project".to_string(),
        };

        let dir = ensure_owner_only_session_dir_in(home.path(), &info).unwrap();

        assert_eq!(unix_mode(&dir), 0o700, "session dir must be 0700");
        assert_eq!(
            unix_mode(dir.parent().unwrap()),
            0o700,
            "<encoded-cwd> dir must be 0700"
        );
        assert_eq!(
            unix_mode(&home.path().join("sessions")),
            0o700,
            "sessions root must be 0700"
        );
    }

    #[test]
    fn ensure_owner_only_session_dir_syncs_each_parent_that_gained_an_entry() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("chat-kind-sync-test"),
            cwd: "/some/project".to_string(),
        };
        let dir = session_dir_in(home.path(), &info);
        let synced = std::cell::RefCell::new(Vec::new());

        ensure_owner_only_session_dir_in_with(
            home.path(),
            &info,
            |path| {
                synced.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();

        assert!(dir.is_dir());
        let synced = synced.borrow();
        assert!(
            synced.iter().any(|path| path == dir.parent().unwrap()),
            "encoded-cwd must be synced after gaining the session direntry, got {synced:?}"
        );
        assert!(
            synced
                .iter()
                .any(|path| path == &home.path().join("sessions")),
            "sessions root must be synced after gaining encoded-cwd, got {synced:?}"
        );
    }

    #[test]
    fn ensure_owner_only_session_dir_does_not_return_ok_when_a_parent_sync_fails() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("chat-kind-sync-fail"),
            cwd: "/some/project".to_string(),
        };

        let error = ensure_owner_only_session_dir_in_with(
            home.path(),
            &info,
            |_| Err(std::io::Error::other("directory barrier failed")),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "directory barrier failed");
        assert!(
            session_dir_in(home.path(), &info).is_dir(),
            "create must still leave the session dir so a retry can finish the barrier"
        );
    }

    #[test]
    fn ensure_owner_only_session_dir_skips_parent_sync_on_a_populated_session() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("chat-kind-occupied"),
            cwd: "/some/project".to_string(),
        };
        let dir = ensure_owner_only_session_dir_in(home.path(), &info).unwrap();
        std::fs::write(dir.join("summary.json"), b"{}").unwrap();

        let synced = std::cell::RefCell::new(Vec::new());
        ensure_owner_only_session_dir_in_with(
            home.path(),
            &info,
            |path| {
                synced.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            synced.borrow().is_empty(),
            "occupied resume must not fsync ancestors, got {:?}",
            synced.borrow()
        );
    }

    /// Hash-encoded `.cwd` contents must hit stable media before the parent dir sync that makes the direntry durable.
    /// Otherwise power loss can freeze a present-but-torn marker and path recovery cannot fall back to missing.
    #[test]
    fn hash_encoded_cwd_marker_is_synced_before_parent_dir_sync() {
        let home = tempfile::TempDir::new().unwrap();
        // URL-encoded form exceeds 255 bytes, so encode writes `.cwd`.
        let long_cwd = format!("/Users/test/{}", "中".repeat(80));
        let info = Info {
            id: agent_client_protocol::SessionId::new("cwd-marker-sync"),
            cwd: long_cwd,
        };
        let cwd_dir = crate::util::distill_home::sessions_cwd_dir_in(home.path(), &info.cwd);
        let cwd_file = cwd_dir.join(".cwd");

        let events = std::cell::RefCell::new(Vec::new());
        ensure_owner_only_session_dir_in_with(
            home.path(),
            &info,
            |path| {
                events.borrow_mut().push(format!("dir:{}", path.display()));
                Ok(())
            },
            |_file| {
                events
                    .borrow_mut()
                    .push(format!("file:{}", cwd_file.display()));
                Ok(())
            },
        )
        .unwrap();

        assert!(
            cwd_file.is_file(),
            "hash-encoded cwd must write a .cwd marker"
        );
        let events = events.borrow();
        let file_pos = events
            .iter()
            .position(|event| event == &format!("file:{}", cwd_file.display()))
            .unwrap_or_else(|| {
                panic!("cwd marker file must be fsynced before parent dir sync, got {events:?}")
            });
        let dir_pos = events
            .iter()
            .position(|event| event == &format!("dir:{}", cwd_dir.display()))
            .unwrap_or_else(|| {
                panic!("encoded-cwd parent must be synced after gaining .cwd, got {events:?}")
            });
        assert!(
            file_pos < dir_pos,
            ".cwd file sync must happen before the parent-dir sync that would freeze the direntry, got {events:?}"
        );
    }
}

/// The caller's pre-lock snapshot must not clobber an agent id a concurrent cold-spawn stamp persisted in between.
#[tokio::test]
async fn stamp_session_identity_prefers_live_agent_id_over_stale_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("identity-race"),
        cwd: "/test".into(),
    };
    let storage = JsonlStorageAdapter::with_explicit_session_dir(dir.path().join("session"));
    let summary = storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert!(
        summary.agent_id.is_none(),
        "snapshot taken before any stamp"
    );

    // A concurrent cold spawn lands first.
    let concurrent = stamp_session_identity(&storage, &info, None).await.unwrap();

    // The stale snapshot (None) must not mint a fresh agent over the live one.
    let stamped = stamp_session_identity(&storage, &info, summary.agent_id.as_deref())
        .await
        .unwrap();
    assert_eq!(stamped.agent_id, concurrent.agent_id);
    assert_ne!(stamped.attempt_id, concurrent.attempt_id);

    let on_disk = storage.load_summary(&info).await.unwrap();
    assert_eq!(on_disk.agent_id, Some(concurrent.agent_id));
    assert_eq!(on_disk.attempt_id, Some(stamped.attempt_id));
}

/// With nothing on disk, the snapshot is the resume source: a stale-but-parseable snapshot id is kept.
#[tokio::test]
async fn stamp_session_identity_falls_back_to_snapshot_when_disk_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("identity-fallback"),
        cwd: "/test".into(),
    };
    let storage = JsonlStorageAdapter::with_explicit_session_dir(dir.path().join("session"));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let stamped = stamp_session_identity(&storage, &info, Some("ag1.c0ffee"))
        .await
        .unwrap();
    assert_eq!(stamped.agent_id, "ag1.c0ffee");

    let on_disk = storage.load_summary(&info).await.unwrap();
    assert_eq!(on_disk.agent_id.as_deref(), Some("ag1.c0ffee"));
    assert_eq!(on_disk.attempt_id, Some(stamped.attempt_id));
}
