//! Tests run against the real store in temp directories only.
//!
//! Deliberately untested: the mapping from an exhausted busy budget to the typed busy error.
//! Forcing it needs a peer connection holding the write lock past the journal crate's multi-second busy budget.
//! That wait would dominate the suite's runtime, and the mapping is one shared helper on every operation's path.

use std::time::Duration;

// One contended write may consume SQLite's five-second busy timeout; this deadline detects a hung phase without racing that valid wait
const TEST_PHASE_TIMEOUT: Duration = Duration::from_secs(30);

fn recv_phase(receiver: &std::sync::mpsc::Receiver<()>, phase: &str) {
    receiver
        .recv_timeout(TEST_PHASE_TIMEOUT)
        .unwrap_or_else(|error| panic!("timed out waiting for {phase}: {error}"));
}

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::default_db_path;
use crate::test_support::{expected_member, member_key, new_member};
use crate::types::{LayoutGrouping, RANK_GAP};

fn temp_store() -> (TempDir, WorkspaceStore) {
    let tmp = TempDir::new().unwrap();
    let store = WorkspaceStore::open(&default_db_path(tmp.path())).unwrap();
    (tmp, store)
}

fn assign(id: &str, kind: MemberKind, rank: Option<i64>) -> RankAssignment {
    RankAssignment {
        key: member_key(id, kind),
        rank,
    }
}

fn pin(id: &str, kind: MemberKind, pinned: bool) -> PinAssignment {
    PinAssignment {
        key: member_key(id, kind),
        pinned,
    }
}

#[test]
fn same_id_rekey_detects_foreign_schema_upgrade() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut store = WorkspaceStore::open(&db).unwrap();
    let seeded = new_member("seeded", MemberKind::Build, MemberOrigin::Local, 100);
    store.insert_member(seeded.clone()).unwrap();

    let peer = rusqlite::Connection::open(store.path()).unwrap();
    peer.pragma_update(None, "user_version", USER_VERSION + 1)
        .unwrap();
    drop(peer);

    assert!(matches!(
        store.rekey(&seeded.key, seeded.key.session_id.clone()),
        Err(StoreError::NewerSchema {
            found,
            supported: USER_VERSION,
        }) if found == USER_VERSION + 1
    ));
    assert_eq!(
        store.schema_state(),
        SchemaState::NewerReadOnly {
            user_version: USER_VERSION + 1,
        }
    );
}

#[test]
fn open_creates_schema_and_roundtrips_snapshot() {
    let (_tmp, mut store) = temp_store();
    let empty = store.snapshot().unwrap();
    assert_eq!(empty.grouping, Grouping::State);
    assert_eq!(empty.members, Vec::new());

    // Inserted out of primary-key order to prove the snapshot ordering.
    let conversation = new_member("beta", MemberKind::Conversation, MemberOrigin::Remote, 200);
    let future = new_member(
        "gamma",
        MemberKind::from_raw("future-kind").unwrap(),
        MemberOrigin::from_raw("future-origin").unwrap(),
        300,
    );
    let build = new_member("alpha", MemberKind::Build, MemberOrigin::Local, 100);
    for member in [&conversation, &future, &build] {
        assert_eq!(
            store.insert_member(member.clone()).unwrap(),
            InsertOutcome::Inserted
        );
    }
    store
        .set_grouping(&Grouping::from_raw("future-grouping").unwrap())
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        snapshot.grouping,
        Grouping::from_raw("future-grouping").unwrap()
    );
    assert_eq!(
        snapshot.members,
        vec![
            expected_member(&build, None, None),
            expected_member(&conversation, None, None),
            expected_member(&future, None, None),
        ]
    );

    // Unknown enum text must reach disk byte-identical.
    let raw = rusqlite::Connection::open(store.path()).unwrap();
    let (kind, origin): (String, String) = raw
        .query_row(
            "SELECT kind, origin FROM members WHERE session_id = 'gamma'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), origin.as_str()),
        ("future-kind", "future-origin")
    );
    drop(raw);

    assert_eq!(
        store.remove_member(&conversation.key).unwrap(),
        RemoveOutcome::Removed
    );
    assert_eq!(
        store.remove_member(&conversation.key).unwrap(),
        RemoveOutcome::NotPresent
    );
    let after_remove = store.snapshot().unwrap();
    assert_eq!(
        after_remove.members,
        vec![
            expected_member(&build, None, None),
            expected_member(&future, None, None),
        ]
    );
}

