// Modified for Distill by Samuel Fajreldines, 2026.
use std::time::Duration;

use tempfile::TempDir;
use distill_sqlite_journal::JournalMode;

use super::*;
use crate::{
    CaptureOutcomeDraft, CaptureRange, ClaimRequest, DreamClaimRequest, ObservationDraft,
    ObservationType, SharedV2Clock, TopicOperation, V2CaptureStore, V2Clock, V2ConsolidationStore,
    ensure_scope_initialized,
};

#[derive(Debug)]
struct FixedClock(i64);

impl V2Clock for FixedClock {
    fn now_unix_seconds(&self) -> i64 {
        self.0
    }
}

fn fixed_clock(now: i64) -> SharedV2Clock {
    std::sync::Arc::new(FixedClock(now))
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("memory-v2");
    let global = root.join("global");
    let workspace = root.join("workspaces/project");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    ensure_scope_initialized(&root, &global, V2MemoryScope::Global).unwrap();
    ensure_scope_initialized(&root, &workspace, V2MemoryScope::Workspace).unwrap();
    (temporary, global, workspace)
}

fn capture_one(workspace: &Path) -> PathBuf {
    capture_one_with_visibility(workspace, "session", true)
}

fn capture_one_with_visibility(workspace: &Path, session: &str, is_exposed: bool) -> PathBuf {
    let store = V2CaptureStore::open(workspace, V2MemoryScope::Workspace).unwrap();
    let range = CaptureRange::try_new(1, 1).unwrap();
    store
        .enqueue_with_visibility(session, range, is_exposed)
        .unwrap();
    let lease = store
        .claim(&ClaimRequest {
            owner: "worker".to_owned(),
            now: 10,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    store
        .commit(
            &lease,
            &CaptureOutcomeDraft::Observations(vec![ObservationDraft {
                observation_type: ObservationType::Project,
                topic_hint: Some("build".to_owned()),
                statement: "Use deterministic fixtures".to_owned(),
                keywords: vec!["fixtures".to_owned()],
                aliases: Vec::new(),
                extraction_model: "test".to_owned(),
                prompt_version: "test-1".to_owned(),
                created_at: 10,
                body: None,
            }]),
            10,
        )
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn forgetting_is_durable_and_reconciliation_cannot_resurrect_observation() {
    let (_temporary, global, workspace) = fixture();
    let relative = capture_one(&workspace);
    let bytes = std::fs::read(workspace.join(&relative)).unwrap();
    let expected_content_hash = blake3::hash(&bytes).to_hex().to_string();
    let store = V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
        .unwrap();
    let result = store
        .forget(&ForgetRequest {
            relative_path: relative.clone(),
            expected_content_hash: expected_content_hash.clone(),
            reason: ForgetReason::UserRequest,
            now: 20,
        })
        .unwrap();
    assert!(!result.was_already_forgotten);
    assert!(!workspace.join(&relative).exists());
    assert!(
        store
            .forget(&ForgetRequest {
                relative_path: relative.clone(),
                expected_content_hash,
                reason: ForgetReason::UserRequest,
                now: 21,
            })
            .unwrap()
            .was_already_forgotten
    );

    V2CaptureStore::open(&workspace, V2MemoryScope::Workspace)
        .unwrap()
        .reconcile()
        .unwrap();
    assert!(!workspace.join(&relative).exists());
    let reopened =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let status = reopened.status(21).unwrap();
    assert_eq!(status.tombstone_count, 1);
    assert_eq!(status.pending_observations, 0);
    assert!(
        !std::fs::read_to_string(workspace.join("MEMORY.md"))
            .unwrap()
            .contains("deterministic fixtures")
    );
}

#[test]
fn settled_tombstones_leave_the_manifest_alone_until_a_file_is_removed() {
    let (_temporary, global, workspace) = fixture();
    let first = capture_one_with_visibility(&workspace, "first", true);
    let second = capture_one_with_visibility(&workspace, "second", true);
    let forget_request = |relative: &Path, now: i64| ForgetRequest {
        relative_path: relative.to_path_buf(),
        expected_content_hash: blake3::hash(&std::fs::read(workspace.join(relative)).unwrap())
            .to_hex()
            .to_string(),
        reason: ForgetReason::UserRequest,
        now,
    };
    let manifest = workspace.join("MEMORY.md");
    let store = V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
        .unwrap();
    store.forget(&forget_request(&first, 20)).unwrap();

    // Reopening reconciles the settled tombstone again; nothing was removed, so the
    // manifest on disk is not rewritten.
    std::fs::write(&manifest, "sentinel").unwrap();
    let reopened =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    assert_eq!("sentinel", std::fs::read_to_string(&manifest).unwrap());

    reopened.forget(&forget_request(&second, 21)).unwrap();
    let regenerated = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        regenerated.starts_with("# Workspace memory index"),
        "{regenerated}"
    );
    assert!(!workspace.join(&second).exists());
}

#[test]
fn archive_exclusion_join_is_served_by_the_normalized_source_path_index() {
    let (_temporary, global, workspace) = fixture();
    capture_one(&workspace);
    V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace).unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    let plan: Vec<String> = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            crate::v2::COMMITTED_UNARCHIVED_FILES_SQL
        ))
        .unwrap()
        .query_map(params![1], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|step| {
            step.contains("SEARCH a USING INDEX consolidation_archives_source_path_normalized")
        }),
        "{plan:#?}"
    );
}

