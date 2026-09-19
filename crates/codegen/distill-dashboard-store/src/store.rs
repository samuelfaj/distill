//! The workspace store: connection ownership and all store operations.
//! The open/create flow lives in `store_open`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::error::{Result, StoreError, classify_busy, classify_unusable};
use crate::schema::{USER_VERSION, read_user_version};
use crate::types::{
    Grouping, InsertOutcome, LayoutApplyOutcome, LayoutPatch, Member, MemberKey, MemberKind,
    MemberMetadata, MemberOrigin, NewMember, PinAssignment, RANK_GAP, RankAssignment, RekeyOutcome,
    RemoveOutcome, SchemaState, SessionId, ValidatedMetadataRef, WORKSPACE_CAPACITY,
    WorkspaceSnapshot,
};

const MEMBER_EXISTS_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM members WHERE session_id = ?1 AND kind = ?2)";

fn read_snapshot(conn: &rusqlite::Connection) -> Result<WorkspaceSnapshot> {
    let grouping: String = conn.query_row("SELECT grouping FROM meta WHERE id = 0", [], |row| {
        row.get(0)
    })?;
    let mut stmt = conn.prepare(
        "SELECT session_id, kind, origin, cwd, title, model, last_turn_summary,
                is_worktree, last_change_unix_ms, pin_rank, order_rank
         FROM members ORDER BY session_id, kind",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Member {
            session_id: SessionId::new(row.get::<_, String>(0)?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            kind: MemberKind::from_stored(row.get(1)?),
            origin: MemberOrigin::from_stored(row.get(2)?),
            cwd: row.get(3)?,
            title: row.get(4)?,
            model: row.get(5)?,
            last_turn_summary: row.get(6)?,
            is_worktree: row.get(7)?,
            last_change_unix_ms: row.get(8)?,
            pin_rank: row.get(9)?,
            order_rank: row.get(10)?,
        })
    })?;
    let members = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let data_version: i64 = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    Ok(WorkspaceSnapshot {
        grouping: Grouping::from_stored(grouping),
        members,
        data_version,
    })
}

