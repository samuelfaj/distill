// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use rusqlite::params;
use tempfile::TempDir;
use distill_tools::types::memory_v2::{
    MemoryV2Access as _, MemoryV2AccessResource, MemoryV2Write, record_memory_v2_read,
    write_memory_v2_file,
};
use distill_tools::types::resources::Resources;
use distill_sqlite_journal::JournalMode;

use super::*;
use crate::V2CaptureStore;
use crate::v2::ensure_scope_initialized;

struct Fixture {
    _temp: TempDir,
    global: PathBuf,
    workspace: PathBuf,
    policy: Arc<V2MemoryAccessPolicy>,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("memory-v2");
        let global = root.join("global");
        let workspace = root.join("workspaces/project");
        std::fs::create_dir_all(&root).unwrap();
        ensure_scope_initialized(&root, &global, V2MemoryScope::Global).unwrap();
        ensure_scope_initialized(&root, &workspace, V2MemoryScope::Workspace).unwrap();
        let policy = Arc::new(V2MemoryAccessPolicy::new(&global, &workspace).unwrap());
        Self {
            _temp: temp,
            global,
            workspace,
            policy,
        }
    }
}

#[test]
fn classifies_both_scopes_and_protected_paths() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .policy
            .classify_path(&fixture.global.join("MEMORY.md"))
            .unwrap(),
        V2PathClass::Manifest(V2MemoryScope::Global)
    );
    assert_eq!(
        fixture
            .policy
            .classify_path(&fixture.workspace.join("topics/rust.md"))
            .unwrap(),
        V2PathClass::Topic(V2MemoryScope::Workspace)
    );
    assert_eq!(
        fixture
            .policy
            .classify_path(&fixture.global.join("observations/_inbox/new.md"))
            .unwrap(),
        V2PathClass::Observation(V2MemoryScope::Global)
    );
    assert_eq!(
        fixture
            .policy
            .classify_path(&fixture.workspace.join("archive/old.md"))
            .unwrap(),
        V2PathClass::ArchivedObservation(V2MemoryScope::Workspace)
    );
    assert_eq!(
        fixture
            .policy
            .classify_path(fixture._temp.path().join("outside.md").as_path())
            .unwrap(),
        V2PathClass::Outside
    );
}

#[test]
fn rejects_traversal_protected_and_non_markdown_writes() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .policy
            .classify_path(&fixture.workspace.join("topics/../archive/x.md"))
            .unwrap(),
        V2PathClass::Protected(V2MemoryScope::Workspace)
    );
    assert!(
        fixture
            .policy
            .write_file(&fixture.workspace.join("MEMORY.md"), b"bad")
            .unwrap_err()
            .contains("protected")
    );
    assert!(
        fixture
            .policy
            .write_file(&fixture.workspace.join("topics/no.txt"), b"bad")
            .unwrap_err()
            .contains(".md")
    );
}

#[test]
fn rejects_recreating_durably_excluded_paths() {
    let fixture = Fixture::new();
    V2CaptureStore::open(&fixture.workspace, V2MemoryScope::Workspace).unwrap();
    let state_path = fixture.workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('forgotten', 'topic', 'topics/forgotten.md', ?1, 1, 'privacy')",
            params!["0".repeat(64)],
        )
        .unwrap();

    assert!(matches!(
        fixture
            .policy
            .write_file_inner(&fixture.workspace.join("topics/forgotten.md"), b"resurrect"),
        Err(V2AccessError::Excluded(_))
    ));
    assert!(matches!(
        fixture
            .policy
            .validate_read_inner(&fixture.workspace.join("topics/forgotten.md")),
        Err(V2AccessError::Excluded(_))
    ));
}

#[test]
fn dot_alias_to_manifest_stays_protected() {
    let fixture = Fixture::new();
    let alias = fixture.workspace.join("topics/../MEMORY.md");
    assert_eq!(
        fixture.policy.classify_path(&alias).unwrap(),
        V2PathClass::Manifest(V2MemoryScope::Workspace)
    );
    assert!(matches!(
        fixture.policy.write_file_inner(&alias, b"bad"),
        Err(V2AccessError::Protected(_))
    ));
}

#[cfg(unix)]
#[test]
fn prefix_symlink_alias_to_manifest_stays_protected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let alias_root = fixture._temp.path().join("workspace-alias");
    symlink(&fixture.workspace, &alias_root).unwrap();
    let alias = alias_root.join("MEMORY.md");

    assert_eq!(
        fixture.policy.classify_path(&alias).unwrap(),
        V2PathClass::Manifest(V2MemoryScope::Workspace)
    );
    assert!(matches!(
        fixture.policy.write_file_inner(&alias, b"bad"),
        Err(V2AccessError::Protected(_))
    ));
    assert!(matches!(
        fixture.policy.validate_read_inner(&alias),
        Err(V2AccessError::Symlink(_))
    ));
}

