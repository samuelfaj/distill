//! Schema v1 DDL and initialization.
//!
//! # Compatibility policy
//!
//! - `user_version` gates **breaking** changes only: a file with a higher version opens read-only (see [`crate::SchemaState`]).
//!   Reads remain available only while the newer schema is read-compatible; writes are always refused.
//! - Additive evolution does not bump the version: newer binaries add nullable columns (with defaults).
//!   They must treat NULL or the default in an added column as "written by an older binary".
//!   Older binaries keep writing because every statement in this crate names its columns.
//!   Decode indexes refer to the explicit projection, never table order.
//! - Bump [`USER_VERSION`] only for changes an older binary's *writes* would corrupt: renamed or retyped columns, a changed meaning of keys or ranks.
//!   A newer binary opening an older file runs its in-place migration inside one IMMEDIATE transaction in [`init_schema`] and stamps the new version.
//!   v1 has no migrations, so today the only step below v1 is the idempotent create.
//!
//! # No secondary indexes, deliberately
//!
//! The table is hard-capped at [`crate::WORKSPACE_CAPACITY`] rows, so the eviction scan and any snapshot sort stay small.
//! An index on `last_change_unix_ms` would tax the hottest write (the per-turn metadata sync updates that column) for no measurable read gain.

use rusqlite::TransactionBehavior;

use crate::error::{Result, StoreError};

/// Schema version stamped via `PRAGMA user_version` when the store is created (or migrated).
pub const USER_VERSION: u32 = 1;

// Every statement is a no-op when its object/row already exists
// STRICT works because the bundled SQLite is at least 3.50 everywhere; typed columns reject a wrongly typed value at the storage layer
// The meta row is seeded here so `set_grouping` is always an UPDATE of row 0, and the CHECK constraint makes the one-row invariant structural
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS members (
    session_id          TEXT    NOT NULL,
    kind                TEXT    NOT NULL,
    origin              TEXT    NOT NULL,
    cwd                 TEXT,
    title               TEXT,
    model               TEXT,
    last_turn_summary   TEXT,
    is_worktree         INTEGER NOT NULL DEFAULT 0,
    last_change_unix_ms INTEGER NOT NULL,
    pin_rank            INTEGER,
    order_rank          INTEGER,
    PRIMARY KEY (session_id, kind),
    CHECK (kind <> 'build' OR cwd IS NOT NULL)
) STRICT;

CREATE TABLE IF NOT EXISTS meta (
    id       INTEGER PRIMARY KEY CHECK (id = 0),
    grouping TEXT NOT NULL DEFAULT 'state'
) STRICT;

INSERT OR IGNORE INTO meta(id, grouping) VALUES (0, 'state');
";

pub(crate) fn read_user_version(conn: &rusqlite::Connection) -> Result<u32> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    u32::try_from(found).map_err(|_| StoreError::Unusable {
        source: rusqlite::Error::IntegralValueOutOfRange(0, found),
    })
}

/// What [`init_schema`] found and did.
pub(crate) enum SchemaInit {
    /// Schema objects exist at the supported version.
    /// `created` reports whether this open stamped the version (first init or a pre-schema file).
    Ready { created: bool },
    /// The in-transaction re-read found a newer file; nothing was written.
    Newer { user_version: u32 },
}

/// Idempotent create-or-heal of schema v1 inside one IMMEDIATE transaction.
/// `PRAGMA user_version` is re-read under the write lock because the caller's gate read is autocommit.
/// A healthy reopen therefore commits zero pages and never takes the write lock for real.
pub(crate) fn init_schema(conn: &mut rusqlite::Connection) -> Result<SchemaInit> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = read_user_version(&tx)?;
    if found > USER_VERSION {
        return Ok(SchemaInit::Newer {
            user_version: found,
        });
    }
    tx.execute_batch(SCHEMA_SQL)?;
    if found < USER_VERSION {
        tx.pragma_update(None, "user_version", USER_VERSION)?;
    }
    tx.commit()?;
    Ok(SchemaInit::Ready {
        created: found < USER_VERSION,
    })
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