#[test]
fn eviction_at_capacity_is_lru_transactional_and_deterministic() {
    let (_tmp, mut store) = temp_store();
    // Three members tie on last_change_unix_ms so both tie-break keys are exercised
    // The kind key orders "build" before "conversation" within m000; the session_id key orders m000 before m001
    for tied in [
        new_member("m000", MemberKind::Build, MemberOrigin::Local, 1000),
        new_member("m000", MemberKind::Conversation, MemberOrigin::Local, 1000),
        new_member("m001", MemberKind::Build, MemberOrigin::Local, 1000),
    ] {
        store.insert_member(tied).unwrap();
    }
    for i in 2..WORKSPACE_CAPACITY - 1 {
        store
            .insert_member(new_member(
                &format!("m{i:03}"),
                MemberKind::Build,
                MemberOrigin::Local,
                1000 + i as i64,
            ))
            .unwrap();
    }

    let newcomer = new_member("z-new", MemberKind::Build, MemberOrigin::Local, 9000);
    let outcome = store.insert_member(newcomer.clone()).unwrap();
    assert_eq!(
        outcome,
        InsertOutcome::InsertedEvicting(vec![member_key("m000", MemberKind::Build)]),
        "among tied m000/m001 rows the kind key picks m000's build row"
    );
    let second = new_member("z-new2", MemberKind::Build, MemberOrigin::Local, 9001);
    assert_eq!(
        store.insert_member(second.clone()).unwrap(),
        InsertOutcome::InsertedEvicting(vec![member_key("m000", MemberKind::Conversation)]),
        "among the remaining tied rows the session_id key picks m000 over m001"
    );
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.members.len(), WORKSPACE_CAPACITY);
    let ids: Vec<&str> = snapshot
        .members
        .iter()
        .map(|m| m.session_id.as_ref())
        .collect();
    assert!(!ids.contains(&"m000"), "both tied m000 rows are evicted");
    assert!(ids.contains(&"m001"), "the tie loser survives");
    assert!(
        snapshot
            .members
            .contains(&expected_member(&newcomer, None, None))
    );
    assert!(
        snapshot
            .members
            .contains(&expected_member(&second, None, None))
    );

    // Overfull file (past bug or lowered capacity) with fewer unpinned rows than the overage: every unpinned row is evicted, the insert proceeds
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let effective = WorkspaceStore::open(&db).unwrap().path().to_path_buf();
    {
        let raw = rusqlite::Connection::open(&effective).unwrap();
        for i in 0..300i64 {
            let pin_rank = if i < 2 { None } else { Some(i) };
            raw.execute(
                "INSERT INTO members (session_id, kind, origin, cwd, title, model,
                                      last_turn_summary, is_worktree, last_change_unix_ms,
                                      pin_rank, order_rank)
                 VALUES (?1, 'build', 'local', '/w', NULL, NULL, NULL, 0, ?2, ?3, NULL)",
                rusqlite::params![format!("s{i:03}"), 1000 + i, pin_rank],
            )
            .unwrap();
        }
    }
    let mut store = WorkspaceStore::open(&db).unwrap();
    let healed = new_member("z-heal", MemberKind::Build, MemberOrigin::Local, 99999);
    let outcome = store.insert_member(healed.clone()).unwrap();
    let InsertOutcome::InsertedEvicting(keys) = outcome else {
        panic!("expected partial-heal eviction, got {outcome:?}");
    };
    let mut evicted_ids: Vec<&str> = keys.iter().map(|k| k.session_id.as_ref()).collect();
    evicted_ids.sort_unstable();
    assert_eq!(evicted_ids, ["s000", "s001"], "both unpinned rows evicted");
    assert!(keys.iter().all(|k| k.kind == MemberKind::Build));
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.members.len(), 299, "300 - 2 unpinned + 1 insert");
    assert!(
        snapshot
            .members
            .iter()
            .any(|m| m.session_id.as_ref() == "z-heal")
    );
}