#[cfg(unix)]
#[test]
fn canonical_root_alias_to_manifest_stays_protected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let real_root = temp.path().join("real-memory-v2");
    let real_global = real_root.join("global");
    let real_workspace = real_root.join("workspaces/project");
    std::fs::create_dir_all(&real_root).unwrap();
    ensure_scope_initialized(&real_root, &real_global, V2MemoryScope::Global).unwrap();
    ensure_scope_initialized(&real_root, &real_workspace, V2MemoryScope::Workspace).unwrap();
    let configured_root = temp.path().join("configured-memory-v2");
    symlink(&real_root, &configured_root).unwrap();
    let policy = V2MemoryAccessPolicy::new(
        &configured_root.join("global"),
        &configured_root.join("workspaces/project"),
    )
    .unwrap();
    let canonical_alias = real_workspace.join("MEMORY.md");

    assert_eq!(
        policy.classify_path(&canonical_alias).unwrap(),
        V2PathClass::Manifest(V2MemoryScope::Workspace)
    );
    assert!(matches!(
        policy.write_file_inner(&canonical_alias, b"bad"),
        Err(V2AccessError::Protected(_))
    ));
}

#[test]
fn case_alias_to_manifest_stays_protected_on_case_insensitive_filesystems() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("CaseMemory");
    let global = root.join("global");
    let workspace = root.join("workspaces/project");
    std::fs::create_dir_all(&root).unwrap();
    ensure_scope_initialized(&root, &global, V2MemoryScope::Global).unwrap();
    ensure_scope_initialized(&root, &workspace, V2MemoryScope::Workspace).unwrap();
    let policy = V2MemoryAccessPolicy::new(&global, &workspace).unwrap();
    let alias = temp.path().join("casememory/workspaces/project/MEMORY.md");
    if !alias.exists() {
        return;
    }

    assert_eq!(
        policy.classify_path(&alias).unwrap(),
        V2PathClass::Manifest(V2MemoryScope::Workspace)
    );
    assert!(matches!(
        policy.write_file_inner(&alias, b"bad"),
        Err(V2AccessError::Protected(_))
    ));
}

#[test]
fn creates_atomically_then_requires_read_for_edits_and_refreshes_manifest() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/style.md");
    assert_eq!(
        fixture
            .policy
            .write_file(&path, b"# Style\n\nUse Rust.")
            .unwrap(),
        MemoryV2Write::Written {
            previous_content: None
        }
    );
    assert!(
        std::fs::read_to_string(fixture.workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/style.md")
    );

    let fresh_policy = V2MemoryAccessPolicy::new(&fixture.global, &fixture.workspace).unwrap();
    assert!(
        fresh_policy
            .write_file(&path, b"# Style\n\nUse Go.")
            .unwrap_err()
            .contains("read")
    );

    let original = std::fs::read(&path).unwrap();
    fresh_policy.record_read(&path, &original).unwrap();
    let result = fresh_policy
        .write_file(&path, b"# Style\n\nUse Rust 2024.")
        .unwrap();
    assert_eq!(
        result,
        MemoryV2Write::Written {
            previous_content: Some(original)
        }
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "# Style\n\nUse Rust 2024."
    );
}

#[test]
fn rejects_nested_topic_and_observation_paths() {
    let fixture = Fixture::new();
    let nested_topic = fixture
        .workspace
        .join("topics/frontend/components/conventions.md");
    let nested_observation = fixture.global.join("observations/_inbox/2026/note.md");

    assert_eq!(
        fixture.policy.classify_path(&nested_topic).unwrap(),
        V2PathClass::Nested(V2MemoryScope::Workspace)
    );
    assert!(matches!(
        fixture
            .policy
            .write_file_inner(&nested_topic, b"# Conventions"),
        Err(V2AccessError::NestedPath(_))
    ));
    assert!(!fixture.workspace.join("topics/frontend").exists());

    assert_eq!(
        fixture.policy.classify_path(&nested_observation).unwrap(),
        V2PathClass::Nested(V2MemoryScope::Global)
    );
    assert!(matches!(
        fixture
            .policy
            .write_file_inner(&nested_observation, b"# Note"),
        Err(V2AccessError::NestedPath(_))
    ));
    assert!(!fixture.global.join("observations/_inbox/2026").exists());
    assert!(
        fixture
            .policy
            .write_file(&nested_topic, b"# Conventions")
            .unwrap_err()
            .contains("directly under topics/")
    );
    assert!(
        !std::fs::read_to_string(fixture.workspace.join("MEMORY.md"))
            .unwrap()
            .contains("conventions.md")
    );
}