// The conflict arm touches metadata columns only: origin and the rank columns are absent, so adopting an existing member can never rewrite them
const UPSERT_MEMBER_SQL: &str = "
INSERT INTO members (session_id, kind, origin, cwd, title, model,
                     last_turn_summary, is_worktree, last_change_unix_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
ON CONFLICT(session_id, kind) DO UPDATE SET
    cwd = excluded.cwd, title = excluded.title, model = excluded.model,
    last_turn_summary = excluded.last_turn_summary,
    is_worktree = excluded.is_worktree,
    last_change_unix_ms = excluded.last_change_unix_ms";

// The delete uses a row-value IN, not `DELETE ... LIMIT`, which needs a non-default SQLite compile flag.
// Ties on last_change_unix_ms break on (session_id, kind) ascending, so two processes racing the same insert converge on the same victim
// An `:excess` above 1 heals an overfull file
const EVICT_SQL: &str = "
DELETE FROM members
WHERE (session_id, kind) IN (
    SELECT session_id, kind FROM members
    WHERE pin_rank IS NULL
    ORDER BY last_change_unix_ms ASC, session_id ASC, kind ASC
    LIMIT :excess
)
RETURNING session_id, kind, last_change_unix_ms";

struct EvictedRow {
    session_id: String,
    kind: String,
    last_change_unix_ms: i64,
}

enum InsertTransactionOutcome {
    UpdatedExisting,
    Inserted {
        victims: Vec<EvictedRow>,
        remaining_overage: i64,
        previous_count: i64,
    },
}

enum WriteAttempt<T> {
    Committed(T),
    RolledBack(StoreError),
    Failed(StoreError),
}

/// Owns the single connection. `Send` but not `Sync` (`rusqlite::Connection`), with no interior locks.
/// A caller that must not create the store must not call it.
#[derive(Debug)]
pub struct WorkspaceStore {
    pub(super) conn: rusqlite::Connection,
    pub(super) schema: SchemaState,
    pub(super) path: PathBuf,
}

impl WorkspaceStore {
    /// The handle's current schema-gate state.
    /// A guarded write can transition it to [`SchemaState::NewerReadOnly`] after a peer upgrades the store.
    pub fn schema_state(&self) -> SchemaState {
        self.schema
    }

    /// The effective (per-host on network mounts) path actually opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One consistent view: grouping, members in primary-key order, and the `data_version` observed by the same read transaction.
    /// Works in both schema states.
    /// [`StoreError::Busy`] when the busy budget elapses, [`StoreError::Sqlite`] otherwise.
    pub fn snapshot(&self) -> Result<WorkspaceSnapshot> {
        let started = Instant::now();
        self.snapshot_inner()
            .map_err(|e| classify_busy(e, "snapshot", started))
    }

    fn snapshot_inner(&self) -> Result<WorkspaceSnapshot> {
        let tx = self.conn.unchecked_transaction()?;
        let snapshot = read_snapshot(&tx)?;
        tx.commit()?;
        Ok(snapshot)
    }

    /// `PRAGMA data_version` on this store's connection, in autocommit and never inside a held transaction, so WAL snapshot pinning cannot freeze it.
    /// The value changes between two reads on a connection exactly when another connection committed to the database in the interim; Commits made on the same connection do not change the value that connection observes. A process that both writes and polls through one handle sees only *foreign* changes and never has to filter out its own; The value is only meaningful compared against a previous read on the same connection. After a reopen, re-seed the baseline from a fresh [`Self::snapshot`].
    /// [`StoreError::Busy`] when the busy budget elapses, [`StoreError::Sqlite`] otherwise.
    pub fn data_version(&self) -> Result<i64> {
        let started = Instant::now();
        self.conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .map_err(StoreError::from)
            .map_err(|e| classify_busy(e, "data_version", started))
    }

    /// Insert a member, or update the metadata columns of an existing one (never its `origin` or ranks).
    /// [`StoreError::NewerSchema`] on a read-only handle,
    pub fn insert_member(&mut self, member: NewMember) -> Result<InsertOutcome> {
        let NewMember {
            key,
            origin,
            metadata,
        } = member;
        let started = Instant::now();
        let outcome = self
            .insert_member_inner(&key, &origin, &metadata)
            .map_err(|e| classify_busy(e, "insert_member", started))?;
        match &outcome {
            InsertOutcome::Inserted | InsertOutcome::InsertedEvicting(_) => tracing::debug!(
                session_id = %key.session_id,
                kind = key.kind.as_str(),
                origin = origin.as_str(),
                "workspace member inserted"
            ),
            InsertOutcome::UpdatedExisting => tracing::trace!(
                session_id = %key.session_id,
                kind = key.kind.as_str(),
                "workspace member metadata updated"
            ),
        }
        Ok(outcome)
    }

    fn insert_member_inner(
        &mut self,
        key: &MemberKey,
        origin: &MemberOrigin,
        metadata: &MemberMetadata,
    ) -> Result<InsertOutcome> {
        let transaction = self.with_write("insert_member", |tx| {
            let metadata = metadata.validated(&key.kind)?;
            let exists: bool = tx.query_row(
                MEMBER_EXISTS_SQL,
                params![key.session_id.as_ref(), key.kind.as_str()],
                |r| r.get(0),
            )?;
            if exists {
                upsert_member(tx, key, origin, metadata)?;
                return Ok(InsertTransactionOutcome::UpdatedExisting);
            }
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM members", [], |r| r.get(0))?;
            let capacity = WORKSPACE_CAPACITY as i64;
            let mut victims = Vec::new();
            let mut remaining_overage = 0;
            if count >= capacity {
                let unpinned: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM members WHERE pin_rank IS NULL",
                    [],
                    |r| r.get(0),
                )?;
                if unpinned == 0 {
                    tracing::warn!(
                        capacity = WORKSPACE_CAPACITY,
                        "workspace is full and every member is pinned; insert refused"
                    );
                    return Err(StoreError::AllPinned {
                        capacity: WORKSPACE_CAPACITY,
                    });
                }
                let excess = count - (capacity - 1);
                let mut stmt = tx.prepare(EVICT_SQL)?;
                let rows = stmt.query_map(rusqlite::named_params! {":excess": excess}, |r| {
                    Ok(EvictedRow {
                        session_id: r.get(0)?,
                        kind: r.get(1)?,
                        last_change_unix_ms: r.get(2)?,
                    })
                })?;
                for row in rows {
                    victims.push(row?);
                }
                drop(stmt);
                remaining_overage = (excess - unpinned).max(0);
            }
            upsert_member(tx, key, origin, metadata)?;
            Ok(InsertTransactionOutcome::Inserted {
                victims,
                remaining_overage,
                previous_count: count,
            })
        })?;
        let InsertTransactionOutcome::Inserted {
            victims,
            remaining_overage,
            previous_count,
        } = transaction
        else {
            return Ok(InsertOutcome::UpdatedExisting);
        };

        // Eviction is silent in the UI by product decision, so these log lines are the only trace of the data removal
        // They are emitted only after the commit, so a rollback cannot leave a false destruction record
        let eviction_ran = !victims.is_empty();
        let mut evicted = Vec::with_capacity(victims.len());
        for victim in victims {
            let session_id = SessionId::new(victim.session_id)?;
            let kind = MemberKind::from_stored(victim.kind);
            tracing::info!(
                session_id = %session_id,
                kind = kind.as_str(),
                last_change_unix_ms = victim.last_change_unix_ms,
                capacity = WORKSPACE_CAPACITY,
                "evicted least-recently-changed workspace member"
            );
            evicted.push(MemberKey { session_id, kind });
        }
        if remaining_overage > 0 {
            // Refusing here would block a user action to make up for a past bug
            // Pinned members stay exempt, so the overage drains as members are unpinned or removed
            tracing::warn!(
                capacity = WORKSPACE_CAPACITY,
                count = previous_count,
                remaining_overage,
                "overfull workspace store healed partially; every unpinned member evicted"
            );
        }
        Ok(if eviction_ran {
            InsertOutcome::InsertedEvicting(evicted)
        } else {
            InsertOutcome::Inserted
        })
    }

    /// Update the metadata columns of an existing member.
    /// Cannot touch `origin` or the rank columns: the parameter type does not carry them.
    /// [`StoreError::NewerSchema`] on a read-only handle,
    pub fn update_member_metadata(
        &mut self,
        key: &MemberKey,
        metadata: &MemberMetadata,
    ) -> Result<()> {
        let started = Instant::now();
        self.with_write("update_member_metadata", |tx| {
            let metadata = metadata.validated(&key.kind)?;
            let changed = tx.execute(
                "UPDATE members SET cwd = ?1, title = ?2, model = ?3,
                        last_turn_summary = ?4, is_worktree = ?5,
                        last_change_unix_ms = ?6
                 WHERE session_id = ?7 AND kind = ?8",
                params![
                    metadata.cwd,
                    metadata.title,
                    metadata.model,
                    metadata.last_turn_summary,
                    metadata.is_worktree,
                    metadata.last_change_unix_ms,
                    key.session_id.as_ref(),
                    key.kind.as_str(),
                ],
            )?;
            if changed == 0 {
                return Err(member_not_found(key));
            }
            Ok(())
        })
        .map_err(|e| classify_busy(e, "update_member_metadata", started))?;
        tracing::trace!(
            session_id = %key.session_id,
            kind = key.kind.as_str(),
            "workspace member metadata updated"
        );
        Ok(())
    }

    /// Remove a member.
    /// Idempotent: an already-removed member returns [`RemoveOutcome::NotPresent`], never an error.
    /// [`StoreError::NewerSchema`] on a read-only handle, [`StoreError::Busy`] / [`StoreError::Sqlite`] from the database.
    pub fn remove_member(&mut self, key: &MemberKey) -> Result<RemoveOutcome> {
        let started = Instant::now();
        let changed = self
            .with_write("remove_member", |tx| {
                Ok(tx.execute(
                    "DELETE FROM members WHERE session_id = ?1 AND kind = ?2",
                    params![key.session_id.as_ref(), key.kind.as_str()],
                )?)
            })
            .map_err(|e| classify_busy(e, "remove_member", started))?;
        let outcome = if changed == 0 {
            RemoveOutcome::NotPresent
        } else {
            RemoveOutcome::Removed
        };
        tracing::info!(
            session_id = %key.session_id,
            kind = key.kind.as_str(),
            ?outcome,
            "workspace member removed"
        );
        Ok(outcome)
    }

    /// Applies all layout fields atomically; rejection includes a fresh post-rollback snapshot when available.
    pub fn apply_layout_patch(&mut self, patch: &LayoutPatch) -> LayoutApplyOutcome {
        let patch = match normalized_layout_patch(patch) {
            Ok(patch) => patch,
            Err(error) => return LayoutApplyOutcome::Failed { error },
        };

        let started = Instant::now();
        let result = self.with_write_attempt("apply_layout_patch", |tx| {
            let mut pin_stmt = tx.prepare(
                "UPDATE members SET pin_rank = ?1
                 WHERE session_id = ?2 AND kind = ?3",
            )?;
            for assignment in &patch.pin_assignments {
                let rank = assignment.pinned.then_some(RANK_GAP);
                let changed = pin_stmt.execute(params![
                    rank,
                    assignment.key.session_id.as_ref(),
                    assignment.key.kind.as_str(),
                ])?;
                if changed == 0 {
                    return Err(member_not_found(&assignment.key));
                }
            }
            drop(pin_stmt);

            if let Some(manual_order) = &patch.manual_order {
                tx.execute(
                    "UPDATE members SET order_rank = NULL WHERE kind = ?1",
                    params![MemberKind::Build.as_str()],
                )?;
                let mut order_stmt = tx.prepare(
                    "UPDATE members SET order_rank = ?1
                     WHERE session_id = ?2 AND kind = ?3",
                )?;
                for (index, key) in manual_order.iter().enumerate() {
                    let rank = i64::try_from(index + 1)
                        .ok()
                        .and_then(|value| value.checked_mul(RANK_GAP))
                        .ok_or(StoreError::InvalidLayoutPatch {
                            reason: "manual order rank overflow",
                        })?;
                    let changed = order_stmt.execute(params![
                        rank,
                        key.session_id.as_ref(),
                        key.kind.as_str(),
                    ])?;
                    if changed == 0 {
                        return Err(member_not_found(key));
                    }
                }
            }

            if let Some(grouping) = &patch.grouping {
                tx.execute(
                    "UPDATE meta SET grouping = ?1 WHERE id = 0",
                    params![grouping.as_ref()],
                )?;
            }
            read_snapshot(tx)
        });

        match result {
            WriteAttempt::Committed(snapshot) => LayoutApplyOutcome::Committed(snapshot),
            WriteAttempt::RolledBack(error) => {
                let error = classify_layout_error(error, started);
                match self.snapshot() {
                    Ok(snapshot) => LayoutApplyOutcome::Rejected { error, snapshot },
                    Err(error) => LayoutApplyOutcome::Failed {
                        error: classify_layout_snapshot_error(error),
                    },
                }
            }
            WriteAttempt::Failed(error) => LayoutApplyOutcome::Failed {
                error: classify_layout_error(error, started),
            },
        }
    }

    /// Write `pin_rank` for every assignment in one all-or-nothing transaction.
    /// The API takes a batch because renumbering a partition whose rank gap ran out must be atomic; a pin toggle passes one element.
    /// [`StoreError::NewerSchema`] on a read-only handle,
    pub fn set_pin_rank(&mut self, assignments: &[RankAssignment]) -> Result<()> {
        self.set_ranks(RankColumn::Pin, assignments)
    }

    /// Write `order_rank` for every assignment; same contract as [`Self::set_pin_rank`].
    /// Same as [`Self::set_pin_rank`].
    pub fn set_order_rank(&mut self, assignments: &[RankAssignment]) -> Result<()> {
        self.set_ranks(RankColumn::Order, assignments)
    }

    fn set_ranks(&mut self, column: RankColumn, assignments: &[RankAssignment]) -> Result<()> {
        let started = Instant::now();
        self.set_ranks_inner(column, assignments)
            .map_err(|e| classify_busy(e, column.op_name(), started))?;
        tracing::debug!(
            column = column.column_name(),
            count = assignments.len(),
            "workspace ranks written"
        );
        Ok(())
    }

    fn set_ranks_inner(
        &mut self,
        column: RankColumn,
        assignments: &[RankAssignment],
    ) -> Result<()> {
        self.with_write(column.op_name(), |tx| {
            let mut stmt = tx.prepare(column.update_sql())?;
            for assignment in assignments {
                let changed = stmt.execute(params![
                    assignment.rank,
                    assignment.key.session_id.as_ref(),
                    assignment.key.kind.as_str(),
                ])?;
                if changed == 0 {
                    return Err(member_not_found(&assignment.key));
                }
            }
            Ok(())
        })
    }

    /// Persist the workspace-wide grouping preference.
    /// [`StoreError::NewerSchema`] on a read-only handle, [`StoreError::Busy`] / [`StoreError::Sqlite`] from the database.
    pub fn set_grouping(&mut self, grouping: &Grouping) -> Result<()> {
        let started = Instant::now();
        self.with_write("set_grouping", |tx| {
            // Schema init seeds the meta row, so the UPDATE always matches row 0
            tx.execute(
                "UPDATE meta SET grouping = ?1 WHERE id = 0",
                params![grouping.as_str()],
            )?;
            Ok(())
        })
        .map_err(|e| classify_busy(e, "set_grouping", started))?;
        tracing::debug!(grouping = grouping.as_str(), "workspace grouping changed");
        Ok(())
    }

    /// Move a member to a new session id in one transaction, ranks included.
    /// When a row with the target key already exists, the two merge deterministically: the target keeps its `origin` and metadata as the live truth.
    /// [`StoreError::NewerSchema`] on a read-only handle,
    pub fn rekey(&mut self, old: &MemberKey, new_session_id: SessionId) -> Result<RekeyOutcome> {
        let started = Instant::now();
        let outcome = self
            .rekey_inner(old, &new_session_id)
            .map_err(|e| classify_busy(e, "rekey", started))?;
        tracing::info!(
            old_session_id = %old.session_id,
            new_session_id = %new_session_id,
            kind = old.kind.as_str(),
            ?outcome,
            "workspace member rekeyed"
        );
        Ok(outcome)
    }

    fn rekey_inner(&mut self, old: &MemberKey, new_session_id: &SessionId) -> Result<RekeyOutcome> {
        self.with_write("rekey", |tx| {
            let old_ranks: Option<(Option<i64>, Option<i64>)> = tx
                .query_row(
                    "SELECT pin_rank, order_rank FROM members WHERE session_id = ?1 AND kind = ?2",
                    params![old.session_id.as_ref(), old.kind.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((old_pin, old_order)) = old_ranks else {
                return Err(member_not_found(old));
            };
            // Short-circuit before the merge arm: there the "target" would be the old row itself and the final delete would destroy it
            if old.session_id == *new_session_id {
                return Ok(RekeyOutcome::NoChange);
            }
            let target_exists: bool = tx.query_row(
                MEMBER_EXISTS_SQL,
                params![new_session_id.as_ref(), old.kind.as_str()],
                |r| r.get(0),
            )?;
            if target_exists {
                tx.execute(
                    "UPDATE members SET
                         pin_rank   = COALESCE(pin_rank, ?1),
                         order_rank = COALESCE(order_rank, ?2)
                     WHERE session_id = ?3 AND kind = ?4",
                    params![
                        old_pin,
                        old_order,
                        new_session_id.as_ref(),
                        old.kind.as_str()
                    ],
                )?;
                tx.execute(
                    "DELETE FROM members WHERE session_id = ?1 AND kind = ?2",
                    params![old.session_id.as_ref(), old.kind.as_str()],
                )?;
                Ok(RekeyOutcome::MergedIntoExisting)
            } else {
                tx.execute(
                    "UPDATE members SET session_id = ?1 WHERE session_id = ?2 AND kind = ?3",
                    params![
                        new_session_id.as_ref(),
                        old.session_id.as_ref(),
                        old.kind.as_str()
                    ],
                )?;
                Ok(RekeyOutcome::Moved)
            }
        })
    }

    fn with_write<T>(
        &mut self,
        op: &'static str,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        match self.with_write_attempt(op, write) {
            WriteAttempt::Committed(value) => Ok(value),
            WriteAttempt::RolledBack(error) | WriteAttempt::Failed(error) => Err(error),
        }
    }

    fn with_write_attempt<T>(
        &mut self,
        op: &'static str,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> WriteAttempt<T> {
        if let Err(error) = self.check_writable(op) {
            return WriteAttempt::Failed(error);
        }
        let tx = match self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(tx) => tx,
            Err(error) => return WriteAttempt::Failed(error.into()),
        };
        let found = match read_user_version(&tx) {
            Ok(found) => found,
            Err(error) => {
                return match tx.rollback() {
                    Ok(()) => WriteAttempt::RolledBack(error),
                    Err(rollback_error) => WriteAttempt::Failed(rollback_error.into()),
                };
            }
        };
        if found > USER_VERSION {
            if let Err(error) = tx.rollback() {
                return WriteAttempt::Failed(error.into());
            }
            self.schema = SchemaState::NewerReadOnly {
                user_version: found,
            };
            if let Err(error) = self.conn.pragma_update(None, "query_only", true) {
                tracing::warn!(
                    op,
                    %error,
                    "workspace store schema gate is read-only but SQLite query_only could not be set"
                );
            }
            tracing::warn!(
                op,
                found,
                supported = USER_VERSION,
                "workspace store was upgraded by another process; switching to read-only"
            );
            return WriteAttempt::Failed(StoreError::NewerSchema {
                found,
                supported: USER_VERSION,
            });
        }

        match write(&tx) {
            Ok(value) => match tx.commit() {
                Ok(()) => WriteAttempt::Committed(value),
                Err(error) => WriteAttempt::Failed(error.into()),
            },
            Err(error) => match tx.rollback() {
                Ok(()) => WriteAttempt::RolledBack(error),
                Err(rollback_error) => WriteAttempt::Failed(rollback_error.into()),
            },
        }
    }

    fn check_writable(&self, op: &'static str) -> Result<()> {
        match self.schema {
            SchemaState::Current => Ok(()),
            SchemaState::NewerReadOnly { user_version } => {
                tracing::debug!(op, "write refused: workspace store handle is read-only");
                Err(StoreError::NewerSchema {
                    found: user_version,
                    supported: USER_VERSION,
                })
            }
        }
    }
}

