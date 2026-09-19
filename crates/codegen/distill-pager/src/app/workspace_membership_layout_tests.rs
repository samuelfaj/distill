// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

#[test]
fn newer_pin_gesture_survives_acknowledgement_of_in_flight_value() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    assert!(
        membership
            .view()
            .unwrap()
            .members
            .first()
            .is_some_and(|m| m.pin_rank.is_some())
    );
    let (store, patch) = take_layout_write(&mut membership);

    membership.request_pin(key("saved"), false).unwrap();
    let mut committed = snapshot(vec![member("saved", "Saved")]);
    let Some(m) = committed.members.get_mut(0) else {
        panic!("expected member: {:?}", committed.members);
    };
    m.pin_rank = Some(distill_dashboard_store::RANK_GAP);
    complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Committed(committed),
    );

    let view = membership.view().unwrap();
    let m = view
        .members
        .first()
        .unwrap_or_else(|| panic!("expected a member"));
    assert_eq!(m.pin_rank, None);
    let (_store, next) = take_layout_write(&mut membership);
    assert_eq!(next.pin_assignments.len(), 1);
    assert!(next.pin_assignments.first().is_some_and(|a| !a.pinned));
}

#[test]
fn busy_layout_retries_once_from_newest_semantic_overlay() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    membership.request_pin(key("saved"), false).unwrap();
    membership
        .request_grouping(WorkspaceGrouping::Directory)
        .unwrap();

    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Failed {
            error: StoreError::Busy { waited_ms: 1 },
        },
    );

    assert!(transition.notices.is_empty());
    let (_store, retry) = take_layout_write(&mut membership);
    assert!(retry.pin_assignments.is_empty());
    assert_eq!(retry.grouping, Some(LayoutGrouping::Directory));
}

#[test]
fn rolled_back_busy_layout_also_retries_without_dropping_optimism() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (store, patch) = take_layout_write(&mut membership);

    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Rejected {
            error: StoreError::Busy { waited_ms: 1 },
            snapshot: snapshot(vec![member("saved", "Saved")]),
        },
    );

    assert!(transition.notices.is_empty());
    assert!(
        membership
            .view()
            .unwrap()
            .members
            .first()
            .is_some_and(|m| m.pin_rank.is_some())
    );
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Layout(_),
            ..
        }]
    ));
}

#[test]
fn busy_retry_budget_resets_when_newer_gesture_returns_to_committed_value() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Failed {
            error: StoreError::Busy { waited_ms: 1 },
        },
    );

    membership.request_pin(key("saved"), false).unwrap();
    assert!(membership.next_effect(vec![]).effects.is_empty());
    membership
        .request_grouping(WorkspaceGrouping::Directory)
        .unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Failed {
            error: StoreError::Busy { waited_ms: 1 },
        },
    );

    assert!(transition.notices.is_empty());
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Layout(_),
            ..
        }]
    ));
}

#[test]
fn terminal_layout_failure_rolls_back_matching_values_but_keeps_newer_edits() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    membership
        .request_grouping(WorkspaceGrouping::Directory)
        .unwrap();

    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Failed {
            error: StoreError::Io(std::io::Error::other("write failed")),
        },
    );

    let view = membership.view().unwrap();
    let m = view
        .members
        .first()
        .unwrap_or_else(|| panic!("expected a member"));
    assert_eq!(m.pin_rank, None);
    assert_eq!(view.grouping, WorkspaceGrouping::Directory);
    assert!(matches!(
        transition.notices.as_slice(),
        [WorkspaceNotice::LayoutFailed { .. }]
    ));
    let (_store, next) = take_layout_write(&mut membership);
    assert_eq!(next.grouping, Some(LayoutGrouping::Directory));
    assert!(next.pin_assignments.is_empty());
}

#[test]
fn fatal_layout_failure_clears_even_newer_optimistic_gestures() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    membership
        .request_grouping(WorkspaceGrouping::Directory)
        .unwrap();
    membership
        .request_removal("saved", RemovalCause::Archive)
        .unwrap();

    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Failed {
            error: StoreError::NewerSchema {
                found: 2,
                supported: 1,
            },
        },
    );

    assert!(membership.writes_disabled());
    let view = membership.view().unwrap();
    assert_eq!(view.grouping, WorkspaceGrouping::State);
    assert_eq!(view.members.len(), 1);
    assert_eq!(view.members.first().and_then(|m| m.pin_rank), None);
    assert!(matches!(
        transition.notices.as_slice(),
        [WorkspaceNotice::ReadOnly]
    ));
    let (store, _) = take_refresh(&mut membership);
    membership.on_refresh_completed(store, Ok(None), &HashSet::new());
    assert!(membership.writes_disabled());
}

