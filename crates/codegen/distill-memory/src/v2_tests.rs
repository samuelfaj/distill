use std::sync::Arc;

use super::*;
use tempfile::TempDir;

#[test]
fn initialization_creates_complete_scope_and_is_idempotent() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");

    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let expected = [
        "topics",
        "observations",
        "observations/_inbox",
        "archive",
        "MEMORY.md",
        "memory_state.sqlite",
        "index.sqlite",
    ];
    for relative in expected {
        assert!(scope.join(relative).exists(), "missing {relative}");
    }

    std::fs::write(scope.join("topics/new.md"), "# New\n\nFresh content.").unwrap();
    std::fs::write(scope.join("MEMORY.md"), "stale sentinel").unwrap();
    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let refreshed_manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(refreshed_manifest.contains("topics/new.md"));
    assert!(!refreshed_manifest.contains("stale sentinel"));

    let state = JournalMode::for_db_path(&scope.join("memory_state.sqlite"))
        .open_readonly(&scope.join("memory_state.sqlite"))
        .unwrap();
    assert_eq!(
        state
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        STATE_SCHEMA_VERSION
    );
    let index = JournalMode::for_db_path(&scope.join("index.sqlite"))
        .open_readonly(&scope.join("index.sqlite"))
        .unwrap();
    assert_eq!(
        index
            .query_row(
                "SELECT value FROM meta WHERE key = 'retrieval_mode'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "fts_only"
    );
}