fn normalized_layout_patch(patch: &LayoutPatch) -> Result<LayoutPatch> {
    let mut pin_values = HashMap::with_capacity(patch.pin_assignments.len());
    for assignment in &patch.pin_assignments {
        if assignment.key.kind != MemberKind::Build {
            return Err(StoreError::InvalidLayoutPatch {
                reason: "layout applies only to build members",
            });
        }
        pin_values.insert(assignment.key.clone(), assignment.pinned);
    }
    let mut pin_assignments = pin_values
        .into_iter()
        .map(|(key, pinned)| PinAssignment { key, pinned })
        .collect::<Vec<_>>();
    pin_assignments.sort_by(|left, right| {
        left.key
            .session_id
            .as_ref()
            .cmp(right.key.session_id.as_ref())
    });

    let manual_order = if let Some(manual_order) = &patch.manual_order {
        let mut order_keys = HashSet::with_capacity(manual_order.len());
        let mut normalized = Vec::with_capacity(manual_order.len());
        for key in manual_order {
            if key.kind != MemberKind::Build {
                return Err(StoreError::InvalidLayoutPatch {
                    reason: "layout applies only to build members",
                });
            }
            if order_keys.insert(key.clone()) {
                normalized.push(key.clone());
            }
        }
        Some(normalized)
    } else {
        None
    };

    Ok(LayoutPatch {
        pin_assignments,
        manual_order,
        grouping: patch.grouping,
    })
}

