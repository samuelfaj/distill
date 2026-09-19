// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::error::StoreError;
use crate::test_support::{expected_member, new_member};
use crate::types::{
    Grouping, MemberKey, MemberKind, MemberMetadata, MemberOrigin, NewMember, RankAssignment,
    SchemaState, SessionId,
};
use crate::{Result, USER_VERSION, default_db_path};

fn assert_all_writes_reject_newer(
    store: &mut WorkspaceStore,
    insert: NewMember,
    existing_key: &MemberKey,
    metadata: &MemberMetadata,
    found: u32,
) {
    let gate = |result: Result<()>| {
        assert!(matches!(
            result,
            Err(StoreError::NewerSchema {
                found: actual,
                supported: USER_VERSION,
            }) if actual == found
        ));
    };
    gate(store.insert_member(insert).map(drop));
    gate(store.update_member_metadata(existing_key, metadata));
    gate(store.remove_member(existing_key).map(drop));
    gate(store.set_pin_rank(&[RankAssignment {
        key: existing_key.clone(),
        rank: Some(1),
    }]));
    gate(store.set_order_rank(&[RankAssignment {
        key: existing_key.clone(),
        rank: Some(1),
    }]));
    gate(store.set_grouping(&Grouping::Directory));
    gate(
        store
            .rekey(existing_key, SessionId::new("other").unwrap())
            .map(drop),
    );
    gate(
        store
            .rekey(existing_key, existing_key.session_id.clone())
            .map(drop),
    );
}

#[derive(Debug, PartialEq)]
struct LogicalDump {
    schema: Vec<(String, Option<String>)>,
    members: Vec<String>,
    meta: String,
    user_version: i64,
}

fn logical_dump(path: &Path) -> LogicalDump {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master ORDER BY name")
        .unwrap();
    let schema = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    drop(stmt);
    let mut stmt = conn
        .prepare(
            "SELECT quote(session_id) || '|' || quote(kind) || '|' || quote(origin) || '|' ||
                    quote(cwd) || '|' || quote(title) || '|' || quote(model) || '|' ||
                    quote(last_turn_summary) || '|' || quote(is_worktree) || '|' ||
                    quote(last_change_unix_ms) || '|' || quote(pin_rank) || '|' ||
                    quote(order_rank)
             FROM members ORDER BY session_id, kind",
        )
        .unwrap();
    let members = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    drop(stmt);
    let meta: String = conn
        .query_row(
            "SELECT quote(id) || '|' || quote(grouping) FROM meta",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    LogicalDump {
        schema,
        members,
        meta,
        user_version,
    }
}

#[test]
fn newer_user_version_opens_read_only() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let seeded = new_member("seeded", MemberKind::Build, MemberOrigin::Local, 100);
    let effective = {
        let mut store = WorkspaceStore::open(&db).unwrap();
        assert_eq!(store.schema_state(), SchemaState::Current);
        store.insert_member(seeded.clone()).unwrap();
        store.path().to_path_buf()
    };

    let raw = rusqlite::Connection::open(&effective).unwrap();
    raw.pragma_update(None, "user_version", 2).unwrap();
    drop(raw);
    let before = logical_dump(&effective);

    let mut store = WorkspaceStore::open(&db).unwrap();
    assert_eq!(
        store.schema_state(),
        SchemaState::NewerReadOnly { user_version: 2 }
    );
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.grouping, Grouping::State);
    assert_eq!(snapshot.members, vec![expected_member(&seeded, None, None)]);
    assert_eq!(store.data_version().unwrap(), snapshot.data_version);

    let refused = new_member("refused", MemberKind::Build, MemberOrigin::Local, 1);
    let key = refused.key.clone();
    assert_all_writes_reject_newer(&mut store, refused.clone(), &key, &refused.metadata, 2);
    drop(store);

    assert_eq!(logical_dump(&effective), before);
}

