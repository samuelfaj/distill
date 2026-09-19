// Modified for Distill by Samuel Fajreldines, 2026.
//! Content-free status, durable forgetting, and bounded retention for memory v2.
//!
//! `memory_state.sqlite` is the serialization point for every operation in this
//! module. Tombstones are committed before files are removed, so all readers
//! must treat the ledger as authoritative even during crash reconciliation.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use distill_tools::types::memory_v2::MemoryV2Access as _;
use distill_sqlite_journal::JournalMode;

use crate::storage::MemoryStorage;
use crate::v2::{V2ManifestBudget, V2MemoryScope, regenerate_scope_manifest};
use crate::{MemoryIndex, SharedV2Clock, V2MemoryAccessPolicy, V2PathClass, system_v2_clock};

/// Largest file `forget` will hash and remove; clients should not offer deletion above it.
pub const MAX_FORGET_FILE_BYTES: u64 = 256 * 1024;
const MAX_GC_ITEMS_PER_CLASS: i64 = 256;
const SECONDS_PER_DAY: i64 = 86_400;

pub type Result<T> = std::result::Result<T, V2MaintenanceError>;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AccessSnapshotError(String);

/// What reconciling one tombstone did to its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TombstoneReconciliation {
    /// The file was still present and has now been unlinked.
    Removed,
    /// Nothing on disk to remove; the tombstone was settled earlier.
    AlreadyGone,
}

