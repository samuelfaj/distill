// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::app::workspace_test_fixtures::{
    key, member, new_member, ready_membership, snapshot, temp_store,
};
use distill_dashboard_store::{Grouping, LayoutGrouping, StoreError};

impl WorkspaceMembership {
    pub(crate) fn set_snapshot_for_test(&mut self, snapshot: WorkspaceSnapshot) {
        self.committed_snapshot = Some(snapshot);
    }

    pub(crate) fn set_ready_for_test(
        &mut self,
        store: WorkspaceStore,
        snapshot: WorkspaceSnapshot,
    ) {
        self.io = WorkspaceIo::Ready {
            store,
            access: StoreAccess::ReadWrite,
        };
        self.committed_snapshot = Some(snapshot);
    }

    pub(crate) fn set_read_only_for_test(
        &mut self,
        store: WorkspaceStore,
        snapshot: WorkspaceSnapshot,
    ) {
        self.io = WorkspaceIo::Ready {
            store,
            access: StoreAccess::ReadOnly,
        };
        self.committed_snapshot = Some(snapshot);
    }

    pub(crate) fn suppress_for_test(&mut self, session_id: SessionId) {
        self.removals.insert(
            session_id,
            RemovalState::SuppressedUntilExplicitLoad {
                cause: SuppressionCause::HistoryDeleted,
            },
        );
    }

    pub(crate) fn removal_pending_for_test(&self, session_id: &SessionId) -> bool {
        matches!(
            self.removals.get(session_id),
            Some(RemovalState::Pending { .. })
        )
    }

    pub(crate) fn has_pending_removals_for_test(&self) -> bool {
        self.removals
            .values()
            .any(|state| matches!(state, RemovalState::Pending { .. }))
    }

    pub(crate) fn removal_suppressed_for_test(&self, session_id: &SessionId) -> bool {
        matches!(
            self.removals.get(session_id),
            Some(RemovalState::SuppressedUntilExplicitLoad { .. })
        )
    }

    fn mark_clean_for_test(&mut self) {
        self.dirty = false;
        self.sync_pending = false;
    }
}

fn take_next_write(
    membership: &mut WorkspaceMembership,
    candidates: Vec<NewMember>,
) -> (WorkspaceStore, WorkspaceMutation) {
    let mut effects = membership.next_effect(candidates).effects;
    assert_eq!(effects.len(), 1, "expected one workspace write");
    let Effect::WriteWorkspace { store, mutation } = effects.pop().unwrap() else {
        panic!("expected workspace write");
    };
    (store, mutation)
}

fn take_refresh(membership: &mut WorkspaceMembership) -> (WorkspaceStore, i64) {
    let mut effects = membership.request_refresh().effects;
    assert_eq!(effects.len(), 1, "expected one workspace refresh");
    let Effect::RefreshWorkspace {
        store,
        known_data_version,
    } = effects.pop().unwrap()
    else {
        panic!("expected workspace refresh");
    };
    (store, known_data_version)
}

fn complete_write(
    membership: &mut WorkspaceMembership,
    store: WorkspaceStore,
    snapshot: Result<WorkspaceSnapshot, String>,
    failures: Vec<WorkspaceMutationFailure>,
    mutation: WorkspaceMutation,
) -> WorkspaceTransition {
    let completion = match mutation {
        WorkspaceMutation::Upsert(members) => WorkspaceWriteCompletion::Upsert {
            members,
            snapshot,
            failures,
        },
        WorkspaceMutation::Remove(keys) => WorkspaceWriteCompletion::Remove {
            keys,
            snapshot,
            failures,
        },
        WorkspaceMutation::Layout(_) => panic!("layout uses complete_layout"),
    };
    membership.on_write_completed(store, completion, &HashSet::new())
}

fn take_layout_write(
    membership: &mut WorkspaceMembership,
) -> (WorkspaceStore, distill_dashboard_store::LayoutPatch) {
    let (store, mutation) = take_next_write(membership, vec![]);
    let WorkspaceMutation::Layout(patch) = mutation else {
        panic!("expected layout write");
    };
    (store, patch)
}

