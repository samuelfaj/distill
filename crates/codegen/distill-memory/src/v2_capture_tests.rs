// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use super::*;
use tempfile::TempDir;
use distill_config_types::MemoryMode;

fn setup() -> (TempDir, V2CaptureStore) {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    crate::v2::ensure_scope_initialized(temp.path(), &scope, V2MemoryScope::Workspace).unwrap();
    let store = V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    (temp, store)
}

fn range(from: u32, through: u32) -> CaptureRange {
    CaptureRange::try_new(from, through).unwrap()
}

fn claim(store: &V2CaptureStore, owner: &str, now: i64) -> CaptureLease {
    store
        .claim(&ClaimRequest {
            owner: owner.to_owned(),
            now,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap()
}

fn observation(statement: &str) -> ObservationDraft {
    ObservationDraft {
        observation_type: ObservationType::Project,
        topic_hint: Some("capture".to_owned()),
        statement: statement.to_owned(),
        keywords: vec!["durable".to_owned(), "sqlite".to_owned()],
        aliases: vec!["persistence".to_owned()],
        extraction_model: "grok-test".to_owned(),
        prompt_version: "capture-v1".to_owned(),
        created_at: 1_788_000_000,
        body: Some("Bounded supporting detail.".to_owned()),
    }
}

fn observations(statement: &str) -> CaptureOutcomeDraft {
    CaptureOutcomeDraft::Observations(vec![observation(statement)])
}

fn prepare(
    store: &V2CaptureStore,
    job: &CaptureJob,
    outcome: &CaptureOutcomeDraft,
) -> PreparedOutcome {
    let safe_session = pinned_safe_session(&store.open_state().unwrap(), &job.session_id).unwrap();
    store.prepare_outcome(job, outcome, &safe_session).unwrap()
}

#[derive(Debug)]
struct AdvancingClock {
    next: AtomicI64,
    calls: AtomicUsize,
}

impl crate::V2Clock for AdvancingClock {
    fn now_unix_seconds(&self) -> i64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.next.fetch_add(100, Ordering::SeqCst)
    }
}

fn observation_pair(first: &str, second: &str) -> CaptureOutcomeDraft {
    CaptureOutcomeDraft::Observations(vec![observation(first), observation(second)])
}

#[test]
fn promotion_updates_running_hidden_job_before_it_commits() {
    let (temp, store) = setup();
    store
        .enqueue_with_visibility("record-only", range(1, 1), false)
        .unwrap();
    let lease = claim(&store, "worker", 10);

    assert_eq!(store.promote_hidden_observations(11).unwrap(), 0);
    let committed = store
        .commit(&lease, &observations("Promoted while running"), 12)
        .unwrap();

    let state_path = temp.path().join("scope/memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM memory_v2_hidden_observations",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        temp.path()
            .join("scope")
            .join(committed.files.first().expect("first file"))
            .is_file()
    );
    assert!(
        std::fs::read_to_string(temp.path().join("scope/MEMORY.md"))
            .unwrap()
            .contains("Promoted while running")
    );
}

#[test]
fn terminal_failure_sanitizes_diagnostic_before_transition() {
    let oversized = "x".repeat(MAX_FAILURE_BYTES + 1);
    for diagnostic in ["", oversized.as_str()] {
        let (temp, store) = setup();
        store.enqueue("terminal", range(1, 1)).unwrap();
        let lease = claim(&store, "worker", 10);

        store.fail_terminal(&lease, 11, diagnostic).unwrap();

        let state_path = temp.path().join("scope/memory_state.sqlite");
        let connection = JournalMode::for_db_path(&state_path)
            .open_readonly(&state_path)
            .unwrap();
        let persisted = connection
            .query_row(
                "SELECT last_error FROM capture_jobs WHERE job_id = ?1",
                params![lease.job.job_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(!persisted.is_empty());
        assert!(persisted.len() <= MAX_FAILURE_BYTES);
        drop(connection);
        assert!(
            store
                .claim_for_session(
                    "terminal",
                    &ClaimRequest {
                        owner: "retry".to_owned(),
                        now: 12,
                        duration: Duration::from_secs(60),
                    },
                )
                .unwrap()
                .is_none(),
            "terminal failures must not become claimable after diagnostic normalization"
        );
        assert_eq!(
            store.cursors("terminal").unwrap().captured,
            1,
            "a terminal failure resolves its range so the captured cursor does not stall"
        );
    }
}

#[test]
fn filename_is_deterministic_sanitized_and_session_distinct() {
    let (temp, store) = setup();
    let first = store.enqueue("../Team Session", range(4, 9)).unwrap();
    let first_lease = claim(&store, "worker-a", 10);
    let first_commit = store
        .commit(&first_lease, &observations("Deterministic alpha fact"), 11)
        .unwrap();
    assert_eq!(first.job_id, first_lease.job.job_id);
    assert_eq!(first.session_id, first_lease.job.session_id);
    assert_eq!(first.range, first_lease.job.range);
    assert_eq!(first_lease.job.attempt, 1);
    let first_relative = &first_commit.files.first().expect("first file");
    assert_eq!(first_relative.components().count(), 3);
    let filename = first_relative.file_name().unwrap().to_str().unwrap();
    assert!(filename.contains("__t000004-000009__n000.md"));
    assert!(!filename.contains(".."));
    assert!(!filename.contains('/'));

    store.enqueue("Team/Session", range(4, 9)).unwrap();
    let second_lease = claim(&store, "worker-b", 20);
    let second_commit = store
        .commit(&second_lease, &observations("Deterministic beta fact"), 21)
        .unwrap();
    assert_ne!(first_commit.files, second_commit.files);
    assert!(temp.path().join("scope").join(first_relative).is_file());

    let uuid = "01A08895-F3C6-7903-A6F9-652C72B44530";
    store.enqueue(uuid, range(1, 1)).unwrap();
    let uuid_lease = claim(&store, "worker-c", 30);
    let uuid_commit = store
        .commit(&uuid_lease, &observations("Deterministic gamma fact"), 31)
        .unwrap();
    assert_eq!(
        uuid_commit
            .files
            .first()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap(),
        "01a08895-f3c6-7903-a6f9-652c72b44530__t000001-000001__n000.md"
    );
}

#[test]
fn filenames_follow_the_session_name_pinned_at_first_enqueue() {
    let (temp, store) = setup();
    let session = "01a08895-f3c6-7903-a6f9-652c72b44530";
    let legacy_name = format!("{session}-{}", "c".repeat(64));
    store.enqueue(session, range(1, 1)).unwrap();
    store
        .open_state()
        .unwrap()
        .execute(
            "UPDATE capture_sessions SET safe_session = ?1 WHERE session_id = ?2",
            params![legacy_name, session],
        )
        .unwrap();

    let lease = claim(&store, "worker", 10);
    let commit = store
        .commit(&lease, &observations("Pinned name fact"), 11)
        .unwrap();
    let first_file = commit.files.first().expect("first file");
    let filename = first_file.file_name().unwrap().to_str().unwrap();
    assert!(filename.starts_with(&format!("{legacy_name}__t000001-000001__n")));
    assert!(temp.path().join("scope").join(first_file).is_file());
}

#[test]
fn schema_migrates_v1_and_reopen_is_idempotent() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    std::fs::create_dir_all(scope.join("observations/_inbox")).unwrap();
    let state_path = scope.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '1');",
        )
        .unwrap();
    drop(connection);

    V2CaptureStore::open(&scope, V2MemoryScope::Global).unwrap();
    V2CaptureStore::open(&scope, V2MemoryScope::Global).unwrap();
    let connection = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "4"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name LIKE 'capture_%'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        5
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'capture_revision'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "0"
    );
}