#[derive(Debug, thiserror::Error)]
pub enum V2MaintenanceError {
    #[error("invalid memory-v2 maintenance request: {0}")]
    Invalid(&'static str),
    #[error("memory-v2 target is unknown or no longer matches its evidence")]
    EvidenceMismatch,
    #[error("memory-v2 target is protected")]
    Protected,
    #[error("memory-v2 maintenance rejected while an active Dream lease exists")]
    ActiveLease,
    #[error("memory-v2 maintenance database failed")]
    Database(#[from] rusqlite::Error),
    #[error("memory-v2 maintenance filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("memory-v2 maintenance access policy rejected the operation")]
    Access(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("memory-v2 maintenance index convergence failed")]
    Index(#[source] rusqlite::Error),
    #[error("memory-v2 maintenance manifest convergence failed")]
    Manifest(#[source] crate::V2StorageError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetReason {
    UserRequest,
    Privacy,
    Incorrect,
    Obsolete,
}

impl ForgetReason {
    /// xai-codegen-lint: allow(manual_strum)
    fn as_str(self) -> &'static str {
        match self {
            Self::UserRequest => "user_request",
            Self::Privacy => "privacy",
            Self::Incorrect => "incorrect",
            Self::Obsolete => "obsolete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetRequest {
    /// Exact path relative to one v2 scope, such as `topics/editor.md`.
    pub relative_path: PathBuf,
    /// BLAKE3 of the bytes the caller deliberately inspected.
    pub expected_content_hash: String,
    pub reason: ForgetReason,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetResult {
    pub tombstone_id: String,
    pub was_already_forgotten: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub archived_observation_days: u64,
    pub terminal_job_days: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcResult {
    pub archived_observations_removed: u64,
    pub terminal_jobs_removed: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DreamLeaseState {
    #[default]
    None,
    Active,
    Expired,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct V2ScopeStatus {
    pub pending_capture_jobs: u64,
    pub running_capture_jobs: u64,
    pub retry_capture_jobs: u64,
    pub failed_capture_jobs: u64,
    pub completed_capture_jobs: u64,
    /// Exposed observations eligible for active consolidation.
    pub pending_observations: u64,
    /// Non-exposed observations retained by record-only or shadow rollout.
    pub hidden_pending_observations: u64,
    pub oldest_pending_age_secs: Option<u64>,
    pub dream_lease: DreamLeaseState,
    pub dream_snapshot_size: u64,
    pub has_coalesced_trigger: bool,
    pub last_success_at: Option<i64>,
    pub last_failure_at: Option<i64>,
    pub archive_count: u64,
    pub tombstone_count: u64,
}

#[derive(Debug)]
pub struct V2MaintenanceStore {
    scope_dir: PathBuf,
    scope: V2MemoryScope,
    state_path: PathBuf,
    access: V2MemoryAccessPolicy,
    clock: SharedV2Clock,
}

impl V2MaintenanceStore {
    pub fn open(
        scope_dir: impl AsRef<Path>,
        scope: V2MemoryScope,
        global_root: &Path,
        workspace_root: &Path,
    ) -> Result<Self> {
        Self::open_with_clock(
            scope_dir,
            scope,
            global_root,
            workspace_root,
            system_v2_clock(),
        )
    }

    pub fn open_with_clock(
        scope_dir: impl AsRef<Path>,
        scope: V2MemoryScope,
        global_root: &Path,
        workspace_root: &Path,
        clock: SharedV2Clock,
    ) -> Result<Self> {
        let scope_dir = scope_dir.as_ref().to_path_buf();
        let access = V2MemoryAccessPolicy::new(global_root, workspace_root)
            .map_err(|source| V2MaintenanceError::Access(Box::new(source)))?;
        let store = Self {
            state_path: scope_dir.join("memory_state.sqlite"),
            scope_dir,
            scope,
            access,
            clock,
        };
        let mut connection = store.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        migrate_v2_hardening(&transaction)?;
        transaction.commit()?;
        match store.reconcile_tombstones() {
            Ok(()) | Err(V2MaintenanceError::ActiveLease) => {}
            Err(error) => return Err(error),
        }
        Ok(store)
    }

    pub fn status(&self, now: i64) -> Result<V2ScopeStatus> {
        validate_timestamp(now)?;
        let connection = self.open_state()?;
        let (pending, running, retry, failed, completed) = connection
            .query_row(
                "SELECT
                SUM(status = 'pending'),
                SUM(status = 'running'),
                SUM(status = 'failed' AND NOT EXISTS (
                    SELECT 1 FROM memory_v2_terminal_capture_failures t
                    WHERE t.job_id = capture_jobs.job_id
                )),
                SUM(status = 'failed' AND EXISTS (
                    SELECT 1 FROM memory_v2_terminal_capture_failures t
                    WHERE t.job_id = capture_jobs.job_id
                )),
                SUM(status = 'completed')
             FROM capture_jobs",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                        row.get::<_, Option<u64>>(2)?.unwrap_or(0),
                        row.get::<_, Option<u64>>(3)?.unwrap_or(0),
                        row.get::<_, Option<u64>>(4)?.unwrap_or(0),
                    ))
                },
            )
            .unwrap_or_default();
        let (pending_observations, hidden_pending_observations, oldest_pending_at) = connection
            .query_row(
                "SELECT
                    SUM(h.relative_path IS NULL),
                    SUM(h.relative_path IS NOT NULL),
                    MIN(CASE WHEN h.relative_path IS NULL THEN o.committed_at END)
                 FROM capture_observation_files f
                 JOIN capture_outcomes o USING(job_id)
                 LEFT JOIN consolidation_archives a
                   ON REPLACE(a.source_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_tombstones t
                   ON REPLACE(t.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_hidden_observations h
                   ON REPLACE(h.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_quarantined_paths q
                   ON REPLACE(q.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 WHERE a.source_path IS NULL AND t.relative_path IS NULL
                   AND q.relative_path IS NULL",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .unwrap_or((0, 0, None));
        let lock = connection
            .query_row(
                "SELECT owner IS NOT NULL, expires_at, generation
                 FROM consolidation_lock WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((false, None, 0));
        let dream_lease = if !lock.0 {
            DreamLeaseState::None
        } else if lock.1.is_some_and(|expires| expires > now) {
            DreamLeaseState::Active
        } else {
            DreamLeaseState::Expired
        };
        let dream_snapshot_size = connection
            .query_row(
                "SELECT COUNT(*) FROM consolidation_claim_items ci
                 JOIN consolidation_operations co USING(operation_id)
                 WHERE co.generation = ?1",
                params![lock.2],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let last_success_at = connection
            .query_row(
                "SELECT MAX(value) FROM (
                    SELECT committed_at AS value FROM capture_outcomes
                    UNION ALL
                    SELECT completed_at AS value FROM consolidation_operations
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap_or(None);
        let last_failure_at = connection
            .query_row(
                "SELECT MAX(terminal_at) FROM v2_job_retention r
                 JOIN capture_jobs j USING(job_id)
                 JOIN memory_v2_terminal_capture_failures t USING(job_id)
                 WHERE j.status = 'failed'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(None);
        Ok(V2ScopeStatus {
            pending_capture_jobs: pending,
            running_capture_jobs: running,
            retry_capture_jobs: retry,
            failed_capture_jobs: failed,
            completed_capture_jobs: completed,
            pending_observations,
            hidden_pending_observations,
            oldest_pending_age_secs: oldest_pending_at
                .map(|oldest| u64::try_from(now.saturating_sub(oldest)).unwrap_or(0)),
            dream_lease,
            dream_snapshot_size,
            has_coalesced_trigger: connection
                .query_row(
                    "SELECT value = '1' FROM meta WHERE key = 'v2_dream_trigger_pending'",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false),
            last_success_at,
            last_failure_at,
            archive_count: connection
                .query_row("SELECT COUNT(*) FROM consolidation_archives", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0),
            tombstone_count: connection.query_row(
                "SELECT COUNT(*) FROM memory_v2_tombstones",
                [],
                |row| row.get(0),
            )?,
        })
    }

    /// Forget one exact, previously inspected v2 topic or observation.
    pub fn forget(&self, request: &ForgetRequest) -> Result<ForgetResult> {
        validate_timestamp(request.now)?;
        validate_relative_target(&request.relative_path)?;
        self.reconcile_tombstones()?;
        if request.expected_content_hash.len() != 64
            || !request
                .expected_content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(V2MaintenanceError::Invalid(
                "expected hash must be 64 hexadecimal bytes",
            ));
        }
        let absolute = self.scope_dir.join(&request.relative_path);
        let class = self
            .access
            .classify_path(&absolute)
            .map_err(|source| V2MaintenanceError::Access(Box::new(source)))?;
        if !matches!(
            class,
            V2PathClass::Topic(scope)
                | V2PathClass::Observation(scope)
                | V2PathClass::ArchivedObservation(scope)
                if scope == self.scope
        ) {
            return Err(V2MaintenanceError::Protected);
        }
        let target_kind = if matches!(class, V2PathClass::Topic(_)) {
            "topic"
        } else {
            "observation"
        };
        let relative = path_text(&request.relative_path)?;
        let tombstone_id =
            deterministic_tombstone_id(target_kind, &relative, &request.expected_content_hash);
        if let Some(existing_hash) = self
            .open_state()?
            .query_row(
                "SELECT provenance_hash FROM memory_v2_tombstones WHERE relative_path = ?1",
                params![relative],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if existing_hash != request.expected_content_hash {
                return Err(V2MaintenanceError::EvidenceMismatch);
            }
            return Ok(ForgetResult {
                tombstone_id,
                was_already_forgotten: true,
            });
        }
        if matches!(class, V2PathClass::ArchivedObservation(_)) {
            let is_known = self.open_state()?.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM consolidation_archives
                    WHERE REPLACE(archive_path, char(92), '/')
                        = REPLACE(?1, char(92), '/')
                 )",
                params![path_text(&request.relative_path)?],
                |row| row.get::<_, bool>(0),
            )?;
            if !is_known {
                return Err(V2MaintenanceError::EvidenceMismatch);
            }
        }
        let bytes = read_regular_bounded(&absolute)?;
        if blake3::hash(&bytes).to_hex().as_str() != request.expected_content_hash {
            return Err(V2MaintenanceError::EvidenceMismatch);
        }
        self.access
            .record_read(&absolute, &bytes)
            .map_err(|source| V2MaintenanceError::Access(Box::new(AccessSnapshotError(source))))?;
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_active_lease = transaction
            .query_row(
                "SELECT owner IS NOT NULL AND expires_at > ?1
                 FROM consolidation_lock WHERE singleton = 1",
                params![request.now],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if has_active_lease {
            return Err(V2MaintenanceError::ActiveLease);
        }
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                tombstone_id,
                target_kind,
                relative,
                request.expected_content_hash,
                request.now,
                request.reason.as_str()
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO memory_v2_audit(
                event_id, action, target_kind, target_id, created_at, reason
             ) VALUES (?1, 'forget', ?2, ?1, ?3, ?4)",
            params![
                tombstone_id,
                target_kind,
                request.now,
                request.reason.as_str()
            ],
        )?;
        transaction.commit()?;
        self.reconcile_tombstones()?;
        Ok(ForgetResult {
            tombstone_id,
            was_already_forgotten: inserted == 0,
        })
    }

    /// Delete expired archives and terminal job metadata in one IMMEDIATE transaction.
    pub fn gc_now(&self, policy: RetentionPolicy) -> Result<GcResult> {
        self.gc(self.clock.now_unix_seconds(), policy)
    }

    pub fn gc(&self, now: i64, policy: RetentionPolicy) -> Result<GcResult> {
        validate_timestamp(now)?;
        self.reconcile_tombstones()?;
        let archive_cutoff = cutoff(now, policy.archived_observation_days)?;
        let job_cutoff = cutoff(now, policy.terminal_job_days)?;
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_active_lease = transaction
            .query_row(
                "SELECT owner IS NOT NULL AND expires_at > ?1
                 FROM consolidation_lock WHERE singleton = 1",
                params![now],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if has_active_lease {
            return Err(V2MaintenanceError::ActiveLease);
        }
        let expired_archives = {
            let mut statement = transaction.prepare(
                "SELECT a.source_path, a.archive_path
                 FROM consolidation_archives a
                 JOIN v2_archive_retention r USING(source_path)
                 WHERE r.archived_at <= ?1 ORDER BY a.source_path
                 LIMIT ?2",
            )?;
            statement
                .query_map(params![archive_cutoff, MAX_GC_ITEMS_PER_CLASS], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let terminal_jobs = {
            let mut statement = transaction.prepare(
                "SELECT j.job_id FROM v2_job_retention r
                 JOIN capture_jobs j USING(job_id)
                 WHERE r.terminal_at <= ?1
                   AND (
                       j.status = 'completed'
                       OR (j.status = 'failed' AND EXISTS (
                           SELECT 1 FROM memory_v2_terminal_capture_failures t
                           WHERE t.job_id = j.job_id
                       ))
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM capture_observation_files f
                       LEFT JOIN consolidation_archives a
                         ON REPLACE(a.source_path, char(92), '/')
                          = REPLACE(f.path, char(92), '/')
                       LEFT JOIN memory_v2_tombstones t
                         ON REPLACE(t.relative_path, char(92), '/')
                          = REPLACE(f.path, char(92), '/')
                       LEFT JOIN memory_v2_quarantined_paths q
                         ON REPLACE(q.relative_path, char(92), '/')
                          = REPLACE(f.path, char(92), '/')
                       WHERE f.job_id = j.job_id
                         AND a.source_path IS NULL AND t.relative_path IS NULL
                         AND q.relative_path IS NULL
                   )
                 ORDER BY j.job_id
                 LIMIT ?2",
            )?;
            statement
                .query_map(params![job_cutoff, MAX_GC_ITEMS_PER_CLASS], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (_, archive_path) in &expired_archives {
            remove_retained_file(&self.scope_dir, archive_path)?;
        }
        // Remove archive and capture rows together. Dropping only the archive row
        // resurrects a pending observation whose file was already unlinked.
        for (source_path, _) in &expired_archives {
            transaction.execute(
                "DELETE FROM consolidation_archives WHERE source_path = ?1",
                params![source_path],
            )?;
            transaction.execute(
                "DELETE FROM v2_archive_retention WHERE source_path = ?1",
                params![source_path],
            )?;
            transaction.execute(
                "DELETE FROM memory_v2_shadow_evaluations
                 WHERE REPLACE(source_path, char(92), '/')
                    = REPLACE(?1, char(92), '/')",
                params![source_path],
            )?;
            transaction.execute(
                "DELETE FROM memory_v2_hidden_observations
                 WHERE REPLACE(relative_path, char(92), '/')
                    = REPLACE(?1, char(92), '/')",
                params![source_path],
            )?;
            transaction.execute(
                "DELETE FROM capture_observation_files
                 WHERE REPLACE(path, char(92), '/')
                    = REPLACE(?1, char(92), '/')",
                params![source_path],
            )?;
        }
        for job_id in &terminal_jobs {
            transaction.execute(
                "DELETE FROM memory_v2_shadow_evaluations
                 WHERE source_path IN (
                    SELECT path FROM capture_observation_files WHERE job_id = ?1
                 )",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM memory_v2_hidden_observations
                 WHERE relative_path IN (
                    SELECT path FROM capture_observation_files WHERE job_id = ?1
                 )",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM capture_observation_files WHERE job_id = ?1",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM capture_outcomes WHERE job_id = ?1",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM capture_visibility WHERE job_id = ?1",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM memory_v2_terminal_capture_failures WHERE job_id = ?1",
                params![job_id],
            )?;
            transaction.execute(
                "DELETE FROM capture_jobs WHERE job_id = ?1",
                params![job_id],
            )?;
        }
        let terminal_jobs_removed = terminal_jobs.len() as u64;
        transaction.execute(
            "DELETE FROM v2_job_retention WHERE job_id IN (
                SELECT job_id FROM v2_job_retention
                WHERE terminal_at <= ?1
                  AND job_id NOT IN (SELECT job_id FROM capture_jobs)
                ORDER BY job_id
                LIMIT ?2
             )",
            params![job_cutoff, MAX_GC_ITEMS_PER_CLASS],
        )?;
        transaction.commit()?;
        Ok(GcResult {
            archived_observations_removed: expired_archives.len() as u64,
            terminal_jobs_removed,
        })
    }

    fn reconcile_tombstones(&self) -> Result<()> {
        let now = self.clock.now_unix_seconds();
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_active_lease = transaction
            .query_row(
                "SELECT owner IS NOT NULL AND expires_at > ?1
                 FROM consolidation_lock WHERE singleton = 1",
                params![now],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if has_active_lease {
            return Err(V2MaintenanceError::ActiveLease);
        }
        let targets = {
            let mut statement = transaction.prepare(
                "SELECT relative_path, target_kind, provenance_hash
                 FROM memory_v2_tombstones ORDER BY relative_path",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut removed_any = false;
        for (relative, kind, expected_content_hash) in &targets {
            let class = if kind == "topic" {
                V2PathClass::Topic(self.scope)
            } else if relative.starts_with("archive/") {
                V2PathClass::ArchivedObservation(self.scope)
            } else {
                V2PathClass::Observation(self.scope)
            };
            match self.remove_expected_tombstoned_file(
                Path::new(relative),
                class,
                expected_content_hash,
                &transaction,
            ) {
                Ok(TombstoneReconciliation::Removed) => removed_any = true,
                Ok(TombstoneReconciliation::AlreadyGone) => {}
                Err(
                    error @ (V2MaintenanceError::EvidenceMismatch | V2MaintenanceError::Protected),
                ) => {
                    tracing::warn!(
                        relative_path = relative,
                        error = %error,
                        "tombstoned path no longer matches its evidence; quarantining the path"
                    );
                    transaction.execute(
                        "INSERT INTO memory_v2_quarantined_paths(
                            relative_path, reason, expected_hash, observed_hash, quarantined_at
                         ) VALUES (?1, 'tombstone_mismatch', ?2, NULL, ?3)
                         ON CONFLICT(relative_path) DO NOTHING",
                        params![relative, expected_content_hash, now],
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
        transaction.commit()?;
        if targets.is_empty() {
            return Ok(());
        }
        self.drop_from_index(targets.iter().map(|target| target.0.as_str()))?;
        // Rendering the manifest re-runs the exclusion scan over the whole state
        // database, so settled tombstones (the common case on every open) skip it.
        if removed_any {
            regenerate_scope_manifest(&self.scope_dir, self.scope, V2ManifestBudget::default())
                .map_err(V2MaintenanceError::Manifest)?;
        }
        Ok(())
    }

    fn remove_expected_tombstoned_file(
        &self,
        relative: &Path,
        class: V2PathClass,
        expected_content_hash: &str,
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<TombstoneReconciliation> {
        let absolute = self.scope_dir.join(relative);
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TombstoneReconciliation::AlreadyGone);
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(V2MaintenanceError::EvidenceMismatch);
        }
        let bytes = read_regular_bounded(&absolute)?;
        if blake3::hash(&bytes).to_hex().as_str() != expected_content_hash {
            return Err(V2MaintenanceError::EvidenceMismatch);
        }
        self.access
            .record_read(&absolute, &bytes)
            .map_err(|source| V2MaintenanceError::Access(Box::new(AccessSnapshotError(source))))?;
        if matches!(class, V2PathClass::Topic(_)) {
            self.access
                .remove_topic_file_in_transaction(&absolute, transaction)
                .map_err(|source| V2MaintenanceError::Access(Box::new(source)))?;
        } else {
            std::fs::remove_file(&absolute)?;
        }
        Ok(TombstoneReconciliation::Removed)
    }

    fn drop_from_index<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let storage = MemoryStorage::new_flat(&self.scope_dir, &self.scope_dir);
        let mut index = MemoryIndex::open_or_create(
            &self.scope_dir.join("index.sqlite"),
            storage,
            distill_config_types::MemoryIndexConfig::default(),
            1,
        )
        .map_err(V2MaintenanceError::Index)?;
        for relative in paths {
            index
                .delete_path(&self.scope_dir.join(relative))
                .map_err(V2MaintenanceError::Index)?;
        }
        Ok(())
    }

    fn open_state(&self) -> Result<rusqlite::Connection> {
        JournalMode::for_db_path(&self.state_path)
            .open(&self.state_path)
            .map_err(V2MaintenanceError::Database)
    }
}

pub(crate) fn migrate_v2_hardening(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS memory_v2_tombstones (
            tombstone_id TEXT PRIMARY KEY,
            target_kind TEXT NOT NULL CHECK(target_kind IN ('observation','topic')),
            relative_path TEXT NOT NULL UNIQUE,
            provenance_hash TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            reason TEXT NOT NULL CHECK(reason IN ('user_request','privacy','incorrect','obsolete'))
        );
        CREATE TABLE IF NOT EXISTS memory_v2_audit (
            event_id TEXT PRIMARY KEY,
            action TEXT NOT NULL CHECK(action = 'forget'),
            target_kind TEXT NOT NULL CHECK(target_kind IN ('observation','topic')),
            target_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            reason TEXT NOT NULL CHECK(reason IN ('user_request','privacy','incorrect','obsolete'))
        );
        CREATE TABLE IF NOT EXISTS v2_archive_retention (
            source_path TEXT PRIMARY KEY,
            archived_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS v2_job_retention (
            job_id TEXT PRIMARY KEY,
            terminal_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS capture_visibility (
            job_id TEXT PRIMARY KEY,
            is_exposed INTEGER NOT NULL CHECK(is_exposed IN (0, 1))
        );
        CREATE TABLE IF NOT EXISTS memory_v2_hidden_observations (
            relative_path TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS memory_v2_quarantined_paths (
            relative_path TEXT PRIMARY KEY,
            reason TEXT NOT NULL CHECK(reason IN (
                'committed_hash_mismatch','tombstone_mismatch'
            )),
            expected_hash TEXT,
            observed_hash TEXT,
            quarantined_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS memory_v2_shadow_evaluations (
            source_path TEXT PRIMARY KEY,
            operation_id TEXT NOT NULL,
            evaluated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS memory_v2_terminal_capture_failures (
            job_id TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS consolidation_lock (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            owner TEXT,
            expires_at INTEGER,
            generation INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS consolidation_operations (
            operation_id TEXT PRIMARY KEY,
            status TEXT NOT NULL CHECK(status IN (
                'claimed','planned','topics_written','archived','completed','failed'
            )),
            owner TEXT NOT NULL,
            generation INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            completed_at INTEGER,
            plan_hash TEXT,
            last_error TEXT
        );
        CREATE TABLE IF NOT EXISTS consolidation_claim_items (
            operation_id TEXT NOT NULL REFERENCES consolidation_operations(operation_id),
            source_path TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            PRIMARY KEY(operation_id, source_path)
        );
        CREATE TABLE IF NOT EXISTS consolidation_archives (
            source_path TEXT PRIMARY KEY,
            operation_id TEXT NOT NULL,
            archive_path TEXT NOT NULL UNIQUE,
            content_hash TEXT NOT NULL
        );
        -- The exclusion joins compare REPLACE(source_path, char(92), '/'); without this
        -- expression index each of them scans every archive row per observation file.
        CREATE INDEX IF NOT EXISTS consolidation_archives_source_path_normalized
            ON consolidation_archives(REPLACE(source_path, char(92), '/'));",
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO consolidation_lock(singleton, generation) VALUES (1, 0)",
        [],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO v2_archive_retention(source_path, archived_at)
         SELECT a.source_path, COALESCE(o.completed_at, o.created_at)
         FROM consolidation_archives a
         JOIN consolidation_operations o USING(operation_id)",
        [],
    )?;
    let has_capture_tables = connection.query_row(
        "SELECT COUNT(*) = 2 FROM sqlite_master
         WHERE type = 'table' AND name IN ('capture_jobs', 'capture_outcomes')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if has_capture_tables {
        connection.execute(
            "INSERT OR IGNORE INTO v2_job_retention(job_id, terminal_at)
             SELECT j.job_id, o.committed_at
             FROM capture_jobs j JOIN capture_outcomes o USING(job_id)
             WHERE j.status = 'completed'",
            [],
        )?;
    }
    connection.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', '4')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    Ok(())
}

fn validate_relative_target(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.extension().and_then(|value| value.to_str()) != Some("md")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !(path.parent() == Some(Path::new("topics"))
            || path.starts_with(Path::new("observations/_inbox"))
            || path.starts_with(Path::new("archive")))
    {
        return Err(V2MaintenanceError::Invalid(
            "target must be one exact v2 Markdown file",
        ));
    }
    Ok(())
}

fn validate_timestamp(now: i64) -> Result<()> {
    if now < 0 {
        Err(V2MaintenanceError::Invalid("timestamp cannot be negative"))
    } else {
        Ok(())
    }
}

fn cutoff(now: i64, days: u64) -> Result<i64> {
    let seconds = i64::try_from(days)
        .ok()
        .and_then(|days| days.checked_mul(SECONDS_PER_DAY))
        .ok_or(V2MaintenanceError::Invalid(
            "retention duration is too large",
        ))?;
    Ok(now.saturating_sub(seconds))
}

fn deterministic_tombstone_id(kind: &str, path: &str, hash: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memory-v2-tombstone\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(hash.as_bytes());
    format!("forget_{}", &hasher.finalize().to_hex()[..24])
}

fn read_regular_bounded(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_FORGET_FILE_BYTES {
        return Err(V2MaintenanceError::Protected);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)?
        .take(MAX_FORGET_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FORGET_FILE_BYTES {
        return Err(V2MaintenanceError::Protected);
    }
    Ok(bytes)
}

fn remove_retained_file(scope_dir: &Path, relative: &str) -> Result<()> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !(relative.starts_with("archive") || relative.starts_with("observations/_inbox"))
    {
        return Err(V2MaintenanceError::Protected);
    }
    let path = scope_dir.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(path)?,
        Ok(_) => return Err(V2MaintenanceError::Protected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or(V2MaintenanceError::Invalid("target path must be UTF-8"))
}

#[cfg(test)]
#[path = "v2_maintenance_tests.rs"]
mod tests;