#[test]
fn concurrent_initialization_is_safe() {
    let temp = TempDir::new().unwrap();
    let root = Arc::new(temp.path().to_path_buf());
    let scope = Arc::new(root.join("scope"));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let scope = scope.clone();
            std::thread::spawn(move || {
                ensure_scope_initialized(&root, &scope, V2MemoryScope::Global).unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    let manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(manifest.starts_with("# Global memory index"));
    assert_eq!(
        std::fs::read_dir(scope.join("observations/_inbox"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn manifest_is_deterministic_and_includes_topics_before_observations() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path();
    std::fs::create_dir_all(scope.join("topics")).unwrap();
    std::fs::create_dir_all(scope.join("observations/_inbox")).unwrap();
    std::fs::write(
        scope.join("topics/zeta.md"),
        "# Zeta\n\nStable project detail.",
    )
    .unwrap();
    std::fs::write(scope.join("topics/alpha.md"), "# Alpha\n\nFirst topic.").unwrap();
    // A manual note whose name sorts after every capture name, but which is
    // older than the capture note: recency must come from modification time.
    let remember = scope.join("observations/_inbox/remember-zzz.md");
    std::fs::write(&remember, "# Old manual note\n\nPending evidence.").unwrap();
    let capture = scope.join("observations/_inbox/01a0-session__t000002-000002__n000.md");
    std::fs::write(&capture, "# Newer capture\n\nLater evidence.").unwrap();
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    std::fs::File::open(&remember)
        .unwrap()
        .set_modified(base)
        .unwrap();
    std::fs::File::open(&capture)
        .unwrap()
        .set_modified(base + std::time::Duration::from_secs(60))
        .unwrap();
    std::fs::write(scope.join("topics/ignored.txt"), "# Not memory").unwrap();
    std::fs::create_dir_all(scope.join("topics/nested")).unwrap();
    std::fs::write(scope.join("topics/nested/ignored.md"), "# Nested").unwrap();

    let first = render_scope_manifest(scope, V2MemoryScope::Workspace, V2ManifestBudget::default())
        .unwrap();
    let second =
        render_scope_manifest(scope, V2MemoryScope::Workspace, V2ManifestBudget::default())
            .unwrap();

    assert_eq!(first, second);
    assert_eq!(first.discovered_entries, 4);
    assert_eq!(first.included_entries, 4);
    let alpha = first.content.find("(`topics/alpha.md`)").unwrap();
    let zeta = first.content.find("(`topics/zeta.md`)").unwrap();
    let newer = first
        .content
        .find("(`observations/_inbox/01a0-session__t000002-000002__n000.md`)")
        .unwrap();
    let older = first
        .content
        .find("(`observations/_inbox/remember-zzz.md`)")
        .unwrap();
    assert!(alpha < zeta && zeta < newer && newer < older);
    assert!(first.content.contains("**Zeta** — Stable project detail."));
    assert!(
        first
            .content
            .contains("**Old manual note** (`observations/_inbox/remember-zzz.md`)")
    );
    assert!(!first.content.contains("Pending evidence."));
    assert!(
        !first
            .content
            .contains(&scope.join("topics").display().to_string())
    );
    assert!(
        first
            .content
            .contains(&format!("> Paths are relative to `{}`.", scope.display()))
    );
    assert!(!first.content.contains("ignored"));
}

#[test]
fn manifest_budget_is_a_hard_utf8_safe_cap() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("topics")).unwrap();
    for index in 0..20 {
        std::fs::write(
            temp.path().join("topics").join(format!("{index:02}.md")),
            format!("# Topic {index}\n\n{}", "🦀".repeat(100)),
        )
        .unwrap();
    }

    let manifest = render_scope_manifest(
        temp.path(),
        V2MemoryScope::Workspace,
        V2ManifestBudget {
            max_bytes: 257,
            max_entries: 3,
            max_description_bytes: 97,
        },
    )
    .unwrap();

    assert!(manifest.content.len() <= 257);
    assert!(std::str::from_utf8(manifest.content.as_bytes()).is_ok());
    assert_eq!(manifest.discovered_entries, 20);
    assert!(manifest.included_entries < 3);
    assert!(manifest.is_truncated);

    let entry_limited = render_scope_manifest(
        temp.path(),
        V2MemoryScope::Workspace,
        V2ManifestBudget {
            max_bytes: MAX_MANIFEST_BYTES,
            max_entries: 3,
            max_description_bytes: 8,
        },
    )
    .unwrap();
    assert_eq!(entry_limited.included_entries, 3);
    assert!(entry_limited.is_truncated);
}

#[test]
fn oversized_entry_does_not_starve_later_manifest_entries() {
    let temp = TempDir::new().unwrap();
    let topics = temp.path().join("topics");
    std::fs::create_dir_all(&topics).unwrap();
    std::fs::write(
        topics.join("00-oversized.md"),
        format!("# {}\n\nlarge", "x".repeat(2_000)),
    )
    .unwrap();
    std::fs::write(topics.join("01-small.md"), "# Small\n\nFits.").unwrap();

    let manifest = render_scope_manifest(
        temp.path(),
        V2MemoryScope::Workspace,
        V2ManifestBudget {
            // Room for the header plus one small entry, but not the 2 KB
            // oversized title.
            max_bytes: 400,
            max_entries: 2,
            max_description_bytes: 32,
        },
    )
    .unwrap();

    assert!(manifest.content.contains("(`topics/01-small.md`)"));
    assert!(!manifest.content.contains("topics/00-oversized.md"));
    assert_eq!(manifest.included_entries, 1);
    assert!(manifest.is_truncated);
}

#[test]
fn zero_budget_produces_empty_bounded_manifest() {
    let temp = TempDir::new().unwrap();
    let manifest = render_scope_manifest(
        temp.path(),
        V2MemoryScope::Global,
        V2ManifestBudget {
            max_bytes: 0,
            max_entries: 0,
            max_description_bytes: 0,
        },
    )
    .unwrap();
    assert!(manifest.content.is_empty());
    assert_eq!(manifest.discovered_entries, 0);
}

#[test]
fn exclusion_ledger_normalizes_windows_separators() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let _capture = crate::V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    std::fs::write(scope.join("topics/forgotten.md"), "# Forgotten").unwrap();
    std::fs::write(
        scope.join("observations/_inbox/hidden.md"),
        "# Hidden observation",
    )
    .unwrap();
    let state_path = scope.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('windows-path', 'topic', ?1, ?2, 1, 'privacy')",
            params!["topics\\forgotten.md", "0".repeat(64)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_hidden_observations(relative_path) VALUES (?1)",
            params!["observations\\_inbox\\hidden.md"],
        )
        .unwrap();
    drop(connection);

    assert!(is_durably_excluded(&scope, Path::new("topics/forgotten.md")).unwrap());
    assert!(is_durably_excluded(&scope, Path::new("observations/_inbox/hidden.md")).unwrap());
    let manifest = render_scope_manifest(
        &scope,
        V2MemoryScope::Workspace,
        V2ManifestBudget::default(),
    )
    .unwrap();
    assert!(!manifest.content.contains("forgotten.md"));
    assert!(!manifest.content.contains("hidden.md"));
}

#[test]
fn manifest_accepts_maintenance_schema_without_capture_tables() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let state_path = scope.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    crate::v2_maintenance::migrate_v2_hardening(&connection).unwrap();
    drop(connection);
    std::fs::write(scope.join("topics/healthy.md"), "# Healthy").unwrap();

    let manifest = render_scope_manifest(
        &scope,
        V2MemoryScope::Workspace,
        V2ManifestBudget::default(),
    )
    .unwrap();
    assert!(manifest.content.contains("topics/healthy.md"));
}

#[cfg(unix)]
#[test]
fn manifest_ignores_symlinks_even_when_the_target_is_markdown() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let outside = temp.path().join("outside.md");
    std::fs::write(&outside, "# Secret\n\nMust not be indexed.").unwrap();
    let topics = temp.path().join("scope/topics");
    std::fs::create_dir_all(&topics).unwrap();
    symlink(&outside, topics.join("escape.md")).unwrap();

    let manifest = render_scope_manifest(
        &temp.path().join("scope"),
        V2MemoryScope::Workspace,
        V2ManifestBudget::default(),
    )
    .unwrap();
    assert_eq!(manifest.discovered_entries, 0);
    assert!(!manifest.content.contains("Secret"));
}

