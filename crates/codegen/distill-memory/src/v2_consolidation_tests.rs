// Modified for Distill by Samuel Fajreldines, 2026.
use std::error::Error as _;
use std::time::Duration;

use super::*;
use crate::{
    CaptureOutcomeDraft, CaptureRange, ClaimRequest, ObservationDraft, ObservationType,
    V2CaptureStore, ensure_scope_initialized,
};
use tempfile::TempDir;
use distill_sqlite_journal::JournalMode;

struct Fixture {
    _temp: TempDir,
    global: PathBuf,
    workspace: PathBuf,
    capture: V2CaptureStore,
    dream: V2ConsolidationStore,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("memory-v2");
        let global = root.join("global");
        let workspace = root.join("workspaces/ws");
        std::fs::create_dir_all(root.join("workspaces")).unwrap();
        ensure_scope_initialized(&root, &global, V2MemoryScope::Global).unwrap();
        ensure_scope_initialized(&root, &workspace, V2MemoryScope::Workspace).unwrap();
        let capture = V2CaptureStore::open(&workspace, V2MemoryScope::Workspace).unwrap();
        let dream =
            V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
                .unwrap();
        Self {
            _temp: temp,
            global,
            workspace,
            capture,
            dream,
        }
    }

    fn add_observation(&self, session: &str, turn: u32, statement: &str, now: i64) -> PathBuf {
        self.add_observation_with_visibility(session, turn, statement, now, true)
    }

    fn add_observation_with_visibility(
        &self,
        session: &str,
        turn: u32,
        statement: &str,
        now: i64,
        is_exposed: bool,
    ) -> PathBuf {
        self.capture
            .enqueue_with_visibility(
                session,
                CaptureRange::try_new(turn, turn).unwrap(),
                is_exposed,
            )
            .unwrap();
        let lease = self
            .capture
            .claim(&ClaimRequest {
                owner: format!("capture-{session}-{turn}"),
                now,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .unwrap();
        self.capture
            .commit(
                &lease,
                &CaptureOutcomeDraft::Observations(vec![ObservationDraft {
                    observation_type: ObservationType::Project,
                    topic_hint: Some("dream".to_owned()),
                    statement: statement.to_owned(),
                    keywords: vec!["durable".to_owned()],
                    aliases: Vec::new(),
                    extraction_model: "fake".to_owned(),
                    prompt_version: "test".to_owned(),
                    created_at: now,
                    body: None,
                }]),
                now + 1,
            )
            .unwrap()
            .files
            .first()
            .expect("committed observation file")
            .clone()
    }

    fn claim(&self, owner: &str, now: i64, duration: u64) -> ConsolidationLease {
        self.dream
            .claim(&DreamClaimRequest {
                owner: owner.to_owned(),
                now,
                duration: Duration::from_secs(duration),
            })
            .unwrap()
            .unwrap()
    }
}

#[test]
fn fixed_snapshot_excludes_arrivals_and_capture_remains_concurrent() {
    let fixture = Fixture::new();
    let first = fixture.add_observation("one", 1, "First durable fact", 10);
    let lease = fixture.claim("dream-a", 20, 60);
    let second = fixture.add_observation("two", 2, "Arrived during Dream", 21);
    assert_eq!(
        lease
            .observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![first.clone()]
    );
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/first.md"),
                content: "# First\n\nFirst durable fact.".to_owned(),
                evidence: vec![first.clone()],
            }],
            22,
        )
        .unwrap();
    assert!(!fixture.workspace.join(first).exists());
    assert!(fixture.workspace.join(&second).exists());
    let next = fixture.claim("dream-b", 23, 60);
    assert_eq!(
        next.observations.first().map(|o| &o.relative_path),
        Some(&second)
    );
}

#[test]
fn independent_handles_are_exclusive_and_generation_fences_stale_workers() {
    let fixture = Fixture::new();
    let evidence = fixture.add_observation("one", 1, "Lock fact", 10);
    let old = fixture.claim("old", 20, 5);
    let other = V2ConsolidationStore::open(
        &fixture.workspace,
        V2MemoryScope::Workspace,
        &fixture.global,
        &fixture.workspace,
    )
    .unwrap();
    assert!(matches!(
        other.claim(&DreamClaimRequest {
            owner: "blocked".to_owned(),
            now: 21,
            duration: Duration::from_secs(5),
        }),
        Err(V2ConsolidationError::Busy)
    ));
    let replacement = other
        .claim(&DreamClaimRequest {
            owner: "replacement".to_owned(),
            now: 25,
            duration: Duration::from_secs(30),
        })
        .unwrap()
        .unwrap();
    assert!(replacement.generation > old.generation);
    assert!(matches!(
        fixture.dream.commit(
            &old,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/stale.md"),
                content: "# Stale".to_owned(),
                evidence: vec![evidence],
            }],
            25,
        ),
        Err(V2ConsolidationError::StaleLease)
    ));
}