#[test]
fn rejects_oversized_writes_before_touching_the_filesystem() {
    let fixture = Fixture::new();
    let topic = fixture.workspace.join("topics/bulky.md");
    let error = fixture
        .policy
        .write_file_inner(&topic, &vec![b'x'; MAX_TOPIC_FILE_BYTES as usize + 1])
        .unwrap_err();
    assert!(matches!(
        error,
        V2AccessError::TooLarge {
            limit_bytes: MAX_TOPIC_FILE_BYTES,
            ..
        }
    ));
    assert!(!topic.exists());
    fixture
        .policy
        .write_file_inner(&topic, &vec![b'x'; MAX_TOPIC_FILE_BYTES as usize])
        .unwrap();
    assert_eq!(
        std::fs::metadata(&topic).unwrap().len(),
        MAX_TOPIC_FILE_BYTES
    );
    assert!(
        std::fs::read_to_string(fixture.workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/bulky.md")
    );

    let observation = fixture.global.join("observations/_inbox/bulky.md");
    let error = fixture
        .policy
        .write_file_inner(
            &observation,
            &vec![b'o'; MAX_WRITE_OBSERVATION_BYTES as usize + 1],
        )
        .unwrap_err();
    assert!(matches!(
        error,
        V2AccessError::TooLarge {
            limit_bytes: MAX_WRITE_OBSERVATION_BYTES,
            ..
        }
    ));
    assert!(!observation.exists());
    fixture
        .policy
        .write_file_inner(
            &observation,
            &vec![b'o'; MAX_WRITE_OBSERVATION_BYTES as usize],
        )
        .unwrap();
    assert_eq!(
        std::fs::metadata(&observation).unwrap().len(),
        MAX_WRITE_OBSERVATION_BYTES
    );
}

#[test]
fn caps_new_file_contents_before_filesystem_access() {
    let fixture = Fixture::new();
    let oversized = vec![b'x'; MAX_TOPIC_FILE_BYTES as usize + 1];
    let outside_path = fixture._temp.path().join("ordinary/massive.md");
    assert_eq!(
        fixture
            .policy
            .write_file_inner(&outside_path, &oversized)
            .unwrap(),
        MemoryV2Write::Outside
    );
    assert!(!outside_path.parent().unwrap().exists());

    let boundary_path = fixture.workspace.join("topics/boundary.md");
    let boundary = vec![b'x'; MAX_TOPIC_FILE_BYTES as usize];
    fixture
        .policy
        .write_file(&boundary_path, &boundary)
        .unwrap();
    assert_eq!(
        std::fs::metadata(boundary_path).unwrap().len(),
        boundary.len() as u64
    );

    let oversized_path = fixture.workspace.join("topics/oversized-write.md");
    let error = fixture
        .policy
        .write_file_inner(&oversized_path, &oversized)
        .unwrap_err();
    assert!(matches!(
        error,
        V2AccessError::TooLarge {
            limit_bytes,
            ..
        } if limit_bytes == MAX_TOPIC_FILE_BYTES
    ));
    assert!(!oversized_path.exists());
}

#[test]
fn rejects_oversized_previous_content_without_replacing_it() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/oversized.md");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_PREVIOUS_CONTENT_BYTES + 1).unwrap();
    fixture.policy.record_read(&path, b"").unwrap();

    let error = fixture
        .policy
        .write_file_inner(&path, b"replacement")
        .unwrap_err();
    assert!(matches!(
        error,
        V2AccessError::TooLarge {
            limit_bytes: MAX_PREVIOUS_CONTENT_BYTES,
            ..
        }
    ));
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        MAX_PREVIOUS_CONTENT_BYTES + 1
    );
}

#[tokio::test]
async fn async_adapter_preserves_read_snapshot_across_blocking_calls() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/async.md");
    fixture.policy.write_file(&path, b"before").unwrap();
    let mut resources = Resources::new();
    resources.insert(MemoryV2AccessResource(fixture.policy.clone()));
    let resources = resources.into_shared();

    record_memory_v2_read(&resources, &path, b"before")
        .await
        .unwrap();
    assert_eq!(
        write_memory_v2_file(&resources, &path, b"after")
            .await
            .unwrap(),
        MemoryV2Write::Written {
            previous_content: Some(b"before".to_vec())
        }
    );
}

#[test]
fn rejects_stale_edits_after_external_mutation() {
    let fixture = Fixture::new();
    let path = fixture.global.join("topics/fact.md");
    fixture.policy.write_file(&path, b"one").unwrap();
    fixture.policy.record_read(&path, b"one").unwrap();
    std::fs::write(&path, "two").unwrap();
    assert!(
        fixture
            .policy
            .write_file(&path, b"three")
            .unwrap_err()
            .contains("changed since")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "two");
}