#[test]
fn read_only_layout_refuses_before_optimistic_mutation() {
    let (_temp, store) = temp_store();
    let mut membership = WorkspaceMembership::default();
    membership.set_read_only_for_test(store, snapshot(vec![member("saved", "Saved")]));

    assert_eq!(
        membership.request_pin(key("saved"), true),
        Err(LayoutRequestError::ReadOnly)
    );
    let view = membership.view().unwrap();
    let m = view
        .members
        .first()
        .unwrap_or_else(|| panic!("expected a member"));
    assert_eq!(m.pin_rank, None);
    assert!(membership.next_effect(vec![]).effects.is_empty());
}

#[test]
fn missing_order_member_rebases_and_retries_remaining_semantic_order() {
    let (_temp, mut membership) = ready_membership(vec![member("a", "A"), member("b", "B")]);
    membership
        .request_manual_order(vec![key("a"), key("b")])
        .unwrap();
    let (store, patch) = take_layout_write(&mut membership);
    let rolled_back = snapshot(vec![member("a", "A")]);

    let transition = complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Rejected {
            error: StoreError::MemberNotFound {
                session_id: "b".into(),
                kind: "build".into(),
            },
            snapshot: rolled_back,
        },
    );
    assert!(transition.notices.is_empty());

    let (_store, retry) = take_layout_write(&mut membership);
    assert_eq!(retry.manual_order, Some(vec![key("a")]));
}

#[test]
fn gesture_accepted_during_refresh_rebases_over_foreign_snapshot() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.mark_clean_for_test();
    let (store, _) = take_refresh(&mut membership);
    membership.request_pin(key("saved"), true).unwrap();
    let mut foreign = snapshot(vec![member("saved", "Peer title")]);
    foreign.grouping = Grouping::Directory;
    foreign.data_version = 2;

    membership.on_refresh_completed(store, Ok(Some(foreign)), &HashSet::new());

    let view = membership.view().unwrap();
    assert_eq!(view.grouping, WorkspaceGrouping::Directory);
    let Some(m) = view.members.first() else {
        panic!("expected member: {:?}", view.members);
    };
    assert_eq!(m.title.as_deref(), Some("Peer title"));
    assert!(m.pin_rank.is_some());
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Layout(_),
            ..
        }]
    ));
}

#[test]
fn lost_layout_writer_reopens_and_preserves_semantic_overlay() {
    let (_temp, mut membership) = ready_membership(vec![member("saved", "Saved")]);
    membership.request_pin(key("saved"), true).unwrap();
    let (lost_store, _) = take_layout_write(&mut membership);
    let db_path = lost_store.path().to_path_buf();
    drop(lost_store);

    membership.on_write_task_lost(db_path.clone(), "writer lost".into());
    let reopened = WorkspaceStore::open(&db_path).unwrap();
    membership.on_store_opened(
        reopened,
        snapshot(vec![member("saved", "Saved")]),
        &HashSet::new(),
    );

    assert!(
        membership
            .view()
            .unwrap()
            .members
            .first()
            .is_some_and(|m| m.pin_rank.is_some())
    );
    assert!(matches!(
        membership.next_effect(vec![]).effects.as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Layout(_),
            ..
        }]
    ));
}

#[test]
fn layout_is_scheduled_between_removal_and_upsert_and_blocks_refresh() {
    let (_temp, mut membership) =
        ready_membership(vec![member("remove", "Remove"), member("pin", "Pin")]);
    membership.mark_clean_for_test();
    membership.request_pin(key("pin"), true).unwrap();
    membership
        .request_removal("remove", RemovalCause::Archive)
        .unwrap();
    assert!(membership.request_refresh().effects.is_empty());

    let (store, removal) = take_next_write(&mut membership, vec![]);
    assert!(matches!(removal, WorkspaceMutation::Remove(_)));
    membership.request_sync();
    complete_write(
        &mut membership,
        store,
        Ok(snapshot(vec![member("pin", "Pin")])),
        vec![],
        removal,
    );

    let (store, layout) = take_next_write(&mut membership, vec![new_member("new", "New")]);
    let WorkspaceMutation::Layout(patch) = layout else {
        panic!("layout must run after removal");
    };
    let mut committed = snapshot(vec![member("pin", "Pin")]);
    let Some(m) = committed.members.get_mut(0) else {
        panic!("expected member: {:?}", committed.members);
    };
    m.pin_rank = Some(distill_dashboard_store::RANK_GAP);
    complete_layout(
        &mut membership,
        store,
        patch,
        LayoutApplyOutcome::Committed(committed),
    );

    assert!(matches!(
        membership
            .next_effect(vec![new_member("new", "New")])
            .effects
            .as_slice(),
        [Effect::WriteWorkspace {
            mutation: WorkspaceMutation::Upsert(members),
            ..
        }] if matches!(members.as_slice(), [m] if m.key == key("new"))
    ));
}