#[test]
fn create_update_rename_merge_split_and_delete_are_evidence_backed() {
    let fixture = Fixture::new();
    let evidence = fixture.add_observation("one", 1, "Topic evidence", 10);
    let lease = fixture.claim("dream", 20, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/a.md"),
                content: "# A\n\nFact A.".to_owned(),
                evidence: vec![evidence.clone()],
            }],
            21,
        )
        .unwrap();
    let a = std::fs::read_to_string(fixture.workspace.join("topics/a.md")).unwrap();
    assert_eq!(a, "# A\n\nFact A.");

    let evidence = fixture.add_observation("two", 2, "More evidence", 30);
    let lease = fixture.claim("dream", 40, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Update {
                path: PathBuf::from("topics/a.md"),
                content: "# A\n\nUpdated A.".to_owned(),
                evidence: vec![evidence],
            }],
            41,
        )
        .unwrap();

    let evidence = fixture.add_observation("three", 3, "Structural evidence", 50);
    let lease = fixture.claim("dream", 60, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Split {
                source: PathBuf::from("topics/a.md"),
                destinations: vec![
                    (PathBuf::from("topics/b.md"), "# B\n\nB.".to_owned()),
                    (PathBuf::from("topics/c.md"), "# C\n\nC.".to_owned()),
                ],
                evidence: vec![evidence],
            }],
            61,
        )
        .unwrap();
    assert!(!fixture.workspace.join("topics/a.md").exists());

    let evidence = fixture.add_observation("four", 4, "Merge evidence", 70);
    let lease = fixture.claim("dream", 80, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Merge {
                sources: vec![PathBuf::from("topics/b.md"), PathBuf::from("topics/c.md")],
                destination: PathBuf::from("topics/d.md"),
                content: "# D\n\nMerged.".to_owned(),
                evidence: vec![evidence],
            }],
            81,
        )
        .unwrap();

    let evidence = fixture.add_observation("five", 5, "Rename evidence", 90);
    let lease = fixture.claim("dream", 100, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Rename {
                from: PathBuf::from("topics/d.md"),
                to: PathBuf::from("topics/e.md"),
                content: "# E\n\nRenamed.".to_owned(),
                evidence: vec![evidence],
            }],
            101,
        )
        .unwrap();

    let evidence = fixture.add_observation("six", 6, "Delete evidence", 110);
    let lease = fixture.claim("dream", 120, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Delete {
                path: PathBuf::from("topics/e.md"),
                evidence: vec![evidence],
            }],
            121,
        )
        .unwrap();
    assert!(!fixture.workspace.join("topics/e.md").exists());
}

#[test]
fn restricted_surface_rejects_protected_arbitrary_and_unclaimed_paths() {
    let fixture = Fixture::new();
    let evidence = fixture.add_observation("one", 1, "Safe evidence", 10);
    let lease = fixture.claim("dream", 20, 60);
    for path in [
        PathBuf::from("MEMORY.md"),
        PathBuf::from("../escape.md"),
        PathBuf::from("archive/nope.md"),
        PathBuf::from("topics/no-extension"),
    ] {
        assert!(matches!(
            fixture.dream.commit(
                &lease,
                &[TopicOperation::Create {
                    path,
                    content: "# Unsafe".to_owned(),
                    evidence: vec![evidence.clone()],
                }],
                21,
            ),
            Err(V2ConsolidationError::Invalid(_)) | Err(V2ConsolidationError::Access(_))
        ));
    }
    assert!(matches!(
        fixture.dream.commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/safe.md"),
                content: "# Safe".to_owned(),
                evidence: vec![PathBuf::from("observations/_inbox/unclaimed.md")],
            }],
            21,
        ),
        Err(V2ConsolidationError::Invalid(_))
    ));
}

#[test]
fn failure_archives_nothing_retry_is_idempotent_and_outputs_converge() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Quasar index token", 10);
    let failed = fixture.claim("failed", 20, 60);
    fixture
        .dream
        .fail_retryable(&failed, 21, "fake planner failed")
        .unwrap();
    assert!(fixture.workspace.join(&observation).exists());
    assert_eq!(
        std::fs::read_dir(fixture.workspace.join("archive"))
            .unwrap()
            .count(),
        0
    );

    let lease = fixture.claim("retry", 22, 60);
    let operations = [TopicOperation::Create {
        path: PathBuf::from("topics/quasar.md"),
        content: "# Quasar\n\nQuasar index token.".to_owned(),
        evidence: vec![observation],
    }];
    let first = fixture.dream.commit(&lease, &operations, 23).unwrap();
    let retry = fixture.dream.commit(&lease, &operations, 24).unwrap();
    assert_eq!(first.status, ConsolidationStatus::Committed);
    assert_eq!(retry.status, ConsolidationStatus::Reconciled);
    let archive_count = std::fs::read_dir(
        fixture
            .workspace
            .join("archive")
            .join(first.operation_id.unwrap()),
    )
    .unwrap()
    .count();
    assert_eq!(archive_count, 1);
    let manifest = std::fs::read_to_string(fixture.workspace.join("MEMORY.md")).unwrap();
    assert!(manifest.contains("topics/quasar.md"));
    assert!(!manifest.contains("Pending observations"));
    let storage = MemoryStorage::new_flat(&fixture.workspace, &fixture.workspace);
    let index = MemoryIndex::open_or_create(
        &fixture.workspace.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    assert!(!index.search_fts("quasar", 10).unwrap().is_empty());
    fixture.capture.reconcile().unwrap();
}

