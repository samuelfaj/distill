use crate::{Member, MemberKey, MemberKind, MemberMetadata, MemberOrigin, NewMember, SessionId};

pub(super) fn member_key(id: &str, kind: MemberKind) -> MemberKey {
    MemberKey {
        session_id: SessionId::new(id).unwrap(),
        kind,
    }
}

pub(super) fn new_member(
    id: &str,
    kind: MemberKind,
    origin: MemberOrigin,
    last_change: i64,
) -> NewMember {
    NewMember {
        key: member_key(id, kind),
        origin,
        metadata: MemberMetadata {
            cwd: Some("/work/project".to_owned()),
            title: Some(format!("title-{id}")),
            model: Some("model-x".to_owned()),
            last_turn_summary: Some(format!("summary-{id}")),
            is_worktree: false,
            last_change_unix_ms: last_change,
        },
    }
}

pub(super) fn expected_member(
    source: &NewMember,
    pin_rank: Option<i64>,
    order_rank: Option<i64>,
) -> Member {
    Member {
        session_id: source.key.session_id.clone(),
        kind: source.key.kind.clone(),
        origin: source.origin.clone(),
        cwd: source.metadata.cwd.clone(),
        title: source.metadata.title.clone(),
        model: source.metadata.model.clone(),
        last_turn_summary: source.metadata.last_turn_summary.clone(),
        is_worktree: source.metadata.is_worktree,
        last_change_unix_ms: source.metadata.last_change_unix_ms,
        pin_rank,
        order_rank,
    }
}