#[test]
fn pinned_members_are_never_evicted_and_all_pinned_refuses() {
    let (_tmp, mut store) = temp_store();
    for i in 0..WORKSPACE_CAPACITY {
        store
            .insert_member(new_member(
                &format!("m{i:03}"),
                MemberKind::Build,
                MemberOrigin::Local,
                1000 + i as i64,
            ))
            .unwrap();
    }
    assert!(matches!(
        store.apply_layout_patch(&LayoutPatch {
            pin_assignments: vec![pin("m000", MemberKind::Build, true)],
            manual_order: None,
            grouping: None,
        }),
        LayoutApplyOutcome::Committed(_)
    ));

    let newcomer = new_member("z-new", MemberKind::Build, MemberOrigin::Local, 9999);
    assert_eq!(
        store.insert_member(newcomer).unwrap(),
        InsertOutcome::InsertedEvicting(vec![member_key("m001", MemberKind::Build)]),
        "the pinned oldest member is exempt; the next-oldest goes"
    );

    let all_pinned: Vec<PinAssignment> = store
        .snapshot()
        .unwrap()
        .members
        .iter()
        .map(|m| PinAssignment {
            key: MemberKey {
                session_id: m.session_id.clone(),
                kind: m.kind.clone(),
            },
            pinned: true,
        })
        .collect();
    assert!(matches!(
        store.apply_layout_patch(&LayoutPatch {
            pin_assignments: all_pinned,
            manual_order: None,
            grouping: None,
        }),
        LayoutApplyOutcome::Committed(_)
    ));

    let before = store.snapshot().unwrap();
    let error = store
        .insert_member(new_member(
            "z-refused",
            MemberKind::Build,
            MemberOrigin::Local,
            10000,
        ))
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::AllPinned {
                capacity: WORKSPACE_CAPACITY
            }
        ),
        "{error:?}"
    );
    let after = store.snapshot().unwrap();
    assert_eq!(after.members, before.members, "the refusal rolls back");
    assert_eq!(after.grouping, before.grouping);
}