#[test]
fn forgetting_matches_legacy_backslash_archive_paths() {
    let (_temporary, global, workspace) = fixture();
    let observation = capture_one(&workspace);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "dreamer".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/build.md"),
                content: "# Build\n\nDeterministic fixtures.".to_owned(),
                evidence: vec![observation],
            }],
            21,
        )
        .unwrap();

    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    let archive_path = connection
        .query_row(
            "SELECT archive_path FROM consolidation_archives",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_archives SET archive_path = ?1",
            params![archive_path.replace('/', "\\")],
        )
        .unwrap();
    let hash = blake3::hash(&std::fs::read(workspace.join(&archive_path)).unwrap())
        .to_hex()
        .to_string();

    V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
        .unwrap()
        .forget(&ForgetRequest {
            relative_path: archive_path.clone().into(),
            expected_content_hash: hash,
            reason: ForgetReason::Privacy,
            now: 22,
        })
        .unwrap();
    assert!(!workspace.join(archive_path).exists());
}

#[test]
fn opening_store_reconciles_crash_window_tombstone_with_injected_clock() {
    let (_temporary, global, workspace) = fixture();
    let relative = capture_one(&workspace);
    let bytes = std::fs::read(workspace.join(&relative)).unwrap();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('crash-window', 'observation', ?1, ?2, 20, 'privacy')",
            params![relative.to_string_lossy(), hash],
        )
        .unwrap();

    V2MaintenanceStore::open_with_clock(
        &workspace,
        V2MemoryScope::Workspace,
        &global,
        &workspace,
        fixed_clock(21),
    )
    .unwrap();

    assert!(!workspace.join(&relative).exists());
    assert!(
        !std::fs::read_to_string(workspace.join("MEMORY.md"))
            .unwrap()
            .contains("deterministic fixtures")
    );
    let storage = MemoryStorage::new_flat(&workspace, &workspace);
    let index = MemoryIndex::open_or_create(
        &workspace.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(
        index
            .search_fts("deterministic fixtures", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn tampered_tombstone_is_quarantined_without_blocking_other_forgets() {
    let (_temporary, global, workspace) = fixture();
    let first = capture_one_with_visibility(&workspace, "first", true);
    let second = capture_one_with_visibility(&workspace, "second", true);
    let first_hash = blake3::hash(&std::fs::read(workspace.join(&first)).unwrap())
        .to_hex()
        .to_string();
    let second_hash = blake3::hash(&std::fs::read(workspace.join(&second)).unwrap())
        .to_hex()
        .to_string();
    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('first-forget', 'observation', ?1, ?2, 20, 'privacy')",
            params![first.to_string_lossy(), first_hash],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('second-forget', 'observation', ?1, ?2, 20, 'privacy')",
            params![second.to_string_lossy(), second_hash],
        )
        .unwrap();
    std::fs::write(workspace.join(&first), "recreated with unrelated bytes").unwrap();

    V2MaintenanceStore::open_with_clock(
        &workspace,
        V2MemoryScope::Workspace,
        &global,
        &workspace,
        fixed_clock(21),
    )
    .unwrap();

    assert!(workspace.join(&first).exists());
    assert!(!workspace.join(&second).exists());
    assert_eq!(
        connection
            .query_row(
                "SELECT reason FROM memory_v2_quarantined_paths WHERE relative_path = ?1",
                params![first.to_string_lossy()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "tombstone_mismatch"
    );
}

#[test]
fn maintenance_backfills_pre_hardening_retention_rows() {
    let (_temporary, global, workspace) = fixture();
    let observation = capture_one(&workspace);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "dreamer".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/backfilled.md"),
                content: "# Backfilled\n\nRetain this archive.".to_owned(),
                evidence: vec![observation],
            }],
            21,
        )
        .unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch("DELETE FROM v2_archive_retention; DELETE FROM v2_job_retention;")
        .unwrap();
    drop(connection);

    V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace).unwrap();
    let connection = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT archived_at FROM v2_archive_retention", [], |row| {
                row.get::<_, i64>(0)
            },)
            .unwrap(),
        21
    );
    assert_eq!(
        connection
            .query_row("SELECT terminal_at FROM v2_job_retention", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        10
    );
}

