// Modified for Distill by Samuel Fajreldines, 2026.
//! Workspace store builders shared by the membership, row, and dispatch test modules.

use std::collections::HashSet;

use distill_dashboard_store::{
    Grouping, Member, MemberKey, MemberKind, MemberMetadata, MemberOrigin, NewMember, SessionId,
    WorkspaceSnapshot, WorkspaceStore,
};

use super::workspace_membership::WorkspaceMembership;

/// Empty store in a temp dir; the dir must outlive the store.
pub(crate) fn temp_store() -> (tempfile::TempDir, WorkspaceStore) {
    let temp = tempfile::tempdir().unwrap();
    let store = WorkspaceStore::open(&temp.path().join("workspace.db")).unwrap();
    (temp, store)
}

pub(crate) fn key(id: &str) -> MemberKey {
    MemberKey {
        session_id: SessionId::new(id).unwrap(),
        kind: MemberKind::Build,
    }
}

pub(crate) fn new_member(id: &str, title: &str) -> NewMember {
    NewMember {
        key: key(id),
        origin: MemberOrigin::Local,
        metadata: MemberMetadata {
            cwd: Some("/tmp".into()),
            title: Some(title.into()),
            model: None,
            last_turn_summary: None,
            is_worktree: false,
            last_change_unix_ms: 1,
        },
    }
}

/// The committed form of [`new_member`], unranked.
pub(crate) fn member(id: &str, title: &str) -> Member {
    let NewMember {
        key,
        origin,
        metadata,
    } = new_member(id, title);
    Member {
        session_id: key.session_id,
        kind: key.kind,
        origin,
        cwd: metadata.cwd,
        title: metadata.title,
        model: metadata.model,
        last_turn_summary: metadata.last_turn_summary,
        is_worktree: metadata.is_worktree,
        last_change_unix_ms: metadata.last_change_unix_ms,
        pin_rank: None,
        order_rank: None,
    }
}

pub(crate) fn snapshot(members: Vec<Member>) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        grouping: Grouping::State,
        members,
        data_version: 1,
    }
}

/// An activated controller over an empty store, opened with a snapshot that already lists `members`.
pub(crate) fn ready_membership(members: Vec<Member>) -> (tempfile::TempDir, WorkspaceMembership) {
    let (temp, store) = temp_store();
    let mut membership = WorkspaceMembership::default();
    membership.activate();
    membership.on_store_opened(store, snapshot(members), &HashSet::new());
    (temp, membership)
}
