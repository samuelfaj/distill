// Modified for Distill by Samuel Fajreldines, 2026.
//! Durable, fenced consolidation of pending memory-v2 observations.
//!
//! A claim records an immutable inbox snapshot in SQLite while capture keeps
//! publishing new files. Model work happens outside transactions. The final
//! topic/archive/index/manifest commit is replayable from a deterministic plan,
//! and every mutating entry point verifies the scope lease generation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use distill_sqlite_journal::JournalMode;

use crate::storage::MemoryStorage;
use crate::v2::{
    V2ManifestBudget, V2MemoryScope, bump_manifest_revision, regenerate_scope_manifest,
};
use crate::{MemoryIndex, SharedV2Clock, V2MemoryAccessPolicy, V2PathClass, system_v2_clock};

const MAX_OWNER_BYTES: usize = 128;
const MAX_CLAIMED_OBSERVATIONS: usize = 512;
const MAX_OBSERVATION_BYTES: u64 = 16 * 1024;
const MAX_TOPIC_BYTES: usize = 256 * 1024;
const MAX_TOPIC_INPUT_COUNT: usize = 128;
const MAX_TOPIC_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOPIC_OPERATIONS: usize = 128;
const MAX_TOPIC_PATH_BYTES: usize = 240;
const MAX_EVIDENCE_PER_OPERATION: usize = 64;
const MAX_FAILURE_BYTES: usize = 512;
/// Upper bound on unresumable durable plans one claim will abandon before it
/// falls back to a fresh deterministic claim; the rest are handled next time.
const MAX_DURABLE_PLAN_PROBES: usize = 64;

pub type Result<T> = std::result::Result<T, V2ConsolidationError>;