#[test]
fn rekey_moves_row_and_ranks_atomically_and_merges_on_conflict() {
    let (_tmp, mut store) = temp_store();

    let original = new_member("a-old", MemberKind::Build, MemberOrigin::Remote, 100);
    store.insert_member(original.clone()).unwrap();
    store
        .set_pin_rank(&[assign("a-old", MemberKind::Build, Some(5))])
        .unwrap();
    store
        .set_order_rank(&[assign("a-old", MemberKind::Build, Some(7))])
        .unwrap();
    assert_eq!(
        store
            .rekey(&original.key, SessionId::new("a-new").unwrap())
            .unwrap(),
        RekeyOutcome::Moved
    );
    let mut moved = expected_member(&original, Some(5), Some(7));
    moved.session_id = SessionId::new("a-new").unwrap();
    assert_eq!(store.snapshot().unwrap().members, vec![moved.clone()]);

    // Merge arm: the target keeps origin/metadata and its own order_rank; its NULL pin_rank is filled from the old row; the old row is gone
    let target = new_member("target", MemberKind::Conversation, MemberOrigin::Local, 200);
    store.insert_member(target.clone()).unwrap();
    store
        .set_order_rank(&[assign("target", MemberKind::Conversation, Some(40))])
        .unwrap();
    let old = new_member("old", MemberKind::Conversation, MemberOrigin::Remote, 300);
    store.insert_member(old.clone()).unwrap();
    store
        .set_pin_rank(&[assign("old", MemberKind::Conversation, Some(11))])
        .unwrap();
    store
        .set_order_rank(&[assign("old", MemberKind::Conversation, Some(22))])
        .unwrap();
    assert_eq!(
        store
            .rekey(&old.key, SessionId::new("target").unwrap())
            .unwrap(),
        RekeyOutcome::MergedIntoExisting
    );
    let merged = expected_member(&target, Some(11), Some(40));
    assert_eq!(
        store.snapshot().unwrap().members,
        vec![moved.clone(), merged.clone()]
    );

    // Self-rekey: reports NoChange and must not fall into the merge arm's delete
    assert_eq!(
        store
            .rekey(&target.key, SessionId::new("target").unwrap())
            .unwrap(),
        RekeyOutcome::NoChange
    );
    assert_eq!(store.snapshot().unwrap().members, vec![moved, merged]);

    // A missing old key errors with nothing changed, whatever the target: absent, self, or an existing member
    // The last must not slip into the merge arm and report a merge that never happened
    let ghost = member_key("ghost", MemberKind::Build);
    let before = store.snapshot().unwrap();
    assert!(matches!(
        store.rekey(&ghost, SessionId::new("ghost2").unwrap()),
        Err(StoreError::MemberNotFound { session_id, kind })
            if session_id == "ghost" && kind == "build"
    ));
    assert!(matches!(
        store.rekey(&ghost, SessionId::new("ghost").unwrap()),
        Err(StoreError::MemberNotFound { .. })
    ));
    assert!(matches!(
        store.rekey(&ghost, SessionId::new("a-new").unwrap()),
        Err(StoreError::MemberNotFound { .. })
    ));
    assert_eq!(store.snapshot().unwrap().members, before.members);
}

#[test]
fn non_build_members_round_trip_without_cwd() {
    let (_tmp, mut store) = temp_store();
    let mut conversation = new_member(
        "conversation",
        MemberKind::Conversation,
        MemberOrigin::Remote,
        1,
    );
    conversation.metadata.cwd = None;
    let mut future = new_member(
        "future",
        MemberKind::from_raw("future-kind").unwrap(),
        MemberOrigin::Remote,
        2,
    );
    future.metadata.cwd = None;

    store.insert_member(conversation.clone()).unwrap();
    store.insert_member(future.clone()).unwrap();
    assert_eq!(
        store.snapshot().unwrap().members,
        vec![
            expected_member(&conversation, None, None),
            expected_member(&future, None, None),
        ]
    );

    conversation.metadata.title = Some("updated conversation".to_owned());
    future.metadata.title = Some("updated future member".to_owned());
    store
        .update_member_metadata(&conversation.key, &conversation.metadata)
        .unwrap();
    store
        .update_member_metadata(&future.key, &future.metadata)
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().members,
        vec![
            expected_member(&conversation, None, None),
            expected_member(&future, None, None),
        ]
    );
}