#[cfg(unix)]
#[test]
fn manifest_rejects_symlinked_source_directory() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("secret.md"),
        "# Secret\n\nMust not be indexed.",
    )
    .unwrap();
    let scope = temp.path().join("scope");
    std::fs::create_dir_all(&scope).unwrap();
    symlink(&outside, scope.join("topics")).unwrap();

    let error = render_scope_manifest(
        &scope,
        V2MemoryScope::Workspace,
        V2ManifestBudget::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::Io {
            source,
            ..
        } if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
}

#[cfg(unix)]
#[test]
fn initialization_rejects_symlinked_scope_and_database_without_touching_targets() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let outside_scope = temp.path().join("outside-scope");
    std::fs::create_dir_all(&outside_scope).unwrap();
    let linked_scope = temp.path().join("linked-scope");
    symlink(&outside_scope, &linked_scope).unwrap();
    let error =
        ensure_scope_initialized(&linked_scope, &linked_scope, V2MemoryScope::Global).unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::Io {
            source,
            ..
        } if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(std::fs::read_dir(&outside_scope).unwrap().count(), 0);
    let error = render_scope_manifest(
        &linked_scope,
        V2MemoryScope::Global,
        V2ManifestBudget::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::Io {
            source,
            ..
        } if source.kind() == std::io::ErrorKind::PermissionDenied
    ));

    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let outside_workspaces = temp.path().join("outside-workspaces");
    std::fs::create_dir_all(&outside_workspaces).unwrap();
    symlink(&outside_workspaces, root.join("workspaces")).unwrap();
    let error = ensure_scope_initialized(
        &root,
        &root.join("workspaces/project"),
        V2MemoryScope::Workspace,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::Io {
            source,
            ..
        } if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(std::fs::read_dir(&outside_workspaces).unwrap().count(), 0);

    let scope = temp.path().join("scope");
    std::fs::create_dir_all(&scope).unwrap();
    let outside_db = temp.path().join("outside.sqlite");
    std::fs::write(&outside_db, "sentinel").unwrap();
    symlink(&outside_db, scope.join("memory_state.sqlite")).unwrap();
    let error =
        ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::Io {
            source,
            ..
        } if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(std::fs::read_to_string(outside_db).unwrap(), "sentinel");
}

#[test]
fn initialization_refuses_network_filesystems_before_creating_state() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    let error = ensure_scope_initialized_with_journal_mode(
        temp.path(),
        &scope,
        V2MemoryScope::Workspace,
        Some(JournalMode::Truncate),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::UnsupportedNetworkFilesystem { ref path } if path == &scope
    ));
    let state_files = std::fs::read_dir(&scope)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("memory_state") || name == "MEMORY.md")
        .collect::<Vec<_>>();
    assert!(state_files.is_empty(), "{state_files:?}");
}

#[test]
fn exclusion_ledger_checks_stat_the_effective_database_path() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let _capture = crate::V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    let bare = scope.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&bare).open(&bare).unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('per-host', 'topic', 'topics/forgotten.md', ?1, 1, 'privacy')",
            params!["0".repeat(64)],
        )
        .unwrap();
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    drop(connection);

    let per_host = JournalMode::Truncate.effective_db_path(&bare);
    assert_ne!(per_host, bare);
    std::fs::rename(&bare, &per_host).unwrap();
    assert!(!bare.exists());

    assert!(
        is_durably_excluded_with_journal_mode(
            &scope,
            Path::new("topics/forgotten.md"),
            Some(JournalMode::Truncate),
        )
        .unwrap()
    );
    assert!(
        excluded_manifest_paths_with_journal_mode(&scope, Some(JournalMode::Truncate))
            .unwrap()
            .contains("topics/forgotten.md")
    );
    assert!(
        !is_durably_excluded_with_journal_mode(
            &scope,
            Path::new("topics/forgotten.md"),
            Some(JournalMode::Wal),
        )
        .unwrap()
    );
}

#[test]
fn regeneration_atomically_replaces_existing_manifest() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Global).unwrap();
    std::fs::write(scope.join("topics/fact.md"), b"# Fact\n\n\xff durable").unwrap();

    let manifest =
        regenerate_scope_manifest(&scope, V2MemoryScope::Global, V2ManifestBudget::default())
            .unwrap();
    assert_eq!(
        std::fs::read_to_string(scope.join("MEMORY.md")).unwrap(),
        manifest.content
    );
    assert!(manifest.content.contains("Fact"));
    assert!(manifest.content.contains('�'));
}
