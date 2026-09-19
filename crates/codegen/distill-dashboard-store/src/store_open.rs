// Modified for Distill by Samuel Fajreldines, 2026.
//! Store creation, connection opening, permission setup, and initial schema gating.

use std::path::{Path, PathBuf};
use std::time::Instant;

use distill_sqlite_journal::JournalMode;

use super::WorkspaceStore;
use crate::error::{Result, classify_open_error};
use crate::owner_only::{create_owner_only, sibling_path, tighten_owner_only};
use crate::schema::{self, SchemaInit, USER_VERSION, read_user_version};
use crate::types::SchemaState;

impl WorkspaceStore {
    /// Create-or-open the store at `db_path`.
    /// [`crate::StoreError::Unusable`] when the file is not a SQLite database (never deleted or recreated),
    /// [`crate::StoreError::Io`] on directory/mode failures or when the path is not a regular file,
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            distill_config::create_dir_all_owner_only(parent)?;
        }
        let mode = JournalMode::for_db_path(db_path);
        let effective = mode.effective_db_path(db_path);
        // O_CREAT|O_EXCL with mode 0o600 runs before SQLite's first open, so the file is never visible at umask defaults
        // SQLite accepts a zero-length file as a fresh database
        // Existing paths are validated without following symlinks
        create_owner_only(&effective)?;
        let opened_at = Instant::now();
        let mut conn = mode
            .open(db_path)
            .map_err(|error| classify_open_error(error.into(), opened_at, &effective))?;
        tighten_owner_only(&effective)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            // Newly created siblings inherit 0600 from the file; this covers ones an earlier binary created while the database was loose
            tighten_owner_only(&sibling_path(&effective, suffix))?;
        }

        // Autocommit fast-path read: a file already known to be newer is gated without ever taking the write lock
        // The authoritative re-read happens inside init_schema's transaction
        let found = read_user_version(&conn)
            .map_err(|error| classify_open_error(error, opened_at, &effective))?;
        if found > USER_VERSION {
            return Self::open_newer_schema(conn, effective, found, opened_at);
        }
        match schema::init_schema(&mut conn)
            .map_err(|error| classify_open_error(error, opened_at, &effective))?
        {
            SchemaInit::Newer { user_version } => {
                Self::open_newer_schema(conn, effective, user_version, opened_at)
            }
            SchemaInit::Ready { created } => {
                if created {
                    tracing::info!(
                        path = %effective.display(),
                        journal_mode = mode.as_ref(),
                        user_version = USER_VERSION,
                        "workspace store created"
                    );
                } else {
                    let member_count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM members", [], |r| r.get(0))
                        .map_err(|error| {
                            classify_open_error(error.into(), opened_at, &effective)
                        })?;
                    tracing::debug!(
                        path = %effective.display(),
                        member_count,
                        "workspace store opened"
                    );
                }
                Ok(Self {
                    conn,
                    schema: SchemaState::Current,
                    path: effective,
                })
            }
        }
    }

    /// Finish an open of a file written by a newer grok: gate at the connection so even a bug in this crate cannot write.
    /// Reads remain available while the newer schema is read-compatible.
    fn open_newer_schema(
        conn: rusqlite::Connection,
        effective: PathBuf,
        found: u32,
        opened_at: Instant,
    ) -> Result<Self> {
        conn.pragma_update(None, "query_only", true)
            .map_err(|error| classify_open_error(error.into(), opened_at, &effective))?;
        tracing::warn!(
            path = %effective.display(),
            found,
            supported = USER_VERSION,
            "workspace store written by a newer grok; opening read-only"
        );
        Ok(Self {
            conn,
            schema: SchemaState::NewerReadOnly {
                user_version: found,
            },
            path: effective,
        })
    }
}

#[cfg(test)]
#[path = "store_open_tests.rs"]
mod tests;