#[test]
fn insert_and_update_enforce_build_cwd_without_writing() {
    let (_tmp, mut store) = temp_store();

    let mut missing_insert = new_member("missing", MemberKind::Build, MemberOrigin::Local, 1);
    missing_insert.metadata.cwd = None;
    assert!(matches!(
        store.insert_member(missing_insert),
        Err(StoreError::CwdRequired)
    ));

    let mut bad_insert = new_member("rel", MemberKind::Build, MemberOrigin::Local, 1);
    bad_insert.metadata.cwd = Some("not/absolute".to_owned());
    assert!(matches!(
        store.insert_member(bad_insert),
        Err(StoreError::CwdNotAbsolute)
    ));
    assert_eq!(store.snapshot().unwrap().members, Vec::new());

    let good = new_member("good", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(good.clone()).unwrap();

    let mut missing_update = good.metadata.clone();
    missing_update.cwd = None;
    assert!(matches!(
        store.update_member_metadata(&good.key, &missing_update),
        Err(StoreError::CwdRequired)
    ));

    let mut relative_update = good.metadata.clone();
    relative_update.cwd = Some("also/relative".to_owned());
    assert!(matches!(
        store.update_member_metadata(&good.key, &relative_update),
        Err(StoreError::CwdNotAbsolute)
    ));

    assert_eq!(
        store.snapshot().unwrap().members,
        vec![expected_member(&good, None, None)],
        "failed updates must leave the row untouched"
    );
}

#[test]
fn metadata_update_never_touches_origin_or_ranks() {
    let (_tmp, mut store) = temp_store();
    let member = new_member("keep", MemberKind::Build, MemberOrigin::Remote, 100);
    store.insert_member(member.clone()).unwrap();
    store
        .set_pin_rank(&[assign("keep", MemberKind::Build, Some(RANK_GAP))])
        .unwrap();
    store
        .set_order_rank(&[assign("keep", MemberKind::Build, Some(2 * RANK_GAP))])
        .unwrap();

    // First metadata path: re-insert with changed metadata and a different origin in the payload; the stored origin and ranks must survive
    let mut adopted = new_member("keep", MemberKind::Build, MemberOrigin::Local, 500);
    adopted.metadata.title = Some("adopted title".to_owned());
    assert_eq!(
        store.insert_member(adopted.clone()).unwrap(),
        InsertOutcome::UpdatedExisting
    );
    let mut expected = expected_member(&adopted, Some(RANK_GAP), Some(2 * RANK_GAP));
    expected.origin = MemberOrigin::Remote;
    assert_eq!(store.snapshot().unwrap().members, vec![expected.clone()]);

    // Second metadata path: the dedicated update call.
    let mut synced = adopted.metadata.clone();
    synced.model = Some("model-y".to_owned());
    synced.last_change_unix_ms = 900;
    store.update_member_metadata(&member.key, &synced).unwrap();
    expected.model = synced.model.clone();
    expected.last_change_unix_ms = 900;
    assert_eq!(store.snapshot().unwrap().members, vec![expected]);

    assert!(matches!(
        store.update_member_metadata(&member_key("ghost", MemberKind::Build), &synced),
        Err(StoreError::MemberNotFound { session_id, kind })
            if session_id == "ghost" && kind == "build"
    ));
}

#[test]
fn rank_batch_is_atomic_and_grouping_persists() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut store = WorkspaceStore::open(&db).unwrap();
    let members: Vec<NewMember> = (0..3)
        .map(|i| {
            let member = new_member(
                &format!("r{i}"),
                MemberKind::Build,
                MemberOrigin::Local,
                100 + i,
            );
            store.insert_member(member.clone()).unwrap();
            member
        })
        .collect();
    let [m0, m1, m2] = members.as_slice() else {
        panic!("expected three members: {members:?}");
    };

    // The renumber operation: every row's rank rewritten in one call
    store
        .set_order_rank(&[
            assign("r0", MemberKind::Build, Some(RANK_GAP)),
            assign("r1", MemberKind::Build, Some(2 * RANK_GAP)),
            assign("r2", MemberKind::Build, Some(3 * RANK_GAP)),
        ])
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().members,
        vec![
            expected_member(m0, None, Some(RANK_GAP)),
            expected_member(m1, None, Some(2 * RANK_GAP)),
            expected_member(m2, None, Some(3 * RANK_GAP)),
        ]
    );

    // A batch with one missing key must change nothing
    let before = store.snapshot().unwrap();
    assert!(matches!(
        store.set_order_rank(&[
            assign("r0", MemberKind::Build, Some(9999)),
            assign("ghost", MemberKind::Build, Some(1)),
        ]),
        Err(StoreError::MemberNotFound { session_id, .. }) if session_id == "ghost"
    ));
    assert_eq!(store.snapshot().unwrap().members, before.members);

    // None clears a rank.
    store
        .set_order_rank(&[assign("r0", MemberKind::Build, None)])
        .unwrap();
    let expected = expected_member(m0, None, None);
    assert_eq!(store.snapshot().unwrap().members.first(), Some(&expected));

    store.set_grouping(&Grouping::Directory).unwrap();
    assert_eq!(store.snapshot().unwrap().grouping, Grouping::Directory);
    drop(store);
    let store = WorkspaceStore::open(&db).unwrap();
    assert_eq!(
        store.snapshot().unwrap().grouping,
        Grouping::Directory,
        "grouping must survive a reopen"
    );
}