#[derive(Debug, thiserror::Error)]
pub enum V2ConsolidationError {
    #[error("invalid consolidation request: {0}")]
    Invalid(String),
    #[error("another memory-v2 consolidation is running")]
    Busy,
    #[error("consolidation lease is stale or no longer owned by this worker")]
    StaleLease,
    #[error("consolidation plan conflicts with durable state: {0}")]
    Conflict(String),
    #[error("consolidation database failed")]
    Database(#[from] rusqlite::Error),
    #[error("consolidation filesystem operation failed at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("memory-v2 access policy rejected consolidation: {0}")]
    Access(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("consolidation index or manifest convergence failed: {0}")]
    Convergence(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamClaimRequest {
    pub owner: String,
    pub now: i64,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedObservation {
    pub relative_path: PathBuf,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationLease {
    pub operation_id: String,
    pub owner: String,
    pub generation: u64,
    pub expires_at: i64,
    pub observations: Vec<ClaimedObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicOperation {
    Create {
        path: PathBuf,
        content: String,
        evidence: Vec<PathBuf>,
    },
    Update {
        path: PathBuf,
        content: String,
        evidence: Vec<PathBuf>,
    },
    Delete {
        path: PathBuf,
        evidence: Vec<PathBuf>,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        content: String,
        evidence: Vec<PathBuf>,
    },
    Merge {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        content: String,
        evidence: Vec<PathBuf>,
    },
    Split {
        source: PathBuf,
        destinations: Vec<(PathBuf, String)>,
        evidence: Vec<PathBuf>,
    },
}

impl TopicOperation {
    fn evidence(&self) -> &[PathBuf] {
        match self {
            Self::Create { evidence, .. }
            | Self::Update { evidence, .. }
            | Self::Delete { evidence, .. }
            | Self::Rename { evidence, .. }
            | Self::Merge { evidence, .. }
            | Self::Split { evidence, .. } => evidence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationInput {
    pub operation_id: String,
    pub observations: Vec<(PathBuf, String)>,
    pub topics: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationStatus {
    NoWork,
    Committed,
    Reconciled,
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationResult {
    pub status: ConsolidationStatus,
    pub operation_id: Option<String>,
    pub observation_count: usize,
    pub affected_topics: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamEligibilityConfig {
    pub min_pending_count: usize,
    pub max_pending_age: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamTriggerDisposition {
    Ineligible,
    Ready,
    Coalesced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamEligibility {
    pub disposition: DreamTriggerDisposition,
    pub pending_count: usize,
    pub oldest_pending_at: Option<i64>,
}

/// Scope-local durable consolidation store.
///
/// Independent handles coordinate through `memory_state.sqlite`; no process
/// global or session-actor state participates in exclusivity.
#[derive(Debug)]
pub struct V2ConsolidationStore {
    scope_dir: PathBuf,
    scope: V2MemoryScope,
    state_path: PathBuf,
    access: V2MemoryAccessPolicy,
    clock: SharedV2Clock,
}

impl V2ConsolidationStore {
    /// Open and additively migrate one initialized v2 scope.
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
            .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
        let store = Self {
            state_path: scope_dir.join("memory_state.sqlite"),
            scope_dir,
            scope,
            access,
            clock,
        };
        store.migrate()?;
        store.reconcile()?;
        Ok(store)
    }

    /// Atomically claim the exact set of observations pending at this instant.
    pub fn claim(&self, request: &DreamClaimRequest) -> Result<Option<ConsolidationLease>> {
        self.claim_with_visibility(request, false)
    }

    /// Claim non-exposed observations for a shadow evaluation.
    pub fn claim_shadow(&self, request: &DreamClaimRequest) -> Result<Option<ConsolidationLease>> {
        self.claim_with_visibility(request, true)
    }

    fn claim_with_visibility(
        &self,
        request: &DreamClaimRequest,
        include_hidden: bool,
    ) -> Result<Option<ConsolidationLease>> {
        self.reconcile()?;
        validate_text("lease owner", &request.owner, MAX_OWNER_BYTES)?;
        validate_timestamp(request.now)?;
        let duration = i64::try_from(request.duration.as_secs())
            .map_err(|_| V2ConsolidationError::Invalid("lease duration is too large".to_owned()))?;
        if duration == 0 {
            return Err(V2ConsolidationError::Invalid(
                "lease duration must be at least one second".to_owned(),
            ));
        }
        let expires_at = request
            .now
            .checked_add(duration)
            .ok_or_else(|| V2ConsolidationError::Invalid("lease expiry overflows".to_owned()))?;
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lock = transaction
            .query_row(
                "SELECT owner, expires_at, generation FROM consolidation_lock WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((None, None, 0));
        if lock.0.is_some() && lock.1.is_some_and(|expiry| expiry > request.now) {
            return Err(V2ConsolidationError::Busy);
        }
        if lock.0.is_some() {
            transaction.execute(
                "UPDATE consolidation_operations SET status = 'failed',
                    last_error = 'lease expired before commit'
                 WHERE generation = ?1
                   AND status IN ('claimed','planned','topics_written')",
                params![lock.2],
            )?;
        }
        let generation = lock.2.saturating_add(1);

        // Re-lease durable plans whose topic writes outlived a failed archive.
        // Their operation ids cannot be reconstructed after new captures arrive.
        // Shadow claims never mutate topics and must not adopt canonical plans.
        if !include_hidden
            && let Some(lease) =
                self.reclaim_durable_plan(&transaction, request, generation, expires_at)?
        {
            transaction.commit()?;
            return Ok(Some(lease));
        }

        let has_capture_files = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'capture_observation_files'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let observations = if has_capture_files {
            let mut statement = transaction.prepare(
                "SELECT f.path, f.content_hash
                 FROM capture_observation_files f
                 JOIN capture_outcomes o USING(job_id)
                 LEFT JOIN consolidation_archives a
                   ON REPLACE(a.source_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_tombstones t
                   ON REPLACE(t.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_hidden_observations h
                   ON REPLACE(h.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_shadow_evaluations se
                   ON REPLACE(se.source_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 LEFT JOIN memory_v2_quarantined_paths q
                   ON REPLACE(q.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
                 WHERE a.source_path IS NULL AND t.relative_path IS NULL
                   AND q.relative_path IS NULL
                   AND (
                       (NOT ?2 AND h.relative_path IS NULL)
                       OR (?2 AND h.relative_path IS NOT NULL AND se.source_path IS NULL)
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM consolidation_claim_items ci
                       JOIN consolidation_operations co ON co.operation_id = ci.operation_id
                       WHERE REPLACE(ci.source_path, char(92), '/')
                           = REPLACE(f.path, char(92), '/')
                         AND co.status IN ('claimed','planned','topics_written','archived')
                   )
                 ORDER BY f.path LIMIT ?1",
            )?;
            statement
                .query_map(params![MAX_CLAIMED_OBSERVATIONS, include_hidden], |row| {
                    Ok(ClaimedObservation {
                        relative_path: PathBuf::from(row.get::<_, String>(0)?),
                        content_hash: row.get(1)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        if observations.is_empty() {
            transaction.execute(
                "INSERT INTO consolidation_lock(singleton, owner, expires_at, generation)
                 VALUES (1, NULL, NULL, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET owner = NULL, expires_at = NULL,
                    generation = excluded.generation",
                params![generation],
            )?;
            transaction.commit()?;
            return Ok(None);
        }
        let operation_id = deterministic_operation_id(&observations);
        transaction.execute(
            "INSERT INTO consolidation_lock(singleton, owner, expires_at, generation)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(singleton) DO UPDATE SET owner = excluded.owner,
                expires_at = excluded.expires_at, generation = excluded.generation",
            params![request.owner, expires_at, generation],
        )?;
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES ('v2_dream_trigger_pending', '0')
             ON CONFLICT(key) DO UPDATE SET value = '0'",
            [],
        )?;
        transaction.execute(
            "INSERT INTO consolidation_operations(
                operation_id, status, owner, generation, created_at, plan_hash
             ) VALUES (?1, 'claimed', ?2, ?3, ?4, NULL)
             ON CONFLICT(operation_id) DO UPDATE SET status = 'claimed',
                owner = excluded.owner, generation = excluded.generation",
            params![operation_id, request.owner, generation, request.now],
        )?;
        transaction.execute(
            "DELETE FROM consolidation_claim_items WHERE operation_id = ?1",
            params![operation_id],
        )?;
        for observation in &observations {
            transaction.execute(
                "INSERT INTO consolidation_claim_items(operation_id, source_path, content_hash)
                 VALUES (?1, ?2, ?3)",
                params![
                    operation_id,
                    path_text(&observation.relative_path)?,
                    observation.content_hash
                ],
            )?;
        }
        transaction.commit()?;
        Ok(Some(ConsolidationLease {
            operation_id,
            owner: request.owner.clone(),
            generation,
            expires_at,
            observations,
        }))
    }

    /// Re-lease the oldest durable plan that has not reached archive commit.
    ///
    /// Returns `None` when no such plan exists. A plan whose claimed
    /// observations are missing or changed on disk can never archive, so it is
    /// abandoned (status `failed`, `plan_hash` cleared, reason recorded) rather
    /// than turning every future claim into the same error. Its topic changes
    /// that already reached disk stay put (the ledger's
    /// `consolidation_topic_changes` records them); the observations return
    /// to the pending set for a fresh claim.
    fn reclaim_durable_plan(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        request: &DreamClaimRequest,
        generation: u64,
        expires_at: i64,
    ) -> Result<Option<ConsolidationLease>> {
        for _ in 0..MAX_DURABLE_PLAN_PROBES {
            let Some(operation_id) = transaction
                .query_row(
                    "SELECT operation_id FROM consolidation_operations
                     WHERE plan_hash IS NOT NULL
                       AND status IN ('planned','topics_written','failed')
                     ORDER BY created_at, operation_id LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let observations = {
                let mut statement = transaction.prepare(
                    "SELECT source_path, content_hash FROM consolidation_claim_items
                     WHERE operation_id = ?1 ORDER BY source_path LIMIT ?2",
                )?;
                statement
                    .query_map(params![operation_id, MAX_CLAIMED_OBSERVATIONS], |row| {
                        Ok(ClaimedObservation {
                            relative_path: PathBuf::from(row.get::<_, String>(0)?),
                            content_hash: row.get(1)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let tombstoned_destination = transaction
                .query_row(
                    "SELECT c.path
                     FROM consolidation_topic_changes c
                     JOIN memory_v2_tombstones t
                       ON REPLACE(t.relative_path, char(92), '/')
                        = REPLACE(c.path, char(92), '/')
                     WHERE c.operation_id = ?1
                     ORDER BY c.path LIMIT 1",
                    params![operation_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let tombstoned_observation = transaction
                .query_row(
                    "SELECT ci.source_path
                     FROM consolidation_claim_items ci
                     JOIN memory_v2_tombstones t
                       ON REPLACE(t.relative_path, char(92), '/')
                        = REPLACE(ci.source_path, char(92), '/')
                     WHERE ci.operation_id = ?1
                     ORDER BY ci.source_path LIMIT 1",
                    params![operation_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let abandon_reason = if let Some(path) = tombstoned_destination {
                Some(format!(
                    "durable plan abandoned: topic destination {path} was forgotten"
                ))
            } else if let Some(path) = tombstoned_observation {
                Some(format!(
                    "durable plan abandoned: claimed observation {path} was forgotten"
                ))
            } else if observations.is_empty() {
                Some("durable plan has no claimed observations".to_owned())
            } else {
                observations.iter().find_map(|observation| {
                    let path = self.scope_dir.join(&observation.relative_path);
                    match read_bounded(&path, MAX_OBSERVATION_BYTES) {
                        Ok(bytes) if content_hash(&bytes) == observation.content_hash => None,
                        Ok(_) => Some(format!(
                            "durable plan abandoned: claimed observation {} changed",
                            observation.relative_path.display()
                        )),
                        Err(error) => Some(format!(
                            "durable plan abandoned: claimed observation {} unreadable: {error}",
                            observation.relative_path.display()
                        )),
                    }
                })
            };
            if let Some(reason) = abandon_reason {
                transaction.execute(
                    "UPDATE consolidation_operations
                     SET status = 'failed', plan_hash = NULL, last_error = ?2
                     WHERE operation_id = ?1",
                    params![operation_id, reason],
                )?;
                continue;
            }
            transaction.execute(
                "INSERT INTO consolidation_lock(singleton, owner, expires_at, generation)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(singleton) DO UPDATE SET owner = excluded.owner,
                    expires_at = excluded.expires_at, generation = excluded.generation",
                params![request.owner, expires_at, generation],
            )?;
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES ('v2_dream_trigger_pending', '0')
                 ON CONFLICT(key) DO UPDATE SET value = '0'",
                [],
            )?;
            transaction.execute(
                "UPDATE consolidation_operations
                 SET status = 'planned', owner = ?2, generation = ?3
                 WHERE operation_id = ?1",
                params![operation_id, request.owner, generation],
            )?;
            return Ok(Some(ConsolidationLease {
                operation_id,
                owner: request.owner.clone(),
                generation,
                expires_at,
                observations,
            }));
        }
        Ok(None)
    }

    /// Evaluate automatic Dream eligibility when capture publishes an outcome.
    ///
    /// This performs no polling. Callers invoke it from the durable
    /// capture-completed event; simultaneous events collapse to one persisted
    /// trigger while a canonical editor is active.
    pub fn on_capture_completed(
        &self,
        now: i64,
        config: DreamEligibilityConfig,
    ) -> Result<DreamEligibility> {
        self.capture_eligibility(now, config, false)
    }

    pub fn on_capture_completed_shadow(
        &self,
        now: i64,
        config: DreamEligibilityConfig,
    ) -> Result<DreamEligibility> {
        self.capture_eligibility(now, config, true)
    }

    fn capture_eligibility(
        &self,
        now: i64,
        config: DreamEligibilityConfig,
        include_hidden: bool,
    ) -> Result<DreamEligibility> {
        validate_timestamp(now)?;
        if config.min_pending_count == 0 {
            return Err(V2ConsolidationError::Invalid(
                "minimum pending count must be positive".to_owned(),
            ));
        }
        let max_age = i64::try_from(config.max_pending_age.as_secs())
            .map_err(|_| V2ConsolidationError::Invalid("pending age is too large".to_owned()))?;
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_capture_files = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'capture_observation_files'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_capture_files {
            transaction.commit()?;
            return Ok(DreamEligibility {
                disposition: DreamTriggerDisposition::Ineligible,
                pending_count: 0,
                oldest_pending_at: None,
            });
        }
        let (pending_count, oldest_pending_at) = transaction.query_row(
            "SELECT COUNT(*), MIN(o.committed_at)
             FROM capture_observation_files f
             JOIN capture_outcomes o USING(job_id)
             LEFT JOIN consolidation_archives a
               ON REPLACE(a.source_path, char(92), '/') = REPLACE(f.path, char(92), '/')
             LEFT JOIN memory_v2_tombstones t
               ON REPLACE(t.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
             LEFT JOIN memory_v2_hidden_observations h
               ON REPLACE(h.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
             LEFT JOIN memory_v2_shadow_evaluations se
               ON REPLACE(se.source_path, char(92), '/') = REPLACE(f.path, char(92), '/')
             LEFT JOIN memory_v2_quarantined_paths q
               ON REPLACE(q.relative_path, char(92), '/') = REPLACE(f.path, char(92), '/')
             WHERE a.source_path IS NULL AND t.relative_path IS NULL
               AND q.relative_path IS NULL
               AND (
                   (NOT ?1 AND h.relative_path IS NULL)
                   OR (?1 AND h.relative_path IS NOT NULL AND se.source_path IS NULL)
               )",
            params![include_hidden],
            |row| Ok((row.get::<_, usize>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let is_old_enough = oldest_pending_at
            .is_some_and(|oldest| max_age > 0 && now.saturating_sub(oldest) >= max_age);
        let is_eligible = pending_count >= config.min_pending_count || is_old_enough;
        let is_active = transaction.query_row(
            "SELECT owner IS NOT NULL AND expires_at > ?1
             FROM consolidation_lock WHERE singleton = 1",
            params![now],
            |row| row.get::<_, bool>(0),
        )?;
        if !is_active {
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES ('v2_dream_trigger_pending', '0')
                 ON CONFLICT(key) DO UPDATE SET value = '0'",
                [],
            )?;
        }
        let disposition = if !is_eligible {
            DreamTriggerDisposition::Ineligible
        } else if is_active {
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES ('v2_dream_trigger_pending', '1')
                 ON CONFLICT(key) DO UPDATE SET value = '1'",
                [],
            )?;
            DreamTriggerDisposition::Coalesced
        } else {
            DreamTriggerDisposition::Ready
        };
        transaction.commit()?;
        Ok(DreamEligibility {
            disposition,
            pending_count,
            oldest_pending_at,
        })
    }

    pub fn has_coalesced_trigger(&self) -> Result<bool> {
        Ok(self
            .open_state()?
            .query_row(
                "SELECT value = '1' FROM meta WHERE key = 'v2_dream_trigger_pending'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Validate a shadow plan and release its claim without mutating topics,
    /// archiving observations, or changing the lexical index and manifest.
    pub fn complete_shadow(
        &self,
        lease: &ConsolidationLease,
        operations: &[TopicOperation],
        now: i64,
    ) -> Result<ConsolidationResult> {
        let result = self.try_complete_shadow(lease, operations, now);
        if let Err(error) = &result {
            // A rejected shadow plan must not strand the canonical lease until
            // expiry. Preserve the original error for classification while
            // best-effort transitioning this owned claim to retryable.
            let _ = self.fail_retryable(lease, now, &error.to_string());
        }
        result
    }

    fn try_complete_shadow(
        &self,
        lease: &ConsolidationLease,
        operations: &[TopicOperation],
        now: i64,
    ) -> Result<ConsolidationResult> {
        validate_timestamp(now)?;
        let changes = self.prepare_changes(lease, operations)?;
        if changes.is_empty() {
            return Err(V2ConsolidationError::Invalid(
                "consolidation plan must contain at least one topic change".to_owned(),
            ));
        }
        let affected_topics = changes.keys().cloned().collect();
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_tx(&transaction, lease, now)?;
        for observation in &lease.observations {
            transaction.execute(
                "INSERT INTO memory_v2_shadow_evaluations(source_path, operation_id, evaluated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_path) DO UPDATE SET
                    operation_id = excluded.operation_id,
                    evaluated_at = excluded.evaluated_at",
                params![
                    path_text(&observation.relative_path)?,
                    lease.operation_id,
                    now
                ],
            )?;
        }
        transaction.execute(
            "UPDATE consolidation_operations
             SET status = 'completed', completed_at = ?2, plan_hash = NULL, last_error = NULL
             WHERE operation_id = ?1",
            params![lease.operation_id, now],
        )?;
        release_lock(&transaction, lease)?;
        transaction.commit()?;
        Ok(ConsolidationResult {
            status: ConsolidationStatus::Shadow,
            operation_id: Some(lease.operation_id.clone()),
            observation_count: lease.observations.len(),
            affected_topics,
        })
    }

    /// Read only the claimed observations plus existing topic Markdown.
    pub fn consolidation_input(
        &self,
        lease: &ConsolidationLease,
        now: i64,
    ) -> Result<ConsolidationInput> {
        self.validate_lease(lease, now)?;
        let mut observations = Vec::with_capacity(lease.observations.len());
        for observation in &lease.observations {
            let path = self.scope_dir.join(&observation.relative_path);
            let content = self.read_policy_file(&path, MAX_OBSERVATION_BYTES)?;
            if content_hash(content.as_bytes()) != observation.content_hash {
                return Err(V2ConsolidationError::Conflict(format!(
                    "claimed observation {} changed",
                    observation.relative_path.display()
                )));
            }
            observations.push((observation.relative_path.clone(), content));
        }
        Ok(ConsolidationInput {
            operation_id: lease.operation_id.clone(),
            observations,
            topics: self.read_topics()?,
        })
    }

    /// Persist a deterministic plan, apply it, archive the claim, and converge.
    pub fn commit(
        &self,
        lease: &ConsolidationLease,
        operations: &[TopicOperation],
        now: i64,
    ) -> Result<ConsolidationResult> {
        validate_timestamp(now)?;
        if self
            .open_state()?
            .query_row(
                "SELECT status = 'completed' FROM consolidation_operations
                 WHERE operation_id = ?1",
                params![lease.operation_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false)
        {
            let connection = self.open_state()?;
            let affected_topics = read_durable_changes(&connection, &lease.operation_id)?
                .into_iter()
                .map(|(path, _)| path)
                .collect();
            return Ok(ConsolidationResult {
                status: ConsolidationStatus::Reconciled,
                operation_id: Some(lease.operation_id.clone()),
                observation_count: lease.observations.len(),
                affected_topics,
            });
        }
        if operations.len() > MAX_TOPIC_OPERATIONS {
            return Err(V2ConsolidationError::Invalid(format!(
                "plan exceeds the {MAX_TOPIC_OPERATIONS}-operation limit"
            )));
        }
        let changes = self.prepare_changes(lease, operations)?;
        if changes.is_empty() {
            return Err(V2ConsolidationError::Invalid(
                "consolidation plan must contain at least one topic change".to_owned(),
            ));
        }
        let plan_hash = hash_changes(&changes);
        {
            let mut connection = self.open_state()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            validate_lease_tx(&transaction, lease, now)?;
            // An empty plan must never reach the durable boundary: once
            // persisted it would be replayed by every later reclaim and fail
            // the same way each time.
            if changes.is_empty() {
                return Err(V2ConsolidationError::Invalid(
                    "consolidation plan must contain at least one topic change".to_owned(),
                ));
            }
            if let Some(existing) = transaction
                .query_row(
                    "SELECT plan_hash FROM consolidation_operations WHERE operation_id = ?1",
                    params![lease.operation_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                && existing != plan_hash
            {
                return Err(V2ConsolidationError::Conflict(
                    "operation was already planned with different topic changes".to_owned(),
                ));
            }
            transaction.execute(
                "UPDATE consolidation_operations SET status = 'planned', plan_hash = ?2
                 WHERE operation_id = ?1",
                params![lease.operation_id, plan_hash],
            )?;
            transaction.execute(
                "DELETE FROM consolidation_topic_changes WHERE operation_id = ?1",
                params![lease.operation_id],
            )?;
            for (path, content) in &changes {
                transaction.execute(
                    "INSERT INTO consolidation_topic_changes(operation_id, path, content_hash, content)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        lease.operation_id,
                        path_text(path)?,
                        content.as_ref().map(|value| content_hash(value.as_bytes())),
                        content
                    ],
                )?;
            }
            transaction.commit()?;
        }
        self.apply_and_archive(lease, now)?;
        self.converge_operation(&lease.operation_id)?;
        self.finish_operation(lease, now, ConsolidationStatus::Committed)
    }

    /// Resume a deterministic plan persisted by an interrupted worker.
    ///
    /// Returns `None` when the claim has not reached the durable-plan boundary.
    pub fn resume_planned(
        &self,
        lease: &ConsolidationLease,
        now: i64,
    ) -> Result<Option<ConsolidationResult>> {
        validate_timestamp(now)?;
        let is_planned = self
            .open_state()?
            .query_row(
                "SELECT plan_hash IS NOT NULL FROM consolidation_operations
                 WHERE operation_id = ?1",
                params![lease.operation_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if !is_planned {
            return Ok(None);
        }
        self.apply_and_archive(lease, now)?;
        self.converge_operation(&lease.operation_id)?;
        self.finish_operation(lease, now, ConsolidationStatus::Reconciled)
            .map(Some)
    }

    /// Mark a model/tool failure retryable and release the canonical editor.
    pub fn fail_retryable(&self, lease: &ConsolidationLease, now: i64, error: &str) -> Result<()> {
        validate_timestamp(now)?;
        let error = sanitize_failure(error);
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_tx(&transaction, lease, now)?;
        // Only a plan that reached the durable boundary may be resumed by a
        // later claim. A claim that never got that far must not leave behind
        // stale plan rows from an earlier life of the same operation id.
        transaction.execute(
            "DELETE FROM consolidation_topic_changes
             WHERE operation_id = ?1
               AND EXISTS (
                   SELECT 1 FROM consolidation_operations
                   WHERE operation_id = ?1 AND status = 'claimed'
               )",
            params![lease.operation_id],
        )?;
        transaction.execute(
            "UPDATE consolidation_operations
             SET plan_hash = CASE WHEN status = 'claimed' THEN NULL ELSE plan_hash END,
                 status = CASE WHEN status = 'archived' THEN 'archived' ELSE 'failed' END,
                 last_error = ?2
             WHERE operation_id = ?1",
            params![lease.operation_id, error],
        )?;
        release_lock(&transaction, lease)?;
        transaction.commit()?;
        Ok(())
    }

    /// Complete index/manifest convergence after a crash beyond archive commit.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let mut connection = self.open_state()?;
        let now = self.clock.now_unix_seconds();
        let operations = {
            let mut statement = connection.prepare(
                "SELECT o.operation_id FROM consolidation_operations o
                 WHERE o.status = 'archived'
                   AND NOT EXISTS (
                     SELECT 1 FROM consolidation_lock l
                     WHERE l.singleton = 1
                       AND l.generation = o.generation
                       AND l.owner = o.owner
                       AND l.expires_at > ?1
                   )
                 ORDER BY o.operation_id",
            )?;
            statement
                .query_map(params![now], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for operation_id in &operations {
            self.converge_operation(operation_id)?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "UPDATE consolidation_operations SET status = 'completed'
                 WHERE operation_id = ?1 AND status = 'archived'",
                params![operation_id],
            )?;
            transaction.execute(
                "UPDATE consolidation_lock SET owner = NULL, expires_at = NULL
                 WHERE singleton = 1 AND generation = (
                    SELECT generation FROM consolidation_operations WHERE operation_id = ?1
                 )",
                params![operation_id],
            )?;
            transaction.commit()?;
        }
        Ok(operations)
    }

    fn apply_and_archive(&self, lease: &ConsolidationLease, now: i64) -> Result<()> {
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_tx(&transaction, lease, now)?;
        let changes = read_durable_changes(&transaction, &lease.operation_id)?;
        if changes.is_empty() {
            return Err(V2ConsolidationError::Invalid(
                "consolidation plan must contain at least one topic change".to_owned(),
            ));
        }
        for (relative, content) in &changes {
            let path = self.scope_dir.join(relative);
            match content {
                Some(content) => {
                    match read_bounded(&path, MAX_TOPIC_BYTES as u64) {
                        Ok(bytes) => {
                            self.access
                                .record_read_typed(&path, &bytes)
                                .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
                        }
                        Err(V2ConsolidationError::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    let outcome = self
                        .access
                        .write_file_typed_in_transaction(&path, content.as_bytes(), &transaction)
                        .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
                    if matches!(
                        outcome,
                        distill_tools::types::memory_v2::MemoryV2Write::Outside
                    ) {
                        return Err(V2ConsolidationError::Access(Box::new(
                            crate::V2AccessError::EscapesScope(path),
                        )));
                    }
                }
                None => {
                    let bytes = match read_bounded(&path, MAX_TOPIC_BYTES as u64) {
                        Ok(bytes) => bytes,
                        Err(V2ConsolidationError::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    self.access
                        .record_read_typed(&path, &bytes)
                        .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
                    self.access
                        .remove_topic_file_in_transaction(&path, &transaction)
                        .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
                }
            }
        }
        transaction.execute(
            "UPDATE consolidation_operations SET status = 'topics_written'
             WHERE operation_id = ?1",
            params![lease.operation_id],
        )?;
        transaction.commit()?;
        drop(connection);

        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_tx(&transaction, lease, now)?;
        let archive_dir = self.scope_dir.join("archive").join(&lease.operation_id);
        std::fs::create_dir_all(&archive_dir).map_err(|source| io_error(&archive_dir, source))?;
        for observation in &lease.observations {
            let source = self.scope_dir.join(&observation.relative_path);
            let file_name = observation.relative_path.file_name().ok_or_else(|| {
                V2ConsolidationError::Invalid("observation path has no filename".to_owned())
            })?;
            let archived_relative = PathBuf::from("archive")
                .join(&lease.operation_id)
                .join(file_name);
            let archived = self.scope_dir.join(&archived_relative);
            copy_idempotently(&source, &archived, &observation.content_hash)?;
            transaction.execute(
                "INSERT INTO consolidation_archives(
                    source_path, operation_id, archive_path, content_hash
                 ) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_path) DO UPDATE SET
                    operation_id = excluded.operation_id,
                    archive_path = excluded.archive_path,
                    content_hash = excluded.content_hash",
                params![
                    path_text(&observation.relative_path)?,
                    lease.operation_id,
                    path_text(&archived_relative)?,
                    observation.content_hash
                ],
            )?;
            transaction.execute(
                "INSERT INTO v2_archive_retention(source_path, archived_at)
                 VALUES (?1, ?2)
                 ON CONFLICT(source_path) DO UPDATE SET archived_at = excluded.archived_at",
                params![path_text(&observation.relative_path)?, now],
            )?;
        }
        transaction.execute(
            "UPDATE consolidation_operations SET status = 'archived'
             WHERE operation_id = ?1",
            params![lease.operation_id],
        )?;
        transaction.commit()?;
        for observation in &lease.observations {
            remove_idempotently(
                &self.scope_dir.join(&observation.relative_path),
                &observation.content_hash,
            )?;
        }
        Ok(())
    }

    fn finish_operation(
        &self,
        lease: &ConsolidationLease,
        now: i64,
        status: ConsolidationStatus,
    ) -> Result<ConsolidationResult> {
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_tx(&transaction, lease, now)?;
        let affected_topics = read_durable_changes(&transaction, &lease.operation_id)?
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        transaction.execute(
            "UPDATE consolidation_operations SET status = 'completed', completed_at = ?2
             WHERE operation_id = ?1",
            params![lease.operation_id, now],
        )?;
        release_lock(&transaction, lease)?;
        transaction.commit()?;
        Ok(ConsolidationResult {
            status,
            operation_id: Some(lease.operation_id.clone()),
            observation_count: lease.observations.len(),
            affected_topics,
        })
    }

    fn converge_operation(&self, operation_id: &str) -> Result<()> {
        let connection = self.open_state()?;
        let changes = read_durable_changes(&connection, operation_id)?;
        let storage = MemoryStorage::new_flat(&self.scope_dir, &self.scope_dir);
        let mut index = MemoryIndex::open_or_create(
            &self.scope_dir.join("index.sqlite"),
            storage,
            distill_config_types::MemoryIndexConfig::default(),
            1,
        )
        .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
        let source = match self.scope {
            V2MemoryScope::Global => "global",
            V2MemoryScope::Workspace => "workspace",
        };
        for (relative, content) in changes {
            let path = self.scope_dir.join(relative);
            if content.is_some() {
                index
                    .reindex_file(&path, source)
                    .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
            } else {
                index
                    .delete_path(&path)
                    .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
            }
        }
        let archived = {
            let mut statement = connection.prepare(
                "SELECT source_path, content_hash FROM consolidation_archives
                 WHERE operation_id = ?1",
            )?;
            statement
                .query_map(params![operation_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (source_path, expected_hash) in archived {
            let source_path = self.scope_dir.join(source_path);
            remove_idempotently(&source_path, &expected_hash)?;
            index
                .delete_path(&source_path)
                .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
        }
        bump_manifest_revision(&self.scope_dir)
            .and_then(|_| {
                regenerate_scope_manifest(&self.scope_dir, self.scope, V2ManifestBudget::default())
            })
            .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
        Ok(())
    }

    fn prepare_changes(
        &self,
        lease: &ConsolidationLease,
        operations: &[TopicOperation],
    ) -> Result<BTreeMap<PathBuf, Option<String>>> {
        let claimed: BTreeSet<_> = lease
            .observations
            .iter()
            .map(|observation| observation.relative_path.clone())
            .collect();
        let mut changes = BTreeMap::new();
        for operation in operations {
            validate_evidence(operation.evidence(), &claimed)?;
            match operation {
                TopicOperation::Create { path, content, .. } => {
                    self.validate_topic_path(path)?;
                    if self.scope_dir.join(path).exists() {
                        return Err(V2ConsolidationError::Conflict(format!(
                            "create target already exists: {}",
                            path.display()
                        )));
                    }
                    insert_change(&mut changes, path, Some(normalize_topic_content(content)?))?;
                }
                TopicOperation::Update { path, content, .. } => {
                    self.require_existing_topic(path)?;
                    insert_change(&mut changes, path, Some(normalize_topic_content(content)?))?;
                }
                TopicOperation::Delete { path, .. } => {
                    self.require_existing_topic(path)?;
                    insert_change(&mut changes, path, None)?;
                }
                TopicOperation::Rename {
                    from, to, content, ..
                } => {
                    self.require_existing_topic(from)?;
                    self.validate_destination(to, std::slice::from_ref(from))?;
                    if from != to {
                        insert_change(&mut changes, from, None)?;
                    }
                    insert_change(&mut changes, to, Some(normalize_topic_content(content)?))?;
                }
                TopicOperation::Merge {
                    sources,
                    destination,
                    content,
                    ..
                } => {
                    if sources.len() < 2 {
                        return Err(V2ConsolidationError::Invalid(
                            "merge requires at least two source topics".to_owned(),
                        ));
                    }
                    self.validate_destination(destination, sources)?;
                    for source in sources {
                        self.require_existing_topic(source)?;
                        if source != destination {
                            insert_change(&mut changes, source, None)?;
                        }
                    }
                    insert_change(
                        &mut changes,
                        destination,
                        Some(normalize_topic_content(content)?),
                    )?;
                }
                TopicOperation::Split {
                    source,
                    destinations,
                    ..
                } => {
                    self.require_existing_topic(source)?;
                    if destinations.len() < 2 {
                        return Err(V2ConsolidationError::Invalid(
                            "split requires at least two destination topics".to_owned(),
                        ));
                    }
                    for (path, _) in destinations {
                        self.validate_destination(path, std::slice::from_ref(source))?;
                    }
                    if !destinations.iter().any(|(path, _)| path == source) {
                        insert_change(&mut changes, source, None)?;
                    }
                    for (path, content) in destinations {
                        insert_change(&mut changes, path, Some(normalize_topic_content(content)?))?;
                    }
                }
            }
        }
        Ok(changes)
    }

    fn require_existing_topic(&self, relative: &Path) -> Result<()> {
        self.validate_topic_path(relative)?;
        if self.scope_dir.join(relative).is_file() {
            Ok(())
        } else {
            Err(V2ConsolidationError::Conflict(format!(
                "topic does not exist: {}",
                relative.display()
            )))
        }
    }

    fn validate_destination(&self, relative: &Path, rewritten_sources: &[PathBuf]) -> Result<()> {
        self.validate_topic_path(relative)?;
        if self.scope_dir.join(relative).is_file()
            && !rewritten_sources.iter().any(|source| source == relative)
        {
            return Err(V2ConsolidationError::Conflict(format!(
                "destination already exists: {}",
                relative.display()
            )));
        }
        Ok(())
    }

    fn validate_topic_path(&self, relative: &Path) -> Result<()> {
        if relative.as_os_str().len() > MAX_TOPIC_PATH_BYTES
            || relative.extension().and_then(|value| value.to_str()) != Some("md")
            || relative.parent() != Some(Path::new("topics"))
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(V2ConsolidationError::Invalid(format!(
                "invalid topic path: {}",
                relative.display()
            )));
        }
        let absolute = self.scope_dir.join(relative);
        let class = self
            .access
            .classify_path(&absolute)
            .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
        if class != V2PathClass::Topic(self.scope) {
            return Err(V2ConsolidationError::Access(Box::new(
                crate::V2AccessError::Protected(absolute),
            )));
        }
        let is_excluded = self.open_state()?.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM memory_v2_tombstones WHERE relative_path = ?1
                UNION ALL
                SELECT 1 FROM memory_v2_quarantined_paths WHERE relative_path = ?1
             )",
            params![path_text(relative)?],
            |row| row.get::<_, bool>(0),
        )?;
        if is_excluded {
            return Err(V2ConsolidationError::Conflict(format!(
                "topic path is durably excluded: {}",
                relative.display()
            )));
        }
        Ok(())
    }

    fn read_topics(&self) -> Result<Vec<(PathBuf, String)>> {
        let topics_dir = self.scope_dir.join("topics");
        let mut paths = Vec::new();
        for entry in
            std::fs::read_dir(&topics_dir).map_err(|source| io_error(&topics_dir, source))?
        {
            let entry = entry.map_err(|source| io_error(&topics_dir, source))?;
            if !entry
                .file_type()
                .map_err(|source| io_error(entry.path(), source))?
                .is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
            {
                continue;
            }
            if paths.len() == MAX_TOPIC_INPUT_COUNT {
                return Err(V2ConsolidationError::Invalid(format!(
                    "topic input exceeds the {MAX_TOPIC_INPUT_COUNT}-file limit"
                )));
            }
            let relative = entry
                .path()
                .strip_prefix(&self.scope_dir)
                .map_err(|_| V2ConsolidationError::Invalid("topic escaped scope".to_owned()))?
                .to_path_buf();
            let is_excluded = self.open_state()?.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM memory_v2_tombstones WHERE relative_path = ?1
                    UNION ALL
                    SELECT 1 FROM memory_v2_quarantined_paths WHERE relative_path = ?1
                 )",
                params![path_text(&relative)?],
                |row| row.get::<_, bool>(0),
            )?;
            if is_excluded {
                continue;
            }
            paths.push(entry.path());
        }
        paths.sort();
        let mut total_bytes = 0usize;
        let mut topics = Vec::with_capacity(paths.len());
        for path in paths {
            let content = self.read_policy_file(&path, MAX_TOPIC_BYTES as u64)?;
            total_bytes = total_bytes.checked_add(content.len()).ok_or_else(|| {
                V2ConsolidationError::Invalid("topic input byte count overflows".to_owned())
            })?;
            if total_bytes > MAX_TOPIC_INPUT_BYTES {
                return Err(V2ConsolidationError::Invalid(format!(
                    "topic input exceeds the {MAX_TOPIC_INPUT_BYTES}-byte limit"
                )));
            }
            topics.push((
                path.strip_prefix(&self.scope_dir)
                    .map_err(|_| V2ConsolidationError::Invalid("topic escaped scope".to_owned()))?
                    .to_path_buf(),
                content,
            ));
        }
        Ok(topics)
    }

    fn read_policy_file(&self, path: &Path, max_bytes: u64) -> Result<String> {
        if !self
            .access
            .validate_read_typed(path)
            .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?
        {
            return Err(V2ConsolidationError::Access(Box::new(
                crate::V2AccessError::EscapesScope(path.to_path_buf()),
            )));
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
        if !metadata.file_type().is_file() || metadata.len() > max_bytes {
            return Err(V2ConsolidationError::Invalid(format!(
                "{} is not a bounded regular file",
                path.display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(path)
            .and_then(|mut file| {
                std::io::Read::by_ref(&mut file)
                    .take(max_bytes + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|source| io_error(path, source))?;
        if bytes.len() as u64 > max_bytes {
            return Err(V2ConsolidationError::Invalid(
                "memory-v2 file exceeds its read limit".to_owned(),
            ));
        }
        self.access
            .record_read_typed(path, &bytes)
            .map_err(|error| V2ConsolidationError::Access(Box::new(error)))?;
        String::from_utf8(bytes)
            .map_err(|_| V2ConsolidationError::Invalid("memory-v2 file is not UTF-8".to_owned()))
    }

    fn validate_lease(&self, lease: &ConsolidationLease, now: i64) -> Result<()> {
        validate_timestamp(now)?;
        validate_lease_tx(&self.open_state()?, lease, now)
    }

    fn migrate(&self) -> Result<()> {
        let mut connection = self.open_state()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS consolidation_lock (
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
            CREATE TABLE IF NOT EXISTS consolidation_topic_changes (
                operation_id TEXT NOT NULL REFERENCES consolidation_operations(operation_id),
                path TEXT NOT NULL,
                content_hash TEXT,
                content TEXT,
                PRIMARY KEY(operation_id, path)
            );
            CREATE TABLE IF NOT EXISTS consolidation_archives (
                source_path TEXT PRIMARY KEY,
                operation_id TEXT NOT NULL REFERENCES consolidation_operations(operation_id),
                archive_path TEXT NOT NULL UNIQUE,
                content_hash TEXT NOT NULL
            );
            INSERT OR IGNORE INTO consolidation_lock(singleton, generation)
            VALUES (1, 0);",
        )?;
        crate::v2_maintenance::migrate_v2_hardening(&transaction)
            .map_err(|error| V2ConsolidationError::Convergence(Box::new(error)))?;
        transaction.commit()?;
        Ok(())
    }

    fn open_state(&self) -> Result<rusqlite::Connection> {
        JournalMode::for_db_path(&self.state_path)
            .open(&self.state_path)
            .map_err(V2ConsolidationError::Database)
    }
}

fn validate_lease_tx(
    connection: &rusqlite::Connection,
    lease: &ConsolidationLease,
    now: i64,
) -> Result<()> {
    let matches = connection.query_row(
        "SELECT COUNT(*) FROM consolidation_lock l
         JOIN consolidation_operations o
           ON o.operation_id = ?1 AND o.generation = l.generation
         WHERE l.singleton = 1 AND l.owner = ?2 AND l.generation = ?3
           AND l.expires_at > ?4 AND o.status != 'failed'",
        params![lease.operation_id, lease.owner, lease.generation, now],
        |row| row.get::<_, u32>(0),
    )?;
    if matches == 1 {
        Ok(())
    } else {
        Err(V2ConsolidationError::StaleLease)
    }
}

fn release_lock(transaction: &rusqlite::Transaction<'_>, lease: &ConsolidationLease) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE consolidation_lock SET owner = NULL, expires_at = NULL
         WHERE singleton = 1 AND owner = ?1 AND generation = ?2",
        params![lease.owner, lease.generation],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(V2ConsolidationError::StaleLease)
    }
}

fn read_durable_changes(
    connection: &rusqlite::Connection,
    operation_id: &str,
) -> Result<Vec<(PathBuf, Option<String>)>> {
    let mut statement = connection.prepare(
        "SELECT path, content FROM consolidation_topic_changes
         WHERE operation_id = ?1 ORDER BY path",
    )?;
    statement
        .query_map(params![operation_id], |row| {
            Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(V2ConsolidationError::Database)
}

fn validate_evidence(evidence: &[PathBuf], claimed: &BTreeSet<PathBuf>) -> Result<()> {
    if evidence.is_empty() || evidence.len() > MAX_EVIDENCE_PER_OPERATION {
        return Err(V2ConsolidationError::Invalid(format!(
            "every topic operation requires 1..={MAX_EVIDENCE_PER_OPERATION} claimed evidence files"
        )));
    }
    if let Some(unclaimed) = evidence.iter().find(|path| !claimed.contains(*path)) {
        return Err(V2ConsolidationError::Invalid(format!(
            "operation cites unclaimed observation: {}",
            unclaimed.display()
        )));
    }
    Ok(())
}

fn insert_change(
    changes: &mut BTreeMap<PathBuf, Option<String>>,
    path: &Path,
    content: Option<String>,
) -> Result<()> {
    if changes.insert(path.to_path_buf(), content).is_some() {
        return Err(V2ConsolidationError::Conflict(format!(
            "plan mutates {} more than once",
            path.display()
        )));
    }
    Ok(())
}

fn sanitize_failure(error: &str) -> String {
    let normalized = error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = normalized.trim();
    let normalized = if normalized.is_empty() {
        "consolidation operation failed"
    } else {
        normalized
    };
    if normalized.len() <= MAX_FAILURE_BYTES {
        return normalized.to_owned();
    }
    let mut end = MAX_FAILURE_BYTES;
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    normalized[..end].trim_end().to_owned()
}

fn normalize_topic_content(content: &str) -> Result<String> {
    let content = strip_legacy_provenance(content);
    let content = content.trim_end();
    if content.is_empty() || content.len() > MAX_TOPIC_BYTES {
        return Err(V2ConsolidationError::Invalid(format!(
            "topic content must contain 1..={MAX_TOPIC_BYTES} bytes"
        )));
    }
    Ok(content.to_owned())
}

fn strip_legacy_provenance(content: &str) -> String {
    const PROVENANCE_PREFIX: &str = "<!-- memory-v2 provenance:";
    let mut remaining = content;
    let mut stripped = String::with_capacity(content.len());
    while let Some(start) = remaining.find(PROVENANCE_PREFIX) {
        stripped.push_str(&remaining[..start]);
        match remaining[start..].find("-->") {
            Some(end) => remaining = &remaining[start + end + "-->".len()..],
            None => return stripped,
        }
    }
    stripped.push_str(remaining);
    stripped
}

fn hash_changes(changes: &BTreeMap<PathBuf, Option<String>>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memory-v2-consolidation-plan\0");
    for (path, content) in changes {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        if let Some(content) = content {
            hasher.update(content.as_bytes());
        }
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

fn deterministic_operation_id(observations: &[ClaimedObservation]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memory-v2-dream\0");
    for observation in observations {
        hasher.update(observation.relative_path.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(observation.content_hash.as_bytes());
        hasher.update(b"\0");
    }
    format!("dream_{}", &hasher.finalize().to_hex()[..24])
}

fn copy_idempotently(source: &Path, destination: &Path, expected_hash: &str) -> Result<()> {
    if destination.exists() {
        let bytes = read_bounded(destination, MAX_OBSERVATION_BYTES)?;
        if content_hash(&bytes) != expected_hash {
            return Err(V2ConsolidationError::Conflict(format!(
                "archive destination differs: {}",
                destination.display()
            )));
        }
        let source_bytes = read_bounded(source, MAX_OBSERVATION_BYTES)?;
        if source_bytes != bytes {
            return Err(V2ConsolidationError::Conflict(format!(
                "archive source differs: {}",
                source.display()
            )));
        }
        return Ok(());
    }
    let bytes = read_bounded(source, MAX_OBSERVATION_BYTES)?;
    if content_hash(&bytes) != expected_hash {
        return Err(V2ConsolidationError::Conflict(format!(
            "archive source hash differs: {}",
            source.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        V2ConsolidationError::Invalid("archive destination has no parent".to_owned())
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| io_error(destination, error))?;
    std::io::Write::write_all(&mut temporary, &bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| io_error(destination, error))?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| io_error(destination, error.error))?;
    Ok(())
}

fn remove_idempotently(source: &Path, expected_hash: &str) -> Result<()> {
    let bytes = match read_bounded(source, MAX_OBSERVATION_BYTES) {
        Ok(bytes) => bytes,
        Err(V2ConsolidationError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if content_hash(&bytes) != expected_hash {
        return Err(V2ConsolidationError::Conflict(format!(
            "archive source hash differs: {}",
            source.display()
        )));
    }
    std::fs::remove_file(source).map_err(|error| io_error(source, error))
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(V2ConsolidationError::Invalid(format!(
            "{} is not a bounded regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .and_then(|mut file| {
            std::io::Read::by_ref(&mut file)
                .take(max_bytes + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| io_error(path, error))?;
    Ok(bytes)
}

fn validate_text(name: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        Err(V2ConsolidationError::Invalid(format!(
            "{name} must contain 1..={max_bytes} safe bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: i64) -> Result<()> {
    if value < 0 {
        Err(V2ConsolidationError::Invalid(
            "timestamp cannot be negative".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| V2ConsolidationError::Invalid("path is not valid UTF-8".to_owned()))
}

fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn io_error(path: impl AsRef<Path>, source: std::io::Error) -> V2ConsolidationError {
    V2ConsolidationError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
#[path = "v2_consolidation_tests.rs"]
mod tests;