#[test]
fn network_mounted_capture_store_fails_before_creating_shared_state() {
    let temp = TempDir::new().unwrap();
    let scope = temp.path().join("scope");
    std::fs::create_dir(&scope).unwrap();

    let error = match V2CaptureStore::open_with_journal_mode(
        &scope,
        V2MemoryScope::Workspace,
        Some(JournalMode::Truncate),
    ) {
        Ok(_) => panic!("network-mounted capture unexpectedly opened"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        V2CaptureError::UnsupportedNetworkFilesystem { path } if path == scope
    ));
    assert!(!scope.join("observations").exists());
    assert!(!scope.join("memory_state.sqlite").exists());
    assert!(
        !JournalMode::Truncate
            .effective_db_path(&scope.join("memory_state.sqlite"))
            .exists()
    );
}

#[test]
fn duplicate_enqueue_is_one_job_and_requested_cursor_is_monotonic() {
    let (_temp, store) = setup();
    let first = store.enqueue("session", range(1, 4)).unwrap();
    let duplicate = store.enqueue("session", range(1, 4)).unwrap();
    store.enqueue("session", range(5, 8)).unwrap();
    store.enqueue("session", range(2, 3)).unwrap();
    assert_eq!(first, duplicate);
    assert_eq!(store.cursors("session").unwrap().requested, 8);
    let connection = store.open_state().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM capture_jobs", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        3
    );
}

#[test]
fn capture_job_persists_source_prompt_locator() {
    let (_temp, store) = setup();
    let job = store.enqueue_for_prompt("session", range(1, 1), 7).unwrap();
    assert_eq!(job.source_prompt_index, 7);
    let lease = claim(&store, "worker", 10);
    assert_eq!(lease.job.source_prompt_index, 7);
    assert!(matches!(
        store.enqueue_for_prompt("session", range(1, 1), 8),
        Err(V2CaptureError::Conflict(_))
    ));
}

#[test]
fn session_claims_are_independent_and_keep_lease_serialization() {
    let (_temp, store) = setup();
    store.enqueue("first", range(1, 1)).unwrap();
    store.enqueue("second", range(1, 1)).unwrap();
    let request = |owner: &str| ClaimRequest {
        owner: owner.to_owned(),
        now: 10,
        duration: Duration::from_secs(60),
    };
    let second = store
        .claim_for_session("second", &request("worker-second"))
        .unwrap()
        .unwrap();
    let first = store
        .claim_for_session("first", &request("worker-first"))
        .unwrap()
        .unwrap();
    assert_eq!(second.job.session_id, "second");
    assert_eq!(first.job.session_id, "first");
    assert!(
        store
            .claim_for_session("first", &request("other"))
            .unwrap()
            .is_none(),
        "an unexpired lease remains exclusive"
    );
}

#[test]
fn observation_commit_converges_file_fts_manifest_and_cursors() {
    let (temp, store) = setup();
    store.enqueue("session", range(1, 5)).unwrap();
    let lease = claim(&store, "worker", 100);
    let result = store
        .commit(
            &lease,
            &observations("Quasar capture token is durable"),
            101,
        )
        .unwrap();
    let path = temp
        .path()
        .join("scope")
        .join(result.files.first().expect("first file"));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("schema_version: 2"));
    assert!(content.contains("type: project"));
    assert!(content.contains("job_id: \"cap_"));

    let storage = MemoryStorage::new_flat(temp.path(), &temp.path().join("scope"));
    let index = crate::MemoryIndex::open_or_create(
        &temp.path().join("scope/index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(!index.search_fts("quasar durable", 10).unwrap().is_empty());
    let manifest = std::fs::read_to_string(temp.path().join("scope/MEMORY.md")).unwrap();
    assert!(manifest.contains("## Pending observations"));
    assert!(manifest.contains(result.files.first().expect("first file").to_str().unwrap()));
    assert_eq!(
        store.cursors("session").unwrap(),
        CaptureCursors {
            requested: 5,
            captured: 5,
            indexed: 5
        }
    );
}

#[test]
fn capture_reindex_preserves_existing_embedding_dimension_and_rows() {
    crate::init_sqlite_vec();
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    let index_path = scope.join("index.sqlite");
    let storage = MemoryStorage::new_flat(temp.path(), &scope);
    let index = crate::MemoryIndex::open_or_create(
        &index_path,
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        4,
    )
    .unwrap();
    if !index.vec_available() {
        return;
    }
    index
        .upsert_embedding("embedding-sentinel", &[0.0, 0.0, 0.0, 0.0])
        .unwrap();
    drop(index);

    store.enqueue("session", range(1, 2)).unwrap();
    store
        .commit(
            &claim(&store, "worker", 10),
            &observations("Capture must preserve vectors"),
            11,
        )
        .unwrap();

    let storage = MemoryStorage::new_flat(temp.path(), &scope);
    let index = crate::MemoryIndex::open_or_create(
        &index_path,
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        4,
    )
    .unwrap();
    assert_eq!(index.embedding_dimensions(), 4);
    assert!(
        index
            .vector_search(&[0.0, 0.0, 0.0, 0.0], 10)
            .unwrap()
            .iter()
            .any(|(chunk_id, _)| chunk_id == "embedding-sentinel")
    );
}

#[test]
fn noop_advances_cursors_without_files_or_index_changes() {
    let (temp, store) = setup();
    let index_path = temp.path().join("scope/index.sqlite");
    let before = JournalMode::for_db_path(&index_path)
        .open_readonly(&index_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, u32>(0)
        })
        .unwrap();
    store.enqueue("session", range(1, 3)).unwrap();
    let result = store
        .commit(&claim(&store, "worker", 10), &CaptureOutcomeDraft::Noop, 11)
        .unwrap();
    assert!(result.files.is_empty());
    assert_eq!(
        std::fs::read_dir(temp.path().join("scope/observations/_inbox"))
            .unwrap()
            .count(),
        0
    );
    let after = JournalMode::for_db_path(&index_path)
        .open_readonly(&index_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, u32>(0)
        })
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        store.cursors("session").unwrap(),
        CaptureCursors {
            requested: 3,
            captured: 3,
            indexed: 3
        }
    );
}