#[test]
fn layout_patch_commits_one_snapshot_and_rewrites_complete_order_subset() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    let b = new_member("b", MemberKind::Build, MemberOrigin::Local, 2);
    let c = new_member("c", MemberKind::Build, MemberOrigin::Local, 3);
    for member in [&a, &b, &c] {
        store.insert_member(member.clone()).unwrap();
    }
    store
        .set_order_rank(&[
            assign("a", MemberKind::Build, Some(9)),
            assign("c", MemberKind::Build, Some(10)),
        ])
        .unwrap();

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![pin("b", MemberKind::Build, true)],
        manual_order: Some(vec![b.key.clone(), a.key.clone()]),
        grouping: Some(LayoutGrouping::Directory),
    });
    let LayoutApplyOutcome::Committed(snapshot) = outcome else {
        panic!("layout patch should commit");
    };

    assert_eq!(snapshot.grouping, Grouping::Directory);
    assert_eq!(
        snapshot.members,
        vec![
            expected_member(&a, None, Some(2 * RANK_GAP)),
            expected_member(&b, Some(RANK_GAP), Some(RANK_GAP)),
            expected_member(&c, None, None),
        ]
    );
    assert_eq!(store.snapshot().unwrap(), snapshot);
}

#[test]
fn missing_layout_member_rolls_back_every_field_and_returns_reliable_snapshot() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(a).unwrap();
    let before = store.snapshot().unwrap();

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![pin("a", MemberKind::Build, true)],
        manual_order: Some(vec![member_key("missing", MemberKind::Build)]),
        grouping: Some(LayoutGrouping::Directory),
    });
    let LayoutApplyOutcome::Rejected { error, snapshot } = outcome else {
        panic!("missing member should reject with rollback snapshot");
    };

    assert!(matches!(
        error,
        StoreError::MemberNotFound { session_id, .. } if session_id == "missing"
    ));
    assert_eq!(snapshot, before);
    assert_eq!(store.snapshot().unwrap(), before);
}

#[test]
fn layout_commit_failure_returns_no_snapshot_and_leaves_committed_state_unchanged() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(a.clone()).unwrap();
    store
        .conn
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE layout_test_parent (id INTEGER PRIMARY KEY);
             CREATE TABLE layout_test_child (
                 parent_id INTEGER REFERENCES layout_test_parent(id)
                     DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER layout_test_commit_failure
             AFTER UPDATE OF pin_rank ON members
             BEGIN
                 INSERT INTO layout_test_child(parent_id) VALUES (999);
             END;",
        )
        .unwrap();

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![pin("a", MemberKind::Build, true)],
        manual_order: None,
        grouping: None,
    });

    assert!(matches!(
        outcome,
        LayoutApplyOutcome::Failed {
            error: StoreError::Sqlite(_)
        }
    ));
    assert_eq!(
        store.snapshot().unwrap().members,
        vec![expected_member(&a, None, None)]
    );
}

#[test]
fn layout_patch_reports_newer_schema_as_typed_failed_outcome() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut store = WorkspaceStore::open(&db).unwrap();
    let peer = rusqlite::Connection::open(store.path()).unwrap();
    peer.pragma_update(None, "user_version", USER_VERSION + 1)
        .unwrap();
    drop(peer);

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![],
        manual_order: None,
        grouping: Some(LayoutGrouping::Directory),
    });

    assert!(matches!(
        outcome,
        LayoutApplyOutcome::Failed {
            error: StoreError::NewerSchema {
                found,
                supported: USER_VERSION
            }
        } if found == USER_VERSION + 1
    ));
    assert_eq!(
        store.schema_state(),
        SchemaState::NewerReadOnly {
            user_version: USER_VERSION + 1
        }
    );
}