#[test]
fn live_handle_becomes_read_only_after_foreign_schema_upgrade() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut store = WorkspaceStore::open(&db).unwrap();
    let seeded = new_member("seeded", MemberKind::Build, MemberOrigin::Local, 100);
    store.insert_member(seeded.clone()).unwrap();

    let peer = rusqlite::Connection::open(store.path()).unwrap();
    peer.pragma_update(None, "user_version", USER_VERSION + 1)
        .unwrap();
    drop(peer);
    let before = logical_dump(store.path());

    let mut refused = new_member("refused", MemberKind::Build, MemberOrigin::Local, 1);
    refused.metadata.cwd = None;
    let invalid_metadata = refused.metadata.clone();
    let key = seeded.key.clone();
    assert_all_writes_reject_newer(
        &mut store,
        refused,
        &key,
        &invalid_metadata,
        USER_VERSION + 1,
    );
    assert_eq!(
        store.schema_state(),
        SchemaState::NewerReadOnly {
            user_version: USER_VERSION + 1,
        }
    );
    assert!(
        store
            .conn
            .pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))
            .unwrap()
    );
    assert_eq!(logical_dump(store.path()), before);
}

#[test]
fn negative_user_version_is_unusable() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let effective = WorkspaceStore::open(&db).unwrap().path().to_path_buf();
    let raw = rusqlite::Connection::open(&effective).unwrap();
    raw.pragma_update(None, "user_version", -1).unwrap();
    drop(raw);

    assert!(matches!(
        WorkspaceStore::open(&db),
        Err(StoreError::Unusable { .. })
    ));
}

#[test]
fn corrupt_file_errors_without_destroying_it() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let dir = db.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let effective = distill_sqlite_journal::JournalMode::for_db_path(&db).effective_db_path(&db);
    let garbage = vec![b'x'; 1024];
    std::fs::write(&effective, &garbage).unwrap();

    let error = WorkspaceStore::open(&db).unwrap_err();
    assert!(matches!(error, StoreError::Unusable { .. }), "{error:?}");

    assert_eq!(std::fs::read(&effective).unwrap(), garbage);
    let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        entries,
        vec![effective],
        "nothing may be quarantined or recreated beside the unusable file"
    );
}

#[cfg(unix)]
#[test]
fn open_refuses_symlinked_db_path() {
    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let effective = distill_sqlite_journal::JournalMode::for_db_path(&db).effective_db_path(&db);
    let target = tmp.path().join("target.db");
    std::os::unix::fs::symlink(&target, &effective).unwrap();

    // Dangling link: nothing may be created through it.
    let error = WorkspaceStore::open(&db).unwrap_err();
    assert!(matches!(error, StoreError::Io(_)), "{error:?}");
    assert!(!target.exists(), "nothing may be created through the link");
    assert!(
        std::fs::symlink_metadata(&effective)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the planted link is refused, not removed"
    );

    // Link to an existing file: refused and the target left untouched.
    std::fs::write(&target, b"payload").unwrap();
    let error = WorkspaceStore::open(&db).unwrap_err();
    assert!(matches!(error, StoreError::Io(_)), "{error:?}");
    assert_eq!(std::fs::read(&target).unwrap(), b"payload");
}

#[cfg(unix)]
#[test]
fn db_file_and_journal_siblings_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    let tmp = TempDir::new().unwrap();
    let db = default_db_path(tmp.path());
    let mut store = WorkspaceStore::open(&db).unwrap();
    store
        .insert_member(new_member(
            "perm-check",
            MemberKind::Build,
            MemberOrigin::Local,
            1,
        ))
        .unwrap();
    let effective = store.path().to_path_buf();
    let dir = effective.parent().unwrap().to_path_buf();
    assert_eq!(mode_of(&dir), 0o700);

    // The live connection has created the journal siblings, which must inherit the database's owner-only mode
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(
        files.len() >= 2,
        "expected journal siblings beside the db, found {files:?}"
    );
    for file in &files {
        assert_eq!(mode_of(file), 0o600, "{}", file.display());
    }
    drop(store);

    std::fs::set_permissions(&effective, std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = WorkspaceStore::open(&db).unwrap();
    assert_eq!(
        mode_of(&effective),
        0o600,
        "reopen must tighten a loosened file"
    );
    drop(store);
}