#[test]
fn retryable_failure_does_not_advance_any_completed_cursor() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 3)).unwrap();
    let failed = claim(&store, "first-worker", 10);
    store
        .fail_retryable(&failed, 11, "malformed structured output")
        .unwrap();
    assert_eq!(
        store.work_state("session", 3).unwrap(),
        CaptureWorkState {
            failed: 1,
            last_error: Some("malformed structured output".to_owned()),
            ..Default::default()
        }
    );
    assert_eq!(
        store.cursors("session").unwrap(),
        CaptureCursors {
            requested: 3,
            captured: 0,
            indexed: 0,
        }
    );
    let retried = store
        .claim_for_session(
            "session",
            &ClaimRequest {
                owner: "retry-worker".to_owned(),
                now: 12,
                duration: Duration::from_secs(60),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(retried.job.attempt, 2);
    store
        .commit(&retried, &CaptureOutcomeDraft::Noop, 13)
        .unwrap();
    assert_eq!(store.cursors("session").unwrap().indexed, 3);
}

#[test]
fn pending_work_is_claimed_before_failed_work_is_retried() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 1)).unwrap();
    let failed = claim(&store, "first-worker", 10);
    store
        .fail_retryable(&failed, 11, "malformed structured output")
        .unwrap();
    store.enqueue("session", range(2, 2)).unwrap();

    let pending = claim(&store, "pending-worker", 12);
    assert_eq!(pending.job.range, range(2, 2));
    assert_eq!(pending.job.attempt, 1);
    store
        .commit(&pending, &CaptureOutcomeDraft::Noop, 13)
        .unwrap();

    let retried = claim(&store, "retry-worker", 14);
    assert_eq!(retried.job.range, range(1, 1));
    assert_eq!(retried.job.attempt, 2);
}

#[test]
fn retryable_failure_text_cannot_strand_a_running_lease() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 1)).unwrap();
    let failed = claim(&store, "first-worker", 10);
    let untrusted = format!("\n{}\0", "é".repeat(MAX_FAILURE_BYTES));
    store.fail_retryable(&failed, 11, &untrusted).unwrap();

    let state = store.work_state("session", 1).unwrap();
    assert_eq!(state.running, 0);
    assert_eq!(state.failed, 1);
    let error = state.last_error.unwrap();
    assert!(!error.is_empty());
    assert!(error.len() <= MAX_FAILURE_BYTES);
    assert!(!error.chars().any(char::is_control));
    assert_eq!(claim(&store, "retry-worker", 12).job.attempt, 2);
}

#[test]
fn cancellation_release_makes_lease_immediately_retryable() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 1)).unwrap();
    let interrupted = claim(&store, "first-worker", 10);

    store
        .release_retryable(&interrupted, "worker interrupted")
        .unwrap();
    let retried = claim(&store, "replacement-worker", 10);
    assert_eq!(retried.job.attempt, 2);
    assert_eq!(
        store.work_state("session", 1).unwrap().running,
        1,
        "the replacement owns the lease without waiting for expiry"
    );
    assert!(matches!(
        store.release_retryable(&interrupted, "stale cleanup"),
        Err(V2CaptureError::StaleLease)
    ));
}

#[test]
fn retry_same_bytes_is_idempotent_and_different_outcome_is_rejected() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let lease = claim(&store, "worker", 10);
    let outcome = observations("Retry-safe immutable fact");
    let first = store.commit(&lease, &outcome, 11).unwrap();
    let second = store.commit(&lease, &outcome, 12).unwrap();
    assert_eq!(first.outcome_hash, second.outcome_hash);
    assert!(second.was_already_committed);
    assert!(matches!(
        store.commit(&lease, &observations("Changed retry bytes"), 12),
        Err(V2CaptureError::Conflict(_))
    ));
}

