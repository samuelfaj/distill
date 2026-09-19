// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashSet;

use distill_dashboard_store::{
    Grouping, LayoutGrouping, Member, MemberKey, MemberKind, MemberOrigin, RANK_GAP, SessionId,
    WorkspaceSnapshot,
};

use super::{LayoutOverlay, WorkspaceGrouping, member_key};

fn member(id: &str, pin_rank: Option<i64>, order_rank: Option<i64>) -> Member {
    Member {
        session_id: SessionId::new(id).unwrap(),
        kind: MemberKind::Build,
        origin: MemberOrigin::Local,
        cwd: Some(format!("/{id}")),
        title: Some(id.to_owned()),
        model: None,
        last_turn_summary: None,
        is_worktree: false,
        last_change_unix_ms: 0,
        pin_rank,
        order_rank,
    }
}

fn snapshot() -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        grouping: Grouping::State,
        members: vec![
            member("a", None, Some(99)),
            member("b", Some(7), None),
            member("c", None, Some(1)),
        ],
        data_version: 4,
    }
}

fn key(id: &str) -> MemberKey {
    MemberKey {
        session_id: SessionId::new(id).unwrap(),
        kind: MemberKind::Build,
    }
}

#[test]
fn view_applies_layout_without_mutating_committed_snapshot() {
    let committed = snapshot();
    let original = committed.clone();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("a"), true);
    overlay.set_manual_order(vec![key("b"), key("a")]);
    overlay.set_grouping(LayoutGrouping::Directory);

    let view = overlay.view(&committed, &HashSet::new());

    assert_eq!(committed, original);
    assert_eq!(view.grouping, WorkspaceGrouping::Directory);
    let [a, b, c] = view.members.as_slice() else {
        panic!("expected 3 members: {:?}", view.members);
    };
    assert_eq!(a.pin_rank, Some(RANK_GAP));
    assert_eq!(a.order_rank, Some(2 * RANK_GAP));
    assert_eq!(b.order_rank, Some(RANK_GAP));
    assert_eq!(c.order_rank, None);
}

#[test]
fn plan_rewrites_complete_explicit_subset_deterministically() {
    let committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_manual_order(vec![key("b"), key("a"), key("b")]);

    let patch = overlay.plan(&committed, &HashSet::new());

    assert_eq!(patch.manual_order, Some(vec![key("b"), key("a")]));
    let first = overlay.view(&committed, &HashSet::new());
    let second = overlay.view(&committed, &HashSet::new());
    assert_eq!(first, second);
}

#[test]
fn committed_values_reconcile_to_an_empty_idempotent_patch() {
    let mut committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("a"), true);
    overlay.set_manual_order(vec![key("b"), key("a")]);
    overlay.set_grouping(LayoutGrouping::Directory);
    let dispatched = overlay.plan(&committed, &HashSet::new());

    if let Some(m) = committed.members.get_mut(0) {
        m.pin_rank = Some(RANK_GAP);
        m.order_rank = Some(2 * RANK_GAP);
    } else {
        panic!("expected member 0: {:?}", committed.members);
    }
    if let Some(m) = committed.members.get_mut(1) {
        m.order_rank = Some(RANK_GAP);
    } else {
        panic!("expected member 1: {:?}", committed.members);
    }
    if let Some(m) = committed.members.get_mut(2) {
        m.order_rank = None;
    } else {
        panic!("expected member 2: {:?}", committed.members);
    }
    committed.grouping = Grouping::Directory;
    overlay.acknowledge(&dispatched);
    overlay.reconcile_committed(&committed);

    assert!(overlay.is_empty());
    assert!(overlay.plan(&committed, &HashSet::new()).is_empty());
}

#[test]
fn exact_acknowledgement_preserves_newer_same_field_gesture() {
    let committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("a"), true);
    let dispatched = overlay.plan(&committed, &HashSet::new());
    overlay.set_pinned(key("a"), false);

    overlay.acknowledge(&dispatched);

    let view = overlay.view(&committed, &HashSet::new());
    let Some(m) = view.members.first() else {
        panic!("expected member: {:?}", view.members);
    };
    assert_eq!(m.pin_rank, None);
    assert!(!overlay.is_empty());
}

#[test]
fn exact_acknowledgement_preserves_newer_order_and_grouping() {
    let committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_manual_order(vec![key("a")]);
    overlay.set_grouping(LayoutGrouping::Directory);
    let dispatched = overlay.plan(&committed, &HashSet::new());
    overlay.set_manual_order(vec![key("b")]);
    overlay.set_grouping(LayoutGrouping::State);

    overlay.acknowledge(&dispatched);

    assert_eq!(overlay.manual_order, Some(vec![key("b")]));
    assert_eq!(overlay.grouping, Some(LayoutGrouping::State));
}

#[test]
fn pending_removal_filters_view_and_patch() {
    let committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("a"), true);
    overlay.set_manual_order(vec![key("a"), key("b")]);
    let pending = HashSet::from([key("a")]);

    let view = overlay.view(&committed, &pending);
    let patch = overlay.plan(&committed, &pending);

    assert_eq!(
        view.members.iter().map(member_key).collect::<HashSet<_>>(),
        HashSet::from([key("b"), key("c")])
    );
    assert!(patch.pin_assignments.is_empty());
    assert_eq!(patch.manual_order, Some(vec![key("b")]));
}

#[test]
fn missing_member_rebase_prunes_only_affected_intent() {
    let committed = snapshot();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("missing"), true);
    overlay.set_pinned(key("a"), true);
    overlay.set_manual_order(vec![key("missing"), key("b")]);

    overlay.rebase_missing_members(&committed);

    let patch = overlay.plan(&committed, &HashSet::new());
    assert_eq!(patch.pin_assignments.len(), 1);
    assert_eq!(
        patch.pin_assignments.first().map(|a| &a.key),
        Some(&key("a"))
    );
    assert_eq!(patch.manual_order, Some(vec![key("b")]));
}

#[test]
fn unknown_grouping_renders_as_state_and_pin_does_not_overwrite_it() {
    let mut committed = snapshot();
    committed.grouping = Grouping::from_raw("future-group").unwrap();
    let mut overlay = LayoutOverlay::default();
    overlay.set_pinned(key("a"), true);

    let view = overlay.view(&committed, &HashSet::new());
    let patch = overlay.plan(&committed, &HashSet::new());

    assert_eq!(view.grouping, WorkspaceGrouping::State);
    assert_eq!(patch.grouping, None);
}