#[test]
fn layout_patch_public_boundary_rejects_invalid_semantics() {
    let (_tmp, mut store) = temp_store();
    let invalid = [
        LayoutPatch {
            pin_assignments: vec![pin("chat", MemberKind::Conversation, true)],
            manual_order: None,
            grouping: None,
        },
        LayoutPatch {
            pin_assignments: vec![],
            manual_order: Some(vec![member_key("chat", MemberKind::Conversation)]),
            grouping: None,
        },
    ];

    for patch in invalid {
        assert!(matches!(
            store.apply_layout_patch(&patch),
            LayoutApplyOutcome::Failed {
                error: StoreError::InvalidLayoutPatch { .. }
            }
        ));
    }
}

#[test]
fn layout_patch_deduplicates_public_assignments_before_writing() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(a.clone()).unwrap();

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![
            pin("a", MemberKind::Build, false),
            pin("a", MemberKind::Build, true),
        ],
        manual_order: Some(vec![a.key.clone(), a.key.clone()]),
        grouping: None,
    });
    let LayoutApplyOutcome::Committed(snapshot) = outcome else {
        panic!("deduplicated patch should commit");
    };
    assert_eq!(
        snapshot.members,
        vec![expected_member(&a, Some(RANK_GAP), Some(RANK_GAP))]
    );
}

#[test]
fn pin_only_layout_patch_preserves_unknown_grouping() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(a).unwrap();
    let future = Grouping::from_raw("future-grouping").unwrap();
    store.set_grouping(&future).unwrap();

    let outcome = store.apply_layout_patch(&LayoutPatch {
        pin_assignments: vec![pin("a", MemberKind::Build, true)],
        manual_order: None,
        grouping: None,
    });
    let LayoutApplyOutcome::Committed(snapshot) = outcome else {
        panic!("pin-only patch should commit");
    };
    assert_eq!(snapshot.grouping, future);
}

#[test]
fn remove_and_reinsert_resets_member_layout() {
    let (_tmp, mut store) = temp_store();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    store.insert_member(a.clone()).unwrap();
    assert!(matches!(
        store.apply_layout_patch(&LayoutPatch {
            pin_assignments: vec![pin("a", MemberKind::Build, true)],
            manual_order: Some(vec![a.key.clone()]),
            grouping: None,
        }),
        LayoutApplyOutcome::Committed(_)
    ));

    store.remove_member(&a.key).unwrap();
    store.insert_member(a.clone()).unwrap();

    assert_eq!(
        store.snapshot().unwrap().members,
        vec![expected_member(&a, None, None)]
    );
}

#[test]
fn peer_layout_patch_advances_data_version_and_converges_snapshot() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut local = WorkspaceStore::open(&db).unwrap();
    let a = new_member("a", MemberKind::Build, MemberOrigin::Local, 1);
    local.insert_member(a.clone()).unwrap();
    let baseline = local.snapshot().unwrap().data_version;
    let mut peer = WorkspaceStore::open(&db).unwrap();

    assert!(matches!(
        peer.apply_layout_patch(&LayoutPatch {
            pin_assignments: vec![pin("a", MemberKind::Build, true)],
            manual_order: Some(vec![a.key.clone()]),
            grouping: Some(LayoutGrouping::Directory),
        }),
        LayoutApplyOutcome::Committed(_)
    ));

    assert_ne!(local.data_version().unwrap(), baseline);
    let refreshed = local.snapshot().unwrap();
    assert_eq!(refreshed.grouping, Grouping::Directory);
    assert_eq!(
        refreshed.members,
        vec![expected_member(&a, Some(RANK_GAP), Some(RANK_GAP))]
    );
}