#[test]
fn expired_lease_is_reclaimed_and_stale_owner_is_fenced() {
    let (_temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let old = store
        .claim(&ClaimRequest {
            owner: "old".to_owned(),
            now: 10,
            duration: Duration::from_secs(5),
        })
        .unwrap()
        .unwrap();
    let new = store
        .claim(&ClaimRequest {
            owner: "new".to_owned(),
            now: 15,
            duration: Duration::from_secs(5),
        })
        .unwrap()
        .unwrap();
    assert_eq!(new.job.attempt, old.job.attempt + 1);
    assert!(matches!(
        store.commit(&old, &CaptureOutcomeDraft::Noop, 15),
        Err(V2CaptureError::StaleLease)
    ));
    store.commit(&new, &CaptureOutcomeDraft::Noop, 16).unwrap();
}

#[test]
fn reconcile_repairs_files_before_outcome_and_outcome_before_index() {
    let (temp, store) = setup();
    store.enqueue("files-first", range(1, 2)).unwrap();
    let files_lease = claim(&store, "worker-files", 10);
    let files_prepared = prepare(
        &store,
        &files_lease.job,
        &observations("Files-first crash token"),
    );
    for file in &files_prepared.files {
        persist_create_only(&temp.path().join("scope").join(&file.path), &file.bytes).unwrap();
    }
    store.reconcile().unwrap();
    assert_eq!(store.cursors("files-first").unwrap().indexed, 2);

    store.enqueue("outcome-first", range(1, 4)).unwrap();
    let outcome_lease = claim(&store, "worker-outcome", 20);
    let outcome_prepared = prepare(
        &store,
        &outcome_lease.job,
        &observations("Outcome-before-index crash token"),
    );
    for file in &outcome_prepared.files {
        persist_create_only(&temp.path().join("scope").join(&file.path), &file.bytes).unwrap();
    }
    let mut connection = store.open_state().unwrap();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    persist_outcome(&transaction, &outcome_lease.job, &outcome_prepared, 21).unwrap();
    transaction.commit().unwrap();
    assert_eq!(store.cursors("outcome-first").unwrap().indexed, 0);
    store.reconcile().unwrap();
    assert_eq!(store.cursors("outcome-first").unwrap().indexed, 4);

    let storage = MemoryStorage::new_flat(temp.path(), &temp.path().join("scope"));
    let index = crate::MemoryIndex::open_or_create(
        &temp.path().join("scope/index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(!index.search_fts("files crash", 10).unwrap().is_empty());
    assert!(!index.search_fts("outcome crash", 10).unwrap().is_empty());
}

#[test]
fn reconcile_tolerates_dream_archival_after_unarchived_snapshot() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");

    store.enqueue("archived", range(1, 2)).unwrap();
    let archived_lease = claim(&store, "archived-worker", 10);
    let archived = prepare(
        &store,
        &archived_lease.job,
        &observations("Ephemeral archive-only token"),
    );
    for file in &archived.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }

    store.enqueue("later", range(1, 4)).unwrap();
    let later_lease = claim(&store, "later-worker", 10);
    let later = prepare(
        &store,
        &later_lease.job,
        &observations("Durable later-row quasar"),
    );
    for file in &later.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }

    let mut connection = store.open_state().unwrap();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    persist_outcome(&transaction, &archived_lease.job, &archived, 11).unwrap();
    persist_outcome(&transaction, &later_lease.job, &later, 11).unwrap();
    transaction.commit().unwrap();

    let archived_path = scope.join(&archived.files.first().expect("first file").path);
    let storage = MemoryStorage::new_flat(temp.path(), &scope);
    let mut index = crate::MemoryIndex::open_or_create(
        &scope.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    index.reindex_file(&archived_path, "workspace").unwrap();
    assert!(
        !index
            .search_fts("ephemeral archive-only", 10)
            .unwrap()
            .is_empty()
    );
    drop(index);

    let archive_relative = PathBuf::from("archive/dream-race/archived.md");
    let archive_path = scope.join(&archive_relative);
    std::fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
    store
        .reconcile_at_with_hooks(
            12,
            || {
                let connection = store.open_state().unwrap();
                connection
                    .execute(
                        "INSERT INTO consolidation_archives(
                            source_path, operation_id, archive_path, content_hash
                         ) VALUES (?1, 'dream-race', ?2, ?3)",
                        params![
                            archived
                                .files
                                .first()
                                .expect("first file")
                                .path
                                .to_str()
                                .unwrap(),
                            archive_relative.to_str().unwrap(),
                            archived.files.first().expect("first file").hash
                        ],
                    )
                    .unwrap();
                std::fs::rename(&archived_path, &archive_path).unwrap();
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();

    let storage = MemoryStorage::new_flat(temp.path(), &scope);
    let index = crate::MemoryIndex::open_or_create(
        &scope.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(
        index
            .search_fts("ephemeral archive-only", 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        !index
            .search_fts("durable later-row quasar", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.cursors("archived").unwrap().indexed, 2);
    assert_eq!(store.cursors("later").unwrap().indexed, 4);
    let manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(
        !manifest.contains(
            archived
                .files
                .first()
                .expect("first file")
                .path
                .to_str()
                .unwrap()
        )
    );
    assert!(
        manifest.contains(
            later
                .files
                .first()
                .expect("first file")
                .path
                .to_str()
                .unwrap()
        )
    );
}

#[test]
fn slow_convergence_does_not_block_capture_queue_writes() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("captured", range(1, 2)).unwrap();
    let lease = claim(&store, "capture-worker", 10);
    let prepared = prepare(&store, &lease.job, &observations("Slow convergence fact"));
    for file in &prepared.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }
    let mut connection = store.open_state().unwrap();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    persist_outcome(&transaction, &lease.job, &prepared, 11).unwrap();
    transaction.commit().unwrap();

    let queue_store = V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    let (convergence_started_tx, convergence_started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let reconcile = std::thread::spawn(move || {
        store.reconcile_at_with(12, || {
            convergence_started_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            Ok(())
        })
    });
    convergence_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (enqueue_finished_tx, enqueue_finished_rx) = std::sync::mpsc::channel();
    let enqueue = std::thread::spawn(move || {
        let result = queue_store.enqueue("queued", range(3, 4));
        enqueue_finished_tx.send(result.is_ok()).unwrap();
    });
    assert!(
        enqueue_finished_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    );

    resume_tx.send(()).unwrap();
    reconcile.join().unwrap().unwrap();
    enqueue.join().unwrap();
}

#[test]
fn stale_reconcile_cannot_replace_newer_cross_handle_manifest() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let first = prepare(
        &store,
        &first_lease.job,
        &observations("First snapshot fact"),
    );
    for file in &first.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }
    let mut connection = store.open_state().unwrap();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    persist_outcome(&transaction, &first_lease.job, &first, 11).unwrap();
    transaction.commit().unwrap();

    store.enqueue("session", range(3, 4)).unwrap();
    let second_lease = claim(&store, "second-worker", 20);
    let newer = V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    let observer = V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    let (older_rendered_tx, older_rendered_rx) = std::sync::mpsc::channel();
    let (resume_older_tx, resume_older_rx) = std::sync::mpsc::channel();
    let older = std::thread::spawn(move || {
        store.reconcile_at_with(21, || {
            older_rendered_tx.send(()).unwrap();
            resume_older_rx.recv().unwrap();
            Ok(())
        })
    });
    older_rendered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let second_result = newer
        .commit(&second_lease, &observations("Concurrent snapshot fact"), 21)
        .unwrap();
    let newer_manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(
        newer_manifest.contains(
            first
                .files
                .first()
                .expect("first file")
                .path
                .to_str()
                .unwrap()
        )
    );
    assert!(
        newer_manifest.contains(
            second_result
                .files
                .first()
                .expect("first file")
                .to_str()
                .unwrap()
        )
    );

    resume_older_tx.send(()).unwrap();
    older.join().unwrap().unwrap();
    let final_manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert_eq!(final_manifest, newer_manifest);
    assert_eq!(observer.cursors("session").unwrap().indexed, 4);
}

#[test]
fn stale_capture_manifest_cannot_replace_topic_writer_manifest() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("session", range(1, 1)).unwrap();
    let lease = claim(&store, "worker", 10);
    store
        .commit(&lease, &observations("Capture snapshot fact"), 11)
        .unwrap();

    store
        .reconcile_at_with(20, || {
            std::fs::write(scope.join("topics/dream.md"), "# Dream\n\nNew topic state.").unwrap();
            crate::v2::bump_manifest_revision(&scope).unwrap();
            crate::regenerate_scope_manifest(
                &scope,
                V2MemoryScope::Workspace,
                V2ManifestBudget::default(),
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

    assert!(
        std::fs::read_to_string(scope.join("MEMORY.md"))
            .unwrap()
            .contains("topics/dream.md")
    );
}

#[test]
fn orphan_adoption_skips_live_lease_and_adopts_only_expired_work() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("abandoned", range(1, 2)).unwrap();
    store.enqueue("active", range(1, 3)).unwrap();
    let abandoned = store
        .claim(&ClaimRequest {
            owner: "abandoned-worker".to_owned(),
            now: 10,
            duration: Duration::from_secs(1),
        })
        .unwrap()
        .unwrap();
    let active = store
        .claim(&ClaimRequest {
            owner: "active-worker".to_owned(),
            now: 10,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    let abandoned_files = prepare(
        &store,
        &abandoned.job,
        &observations("Abandoned orphan fact"),
    );
    let active_files = prepare(&store, &active.job, &observations("Old active orphan fact"));
    for file in abandoned_files.files.iter().chain(&active_files.files) {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }

    store.reconcile_at_with(12, || Ok(())).unwrap();
    let connection = store.open_state().unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM capture_outcomes WHERE job_id = ?1",
                params![abandoned.job.job_id],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM capture_outcomes WHERE job_id = ?1",
                params![active.job.job_id],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
    drop(connection);

    let result = store
        .commit(&active, &observations("New active retry fact"), 13)
        .unwrap();
    assert!(
        std::fs::read_to_string(scope.join(result.files.first().expect("first file")))
            .unwrap()
            .contains("New active retry fact")
    );
}

#[test]
fn reconcile_uses_one_injected_timestamp_for_adoption_and_quarantine() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    let committed = commit_one(&store, range(1, 1), "Committed fact", 10);
    std::fs::write(scope.join(&committed), b"tampered observation").unwrap();

    store.enqueue("orphan", range(1, 2)).unwrap();
    let orphan = store
        .claim_for_session(
            "orphan",
            &ClaimRequest {
                owner: "abandoned-worker".to_owned(),
                now: 10,
                duration: Duration::from_secs(1),
            },
        )
        .unwrap()
        .unwrap();
    let prepared = prepare(&store, &orphan.job, &observations("Orphan fact"));
    for file in &prepared.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }

    let clock = Arc::new(AdvancingClock {
        next: AtomicI64::new(71),
        calls: AtomicUsize::new(0),
    });
    let injected =
        V2CaptureStore::open_with_clock(&scope, V2MemoryScope::Workspace, clock.clone()).unwrap();
    injected.reconcile().unwrap();

    let connection = injected.open_state().unwrap();
    let adopted_at = connection
        .query_row(
            "SELECT committed_at FROM capture_outcomes WHERE job_id = ?1",
            params![orphan.job.job_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let quarantined_at = connection
        .query_row(
            "SELECT quarantined_at FROM memory_v2_quarantined_paths
             WHERE relative_path = ?1",
            params![committed.to_string_lossy()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!((adopted_at, quarantined_at), (71, 71));
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn malformed_orphan_does_not_block_reconcile_or_retry_commit() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let prepared = prepare(
        &store,
        &first_lease.job,
        &observations("Original orphan fact"),
    );
    std::fs::write(
        scope.join(&prepared.files.first().expect("first file").path),
        b"not an observation",
    )
    .unwrap();

    store.reconcile_at_with(71, || Ok(())).unwrap();
    assert_eq!(
        store
            .open_state()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM capture_outcomes WHERE job_id = ?1",
                params![first_lease.job.job_id],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );

    let retry_lease = claim(&store, "retry-worker", 71);
    let result = store
        .commit(&retry_lease, &observations("Recovered retry fact"), 72)
        .unwrap();
    assert!(
        std::fs::read_to_string(scope.join(result.files.first().expect("first file")))
            .unwrap()
            .contains("Recovered retry fact")
    );
}

#[test]
fn tombstoned_orphan_member_completes_job_as_noop() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    store.enqueue("session", range(1, 2)).unwrap();
    let lease = claim(&store, "abandoned-worker", 10);
    let prepared = prepare(
        &store,
        &lease.job,
        &observation_pair("Forgotten orphan fact", "Remaining orphan fact"),
    );
    for file in &prepared.files {
        persist_create_only(&scope.join(&file.path), &file.bytes).unwrap();
    }
    let forgotten = prepared.files.first().expect("first file");
    store
        .open_state()
        .unwrap()
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('forgotten-orphan', 'observation', ?1, ?2, 70, 'privacy')",
            params![path_text(&forgotten.path).unwrap(), forgotten.hash],
        )
        .unwrap();
    std::fs::remove_file(scope.join(&forgotten.path)).unwrap();

    store.reconcile_at_with(71, || Ok(())).unwrap();

    assert_eq!(
        store
            .open_state()
            .unwrap()
            .query_row(
                "SELECT outcome_kind FROM capture_outcomes WHERE job_id = ?1",
                params![lease.job.job_id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "noop"
    );
    assert_eq!(
        store
            .open_state()
            .unwrap()
            .query_row(
                "SELECT o.committed_at, r.terminal_at
                 FROM capture_outcomes o
                 JOIN v2_job_retention r USING(job_id)
                 WHERE o.job_id = ?1",
                params![lease.job.job_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
        (71, 71)
    );
    assert_eq!(
        store.cursors("session").unwrap(),
        CaptureCursors {
            requested: 2,
            captured: 2,
            indexed: 2,
        }
    );
    assert!(
        !scope
            .join(&prepared.files.get(1).expect("second file").path)
            .exists()
    );
}

#[test]
fn leased_retry_replaces_partial_same_job_files_with_different_count() {
    let (temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let first_outcome = observation_pair("Old first fact", "Old second fact");
    let first_prepared = prepare(&store, &first_lease.job, &first_outcome);
    let first_path = temp
        .path()
        .join("scope")
        .join(&first_prepared.files.first().expect("first file").path);
    let second_path = temp
        .path()
        .join("scope")
        .join(&first_prepared.files.get(1).expect("second file").path);
    persist_create_only(
        &first_path,
        &first_prepared.files.first().expect("first file").bytes,
    )
    .unwrap();
    persist_create_only(
        &second_path,
        &first_prepared.files.get(1).expect("second file").bytes,
    )
    .unwrap();

    let retry_lease = claim(&store, "retry-worker", 70);
    let result = store
        .commit(&retry_lease, &observations("New first fact"), 71)
        .unwrap();

    assert_eq!(result.files.len(), 1);
    assert!(
        std::fs::read_to_string(first_path)
            .unwrap()
            .contains("New first fact")
    );
    assert!(!second_path.exists());
}

#[test]
fn leased_noop_retry_removes_partial_same_job_files() {
    let (temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let prepared = prepare(&store, &first_lease.job, &observations("Abandoned fact"));
    let path = temp
        .path()
        .join("scope")
        .join(&prepared.files.first().expect("first file").path);
    persist_create_only(&path, &prepared.files.first().expect("first file").bytes).unwrap();

    let retry_lease = claim(&store, "retry-worker", 70);
    let result = store
        .commit(&retry_lease, &CaptureOutcomeDraft::Noop, 71)
        .unwrap();

    assert!(result.files.is_empty());
    assert!(!path.exists());
}

#[test]
fn leased_retry_accepts_partial_same_job_files_with_identical_bytes() {
    let (temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let outcome = observation_pair("Stable first fact", "Stable second fact");
    let prepared = prepare(&store, &first_lease.job, &outcome);
    persist_create_only(
        &temp
            .path()
            .join("scope")
            .join(&prepared.files.first().expect("first file").path),
        &prepared.files.first().expect("first file").bytes,
    )
    .unwrap();

    let retry_lease = claim(&store, "retry-worker", 70);
    let result = store.commit(&retry_lease, &outcome, 71).unwrap();
    assert_eq!(result.files.len(), 2);
}

#[test]
fn leased_retry_replaces_malformed_same_job_orphan() {
    let (temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let first_lease = claim(&store, "first-worker", 10);
    let outcome = observations("Original fact");
    let prepared = prepare(&store, &first_lease.job, &outcome);
    let path = temp
        .path()
        .join("scope")
        .join(&prepared.files.first().expect("first file").path);
    let foreign = String::from_utf8(prepared.files.first().expect("first file").bytes.clone())
        .unwrap()
        .replace(
            &format!("job_id: \"{}\"", first_lease.job.job_id),
            "job_id: \"cap_foreign\"",
        )
        .into_bytes();
    persist_create_only(&path, &foreign).unwrap();

    let retry_lease = claim(&store, "retry-worker", 70);
    store
        .commit(&retry_lease, &observations("Retry fact"), 71)
        .unwrap();
    let replacement = std::fs::read_to_string(path).unwrap();
    assert!(replacement.contains("Retry fact"));
    assert!(!replacement.contains("cap_foreign"));
}

#[test]
fn recovery_directory_and_database_scans_fail_at_named_caps() {
    let (temp, store) = setup();
    let inbox = temp.path().join("scope/observations/_inbox");
    for index in 0..=MAX_INBOX_ENTRIES {
        std::fs::write(inbox.join(format!("unrelated-{index}.tmp")), []).unwrap();
    }
    assert!(matches!(
        store.reconcile(),
        Err(V2CaptureError::RecoveryDirectoryLimit { limit, .. })
            if limit == MAX_INBOX_ENTRIES
    ));

    let (_temp, store) = setup();
    store.ensure_session("session").unwrap();
    let mut connection = store.open_state().unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..=MAX_RECOVERY_JOBS {
        transaction
            .execute(
                "INSERT INTO capture_jobs(
                    job_id, session_id, from_turn, through_turn, status, attempt
                 ) VALUES (?1, 'session', ?2, ?2, 'pending', 0)",
                params![format!("job-{index}"), index],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    assert!(matches!(
        store.reconcile(),
        Err(V2CaptureError::RecoveryRowLimit { rows, limit })
            if rows == "uncommitted capture jobs" && limit == MAX_RECOVERY_JOBS
    ));
}

#[test]
fn committed_observation_scan_has_a_named_row_cap() {
    let (_temp, store) = setup();
    store.ensure_session("session").unwrap();
    let mut connection = store.open_state().unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..=MAX_COMMITTED_OBSERVATION_ROWS {
        let job_id = format!("job-{index}");
        transaction
            .execute(
                "INSERT INTO capture_jobs(
                    job_id, session_id, from_turn, through_turn, status, attempt
                 ) VALUES (?1, 'session', ?2, ?2, 'completed', 1)",
                params![job_id, index],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO capture_outcomes(job_id, outcome_kind, outcome_hash, committed_at)
                 VALUES (?1, 'observations', 'hash', 0)",
                params![job_id],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO capture_observation_files(job_id, ordinal, path, content_hash)
                 VALUES (?1, 0, ?2, 'hash')",
                params![job_id, format!("observations/_inbox/{index}.md")],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    assert!(matches!(
        store.reconcile(),
        Err(V2CaptureError::RecoveryRowLimit { rows, limit })
            if rows == "committed observation files"
                && limit == MAX_COMMITTED_OBSERVATION_ROWS
    ));
}

#[test]
fn convergence_errors_preserve_typed_sources() {
    use std::error::Error as _;

    let (temp, store) = setup();
    std::fs::remove_file(temp.path().join("scope/MEMORY.md")).unwrap();
    std::fs::create_dir(temp.path().join("scope/MEMORY.md")).unwrap();
    let error = store.reconcile().unwrap_err();
    assert!(matches!(&error, V2CaptureError::Manifest(_)));
    assert!(error.source().is_some());

    let (temp, store) = setup();
    store.enqueue("session", range(1, 2)).unwrap();
    let lease = claim(&store, "worker", 10);
    let prepared = prepare(&store, &lease.job, &observations("Index source fact"));
    for file in &prepared.files {
        persist_create_only(&temp.path().join("scope").join(&file.path), &file.bytes).unwrap();
    }
    let mut connection = store.open_state().unwrap();
    let transaction = connection.transaction().unwrap();
    persist_outcome(&transaction, &lease.job, &prepared, 11).unwrap();
    transaction.commit().unwrap();
    std::fs::remove_file(temp.path().join("scope/index.sqlite")).unwrap();
    std::fs::create_dir(temp.path().join("scope/index.sqlite")).unwrap();
    let error = store.reconcile().unwrap_err();
    assert!(matches!(&error, V2CaptureError::Index { .. }));
    assert!(error.source().is_some());
}

fn commit_one(store: &V2CaptureStore, range: CaptureRange, statement: &str, now: i64) -> PathBuf {
    store.enqueue("session", range).unwrap();
    let lease = claim(store, "worker", now);
    store
        .commit(&lease, &observations(statement), now + 1)
        .unwrap()
        .files
        .remove(0)
}

fn indexed_chunks_for(scope: &Path, relative: &Path) -> Vec<String> {
    let index_path = scope.join("index.sqlite");
    let connection = JournalMode::for_db_path(&index_path)
        .open_readonly(&index_path)
        .unwrap();
    let mut statement = connection
        .prepare("SELECT text FROM chunks WHERE path = ?1 ORDER BY rowid")
        .unwrap();
    statement
        .query_map(
            params![scope.join(relative).to_string_lossy().to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn reconcile_respects_legacy_backslash_exclusions() {
    for exclusion in ["archive", "tombstone", "hidden", "quarantine"] {
        let (temp, store) = setup();
        let scope = temp.path().join("scope");
        let relative = commit_one(&store, range(1, 1), "Excluded legacy path token", 10);
        let legacy_path = relative.to_string_lossy().replace('/', "\\");
        let connection = store.open_state().unwrap();
        match exclusion {
            "archive" => {
                connection
                    .execute(
                        "INSERT INTO consolidation_archives(
                            source_path, operation_id, archive_path, content_hash
                         ) VALUES (?1, 'operation', 'archive.md', 'hash')",
                        params![legacy_path],
                    )
                    .unwrap();
            }
            "tombstone" => {
                connection
                    .execute(
                        "INSERT INTO memory_v2_tombstones(
                            tombstone_id, target_kind, relative_path, provenance_hash,
                            created_at, reason
                         ) VALUES ('tombstone', 'observation', ?1, 'hash', 11, 'obsolete')",
                        params![legacy_path],
                    )
                    .unwrap();
            }
            "hidden" => {
                connection
                    .execute(
                        "INSERT INTO memory_v2_hidden_observations(relative_path) VALUES (?1)",
                        params![legacy_path],
                    )
                    .unwrap();
            }
            "quarantine" => {
                connection
                    .execute(
                        "INSERT INTO memory_v2_quarantined_paths(
                            relative_path, reason, expected_hash, observed_hash, quarantined_at
                         ) VALUES (?1, 'committed_hash_mismatch', 'expected', 'observed', 11)",
                        params![legacy_path],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }

        let storage = MemoryStorage::new_flat(temp.path(), &scope);
        let mut index = crate::MemoryIndex::open_or_create(
            &scope.join("index.sqlite"),
            storage,
            distill_config_types::MemoryIndexConfig::default(),
            1,
        )
        .unwrap();
        index.delete_path(&scope.join(&relative)).unwrap();
        drop(index);

        store.reconcile().unwrap();
        assert!(
            indexed_chunks_for(&scope, &relative).is_empty(),
            "{exclusion} path with legacy separators must stay excluded"
        );
    }
}

#[test]
fn missing_inbox_file_does_not_wedge_later_commits() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    let first = commit_one(&store, range(1, 1), "Alpha wedge token", 10);
    let second = commit_one(&store, range(2, 2), "Beta wedge token", 20);
    assert!(!indexed_chunks_for(&scope, &first).is_empty());
    std::fs::remove_file(scope.join(&first)).unwrap();

    let third = commit_one(&store, range(3, 3), "Gamma wedge token", 30);

    let cursors = store.cursors("session").unwrap();
    assert_eq!(cursors.captured, 3);
    assert_eq!(cursors.indexed, cursors.captured);
    assert!(indexed_chunks_for(&scope, &first).is_empty());
    assert!(!indexed_chunks_for(&scope, &second).is_empty());
    assert!(!indexed_chunks_for(&scope, &third).is_empty());
    let manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(!manifest.contains(first.to_str().unwrap()));
    assert!(manifest.contains(second.to_str().unwrap()));
    assert!(manifest.contains(third.to_str().unwrap()));
    assert_eq!(
        store
            .open_state()
            .unwrap()
            .query_row(
                "SELECT reason FROM memory_v2_quarantined_paths WHERE relative_path = ?1",
                params![first.to_string_lossy()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "committed_hash_mismatch"
    );
    store.reconcile().unwrap();
}

#[test]
fn edited_inbox_file_is_durably_quarantined_from_index_and_manifest() {
    let (temp, store) = setup();
    let scope = temp.path().join("scope");
    let first = commit_one(&store, range(1, 1), "Original edited token", 10);
    assert!(!indexed_chunks_for(&scope, &first).is_empty());
    let edited = std::fs::read_to_string(scope.join(&first))
        .unwrap()
        .replace("Original edited token", "Tampered replacement token");
    std::fs::write(scope.join(&first), &edited).unwrap();
    crate::regenerate_scope_manifest(
        &scope,
        V2MemoryScope::Workspace,
        V2ManifestBudget::default(),
    )
    .unwrap();
    assert!(
        !std::fs::read_to_string(scope.join("MEMORY.md"))
            .unwrap()
            .contains(first.to_str().unwrap()),
        "manifest rendering must verify committed inbox bytes before reconciliation"
    );

    let second = commit_one(&store, range(2, 2), "Second edited token", 20);

    let cursors = store.cursors("session").unwrap();
    assert_eq!(cursors.indexed, cursors.captured);
    assert!(indexed_chunks_for(&scope, &first).is_empty());
    assert!(!indexed_chunks_for(&scope, &second).is_empty());
    let manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(!manifest.contains(first.to_str().unwrap()));
    assert!(manifest.contains(second.to_str().unwrap()));
    assert_eq!(
        store
            .open_state()
            .unwrap()
            .query_row(
                "SELECT reason FROM memory_v2_quarantined_paths WHERE relative_path = ?1",
                params![first.to_string_lossy()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "committed_hash_mismatch"
    );
    store.reconcile().unwrap();
    assert!(
        !std::fs::read_to_string(scope.join("MEMORY.md"))
            .unwrap()
            .contains(first.to_str().unwrap())
    );
    let storage = MemoryStorage::new_flat(temp.path(), &scope);
    let index = crate::MemoryIndex::open_or_create(
        &scope.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(
        index
            .search_fts("tampered replacement", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn completed_job_retries_reconciliation_without_reextracting() {
    let (temp, store) = setup();
    let manifest = temp.path().join("scope/MEMORY.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    store.enqueue("session", range(1, 2)).unwrap();
    let lease = claim(&store, "worker", 10);

    assert!(
        store
            .commit(&lease, &observations("Reconcile retry token"), 11)
            .is_err()
    );
    assert_eq!(
        store.cursors("session").unwrap(),
        CaptureCursors {
            requested: 2,
            captured: 2,
            indexed: 0,
        }
    );
    assert!(
        store
            .claim_for_session(
                "session",
                &ClaimRequest {
                    owner: "must-not-reextract".to_owned(),
                    now: 12,
                    duration: Duration::from_secs(60),
                },
            )
            .unwrap()
            .is_none()
    );

    std::fs::remove_dir(&manifest).unwrap();
    store.reconcile().unwrap();
    assert_eq!(store.cursors("session").unwrap().indexed, 2);
}

#[test]
fn independent_handles_for_two_sessions_preserve_files_and_manifest() {
    let (temp, first) = setup();
    let scope = temp.path().join("scope");
    let second = V2CaptureStore::open(&scope, V2MemoryScope::Workspace).unwrap();
    first.enqueue("session-a", range(1, 3)).unwrap();
    first.enqueue("session-b", range(1, 3)).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [
        (first, "worker-a", "Concurrent alpha token"),
        (second, "worker-b", "Concurrent beta token"),
    ]
    .into_iter()
    .map(|(store, owner, statement)| {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            let lease = claim(&store, owner, 10);
            store.commit(&lease, &observations(statement), 11).unwrap()
        })
    })
    .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_ne!(
        results.first().expect("first result").files,
        results.get(1).expect("second result").files
    );
    assert!(
        scope
            .join(
                results
                    .first()
                    .expect("first result")
                    .files
                    .first()
                    .expect("first file")
            )
            .is_file()
    );
    assert!(
        scope
            .join(
                results
                    .get(1)
                    .expect("second result")
                    .files
                    .first()
                    .expect("first file")
            )
            .is_file()
    );
    let manifest = std::fs::read_to_string(scope.join("MEMORY.md")).unwrap();
    assert!(
        manifest.contains(
            results
                .first()
                .expect("first result")
                .files
                .first()
                .expect("first file")
                .to_str()
                .unwrap()
        )
    );
    assert!(
        manifest.contains(
            results
                .get(1)
                .expect("second result")
                .files
                .first()
                .expect("first file")
                .to_str()
                .unwrap()
        )
    );
}

#[test]
fn invalid_ranges_bounds_controls_and_symlinks_fail_closed() {
    assert!(CaptureRange::try_new(2, 1).is_err());
    assert!(CaptureRange::try_new(0, 1_000_000).is_err());
    assert!(ObservationType::parse("instruction").is_err());
    let (_temp, store) = setup();
    assert!(store.enqueue("bad\nsession", range(1, 2)).is_err());
    store.enqueue("session", range(1, 2)).unwrap();
    let lease = claim(&store, "worker", 10);
    let mut invalid = observation("valid");
    invalid.statement = "x".repeat(MAX_STATEMENT_BYTES + 1);
    assert!(matches!(
        store.commit(
            &lease,
            &CaptureOutcomeDraft::Observations(vec![invalid]),
            11
        ),
        Err(V2CaptureError::Invalid(_))
    ));
    let mut invalid = observation("valid");
    invalid.keywords = (0..=MAX_KEYWORDS).map(|index| index.to_string()).collect();
    assert!(
        store
            .commit(
                &lease,
                &CaptureOutcomeDraft::Observations(vec![invalid]),
                11
            )
            .is_err()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let scope = temp.path().join("scope");
        std::fs::create_dir_all(scope.join("observations")).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, scope.join("observations/_inbox")).unwrap();
        assert!(V2CaptureStore::open(&scope, V2MemoryScope::Workspace).is_err());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    }
}

#[test]
fn v2_sources_are_evergreen_while_legacy_sessions_still_decay() {
    let temp = TempDir::new().unwrap();
    let v2 = MemoryStorage::new_for_mode(
        Path::new("/home/test/project"),
        Some(temp.path()),
        MemoryMode::V2,
    );
    assert_eq!(
        v2.classify_source(&v2.workspace_dir().join("topics/project.md")),
        "workspace"
    );
    assert_eq!(
        v2.classify_source(&v2.workspace_dir().join("observations/_inbox/capture.md")),
        "workspace"
    );
    assert_eq!(
        v2.classify_source(&v2.global_dir().join("observations/_inbox/capture.md")),
        "global"
    );

    let legacy = MemoryStorage::with_paths(
        temp.path().join("legacy"),
        temp.path().join("legacy/workspace"),
    );
    assert_eq!(
        legacy.classify_source(&legacy.workspace_dir().join("sessions/log.md")),
        "session"
    );
    assert_eq!(
        legacy.classify_source(&legacy.workspace_dir().join("topic.md")),
        "session"
    );
    assert_eq!(
        legacy.classify_source(&legacy.workspace_dir().join("MEMORY.md")),
        "workspace"
    );
}