#[test]
fn repeated_failed_claims_do_not_duplicate_pending_observations() {
    let fixture = Fixture::new();
    let p = fixture.add_observation("one", 1, "Pending P", 10);
    let first = fixture.claim("dream", 20, 60);
    fixture
        .dream
        .fail_retryable(&first, 21, "planner failed")
        .unwrap();
    let q = fixture.add_observation("two", 2, "Pending Q", 22);
    let second = fixture.claim("dream", 23, 60);
    assert_ne!(second.operation_id, first.operation_id);
    fixture
        .dream
        .fail_retryable(&second, 24, "planner failed again")
        .unwrap();

    // P now has claim rows in two failed operations; it must be listed once.
    let third = fixture
        .dream
        .claim(&DreamClaimRequest {
            owner: "dream".to_owned(),
            now: 25,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .expect("observations pending after failed claims remain claimable");
    let mut expected = vec![p, q];
    expected.sort();
    assert_eq!(
        third
            .observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn empty_plan_preserves_claimed_observations() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Keep this fact", 10);
    let lease = fixture.claim("dream", 20, 60);

    assert!(matches!(
        fixture.dream.commit(&lease, &[], 21),
        Err(V2ConsolidationError::Invalid(_))
    ));
    assert!(
        fixture.dream.resume_planned(&lease, 21).unwrap().is_none(),
        "rejected empty plans must not cross the durable-plan boundary"
    );
    assert!(fixture.workspace.join(&observation).is_file());
    fixture
        .dream
        .fail_retryable(&lease, 21, "empty model plan")
        .unwrap();

    // The rejected empty plan must not have reached the durable boundary,
    // otherwise the reclaim would replay it and fail forever.
    let retry = fixture.claim("dream", 22, 60);
    assert_eq!(retry.operation_id, lease.operation_id);
    assert!(fixture.dream.resume_planned(&retry, 23).unwrap().is_none());
    let result = fixture
        .dream
        .commit(
            &retry,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/kept.md"),
                content: "# Kept\n\nKeep this fact.".to_owned(),
                evidence: vec![observation.clone()],
            }],
            24,
        )
        .unwrap();
    assert_eq!(result.status, ConsolidationStatus::Committed);
    assert!(fixture.workspace.join("topics/kept.md").is_file());
    assert!(!fixture.workspace.join(observation).exists());
}

fn install_reject_archive_trigger(fixture: &Fixture) {
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_consolidation_archive
             BEFORE INSERT ON consolidation_archives
             BEGIN SELECT RAISE(ABORT, 'injected archive failure'); END;",
        )
        .unwrap();
}

fn drop_reject_archive_trigger(fixture: &Fixture) {
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_consolidation_archive")
        .unwrap();
}

#[test]
fn failed_durable_plan_is_reclaimed_after_new_captures_arrive() {
    let fixture = Fixture::new();
    let p = fixture.add_observation("one", 1, "Durable P", 10);
    let first = fixture.claim("dream", 20, 60);
    install_reject_archive_trigger(&fixture);
    assert!(matches!(
        fixture.dream.commit(
            &first,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/durable.md"),
                content: "# Durable\n\nDurable P.".to_owned(),
                evidence: vec![p.clone()],
            }],
            21,
        ),
        Err(V2ConsolidationError::Database(_))
    ));
    fixture
        .dream
        .fail_retryable(&first, 22, "archive failed")
        .unwrap();
    let q = fixture.add_observation("two", 2, "Later Q", 23);
    drop_reject_archive_trigger(&fixture);

    // The pending set is now {P, Q}, so a deterministic id would differ; the
    // durable plan for {P} must be re-leased instead of orphaned.
    let resumed = fixture.claim("dream", 24, 60);
    assert_eq!(resumed.operation_id, first.operation_id);
    assert_eq!(
        resumed
            .observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![p.clone()]
    );
    let result = fixture
        .dream
        .resume_planned(&resumed, 25)
        .unwrap()
        .expect("re-leased durable plan is resumable");
    assert_eq!(result.status, ConsolidationStatus::Reconciled);
    assert!(fixture.workspace.join("topics/durable.md").is_file());
    assert!(!fixture.workspace.join(&p).exists());
    assert_eq!(
        std::fs::read_dir(fixture.workspace.join("archive").join(&first.operation_id))
            .unwrap()
            .count(),
        1
    );
    assert!(fixture.workspace.join(&q).is_file());

    let next = fixture.claim("dream", 26, 60);
    assert_ne!(next.operation_id, first.operation_id);
    assert_eq!(
        next.observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![q]
    );
}