#[test]
fn two_connections_interleave_without_loss_and_data_version_fires_foreign_only() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());

    let mut a = WorkspaceStore::open(&db).unwrap();
    // The data_version inside the snapshot is the poll baseline: the two must agree when nothing happened in between
    let base_a = a.snapshot().unwrap().data_version;
    let m1 = new_member("m1", MemberKind::Build, MemberOrigin::Local, 100);
    a.insert_member(m1.clone()).unwrap();
    assert_eq!(
        a.data_version().unwrap(),
        base_a,
        "a connection's own commits must be invisible to its poll"
    );

    let (a_events, a_events_rx) = std::sync::mpsc::channel::<()>();
    let (b_events, b_events_rx) = std::sync::mpsc::channel::<()>();
    let (b_done, b_done_rx) = std::sync::mpsc::channel::<()>();
    let db_for_b = db.clone();
    let b_thread = std::thread::spawn(move || -> Result<()> {
        let mut b = WorkspaceStore::open(&db_for_b).unwrap();
        let first = b.snapshot().unwrap();
        assert_eq!(
            first
                .members
                .iter()
                .map(|m| m.session_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["m1"],
            "the second connection sees the first one's insert"
        );
        let base_b = first.data_version;
        b.insert_member(new_member(
            "m2",
            MemberKind::Conversation,
            MemberOrigin::Remote,
            200,
        ))
        .unwrap();
        assert_eq!(b.data_version().unwrap(), base_b, "own commit invisible");
        b_events.send(()).unwrap();

        recv_phase(&a_events_rx, "connection A phase");
        assert_ne!(
            b.data_version().unwrap(),
            base_b,
            "the peer's removal must fire the poll"
        );
        let after_removal = b.snapshot().unwrap();
        assert_eq!(
            after_removal
                .members
                .iter()
                .map(|m| m.session_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["m2"]
        );
        let base_b2 = after_removal.data_version;
        b_events.send(()).unwrap();

        recv_phase(&a_events_rx, "connection A phase");
        assert_eq!(
            b.data_version().unwrap(),
            base_b2,
            "a healthy reopen commits zero pages and must not look foreign"
        );
        // The peer waits for this ack before its first commit, so the poll above cannot observe a legitimate foreign change instead of the reopen
        b_events.send(()).unwrap();
        for i in 0..10i64 {
            b.insert_member(new_member(
                &format!("b{i:02}"),
                MemberKind::Build,
                MemberOrigin::Local,
                300 + i,
            ))
            .unwrap();
        }
        b_done.send(()).unwrap();
        Ok(())
    });

    recv_phase(&b_events_rx, "connection B phase");
    assert_ne!(
        a.data_version().unwrap(),
        base_a,
        "the peer's insert must fire the poll"
    );
    let both = a.snapshot().unwrap();
    assert_eq!(
        both.members
            .iter()
            .map(|m| m.session_id.as_ref())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
    assert_eq!(a.remove_member(&m1.key).unwrap(), RemoveOutcome::Removed);
    let base_a2 = a.data_version().unwrap();
    a_events.send(()).unwrap();

    recv_phase(&b_events_rx, "connection B phase");
    drop(WorkspaceStore::open(&db).unwrap());
    assert_eq!(
        a.data_version().unwrap(),
        base_a2,
        "a healthy reopen commits zero pages and must not look foreign"
    );
    a_events.send(()).unwrap();
    recv_phase(&b_events_rx, "connection B phase");

    // Unsynchronized concurrent inserts from both connections
    // When the scheduler produces contention the busy timeout absorbs it; no write may be lost either way
    for i in 0..10i64 {
        a.insert_member(new_member(
            &format!("a{i:02}"),
            MemberKind::Build,
            MemberOrigin::Local,
            400 + i,
        ))
        .unwrap();
    }
    recv_phase(&b_done_rx, "connection B completion");
    b_thread.join().unwrap().unwrap();
    let final_ids: Vec<String> = a
        .snapshot()
        .unwrap()
        .members
        .iter()
        .map(|m| m.session_id.as_ref().to_owned())
        .collect();
    let mut expected: Vec<String> = (0..10).map(|i| format!("a{i:02}")).collect();
    expected.extend((0..10).map(|i| format!("b{i:02}")));
    expected.push("m2".to_owned());
    assert_eq!(final_ids, expected, "interleaved writes must all survive");
}