fn complete_layout(
    membership: &mut WorkspaceMembership,
    store: WorkspaceStore,
    patch: distill_dashboard_store::LayoutPatch,
    outcome: LayoutApplyOutcome,
) -> WorkspaceTransition {
    membership.on_write_completed(
        store,
        WorkspaceWriteCompletion::Layout { patch, outcome },
        &HashSet::new(),
    )
}

#[test]
fn ordinary_sync_does_not_activate_unopened_store() {
    let mut membership = WorkspaceMembership::default();

    membership.request_sync();
    assert!(membership.next_effect(vec![]).effects.is_empty());

    membership.activate();
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
}

#[test]
fn explicit_load_does_not_activate_pristine_controller() {
    let mut membership = WorkspaceMembership::default();

    membership.on_explicit_session_load("saved");

    assert!(membership.next_effect(vec![]).effects.is_empty());
    assert!(membership.explicit_adoptions.is_empty());
}

#[test]
fn removal_before_open_loads_store_then_precedes_upsert() {
    let mut membership = WorkspaceMembership::default();
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));

    let (_temp, store) = temp_store();
    membership.on_store_opened(
        store,
        snapshot(vec![member("saved", "Saved")]),
        &HashSet::new(),
    );
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Remove(keys),
            ..
        }] if keys == &[key("saved")]
    ));
}

#[test]
fn busy_removal_retries_once_then_becomes_terminal() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);
    let first = complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Saved")])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "busy".into(),
            retryable: true,
        }],
        mutation,
    );
    assert!(first.notices.is_empty());
    assert!(membership.removal_pending_for_test(&SessionId::new("saved").unwrap()));

    let (store, mutation) = take_next_write(&mut membership, vec![]);
    let second = complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Saved")])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "busy again".into(),
            retryable: true,
        }],
        mutation,
    );
    assert!(matches!(
        second.notices.as_slice(),
        [WorkspaceNotice::ArchiveFailed { count: 1, .. }]
    ));
}

#[test]
fn failed_archive_restores_optimistic_pin_and_order() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    membership.request_manual_order(vec![key("saved")]).unwrap();
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    assert!(membership.view().unwrap().members.is_empty());
    let (store, mutation) = take_next_write(&mut membership, vec![]);

    let transition = complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Saved")])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "write failed".into(),
            retryable: false,
        }],
        mutation,
    );

    assert!(matches!(
        transition.notices.as_slice(),
        [WorkspaceNotice::ArchiveFailed { .. }]
    ));
    let view = membership.view().unwrap();
    assert_eq!(view.members.len(), 1);
    assert!(view.members.first().is_some_and(|m| m.pin_rank.is_some()));
    assert_eq!(membership.effective_manual_order(), vec![key("saved")]);
}

#[test]
fn successful_retained_view_removal_becomes_suppression_tombstone() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![],
        mutation,
    );
    let id = SessionId::new("saved").unwrap();
    assert!(membership.removal_suppressed_for_test(&id));
    assert!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .is_empty()
    );

    membership.on_explicit_session_load("saved");
    membership.request_sync();
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn session_stays_hidden_from_removal_request_until_explicit_load() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    let id = SessionId::new("saved").unwrap();
    assert!(!membership.is_session_hidden(&id));

    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    assert!(membership.is_session_hidden(&id), "pending removal hides");

    let (store, mutation) = take_next_write(&mut membership, vec![]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![],
        mutation,
    );
    assert!(
        membership.is_session_hidden(&id),
        "suppression tombstone hides"
    );

    membership.on_explicit_session_load("saved");
    assert!(!membership.is_session_hidden(&id));
}