#[test]
fn durable_plan_with_changed_observation_is_abandoned_not_replayed() {
    let fixture = Fixture::new();
    let p = fixture.add_observation("one", 1, "Mutable P", 10);
    let first = fixture.claim("dream", 20, 60);
    install_reject_archive_trigger(&fixture);
    assert!(
        fixture
            .dream
            .commit(
                &first,
                &[TopicOperation::Create {
                    path: PathBuf::from("topics/mutable.md"),
                    content: "# Mutable\n\nMutable P.".to_owned(),
                    evidence: vec![p.clone()],
                }],
                21,
            )
            .is_err()
    );
    fixture
        .dream
        .fail_retryable(&first, 22, "archive failed")
        .unwrap();
    let q = fixture.add_observation("two", 2, "Later Q", 23);
    drop_reject_archive_trigger(&fixture);
    std::fs::write(fixture.workspace.join(&p), "edited after the plan").unwrap();

    // The archive step can never succeed for a changed observation, so the
    // plan must be abandoned instead of bricking every later claim.
    let next = fixture.claim("dream", 24, 60);
    assert_ne!(next.operation_id, first.operation_id);
    assert!(fixture.dream.resume_planned(&next, 25).unwrap().is_none());
    let mut expected = vec![p, q];
    expected.sort();
    assert_eq!(
        next.observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        expected
    );
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    let (status, plan_hash, last_error) = connection
        .query_row(
            "SELECT status, plan_hash, last_error FROM consolidation_operations
             WHERE operation_id = ?1",
            params![first.operation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert!(plan_hash.is_none());
    assert!(last_error.contains("abandoned"));
}

#[test]
fn archive_database_failure_leaves_capture_source_recoverable() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Recoverable archive", 10);
    let lease = fixture.claim("dream", 20, 60);
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_consolidation_archive
             BEFORE INSERT ON consolidation_archives
             BEGIN SELECT RAISE(ABORT, 'injected archive failure'); END;",
        )
        .unwrap();
    let revision_before = connection
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'capture_revision'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    drop(connection);
    let operations = [TopicOperation::Create {
        path: PathBuf::from("topics/recoverable.md"),
        content: "# Recoverable".to_owned(),
        evidence: vec![observation.clone()],
    }];

    assert!(matches!(
        fixture.dream.commit(&lease, &operations, 21),
        Err(V2ConsolidationError::Database(_))
    ));
    assert!(fixture.workspace.join(&observation).is_file());
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    let status = connection
        .query_row(
            "SELECT status FROM consolidation_operations WHERE operation_id = ?1",
            params![lease.operation_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let revision_after = connection
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'capture_revision'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(status, "topics_written");
    assert_eq!(revision_after, revision_before + 1);
    drop(connection);
    fixture.capture.reconcile().unwrap();

    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_consolidation_archive")
        .unwrap();
    drop(connection);
    fixture.dream.resume_planned(&lease, 22).unwrap().unwrap();
    assert!(!fixture.workspace.join(observation).exists());
}

#[test]
fn active_archived_operation_is_not_reconciled_by_another_handle() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Active archive", 10);
    let lease = fixture.claim("dream", 20, 60);
    fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/active.md"),
                content: "# Active".to_owned(),
                evidence: vec![observation],
            }],
            21,
        )
        .unwrap();
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_operations SET status = 'archived'
             WHERE operation_id = ?1",
            params![lease.operation_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_lock SET owner = ?1, expires_at = ?2, generation = ?3
             WHERE singleton = 1",
            params![lease.owner, i64::MAX, lease.generation],
        )
        .unwrap();
    drop(connection);

    assert!(fixture.dream.reconcile().unwrap().is_empty());
    fixture
        .dream
        .finish_operation(&lease, 22, ConsolidationStatus::Reconciled)
        .unwrap();
}

#[test]
fn topic_prompt_rejects_excess_file_count() {
    let fixture = Fixture::new();
    let _observation = fixture.add_observation("one", 1, "Bounded topics", 10);
    let lease = fixture.claim("dream", 20, 60);
    for index in 0..=MAX_TOPIC_INPUT_COUNT {
        std::fs::write(
            fixture
                .workspace
                .join("topics")
                .join(format!("{index:03}.md")),
            "# Topic",
        )
        .unwrap();
    }

    assert!(matches!(
        fixture.dream.consolidation_input(&lease, 21),
        Err(V2ConsolidationError::Invalid(_))
    ));
}

#[test]
fn topic_prompt_rejects_excess_total_bytes() {
    let fixture = Fixture::new();
    let _observation = fixture.add_observation("one", 1, "Bounded topic bytes", 10);
    let lease = fixture.claim("dream", 20, 60);
    for index in 0..=MAX_TOPIC_INPUT_BYTES / MAX_TOPIC_BYTES {
        std::fs::write(
            fixture
                .workspace
                .join("topics")
                .join(format!("{index:03}.md")),
            vec![b'x'; MAX_TOPIC_BYTES],
        )
        .unwrap();
    }

    assert!(matches!(
        fixture.dream.consolidation_input(&lease, 21),
        Err(V2ConsolidationError::Invalid(_))
    ));
}