#[test]
fn manifest_refresh_error_retains_storage_source() {
    use std::error::Error as _;

    let fixture = Fixture::new();
    let manifest = fixture.workspace.join("MEMORY.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let path = fixture.workspace.join("topics/source.md");
    let error = fixture
        .policy
        .write_file_inner(&path, b"content")
        .unwrap_err();
    assert!(matches!(error, V2AccessError::ManifestRefresh { .. }));
    assert!(error.source().is_some());
    assert!(!path.exists());
}

#[test]
fn manifest_refresh_failure_restores_existing_content() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/existing.md");
    fixture.policy.write_file(&path, b"before").unwrap();
    fixture.policy.record_read(&path, b"before").unwrap();

    let manifest = fixture.workspace.join("MEMORY.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let error = fixture
        .policy
        .write_file_inner(&path, b"after")
        .unwrap_err();
    assert!(matches!(error, V2AccessError::ManifestRefresh { .. }));
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
}

#[test]
fn manifest_refresh_failure_restores_deleted_topic() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/deleted.md");
    fixture.policy.write_file(&path, b"before").unwrap();
    fixture.policy.record_read(&path, b"before").unwrap();

    let manifest = fixture.workspace.join("MEMORY.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let error = fixture.policy.remove_topic_file(&path).unwrap_err();
    assert!(matches!(error, V2AccessError::ManifestRefresh { .. }));
    assert_eq!(std::fs::read(&path).unwrap(), b"before");

    std::fs::remove_dir(&manifest).unwrap();
    fixture.policy.remove_topic_file(&path).unwrap();
    assert!(!path.exists());
    assert!(
        !std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("topics/deleted.md")
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlink_file_and_directory_escapes() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let outside = fixture._temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "secret").unwrap();
    symlink(
        outside.join("secret.md"),
        fixture.workspace.join("topics/link.md"),
    )
    .unwrap();
    symlink(&outside, fixture.global.join("topics/linked-dir")).unwrap();

    assert!(
        fixture
            .policy
            .validate_read(&fixture.workspace.join("topics/link.md"))
            .unwrap_err()
            .contains("Symbolic")
            || fixture
                .policy
                .validate_read(&fixture.workspace.join("topics/link.md"))
                .unwrap_err()
                .contains("symbolic")
    );
    assert!(
        fixture
            .policy
            .validate_read(&fixture.global.join("topics/linked-dir/secret.md"))
            .is_err()
    );
}

#[test]
fn nonexistent_target_under_existing_writable_directory_is_safe() {
    let fixture = Fixture::new();
    let path = fixture.global.join("observations/_inbox/future.md");
    assert_eq!(
        fixture.policy.classify_path(&path).unwrap(),
        V2PathClass::Observation(V2MemoryScope::Global)
    );
    assert!(fixture.policy.validate_read(&path).unwrap());
}

#[test]
fn observations_are_create_only() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("observations/_inbox/note.md");
    let original = b"# Note\n\nfirst version";
    assert!(fixture.policy.preflight_write(&path, original).unwrap());
    fixture.policy.write_file(&path, original).unwrap();
    fixture.policy.record_read(&path, original).unwrap();

    let edited = b"# Note\n\nsecond version";
    let error = fixture.policy.preflight_write(&path, edited).unwrap_err();
    assert!(error.contains("append-only"), "{error}");
    let error = fixture.policy.write_file(&path, edited).unwrap_err();
    assert!(error.contains("append-only"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn atomic_replacement_never_exposes_partial_content() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/concurrent.md");
    let old = vec![b'a'; MAX_TOPIC_FILE_BYTES as usize];
    let new = vec![b'b'; MAX_TOPIC_FILE_BYTES as usize];
    fixture.policy.write_file(&path, &old).unwrap();
    fixture.policy.record_read(&path, &old).unwrap();

    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        for _ in 0..500 {
            let bytes = std::fs::read(&reader_path).unwrap();
            assert!(
                bytes.iter().all(|byte| *byte == b'a') || bytes.iter().all(|byte| *byte == b'b')
            );
        }
    });
    fixture.policy.write_file(&path, &new).unwrap();
    reader.join().unwrap();
}

#[test]
fn removal_rejects_oversized_topic_without_deleting_it() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("topics/oversized.md");
    let bytes = vec![b'x'; MAX_TOPIC_FILE_BYTES as usize + 1];
    std::fs::write(&path, &bytes).unwrap();
    fixture.policy.record_read_typed(&path, &bytes).unwrap();

    assert!(matches!(
        fixture.policy.remove_topic_file(&path),
        Err(V2AccessError::Inspect { .. })
    ));
    assert!(path.is_file());
}