#[test]
fn explicit_load_during_pending_removal_prevents_success_tombstone() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);

    membership.request_sync();
    membership.on_explicit_session_load("saved");
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![],
        mutation,
    );

    let id = SessionId::new("saved").unwrap();
    assert!(!membership.removal_suppressed_for_test(&id));
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Reloaded")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn explicit_load_during_pending_removal_prevents_terminal_failure_tombstone() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);

    membership.request_sync();
    membership.on_explicit_session_load("saved");
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Saved")])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "failed".into(),
            retryable: false,
        }],
        mutation,
    );

    let id = SessionId::new("saved").unwrap();
    assert!(!membership.removal_suppressed_for_test(&id));
    assert!(membership.wants_upsert_candidates());
}

#[test]
fn terminal_retained_view_failure_keeps_suppression_until_explicit_load() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Saved")])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "all pinned".into(),
            retryable: false,
        }],
        mutation,
    );
    let id = SessionId::new("saved").unwrap();
    assert!(membership.removal_suppressed_for_test(&id));

    membership.request_sync();
    assert!(
        membership
            .next_effect(vec![new_member("saved", "Changed")])
            .effects
            .is_empty()
    );
    membership.on_explicit_session_load("saved");
    membership.request_sync();
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Changed")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn upsert_busy_retries_once_and_metadata_change_resets_suppression() {
    let (_temp, mut membership) = ready_membership(vec![]);
    let original = new_member("saved", "Original");
    let (store, mutation) = take_next_write(&mut membership, vec![original.clone()]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "busy".into(),
            retryable: true,
        }],
        mutation,
    );
    let (store, mutation) = take_next_write(&mut membership, vec![original.clone()]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![WorkspaceMutationFailure {
            key: key("saved"),
            error: "busy again".into(),
            retryable: true,
        }],
        mutation,
    );
    membership.request_sync();
    assert!(
        membership.next_effect(vec![original]).effects.is_empty(),
        "identical terminally failed metadata stays suppressed"
    );
    membership.request_sync();
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Changed")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn request_during_write_is_deferred_until_store_returns() {
    let (_temp, mut membership) = ready_membership(vec![]);
    let (store, mutation) = take_next_write(&mut membership, vec![new_member("saved", "Original")]);

    membership.request_sync();
    assert!(!membership.wants_upsert_candidates());
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("saved", "Original")])),
        vec![],
        mutation,
    );

    assert!(membership.wants_upsert_candidates());
    assert!(matches!(
        membership
            .next_effect(vec![
                new_member("saved", "Original"),
                new_member("next", "Next"),
            ])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(members),
            ..
        }] if matches!(members.as_slice(), [m] if m.key.session_id.as_ref() == "next")
    ));
}

#[test]
fn capacity_reserves_space_for_pinned_non_candidates() {
    let stored = (0..WORKSPACE_CAPACITY - 1)
        .map(|index| {
            let mut member = member(&format!("pinned-{index}"), "Pinned");
            member.pin_rank = Some(index as i64);
            member
        })
        .collect();
    let (_temp, mut membership) = ready_membership(stored);
    let (_, WorkspaceMutation::Upsert(members)) = take_next_write(
        &mut membership,
        vec![
            new_member("new-0", "New zero"),
            new_member("new-1", "New one"),
        ],
    ) else {
        panic!("expected upsert");
    };
    assert_eq!(members.len(), 1);
}