#[test]
fn dream_filters_match_legacy_backslash_paths() {
    let fixture = Fixture::new();
    let tombstoned = fixture.add_observation("tombstoned", 1, "Forgotten fact", 10);
    let hidden = fixture.add_observation_with_visibility("hidden", 1, "Hidden fact", 10, false);
    let quarantined = fixture.add_observation("quarantined", 1, "Quarantined fact", 10);
    let archived = fixture.add_observation("archived", 1, "Archived fact", 10);
    let legacy = |path: &Path| path_text(path).unwrap().replace('/', "\\");
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('legacy-tombstone', 'observation', ?1, ?2, 12, 'privacy')",
            params![legacy(&tombstoned), "0".repeat(64)],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE memory_v2_hidden_observations SET relative_path = ?1
             WHERE REPLACE(relative_path, char(92), '/') = ?2",
            params![legacy(&hidden), path_text(&hidden).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_shadow_evaluations(source_path, operation_id, evaluated_at)
             VALUES (?1, 'legacy-shadow', 12)",
            params![legacy(&hidden)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_quarantined_paths(
                relative_path, reason, expected_hash, observed_hash, quarantined_at
             ) VALUES (?1, 'committed_hash_mismatch', NULL, NULL, 12)",
            params![legacy(&quarantined)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO consolidation_archives(
                source_path, operation_id, archive_path, content_hash
             ) VALUES (?1, 'legacy-archive', 'archive/legacy.md', ?2)",
            params![legacy(&archived), "0".repeat(64)],
        )
        .unwrap();
    drop(connection);

    assert!(
        fixture
            .dream
            .claim(&DreamClaimRequest {
                owner: "active-worker".to_owned(),
                now: 20,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .dream
            .claim_shadow(&DreamClaimRequest {
                owner: "shadow-worker".to_owned(),
                now: 20,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(
                20,
                DreamEligibilityConfig {
                    min_pending_count: 1,
                    max_pending_age: Duration::from_secs(1),
                },
            )
            .unwrap()
            .pending_count,
        0
    );
}

#[test]
fn capture_events_use_count_or_age_and_coalesce_while_active() {
    let fixture = Fixture::new();
    fixture.add_observation("one", 1, "Old pending fact", 10);
    let count_config = DreamEligibilityConfig {
        min_pending_count: 2,
        max_pending_age: Duration::from_secs(100),
    };
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(20, count_config)
            .unwrap(),
        DreamEligibility {
            disposition: DreamTriggerDisposition::Ineligible,
            pending_count: 1,
            oldest_pending_at: Some(11),
        }
    );
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(111, count_config)
            .unwrap()
            .disposition,
        DreamTriggerDisposition::Ready
    );
    fixture.add_observation("two", 2, "Count-triggering fact", 112);
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(114, count_config)
            .unwrap()
            .disposition,
        DreamTriggerDisposition::Ready
    );
    let _lease = fixture.claim("active", 115, 60);
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(116, count_config)
            .unwrap()
            .disposition,
        DreamTriggerDisposition::Coalesced
    );
    assert!(fixture.dream.has_coalesced_trigger().unwrap());
    fixture
        .dream
        .fail_retryable(&_lease, 117, "retry after coalescing")
        .unwrap();
    assert_eq!(
        fixture
            .dream
            .on_capture_completed(118, count_config)
            .unwrap()
            .disposition,
        DreamTriggerDisposition::Ready
    );
    assert!(!fixture.dream.has_coalesced_trigger().unwrap());
}

#[test]
fn expired_worker_resumes_durable_plan_before_claiming_new_arrivals() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Recoverable plan", 10);
    let old = fixture.claim("old", 20, 5);
    let content = format!(
        "# Recovered\n\nRecoverable plan.\n\n<!-- memory-v2 provenance: operation={}; evidence={} -->",
        old.operation_id,
        observation.display()
    );
    let connection = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_operations SET status = 'planned', plan_hash = 'durable'
             WHERE operation_id = ?1",
            params![old.operation_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO consolidation_topic_changes(operation_id, path, content_hash, content)
             VALUES (?1, 'topics/recovered.md', ?2, ?3)",
            params![
                old.operation_id,
                blake3::hash(content.as_bytes()).to_hex().to_string(),
                content
            ],
        )
        .unwrap();
    drop(connection);
    fixture
        .dream
        .fail_retryable(&old, 21, "retry durable plan")
        .unwrap();

    let later = fixture.add_observation("two", 2, "Later unplanned fact", 22);
    let replacement = fixture.claim("replacement", 25, 60);
    assert_eq!(replacement.operation_id, old.operation_id);
    assert_eq!(replacement.generation, old.generation + 1);
    assert_eq!(
        replacement
            .observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![observation.clone()]
    );
    assert!(matches!(
        fixture.dream.resume_planned(&old, 26),
        Err(V2ConsolidationError::StaleLease)
    ));
    let result = fixture
        .dream
        .resume_planned(&replacement, 26)
        .unwrap()
        .unwrap();
    assert_eq!(result.status, ConsolidationStatus::Reconciled);
    assert!(fixture.workspace.join("topics/recovered.md").is_file());
    assert!(!fixture.workspace.join(observation).exists());
    assert!(fixture.workspace.join(&later).is_file());

    let next = fixture.claim("next", 27, 60);
    assert_ne!(next.operation_id, old.operation_id);
    assert_eq!(
        next.observations.first().map(|o| &o.relative_path),
        Some(&later)
    );
}

#[test]
fn expired_worker_abandons_plan_with_tombstoned_topic_destination() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Forgotten destination plan", 10);
    let old = fixture.claim("old", 20, 5);
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_operations SET status = 'planned', plan_hash = 'durable'
             WHERE operation_id = ?1",
            params![old.operation_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO consolidation_topic_changes(operation_id, path, content_hash, content)
             VALUES (?1, 'topics/forgotten.md', ?2, '# Forgotten')",
            params![
                old.operation_id,
                blake3::hash(b"# Forgotten").to_hex().to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES (
                'forgotten-topic', 'topic', 'topics/forgotten.md',
                'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                24, 'privacy'
             )",
            [],
        )
        .unwrap();
    let later = fixture.add_observation("two", 2, "Later claim remains reachable", 24);

    let replacement = fixture.claim("replacement", 25, 60);
    assert_ne!(replacement.operation_id, old.operation_id);
    assert!(
        fixture
            .dream
            .resume_planned(&replacement, 26)
            .unwrap()
            .is_none()
    );
    let mut expected = vec![observation, later];
    expected.sort();
    assert_eq!(
        replacement
            .observations
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT plan_hash FROM consolidation_operations WHERE operation_id = ?1",
                params![old.operation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
}

#[test]
fn expired_worker_abandons_plan_with_tombstoned_observation() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Forgotten evidence plan", 10);
    let old = fixture.claim("old", 20, 5);
    let state_path = fixture.workspace.join("memory_state.sqlite");
    let connection = JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_operations SET status = 'planned', plan_hash = 'durable'
             WHERE operation_id = ?1",
            params![old.operation_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO consolidation_topic_changes(operation_id, path, content_hash, content)
             VALUES (?1, 'topics/recovered.md', ?2, '# Recovered')",
            params![
                old.operation_id,
                blake3::hash(b"# Recovered").to_hex().to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('forgotten-observation', 'observation', ?1, ?2, 24, 'privacy')",
            params![path_text(&observation).unwrap(), "0".repeat(64)],
        )
        .unwrap();

    assert!(
        fixture
            .dream
            .claim(&DreamClaimRequest {
                owner: "replacement".to_owned(),
                now: 25,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT plan_hash FROM consolidation_operations WHERE operation_id = ?1",
                params![old.operation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
    assert!(fixture.workspace.join(observation).is_file());
}

#[test]
fn shadow_plan_is_validated_without_topics_or_archival() {
    let fixture = Fixture::new();
    let observation =
        fixture.add_observation_with_visibility("shadow", 1, "Shadow-only fact", 10, false);
    let lease = fixture
        .dream
        .claim_shadow(&DreamClaimRequest {
            owner: "shadow-worker".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();
    let result = fixture
        .dream
        .complete_shadow(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/shadow.md"),
                content: "# Shadow\n\nShadow-only fact.".to_owned(),
                evidence: vec![observation.clone()],
            }],
            21,
        )
        .unwrap();
    assert_eq!(result.status, ConsolidationStatus::Shadow);
    assert_eq!(
        result.affected_topics,
        vec![PathBuf::from("topics/shadow.md")]
    );
    assert!(fixture.workspace.join(&observation).is_file());
    assert!(!fixture.workspace.join("topics/shadow.md").exists());
    assert!(
        fixture
            .dream
            .claim_shadow(&DreamClaimRequest {
                owner: "second-shadow-worker".to_owned(),
                now: 22,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.capture.promote_hidden_observations(23).unwrap(), 1);
    assert_eq!(
        fixture
            .claim("active-worker", 23, 60)
            .observations
            .first()
            .map(|o| &o.relative_path),
        Some(&observation)
    );
}

#[test]
fn empty_shadow_plan_is_retryable_without_consuming_observations() {
    let fixture = Fixture::new();
    let observation =
        fixture.add_observation_with_visibility("shadow-empty", 1, "Retry empty plan", 10, false);
    let lease = fixture
        .dream
        .claim_shadow(&DreamClaimRequest {
            owner: "empty-shadow-worker".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();

    let error = fixture.dream.complete_shadow(&lease, &[], 21).unwrap_err();
    assert!(matches!(error, V2ConsolidationError::Invalid(_)));
    let state = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open_readonly(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    assert_eq!(
        state
            .query_row(
                "SELECT COUNT(*) FROM memory_v2_shadow_evaluations",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        0
    );
    drop(state);

    let replacement = fixture
        .dream
        .claim_shadow(&DreamClaimRequest {
            owner: "replacement-shadow-worker".to_owned(),
            now: 22,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .expect("an empty shadow plan must release its observation for retry");
    assert_eq!(
        replacement.observations.first().map(|o| &o.relative_path),
        Some(&observation)
    );
}

#[test]
fn rejected_shadow_plan_is_retryable_and_releases_lease() {
    let fixture = Fixture::new();
    let observation =
        fixture.add_observation_with_visibility("shadow-retry", 1, "Retry this fact", 10, false);
    let lease = fixture
        .dream
        .claim_shadow(&DreamClaimRequest {
            owner: "first-shadow-worker".to_owned(),
            now: 20,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .unwrap();

    let error = fixture
        .dream
        .complete_shadow(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/retry.md"),
                content: "# Retry\n\nRetry this fact.".to_owned(),
                evidence: Vec::new(),
            }],
            21,
        )
        .unwrap_err();
    assert!(matches!(error, V2ConsolidationError::Invalid(_)));

    let state = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    let (status, last_error) = state
        .query_row(
            "SELECT status, last_error FROM consolidation_operations WHERE operation_id = ?1",
            params![lease.operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert!(last_error.contains("evidence"));
    drop(state);

    let replacement = fixture
        .dream
        .claim_shadow(&DreamClaimRequest {
            owner: "replacement-shadow-worker".to_owned(),
            now: 22,
            duration: Duration::from_secs(60),
        })
        .unwrap()
        .expect("failed shadow plan must release its lease for retry");
    assert_eq!(
        replacement.observations.first().map(|o| &o.relative_path),
        Some(&observation)
    );
}

#[test]
fn retryable_failure_sanitizes_diagnostic_and_releases_lease() {
    let fixture = Fixture::new();
    fixture.add_observation("retry-diagnostic", 1, "Retry this fact", 10);
    let lease = fixture.claim("first-worker", 20, 60);
    let diagnostic = format!("\n{}\0", "é".repeat(MAX_FAILURE_BYTES));

    fixture
        .dream
        .fail_retryable(&lease, 21, &diagnostic)
        .unwrap();

    let state = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    let (status, last_error) = state
        .query_row(
            "SELECT status, last_error
             FROM consolidation_operations
             WHERE operation_id = ?1",
            params![lease.operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert!(!last_error.is_empty());
    assert!(last_error.len() <= MAX_FAILURE_BYTES);
    assert!(!last_error.chars().any(char::is_control));
    drop(state);

    assert!(
        fixture
            .dream
            .claim(&DreamClaimRequest {
                owner: "replacement-worker".to_owned(),
                now: 22,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_some()
    );
}

#[test]
fn explicitly_forgotten_topic_path_cannot_be_recreated() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Do not resurrect", 10);
    let state = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    state
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('forgotten', 'topic', 'topics/forgotten.md', ?1, 15, 'user_request')",
            params!["0".repeat(64)],
        )
        .unwrap();
    let lease = fixture.claim("dream", 20, 60);
    assert!(matches!(
        fixture.dream.commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/forgotten.md"),
                content: "# Forgotten\n\nDo not resurrect.".to_owned(),
                evidence: vec![observation],
            }],
            21,
        ),
        Err(V2ConsolidationError::Conflict(_))
    ));
    assert!(!fixture.workspace.join("topics/forgotten.md").exists());
}

#[test]
fn archived_operation_reconciles_index_and_manifest_idempotently() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Boundary quasar", 10);
    let lease = fixture.claim("dream", 20, 60);
    let result = fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/boundary.md"),
                content: "# Boundary\n\nBoundary quasar.".to_owned(),
                evidence: vec![observation],
            }],
            21,
        )
        .unwrap();
    let operation_id = result.operation_id.unwrap();
    let connection = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    connection
        .execute(
            "UPDATE consolidation_operations SET status = 'archived'
             WHERE operation_id = ?1",
            params![operation_id],
        )
        .unwrap();
    drop(connection);
    let storage = MemoryStorage::new_flat(&fixture.workspace, &fixture.workspace);
    let mut index = MemoryIndex::open_or_create(
        &fixture.workspace.join("index.sqlite"),
        storage,
        distill_config_types::MemoryIndexConfig::default(),
        1,
    )
    .unwrap();
    index
        .delete_path(&fixture.workspace.join("topics/boundary.md"))
        .unwrap();
    std::fs::write(fixture.workspace.join("MEMORY.md"), "stale").unwrap();

    assert_eq!(fixture.dream.reconcile().unwrap(), vec![operation_id]);
    assert!(fixture.dream.reconcile().unwrap().is_empty());
    assert!(!index.search_fts("boundary quasar", 10).unwrap().is_empty());
    assert!(
        std::fs::read_to_string(fixture.workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/boundary.md")
    );
}

#[test]
fn post_archive_convergence_failure_remains_recoverable_on_next_claim() {
    let fixture = Fixture::new();
    let observation = fixture.add_observation("one", 1, "Recover after convergence", 10);
    let lease = fixture.claim("dream", 20, 60);
    std::fs::remove_file(fixture.workspace.join("index.sqlite")).unwrap();
    std::fs::create_dir(fixture.workspace.join("index.sqlite")).unwrap();

    let error = fixture
        .dream
        .commit(
            &lease,
            &[TopicOperation::Create {
                path: PathBuf::from("topics/recovered.md"),
                content: "# Recovered\n\nRecover after convergence.".to_owned(),
                evidence: vec![observation.clone()],
            }],
            21,
        )
        .unwrap_err();
    assert!(matches!(&error, V2ConsolidationError::Convergence(_)));
    assert!(error.source().unwrap().is::<rusqlite::Error>());
    fixture
        .dream
        .fail_retryable(&lease, 21, "index convergence failed")
        .unwrap();
    assert!(!fixture.workspace.join(&observation).exists());
    let connection = JournalMode::for_db_path(&fixture.workspace.join("memory_state.sqlite"))
        .open(&fixture.workspace.join("memory_state.sqlite"))
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT status FROM consolidation_operations WHERE operation_id = ?1",
                params![lease.operation_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "archived"
    );
    drop(connection);

    std::fs::remove_dir(fixture.workspace.join("index.sqlite")).unwrap();
    let reopened = V2ConsolidationStore::open(
        &fixture.workspace,
        V2MemoryScope::Workspace,
        &fixture.global,
        &fixture.workspace,
    )
    .unwrap();
    assert!(
        reopened
            .claim(&DreamClaimRequest {
                owner: "next".to_owned(),
                now: 22,
                duration: Duration::from_secs(60),
            })
            .unwrap()
            .is_none()
    );
    assert!(
        std::fs::read_to_string(fixture.workspace.join("MEMORY.md"))
            .unwrap()
            .contains("topics/recovered.md")
    );
}

#[test]
fn access_errors_retain_the_typed_source() {
    let temp = TempDir::new().unwrap();
    let global = temp.path().join("missing-global");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let error =
        V2ConsolidationStore::open(&workspace, V2MemoryScope::Workspace, &global, &workspace)
            .unwrap_err();
    assert!(matches!(&error, V2ConsolidationError::Access(_)));
    assert!(error.source().unwrap().is::<crate::V2AccessError>());
}

#[test]
fn structural_operations_do_not_overwrite_unrelated_destinations() {
    let fixture = Fixture::new();
    let evidence = fixture.add_observation("one", 1, "Structural collision", 10);
    for topic in ["a.md", "b.md", "c.md"] {
        std::fs::write(
            fixture.workspace.join("topics").join(topic),
            format!("# {topic}"),
        )
        .unwrap();
    }
    let lease = fixture.claim("dream", 20, 60);

    for operation in [
        TopicOperation::Rename {
            from: PathBuf::from("topics/a.md"),
            to: PathBuf::from("topics/b.md"),
            content: "# Renamed".to_owned(),
            evidence: vec![evidence.clone()],
        },
        TopicOperation::Merge {
            sources: vec![PathBuf::from("topics/a.md"), PathBuf::from("topics/c.md")],
            destination: PathBuf::from("topics/b.md"),
            content: "# Merged".to_owned(),
            evidence: vec![evidence.clone()],
        },
        TopicOperation::Split {
            source: PathBuf::from("topics/a.md"),
            destinations: vec![
                (PathBuf::from("topics/b.md"), "# B split".to_owned()),
                (PathBuf::from("topics/new.md"), "# New".to_owned()),
            ],
            evidence: vec![evidence.clone()],
        },
    ] {
        assert!(matches!(
            fixture.dream.prepare_changes(&lease, &[operation]),
            Err(V2ConsolidationError::Conflict(_))
        ));
    }

    assert!(
        fixture
            .dream
            .prepare_changes(
                &lease,
                &[TopicOperation::Rename {
                    from: PathBuf::from("topics/a.md"),
                    to: PathBuf::from("topics/a.md"),
                    content: "# Rewritten".to_owned(),
                    evidence: vec![evidence.clone()],
                }],
            )
            .is_ok()
    );
    assert!(
        fixture
            .dream
            .prepare_changes(
                &lease,
                &[TopicOperation::Merge {
                    sources: vec![PathBuf::from("topics/a.md"), PathBuf::from("topics/c.md")],
                    destination: PathBuf::from("topics/a.md"),
                    content: "# Rewritten merge".to_owned(),
                    evidence: vec![evidence.clone()],
                }],
            )
            .is_ok()
    );
    assert!(
        fixture
            .dream
            .prepare_changes(
                &lease,
                &[TopicOperation::Split {
                    source: PathBuf::from("topics/a.md"),
                    destinations: vec![
                        (PathBuf::from("topics/a.md"), "# Rewritten split".to_owned()),
                        (PathBuf::from("topics/new.md"), "# New".to_owned()),
                    ],
                    evidence: vec![evidence],
                }],
            )
            .is_ok()
    );
}

#[test]
fn topic_content_size_limit_is_enforced() {
    let at_limit = "x".repeat(MAX_TOPIC_BYTES);
    assert_eq!(
        normalize_topic_content(&at_limit).unwrap().len(),
        MAX_TOPIC_BYTES
    );
    assert!(matches!(
        normalize_topic_content(&format!("{at_limit}x")),
        Err(V2ConsolidationError::Invalid(_))
    ));
    assert!(matches!(
        normalize_topic_content("  \n"),
        Err(V2ConsolidationError::Invalid(_))
    ));
}

#[test]
fn legacy_provenance_comments_are_stripped_from_topic_content() {
    let content = "# Topic\nSummary.\n\n<!-- memory-v2 provenance: operation=old; evidence=a.md -->\n\
                   <!-- memory-v2 provenance: operation=older; evidence=b.md -->\n";
    assert_eq!(
        normalize_topic_content(content).unwrap(),
        "# Topic\nSummary."
    );
}