fn classify_layout_error(error: StoreError, started: Instant) -> StoreError {
    let error = match error {
        StoreError::Sqlite(error) => classify_unusable(error),
        other => other,
    };
    classify_busy(error, "apply_layout_patch", started)
}

fn classify_layout_snapshot_error(error: StoreError) -> StoreError {
    match error {
        StoreError::Sqlite(error) => classify_unusable(error),
        other => other,
    }
}

/// The two gapped-rank columns; both setters share this code because gap exhaustion (and its atomic renumber) applies to either.
#[derive(Clone, Copy)]
enum RankColumn {
    Pin,
    Order,
}

impl RankColumn {
    fn column_name(self) -> &'static str {
        match self {
            Self::Pin => "pin_rank",
            Self::Order => "order_rank",
        }
    }

    fn op_name(self) -> &'static str {
        match self {
            Self::Pin => "set_pin_rank",
            Self::Order => "set_order_rank",
        }
    }

    fn update_sql(self) -> &'static str {
        match self {
            Self::Pin => "UPDATE members SET pin_rank = ?1 WHERE session_id = ?2 AND kind = ?3",
            Self::Order => "UPDATE members SET order_rank = ?1 WHERE session_id = ?2 AND kind = ?3",
        }
    }
}

fn upsert_member(
    tx: &rusqlite::Transaction<'_>,
    key: &MemberKey,
    origin: &MemberOrigin,
    metadata: ValidatedMetadataRef<'_>,
) -> rusqlite::Result<()> {
    tx.execute(
        UPSERT_MEMBER_SQL,
        params![
            key.session_id.as_ref(),
            key.kind.as_str(),
            origin.as_str(),
            metadata.cwd,
            metadata.title,
            metadata.model,
            metadata.last_turn_summary,
            metadata.is_worktree,
            metadata.last_change_unix_ms,
        ],
    )?;
    Ok(())
}

fn member_not_found(key: &MemberKey) -> StoreError {
    StoreError::MemberNotFound {
        session_id: key.session_id.to_string(),
        kind: key.kind.as_str().to_owned(),
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