#[test]
fn post_write_snapshot_failure_reopens_without_suppressing_upsert() {
    let (_temp, mut membership) = ready_membership(vec![]);
    let candidate = new_member("saved", "Saved");
    let (store, mutation) = take_next_write(&mut membership, vec![candidate.clone()]);
    let db_path = store.path().to_path_buf();
    let transition = complete_write(
        &mut membership,
        store,
        Err("snapshot failed".into()),
        vec![],
        mutation,
    );
    assert!(matches!(
        transition.effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    assert!(matches!(
        transition.notices.as_slice(),
        [WorkspaceNotice::Refreshing { .. }]
    ));

    let reopened = WorkspaceStore::open(&db_path).unwrap();
    membership.on_store_opened(reopened, snapshot(vec![]), &HashSet::new());
    assert!(matches!(
        membership.next_effect(vec![candidate]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn post_write_reopen_retries_once_then_parks() {
    let (_temp, mut membership) = ready_membership(vec![]);
    let (store, mutation) = take_next_write(&mut membership, vec![new_member("saved", "Saved")]);
    assert!(matches!(
        complete_write(
            &mut membership,
            store,
            Err("snapshot failed".into()),
            vec![],
            mutation,
        )
        .effects
        .as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    let retry = membership.on_store_open_failed("reopen failed".into(), true);
    assert!(matches!(
        retry.effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    assert!(retry.notices.is_empty());
    let parked = membership.on_store_open_failed("retry failed".into(), true);
    assert!(parked.effects.is_empty());
    assert!(matches!(
        parked.notices.as_slice(),
        [WorkspaceNotice::LoadFailed { .. }]
    ));
}

#[test]
fn changed_refresh_suppresses_removed_live_member_until_explicit_load() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let (store, known_data_version) = take_refresh(&mut membership);
    assert_eq!(known_data_version, 1);
    let live = HashSet::from([SessionId::new("saved").unwrap()]);

    membership.on_refresh_completed(store, Ok(Some(snapshot(vec![]))), &live);

    let id = SessionId::new("saved").unwrap();
    assert!(membership.removal_suppressed_for_test(&id));
    membership.request_sync();
    assert!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .is_empty()
    );

    membership.on_explicit_session_load("saved");
    assert!(matches!(
        membership
            .next_effect(vec![new_member("saved", "Saved")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(_),
            ..
        }]
    ));
}

#[test]
fn explicit_load_during_refresh_prevents_new_snapshot_suppression() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let (store, _) = take_refresh(&mut membership);
    membership.on_explicit_session_load("saved");
    let live = HashSet::from([SessionId::new("saved").unwrap()]);

    membership.on_refresh_completed(store, Ok(Some(snapshot(vec![]))), &live);

    let id = SessionId::new("saved").unwrap();
    assert!(!membership.removal_suppressed_for_test(&id));
    assert!(membership.wants_upsert_candidates());
}

#[test]
fn peer_readd_clears_snapshot_removal_suppression() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let live = HashSet::from([SessionId::new("saved").unwrap()]);
    let (store, _) = take_refresh(&mut membership);
    membership.on_refresh_completed(store, Ok(Some(snapshot(vec![]))), &live);
    assert!(membership.removal_suppressed_for_test(&SessionId::new("saved").unwrap()));

    let (store, _) = take_refresh(&mut membership);
    membership.on_refresh_completed(
        store,
        Ok(Some(snapshot(vec![member("saved", "Saved")]))),
        &live,
    );

    assert!(!membership.removal_suppressed_for_test(&SessionId::new("saved").unwrap()));
}

#[test]
fn peer_readd_does_not_clear_history_deleted_suppression() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::HistoryDeletedWithRetainedView)
        .unwrap();
    let (store, mutation) = take_next_write(&mut membership, vec![]);
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![])),
        vec![],
        mutation,
    );
    assert!(membership.next_effect(vec![]).effects.is_empty());
    let (store, _) = take_refresh(&mut membership);

    membership.on_refresh_completed(
        store,
        Ok(Some(snapshot(vec![member("saved", "Peer re-add")]))),
        &HashSet::new(),
    );

    assert!(membership.removal_suppressed_for_test(&SessionId::new("saved").unwrap()));
    assert_eq!(membership.snapshot().unwrap().members.len(), 1);
}

#[test]
fn read_only_publication_rejects_pending_but_retains_suppression() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.suppress_for_test(SessionId::new("suppressed").unwrap());
    membership
        .request_removal("pending", RemovalCause::Archive)
        .unwrap();

    assert!(membership.publish_snapshot(
        StoreAccess::ReadOnly,
        snapshot(vec![
            member("saved", "Saved"),
            member("suppressed", "Suppressed"),
            member("pending", "Pending"),
        ]),
        &HashSet::new(),
    ));

    assert!(!membership.has_pending_removals());
    assert!(membership.removal_suppressed_for_test(&SessionId::new("suppressed").unwrap()));
    assert_eq!(membership.snapshot().unwrap().members.len(), 3);
}