#[test]
fn status_matches_legacy_backslash_tombstones() {
    let (_temporary, global, workspace) = fixture();
    let observation = capture_one(&workspace);
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('legacy', 'observation', ?1, ?2, 11, 'privacy')",
            params![
                path_text(&observation).unwrap().replace('/', "\\"),
                "0".repeat(64)
            ],
        )
        .unwrap();

    let status =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap()
            .status(12)
            .unwrap();
    assert_eq!(status.pending_observations, 0);
    assert_eq!(status.hidden_pending_observations, 0);
}

#[test]
fn retention_gc_is_idempotent_and_refuses_active_dream() {
    let (_temporary, global, workspace) = fixture();
    let relative = capture_one(&workspace);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "dreamer".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    let maintenance = V2MaintenanceStore::open_with_clock(
        &workspace,
        V2MemoryScope::Workspace,
        &global,
        &workspace,
        fixed_clock(21),
    )
    .unwrap();
    assert!(matches!(
        maintenance.gc(
            21,
            RetentionPolicy {
                archived_observation_days: 0,
                terminal_job_days: 0,
            }
        ),
        Err(V2MaintenanceError::ActiveLease)
    ));
    dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/build.md"),
                content: "# Build\n\nDeterministic fixtures.".to_owned(),
                evidence: vec![relative],
            }],
            30,
        )
        .unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    let archive_path = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap()
        .query_row(
            "SELECT archive_path FROM consolidation_archives",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert!(workspace.join(&archive_path).is_file());
    let first = maintenance
        .gc(
            31,
            RetentionPolicy {
                archived_observation_days: 0,
                terminal_job_days: 0,
            },
        )
        .unwrap();
    assert_eq!(first.archived_observations_removed, 1);
    assert_eq!(first.terminal_jobs_removed, 1);
    assert!(!workspace.join(archive_path).exists());
    let connection = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM consolidation_archives", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap(),
        0
    );
    assert_eq!(maintenance.status(31).unwrap().archive_count, 0);
    assert_eq!(
        maintenance
            .gc(
                31,
                RetentionPolicy {
                    archived_observation_days: 0,
                    terminal_job_days: 0,
                },
            )
            .unwrap(),
        GcResult::default()
    );
}

