use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

#[test]
fn schema_requires_cwd_only_for_build_members() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("constraint.db");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    assert!(matches!(
        init_schema(&mut conn).unwrap(),
        SchemaInit::Ready { created: true }
    ));

    let insert = |kind: &str, cwd: Option<&str>| {
        conn.execute(
            "INSERT INTO members (
                 session_id, kind, origin, cwd, last_change_unix_ms
             ) VALUES (?1, ?2, 'local', ?3, 0)",
            rusqlite::params![format!("{kind}-session"), kind, cwd],
        )
    };
    assert!(insert("build", None).is_err());
    assert_eq!(insert("build", Some("/work/project")).unwrap(), 1);
    assert_eq!(insert("conversation", None).unwrap(), 1);
    assert_eq!(insert("future-kind", None).unwrap(), 1);
}

// The newer-version arm is reachable through the public API only when a peer commits between the open's gate read and the init transaction
// This test pins it by calling init_schema directly
#[test]
fn init_writes_nothing_to_a_newer_schema_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("newer.db");
    let newer_version = USER_VERSION + 1;
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", newer_version)
            .unwrap();
    }

    let mut conn = rusqlite::Connection::open(&path).unwrap();
    // A commit by `conn` would bump this peer connection's data_version.
    let peer = rusqlite::Connection::open(&path).unwrap();
    let peer_baseline: i64 = peer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();

    let outcome = init_schema(&mut conn).unwrap();
    assert!(
        matches!(outcome, SchemaInit::Newer { user_version } if user_version == newer_version),
        "the in-transaction re-read must report the newer version"
    );

    let object_count: i64 = peer
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        object_count, 0,
        "no schema objects may be created in a newer file"
    );
    let stamped: u32 = peer
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stamped, newer_version, "the version must not be downgraded");
    let peer_after: i64 = peer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        peer_after, peer_baseline,
        "the newer arm must commit zero pages"
    );
}