#[test]
fn unchanged_and_failed_refresh_restore_the_handle_without_dirtying() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let (store, _) = take_refresh(&mut membership);

    let unchanged = membership.on_refresh_completed(store, Ok(None), &HashSet::new());
    assert!(unchanged.effects.is_empty());
    assert!(unchanged.notices.is_empty());
    assert!(!membership.dirty);

    let (store, _) = take_refresh(&mut membership);
    let failed = membership.on_refresh_completed(store, Err("busy".into()), &HashSet::new());
    assert!(matches!(
        failed.notices.as_slice(),
        [WorkspaceNotice::RefreshFailed { .. }]
    ));
    assert_eq!(membership.snapshot().unwrap().members.len(), 1);
    assert!(matches!(
        membership.request_refresh().effects.as_slice(),
        [Effect::RefreshWorkspace { .. }]
    ));
}

#[test]
fn foreign_commit_refreshes_through_the_same_store_handle() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("workspace.db");
    let store = WorkspaceStore::open(&db_path).unwrap();
    let initial = store.snapshot().unwrap();
    let mut membership = WorkspaceMembership::default();
    membership.on_store_opened(store, initial, &HashSet::new());
    let mut peer = WorkspaceStore::open(&db_path).unwrap();
    peer.insert_member(new_member("foreign", "Foreign"))
        .unwrap();

    let (store, known_data_version) = take_refresh(&mut membership);
    assert_ne!(store.data_version().unwrap(), known_data_version);
    let refreshed = store.snapshot().unwrap();
    membership.on_refresh_completed(store, Ok(Some(refreshed)), &HashSet::new());

    assert_eq!(
        membership
            .snapshot()
            .unwrap()
            .members
            .first()
            .map(|m| m.session_id.as_ref()),
        Some("foreign")
    );
}

#[test]
fn sync_requested_during_refresh_runs_after_handle_returns() {
    let (_temp, mut membership) = ready_membership(vec![]);
    membership.mark_clean_for_test();
    let (store, _) = take_refresh(&mut membership);

    membership.request_sync();
    assert!(membership.next_effect(vec![]).effects.is_empty());
    membership.on_refresh_completed(store, Ok(None), &HashSet::new());

    assert!(membership.wants_upsert_candidates());
}

#[test]
fn read_only_access_remains_visible_while_refresh_owns_handle() {
    let (temp, store) = temp_store();
    let mut membership = WorkspaceMembership::default();
    membership.set_read_only_for_test(store, snapshot(vec![]));
    membership.mark_clean_for_test();

    let (store, _) = take_refresh(&mut membership);

    assert!(membership.writes_disabled());
    assert_eq!(
        membership.request_removal("saved", RemovalCause::Archive),
        Err(RemovalRequestError::ReadOnly)
    );
    let db_path = store.path().to_path_buf();
    drop(store);
    membership.on_refresh_task_lost(db_path, "task lost".into());
    assert!(membership.writes_disabled());
    assert_eq!(
        membership.request_removal("saved", RemovalCause::Archive),
        Err(RemovalRequestError::ReadOnly)
    );
    drop(temp);
}