#[test]
fn archive_gc_before_job_expiry_does_not_resurrect_pending_observations() {
    let (_temporary, global, workspace) = fixture();
    let relative = capture_one(&workspace);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "dreamer".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/build.md"),
                content: "# Build\n\nDeterministic fixtures.".to_owned(),
                evidence: vec![relative.clone()],
            }],
            30,
        )
        .unwrap();
    assert!(!workspace.join(&relative).exists());
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "UPDATE capture_observation_files SET path = ?1",
            params![relative.to_string_lossy().replace('/', "\\")],
        )
        .unwrap();

    let maintenance =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let archive_only = RetentionPolicy {
        archived_observation_days: 0,
        terminal_job_days: 365,
    };
    let first = maintenance.gc(31, archive_only).unwrap();
    assert_eq!(first.archived_observations_removed, 1);
    assert_eq!(first.terminal_jobs_removed, 0);

    let status = maintenance.status(32).unwrap();
    assert_eq!(status.pending_observations, 0);
    assert_eq!(status.hidden_pending_observations, 0);
    assert_eq!(status.completed_capture_jobs, 1);
    assert!(
        dream
            .claim(&DreamClaimRequest {
                owner: "dreamer".to_owned(),
                now: 33,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none(),
        "an expired archive must not become claimable again"
    );
    V2CaptureStore::open(&workspace, V2MemoryScope::Workspace)
        .unwrap()
        .reconcile()
        .unwrap();

    let second = maintenance
        .gc(
            34,
            RetentionPolicy {
                archived_observation_days: 0,
                terminal_job_days: 0,
            },
        )
        .unwrap();
    assert_eq!(second.terminal_jobs_removed, 1);
    assert_eq!(maintenance.status(35).unwrap().completed_capture_jobs, 0);
}

#[test]
fn retention_gc_preserves_unarchived_hidden_and_shadow_observations() {
    let (_temporary, global, workspace) = fixture();
    let hidden = capture_one_with_visibility(&workspace, "shadow-session", false);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim_shadow(&DreamClaimRequest {
            owner: "shadow-worker".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    dream
        .complete_shadow(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/shadow-retention.md"),
                content: "# Shadow retention\n\nPreserve this observation.".to_owned(),
                evidence: vec![hidden.clone()],
            }],
            21,
        )
        .unwrap();
    let maintenance =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let before = maintenance.status(22).unwrap();
    assert_eq!(before.pending_observations, 0);
    assert_eq!(before.hidden_pending_observations, 1);

    assert_eq!(
        maintenance
            .gc(
                22,
                RetentionPolicy {
                    archived_observation_days: 0,
                    terminal_job_days: 0,
                },
            )
            .unwrap(),
        GcResult::default()
    );
    assert!(workspace.join(&hidden).exists());
    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open_readonly(&state_path)
        .unwrap();
    for table in [
        "capture_jobs",
        "capture_observation_files",
        "memory_v2_hidden_observations",
        "memory_v2_shadow_evaluations",
    ] {
        let count = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap();
        assert_eq!(count, 1, "{table} was collected while still pending");
    }
    drop(connection);
    let capture = V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
    assert_eq!(capture.promote_hidden_observations(23).unwrap(), 1);
    let active = dream
        .claim(&DreamClaimRequest {
            owner: "active-worker".to_owned(),
            now: 24,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        active.observations.first().map(|o| &o.relative_path),
        Some(&hidden)
    );
    assert_eq!(
        maintenance
            .gc(
                22,
                RetentionPolicy {
                    archived_observation_days: 0,
                    terminal_job_days: 0,
                },
            )
            .unwrap_err()
            .to_string(),
        V2MaintenanceError::ActiveLease.to_string()
    );
}

#[test]
fn status_does_not_reconcile_tombstones_during_active_dream() {
    let (_temporary, global, workspace) = fixture();
    let relative = capture_one(&workspace);
    let bytes = std::fs::read(workspace.join(&relative)).unwrap();
    let content_hash = blake3::hash(&bytes).to_hex().to_string();
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "dreamer".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('pending-forget', 'observation', ?1, ?2, 21, 'privacy')",
            params![relative.to_string_lossy(), content_hash],
        )
        .unwrap();

    let maintenance = V2MaintenanceStore::open_with_clock(
        &workspace,
        V2MemoryScope::Workspace,
        &global,
        &workspace,
        fixed_clock(21),
    )
    .unwrap();
    assert_eq!(
        maintenance.status(21).unwrap().dream_lease,
        DreamLeaseState::Active
    );
    assert!(workspace.join(&relative).exists());
    assert!(matches!(
        maintenance.gc(
            21,
            RetentionPolicy {
                archived_observation_days: 0,
                terminal_job_days: 0,
            }
        ),
        Err(V2MaintenanceError::ActiveLease)
    ));
    assert!(workspace.join(&relative).exists());

    dream.fail_retryable(&lease, 22, "test release").unwrap();
    maintenance
        .gc(
            22,
            RetentionPolicy {
                archived_observation_days: 0,
                terminal_job_days: 0,
            },
        )
        .unwrap();
    assert!(!workspace.join(relative).exists());
}

#[test]
fn maintenance_access_errors_preserve_their_source() {
    use std::error::Error as _;

    let (_temporary, _global, workspace) = fixture();
    let missing_global = workspace.join("missing-global");
    let error = V2MaintenanceStore::open(
        &workspace,
        V2MemoryScope::Workspace,
        &missing_global,
        &workspace,
    )
    .unwrap_err();
    assert!(matches!(error, V2MaintenanceError::Access(_)));
    assert!(error.source().is_some());
}

#[test]
fn active_promotion_exposes_record_only_and_shadow_history_without_erasing_evaluation() {
    let (_temporary, global, workspace) = fixture();
    let record_only = capture_one_with_visibility(&workspace, "record-only", false);
    let capture = V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
    assert_eq!(capture.promote_hidden_observations(15).unwrap(), 1);
    assert_eq!(capture.promote_hidden_observations(15).unwrap(), 0);

    let shadow = capture_one_with_visibility(&workspace, "shadow", false);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim_shadow(&DreamClaimRequest {
            owner: "shadow-worker".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        lease.observations.first().map(|o| &o.relative_path),
        Some(&shadow)
    );
    dream
        .complete_shadow(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/shadow-promotion.md"),
                content: "# Shadow promotion\n\nPromote this observation.".to_owned(),
                evidence: vec![shadow.clone()],
            }],
            21,
        )
        .unwrap();
    assert_eq!(capture.promote_hidden_observations(22).unwrap(), 1);

    let state_path = workspace.join("memory_state.sqlite");
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
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM memory_v2_shadow_evaluations",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    let active = dream
        .claim(&DreamClaimRequest {
            owner: "active-worker".to_owned(),
            now: 23,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    assert_eq!(active.observations.len(), 2);
    assert!(
        active
            .observations
            .iter()
            .any(|observation| observation.relative_path == record_only)
    );
    assert!(
        active
            .observations
            .iter()
            .any(|observation| observation.relative_path == shadow)
    );
}

#[test]
fn status_buckets_retry_terminal_failure_and_visibility_disjointly() {
    let (_temporary, global, workspace) = fixture();
    capture_one_with_visibility(&workspace, "visible", true);
    capture_one_with_visibility(&workspace, "hidden", false);
    let capture = V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
    capture
        .enqueue("retry", CaptureRange::try_new(1, 1).unwrap())
        .unwrap();
    let retry = capture
        .claim_for_session(
            "retry",
            &ClaimRequest {
                owner: "retry-worker".to_owned(),
                now: 20,
                duration: Duration::from_secs(60),
            },
        )
        .unwrap()
        .unwrap();
    capture.fail_retryable(&retry, 21, "retry").unwrap();
    capture
        .enqueue("terminal", CaptureRange::try_new(1, 1).unwrap())
        .unwrap();
    let terminal = capture
        .claim_for_session(
            "terminal",
            &ClaimRequest {
                owner: "terminal-worker".to_owned(),
                now: 20,
                duration: Duration::from_secs(60),
            },
        )
        .unwrap()
        .unwrap();
    capture.fail_terminal(&terminal, 21, "terminal").unwrap();

    let status =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap()
            .status(22)
            .unwrap();
    assert_eq!(status.retry_capture_jobs, 1);
    assert_eq!(status.failed_capture_jobs, 1);
    assert_eq!(status.pending_observations, 1);
    assert_eq!(status.hidden_pending_observations, 1);
    assert!(
        capture
            .claim_for_session(
                "terminal",
                &ClaimRequest {
                    owner: "other".to_owned(),
                    now: 22,
                    duration: Duration::from_secs(60),
                },
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn forgetting_rejects_broad_protected_and_stale_requests() {
    let (_temporary, global, workspace) = fixture();
    let store = V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
        .unwrap();
    for relative_path in ["topics", "../topics/fact.md", "MEMORY.md"] {
        assert!(matches!(
            store.forget(&ForgetRequest {
                relative_path: relative_path.into(),
                expected_content_hash: "0".repeat(64),
                reason: ForgetReason::Privacy,
                now: 1,
            }),
            Err(V2MaintenanceError::Invalid(_) | V2MaintenanceError::Protected)
        ));
    }
}

#[test]
fn manifest_exclusions_support_schema_three_without_quarantine_table() {
    let (_temporary, _global, workspace) = fixture();
    V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute_batch(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version';
             DROP TABLE memory_v2_quarantined_paths;
             INSERT INTO memory_v2_tombstones(
                 tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES (
                 'schema-three-tombstone', 'topic', 'topics/forgotten.md',
                 'tombstone-hash', 1, 'obsolete'
             );
             INSERT INTO memory_v2_hidden_observations(relative_path)
             VALUES ('observations/_inbox/hidden.md');",
        )
        .unwrap();

    let excluded = crate::v2::excluded_manifest_paths(&workspace).unwrap();
    assert_eq!(excluded.len(), 2);
    assert!(excluded.contains("topics/forgotten.md"));
    assert!(excluded.contains("observations/_inbox/hidden.md"));
}

#[test]
fn manifest_integrity_scan_matches_legacy_backslash_archives() {
    let (_temporary, _global, workspace) = fixture();
    let observation = capture_one(&workspace);
    let legacy_path = observation.to_string_lossy().replace('/', "\\");
    let state_path = workspace.join("memory_state.sqlite");
    JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap()
        .execute(
            "INSERT INTO consolidation_archives(
                source_path, operation_id, archive_path, content_hash
             ) VALUES (?1, 'legacy-operation', 'archive/legacy.md', 'hash')",
            params![legacy_path],
        )
        .unwrap();
    std::fs::remove_file(workspace.join(&observation)).unwrap();

    let excluded = crate::v2::excluded_manifest_paths(&workspace).unwrap();
    assert!(
        !excluded.contains(observation.to_str().unwrap()),
        "an archived capture must not be treated as a damaged committed observation"
    );
}

#[test]
fn manifest_fails_closed_when_current_exclusion_ledger_is_partially_corrupt() {
    let (_temporary, _global, workspace) = fixture();
    V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
    let state_path = workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "4"
    );
    connection
        .execute("DROP TABLE memory_v2_hidden_observations", [])
        .unwrap();
    assert!(matches!(
        crate::render_scope_manifest(
            &workspace,
            V2MemoryScope::Workspace,
            V2ManifestBudget::default(),
        ),
        Err(crate::V2StorageError::Database { .. })
    ));
}

#[test]
fn active_lifecycle_converges_through_dream_forget_and_restart() {
    let (_temporary, global, workspace) = fixture();
    let observation = capture_one(&workspace);
    let dream =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let lease = dream
        .claim(&DreamClaimRequest {
            owner: "e2e-dream".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/build.md"),
                content: "# Build\n\nUse deterministic fixtures.".to_owned(),
                evidence: vec![observation.clone()],
            }],
            21,
        )
        .unwrap();
    assert!(!workspace.join(&observation).exists());
    let topic = workspace.join("topics/build.md");
    assert!(topic.is_file());
    assert!(
        std::fs::read_to_string(workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/build.md")
    );

    let topic_hash = blake3::hash(&std::fs::read(&topic).unwrap())
        .to_hex()
        .to_string();
    let maintenance =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    maintenance
        .forget(&ForgetRequest {
            relative_path: PathBuf::from("topics/build.md"),
            expected_content_hash: topic_hash,
            reason: ForgetReason::Incorrect,
            now: 22,
        })
        .unwrap();

    V2CaptureStore::open(&workspace, V2MemoryScope::Workspace)
        .unwrap()
        .reconcile()
        .unwrap();
    let reopened =
        V2MaintenanceStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap();
    let status = reopened.status(23).unwrap();
    assert_eq!(status.tombstone_count, 1);
    assert_eq!(status.archive_count, 1);
    assert!(!topic.exists());
    assert!(
        !std::fs::read_to_string(workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/build.md")
    );
    let storage = MemoryStorage::new_flat(&workspace, &workspace);
    let index = MemoryIndex::open_or_create(
        &workspace.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(
        index
            .search_fts("deterministic fixtures", 10)
            .unwrap()
            .is_empty()
    );
}