#[test]
fn clean_refresh_task_loss_reopens_without_fabricating_a_write() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let (lost_store, _) = take_refresh(&mut membership);
    let db_path = lost_store.path().to_path_buf();
    drop(lost_store);

    let transition = membership.on_refresh_task_lost(db_path.clone(), "task lost".into());
    assert!(matches!(
        transition.effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    let reopened = WorkspaceStore::open(&db_path).unwrap();
    membership.on_store_opened(
        reopened,
        snapshot(vec![member("saved", "Saved")]),
        &HashSet::new(),
    );

    assert!(membership.next_effect(vec![]).effects.is_empty());
}

#[test]
fn write_publication_suppresses_foreign_removal_of_live_member() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    let (store, mutation) = take_next_write(&mut membership, vec![new_member("next", "Next")]);
    let WorkspaceMutation::Upsert(members) = mutation else {
        panic!("expected upsert");
    };
    let live = HashSet::from([SessionId::new("saved").unwrap()]);

    membership.on_write_completed(
        store,
        WorkspaceWriteCompletion::Upsert {
            members,
            snapshot: Ok(snapshot(vec![member("next", "Next")])),
            failures: vec![],
        },
        &live,
    );

    assert!(membership.removal_suppressed_for_test(&SessionId::new("saved").unwrap()));
}

#[test]
fn lost_writer_reopens_and_preserves_queued_removal() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    let (lost_store, _mutation) = take_next_write(&mut membership, vec![]);
    let db_path = lost_store.path().to_path_buf();
    drop(lost_store);

    let transition = membership.on_write_task_lost(db_path.clone(), "writer lost".into());
    assert!(matches!(
        transition.effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    let reopened = WorkspaceStore::open(&db_path).unwrap();
    membership.on_store_opened(
        reopened,
        snapshot(vec![member("saved", "Saved")]),
        &HashSet::new(),
    );
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Remove(keys),
            ..
        }] if keys == &[key("saved")]
    ));
}

#[test]
fn read_only_open_restores_optimistic_removal() {
    let mut membership = WorkspaceMembership::default();
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    let (_temp, store) = temp_store();
    let store_snapshot = snapshot(vec![member("saved", "Saved")]);
    membership.io = WorkspaceIo::Ready {
        store,
        access: StoreAccess::ReadOnly,
    };
    assert!(membership.publish_snapshot(StoreAccess::ReadOnly, store_snapshot, &HashSet::new()));
    assert_eq!(membership.snapshot().unwrap().members.len(), 1);
    assert!(membership.removals.is_empty());
}

#[test]
fn failed_reopen_waits_for_later_request() {
    let mut membership = WorkspaceMembership::default();
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();
    membership.next_effect(vec![]);
    let failure = membership.on_store_open_failed("unavailable".into(), true);
    assert!(
        failure.effects.is_empty(),
        "queued removals must park without an automatic retry"
    );
    assert!(membership.next_effect(vec![]).effects.is_empty());

    membership.request_sync();
    assert!(
        membership.next_effect(vec![]).effects.is_empty(),
        "ordinary sync must not reactivate a parked open"
    );
    membership.activate();
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
}

#[test]
fn initial_open_retries_once_then_waits_for_request() {
    let mut membership = WorkspaceMembership::default();
    membership.activate();
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    let retry = membership.on_store_open_failed("initial failure".into(), true);
    assert!(matches!(
        retry.effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
    assert!(retry.notices.is_empty());
    let parked = membership.on_store_open_failed("retry failure".into(), true);
    assert!(parked.effects.is_empty());
    assert!(matches!(
        parked.notices.as_slice(),
        [WorkspaceNotice::LoadFailed { .. }]
    ));
    assert!(membership.next_effect(vec![]).effects.is_empty());

    membership.request_sync();
    assert!(membership.next_effect(vec![]).effects.is_empty());
    membership.activate();
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::LoadWorkspaceSnapshot { .. }]
    ));
}

#[test]
fn non_retryable_open_failure_parks_immediately() {
    let mut membership = WorkspaceMembership::default();
    membership.activate();
    membership.next_effect(vec![]);

    let parked = membership.on_store_open_failed("permission denied".into(), false);

    assert!(parked.effects.is_empty());
    assert!(matches!(
        parked.notices.as_slice(),
        [WorkspaceNotice::LoadFailed { .. }]
    ));
    membership.request_sync();
    assert!(membership.next_effect(vec![]).effects.is_empty());
}

#[path = "workspace_membership_layout_tests.rs"]
mod layout_tests;
