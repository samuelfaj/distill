// Modified for Distill by Samuel Fajreldines, 2026.
//! SQLite-backed FTS5 index for session search.
//!
//! Modelled after the memory system's `MemoryIndex` and `schema.rs`, but purpose-built for searching across *sessions* (titles and user prompts).
//!
//! ## Schema
//!
//! - `meta`: key-value metadata (schema version, bootstrap marker/claim)
//! - `session_docs`: one row per session (title, content, content_hash)
//! - `session_docs_fts`: content-synced FTS5 over title and content (not cwd)
//!
//! FTS is kept in sync with `session_docs` via `AFTER INSERT/UPDATE/DELETE` triggers so callers never need to touch the FTS table directly.
//! The `cwd` column is intentionally excluded from the FTS table: it is a filter dimension only, applied via JOIN on `session_docs`.
//!
//! The index is a rebuildable cache: an unusable file is quarantined and recreated once (see the crate's `recovery` module and [`with_index`]).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use distill_sqlite_journal::JournalMode;

use crate::recovery;

/// Bump when making breaking schema changes that require dropping and recreating tables, or to force a rebuild of stale index content.
/// v3 to v4 forced a rebuild because messages with JSON escapes were silently dropped at indexing.
const SCHEMA_VERSION: &str = "4";

/// Lease stamp for the bootstrap claim, held while a reindex runs, stored as `"{unix_secs}:{owner_token}"`.
/// `CAST` reads the numeric prefix, and the token fences refresh/release to the owner.
pub(crate) const META_KEY_BOOTSTRAP_CLAIM: &str = "bootstrap_claimed_at";

/// Unix seconds of the last completed full reindex; its presence is the completed-bootstrap marker.
pub(crate) const META_KEY_LAST_BOOTSTRAP: &str = "last_bootstrap_at";

/// On-disk schema version row; bump [`SCHEMA_VERSION`] to force a rebuild.
pub(crate) const META_KEY_SCHEMA_VERSION: &str = "session_search_schema_version";

/// SQL that extracts the owner token from a claim stamp; the single source for every fenced statement, paired with [`claim_stamp`].
const CLAIM_TOKEN_SQL: &str = "substr(value, instr(value, ':') + 1)";

fn claim_stamp(now_unix: i64, token: &str) -> String {
    format!("{now_unix}:{token}")
}

/// A document to be indexed for session search.
#[derive(Debug, Clone)]
pub struct SessionDoc {
    pub session_id: String,
    pub cwd: String,
    pub updated_at_unix: i64,
    pub title: String,
    /// Concatenated user prompts (the searchable body).
    pub content: String,
    /// blake3 hash of `content`, used to skip redundant upserts.
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct SessionSearchRow {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at_unix: i64,
    pub score: f32,
    pub matched_fields: Vec<String>,
    pub snippet: Option<String>,
}

/// Result of a `SessionSearchIndex::query()` call.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub results: Vec<SessionSearchRow>,
    pub next_offset: Option<usize>,
    pub total_estimate: Option<usize>,
}

/// Wraps a `rusqlite::Connection` pointing at `session_search.sqlite`.
pub struct SessionSearchIndex {
    db: Connection,
}

pub fn with_index<R>(
    db_path: &Path,
    op: impl Fn(&SessionSearchIndex) -> Result<R, rusqlite::Error>,
) -> Result<R, rusqlite::Error> {
    let index = SessionSearchIndex::open_or_create(db_path)?;
    match op(&index) {
        Ok(value) => Ok(value),
        Err(e) if recovery::is_unusable_db_error(&e) => {
            drop(index);
            recovery::heal_unusable(
                db_path,
                &e,
                SessionSearchIndex::probe_usable,
                SessionSearchIndex::recreate,
            );
            let index = SessionSearchIndex::open_or_create(db_path)?;
            op(&index)
        }
        Err(e) => Err(e),
    }
}

impl SessionSearchIndex {
    /// When the stored schema version is older than [`SCHEMA_VERSION`], drops and recreates all tables (the index can be
    /// rebuilt). A newer stored version is tolerated read/write without dropping. If the existing file is corrupt or not a
    /// database, quarantines it and opens a fresh empty index (see `recovery::heal_unusable`).
    pub fn open_or_create(db_path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            // The parent is usually the sessions root; never (re)create it with loose permissions
            let _ = distill_config::create_dir_all_owner_only(parent);
        }

        let journal_mode = JournalMode::for_db_path(db_path);

        match Self::open_with_journal_mode(db_path, journal_mode) {
            Ok(index) => Ok(index),
            Err(e) if recovery::is_unusable_db_error(&e) => {
                recovery::heal_unusable(db_path, &e, Self::probe_usable, Self::recreate);
                Self::open_with_journal_mode(db_path, journal_mode)
            }
            Err(e) => Err(e),
        }
    }

    fn probe_usable(db_path: &Path) -> Result<bool, rusqlite::Error> {
        let conn = JournalMode::for_db_path(db_path).open_readonly(db_path)?;
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let first: Option<String> = stmt.query_map([], |row| row.get(0))?.next().transpose()?;
        Ok(first.as_deref() == Some("ok"))
    }

    fn recreate(db_path: &Path) -> Result<(), rusqlite::Error> {
        Self::open_with_journal_mode(db_path, JournalMode::for_db_path(db_path)).map(|_| ())
    }

    fn open_with_journal_mode(
        db_path: &Path,
        journal_mode: JournalMode,
    ) -> Result<Self, rusqlite::Error> {
        // busy_timeout and the journal pragma live in the helper (see JournalMode::open)
        let mut db = journal_mode.open(db_path)?;

        let stored_version: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_KEY_SCHEMA_VERSION],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);

        // One-way ratchet: drop only on UPGRADE (stored < current). A newer index is safe to read: bumps regenerate content only
        // (the table schema is column-identical). The newer binary re-upserts any rows we write via content-hash mismatch `None`
        // means a fresh DB; a non-integer stored value is legacy or corrupt and parses as 0
        let current: u64 = SCHEMA_VERSION
            .parse()
            .expect("SCHEMA_VERSION is an integer");
        let stored: Option<u64> = stored_version.as_deref().map(|v| v.parse().unwrap_or(0));
        let owned_by_newer = stored.is_some_and(|s| s > current);
        if stored.is_some_and(|s| s < current) {
            // The marker and claim die with the tables A surviving marker reads as "bootstrap complete" over an empty index, and a
            // stale claim blocks the rebuild until the lease expires. Other `meta` keys are preserved. Immediate: a deferred begin
            // can fail with SQLITE_BUSY_SNAPSHOT, which skips the busy handler
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "
                DROP TRIGGER IF EXISTS session_docs_ai;
                DROP TRIGGER IF EXISTS session_docs_ad;
                DROP TRIGGER IF EXISTS session_docs_au;
                DROP TABLE IF EXISTS session_docs_fts;
                DROP TABLE IF EXISTS session_docs;
                ",
            )?;
            tx.execute(
                "DELETE FROM meta WHERE key IN (?1, ?2)",
                params![META_KEY_LAST_BOOTSTRAP, META_KEY_BOOTSTRAP_CLAIM],
            )?;
            tx.commit()?;
        } else if owned_by_newer {
            tracing::debug!(
                stored = stored.unwrap_or_default(),
                current,
                "session search index owned by a newer schema version; keeping tables"
            );
        }

        db.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS session_docs (
                session_id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL
            );

            -- FTS only indexes title + content (searchable columns).
            -- cwd is NOT in the FTS table — it is a filter dimension only,
            -- applied via JOIN on session_docs.
            CREATE VIRTUAL TABLE IF NOT EXISTS session_docs_fts USING fts5(
                title,
                content,
                content='session_docs',
                content_rowid='rowid'
            );

            -- Keep FTS in sync via triggers so callers only touch session_docs.
            CREATE TRIGGER IF NOT EXISTS session_docs_ai AFTER INSERT ON session_docs BEGIN
                INSERT INTO session_docs_fts(rowid, title, content)
                VALUES (new.rowid, new.title, new.content);
            END;

            CREATE TRIGGER IF NOT EXISTS session_docs_ad AFTER DELETE ON session_docs BEGIN
                INSERT INTO session_docs_fts(session_docs_fts, rowid, title, content)
                VALUES ('delete', old.rowid, old.title, old.content);
            END;

            CREATE TRIGGER IF NOT EXISTS session_docs_au AFTER UPDATE ON session_docs BEGIN
                INSERT INTO session_docs_fts(session_docs_fts, rowid, title, content)
                VALUES ('delete', old.rowid, old.title, old.content);
                INSERT INTO session_docs_fts(rowid, title, content)
                VALUES (new.rowid, new.title, new.content);
            END;
            ",
        )?;

        // Persist the schema version, but never regress the row a newer generation owns (it would re-trigger that binary's upgrade drop)
        if stored != Some(current) && !owned_by_newer {
            db.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
                params![META_KEY_SCHEMA_VERSION, SCHEMA_VERSION],
            )?;
        }

        Ok(Self { db })
    }

    /// The content-synced FTS triggers update `session_docs_fts` automatically.
    pub fn upsert_doc(&self, doc: &SessionDoc) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "INSERT INTO session_docs(session_id, cwd, updated_at, title, content, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                 cwd = excluded.cwd,
                 updated_at = excluded.updated_at,
                 title = excluded.title,
                 content = excluded.content,
                 content_hash = excluded.content_hash",
            params![
                doc.session_id,
                doc.cwd,
                doc.updated_at_unix,
                doc.title,
                doc.content,
                doc.content_hash
            ],
        )?;
        Ok(())
    }

    /// Insert a session document only if no row exists for its `session_id`. Atomic alternative to a check-then-insert: the
    /// index DB is shared across processes. A separate check and insert could clobber a full-content row written between
    /// them.
    pub fn insert_doc_if_absent(&self, doc: &SessionDoc) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "INSERT INTO session_docs(session_id, cwd, updated_at, title, content, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO NOTHING",
            params![
                doc.session_id,
                doc.cwd,
                doc.updated_at_unix,
                doc.title,
                doc.content,
                doc.content_hash
            ],
        )?;
        Ok(())
    }

    pub fn delete_doc(&self, session_id: &str) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "DELETE FROM session_docs WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Return the stored content_hash for a session, if any.
    ///
    /// Callers use it to skip redundant upserts when the content hasn't changed.
    pub fn get_content_hash(&self, session_id: &str) -> Result<Option<String>, rusqlite::Error> {
        self.db
            .query_row(
                "SELECT content_hash FROM session_docs WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, rusqlite::Error> {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    }

    /// Write a value to the `meta` key-value table (insert or replace).
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<(), rusqlite::Error> {
        self.db
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Returns `true` when this process claimed the bootstrap under `token`.
    /// An expired (older than `lease`), future-dated (clock rollback), or unparsable claim is taken over; a live peer claim is not.
    pub(crate) fn try_claim_bootstrap(
        &self,
        now_unix: i64,
        lease: Duration,
        token: &str,
    ) -> Result<bool, rusqlite::Error> {
        let lease_secs = lease.as_secs() as i64;
        // One upsert keeps the check-and-claim atomic across processes.
        let changed = self.db.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(meta.value AS INTEGER) <= ?3
                OR CAST(meta.value AS INTEGER) > ?4",
            params![
                META_KEY_BOOTSTRAP_CLAIM,
                claim_stamp(now_unix, token),
                now_unix.saturating_sub(lease_secs),
                now_unix.saturating_add(lease_secs),
            ],
        )?;
        Ok(changed == 1)
    }

    /// Re-stamp the lease.
    /// Fenced on `token`: returns `false` without writing when the claim is no longer ours (expired and taken over, or already released).
    /// So a stale claimant can never clobber a successor.
    pub(crate) fn refresh_bootstrap_claim(
        &self,
        now_unix: i64,
        token: &str,
    ) -> Result<bool, rusqlite::Error> {
        let changed = self.db.execute(
            &format!("UPDATE meta SET value = ?2 WHERE key = ?1 AND {CLAIM_TOKEN_SQL} = ?3"),
            params![
                META_KEY_BOOTSTRAP_CLAIM,
                claim_stamp(now_unix, token),
                token
            ],
        )?;
        Ok(changed == 1)
    }

    /// Write `key = value` only while the bootstrap claim is still held under `token`; returns `false` (no write) otherwise.
    pub(crate) fn set_meta_if_claim_owner(
        &self,
        key: &str,
        value: &str,
        token: &str,
    ) -> Result<bool, rusqlite::Error> {
        let changed = self.db.execute(
            &format!(
                "INSERT INTO meta(key, value)
                 SELECT ?1, ?2
                 WHERE EXISTS (
                     SELECT 1 FROM meta WHERE key = ?3 AND {CLAIM_TOKEN_SQL} = ?4
                 )
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value"
            ),
            params![key, value, META_KEY_BOOTSTRAP_CLAIM, token],
        )?;
        Ok(changed == 1)
    }

    /// Delete the claim, fenced on `token` so only the current owner frees it.
    /// Returns `false` when the claim was already released or taken over.
    pub(crate) fn release_bootstrap_claim(&self, token: &str) -> Result<bool, rusqlite::Error> {
        let changed = self.db.execute(
            &format!("DELETE FROM meta WHERE key = ?1 AND {CLAIM_TOKEN_SQL} = ?2"),
            params![META_KEY_BOOTSTRAP_CLAIM, token],
        )?;
        Ok(changed == 1)
    }

    /// Refresh the claim under `token` and delete indexed session ids not in `keep`, in one Immediate transaction.
    /// Returns `false` without deleting when this process is not the claim owner.
    pub(crate) fn prune_missing_if_claim_owner(
        &self,
        now_unix: i64,
        token: &str,
        keep: &std::collections::HashSet<String>,
    ) -> Result<bool, rusqlite::Error> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        if !self.refresh_bootstrap_claim(now_unix, token)? {
            return Ok(false);
        }
        for id in self.all_indexed_session_ids()? {
            if !keep.contains(&id) {
                self.delete_doc(&id)?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Every session id in the index; the reindex uses this to find and prune rows whose sessions are gone from disk.
    pub fn all_indexed_session_ids(&self) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self.db.prepare("SELECT session_id FROM session_docs")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    /// When that intersection matches nothing the query reruns as an OR, so partial matches still show up. A query shaped
    /// like a session id (a full UUID or a hyphenated hex prefix) matches `session_docs.session_id` directly. FTS only
    /// indexes title and content, and a hyphenated UUID `MATCH` looks for tokens that were never indexed.
    pub fn query(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
        offset: usize,
        include_content: bool,
    ) -> Result<QueryResult, rusqlite::Error> {
        if is_session_id_like(query) {
            let id_hits = self.query_session_id(query, cwd, limit, offset)?;
            if !id_hits.results.is_empty() || uuid::Uuid::try_parse(query.trim()).is_ok() {
                return Ok(id_hits);
            }
        }

        let Some((and_query, or_query)) = Self::build_match_queries(query) else {
            return Ok(QueryResult {
                results: Vec::new(),
                next_offset: None,
                total_estimate: Some(0),
            });
        };

        let result = self.run_match_query(&and_query, cwd, limit, offset, include_content)?;
        // Gate the fallback on the total (not the page) so every offset of one logical query is served by the same match string
        if result.total_estimate == Some(0) && and_query != or_query {
            return self.run_match_query(&or_query, cwd, limit, offset, include_content);
        }
        Ok(result)
    }

    /// Substring match on `session_docs.session_id` (not in the FTS table).
    fn query_session_id(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<QueryResult, rusqlite::Error> {
        let needle = query.trim();
        if needle.is_empty() {
            return Ok(QueryResult {
                results: Vec::new(),
                next_offset: None,
                total_estimate: Some(0),
            });
        }

        let total: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM session_docs d
             WHERE instr(lower(d.session_id), lower(?1)) > 0
               AND (?2 IS NULL OR d.cwd = ?2)",
            params![needle, cwd],
            |row| row.get(0),
        )?;

        let mut stmt = self.db.prepare(
            "SELECT d.session_id, d.cwd, d.title, d.updated_at
             FROM session_docs d
             WHERE instr(lower(d.session_id), lower(?1)) > 0
               AND (?2 IS NULL OR d.cwd = ?2)
             ORDER BY d.updated_at DESC, d.session_id ASC
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(params![needle, cwd, limit as i64, offset as i64], |row| {
            Ok(SessionSearchRow {
                session_id: row.get(0)?,
                cwd: row.get(1)?,
                title: row.get(2)?,
                updated_at_unix: row.get(3)?,
                score: 1.0,
                matched_fields: vec!["session_id".to_string()],
                snippet: None,
            })
        })?;
        let results: Vec<SessionSearchRow> = rows.collect::<Result<_, _>>()?;
        let total_usize = usize::try_from(total).unwrap_or(0);
        let next_offset = (offset + results.len() < total_usize).then_some(offset + results.len());
        Ok(QueryResult {
            results,
            next_offset,
            total_estimate: Some(total_usize),
        })
    }

    /// Execute one FTS5 MATCH string; `total_estimate` is computed with the same match string that produced the rows.
    fn run_match_query(
        &self,
        match_query: &str,
        cwd: Option<&str>,
        limit: usize,
        offset: usize,
        include_content: bool,
    ) -> Result<QueryResult, rusqlite::Error> {
        let total: i64 = self.db.query_row(
            "SELECT COUNT(*)
             FROM session_docs_fts
             JOIN session_docs d ON d.rowid = session_docs_fts.rowid
             WHERE session_docs_fts MATCH ?1
               AND (?2 IS NULL OR d.cwd = ?2)",
            params![match_query, cwd],
            |row| row.get(0),
        )?;

        let snippet_expr = if include_content {
            "snippet(session_docs_fts, 1, '[', ']', ' … ', 18)"
        } else {
            "NULL"
        };

        // BM25 weights: title 10.0, content 1.0
        let sql = format!(
            "SELECT
               d.session_id,
               d.cwd,
               d.title,
               d.updated_at,
               bm25(session_docs_fts, 10.0, 1.0) AS rank,
               {snippet_expr} AS snippet,
               highlight(session_docs_fts, 0, '\x01', '\x02') AS hl_title,
               highlight(session_docs_fts, 1, '\x01', '\x02') AS hl_content
             FROM session_docs_fts
             JOIN session_docs d ON d.rowid = session_docs_fts.rowid
             WHERE session_docs_fts MATCH ?1
               AND (?2 IS NULL OR d.cwd = ?2)
             ORDER BY rank ASC, d.updated_at DESC, d.session_id ASC
             LIMIT ?3 OFFSET ?4"
        );

        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(
            params![match_query, cwd, limit as i64, offset as i64],
            |row| {
                let session_id: String = row.get("session_id")?;
                let row_cwd: String = row.get("cwd")?;
                let title: String = row.get("title")?;
                let updated_at_unix: i64 = row.get("updated_at")?;
                let rank: f64 = row.get("rank")?;
                let snippet: Option<String> = row.get("snippet")?;
                let hl_title: String = row.get("hl_title")?;
                let hl_content: String = row.get("hl_content")?;

                let score = if rank.is_finite() {
                    -(rank as f32)
                } else {
                    0.0
                };

                let mut matched_fields = Vec::new();
                if hl_title.contains('\x01') {
                    matched_fields.push("title".to_string());
                }
                if hl_content.contains('\x01') {
                    matched_fields.push("content".to_string());
                }
                if matched_fields.is_empty() {
                    matched_fields.push("content".to_string());
                }

                Ok(SessionSearchRow {
                    session_id,
                    cwd: row_cwd,
                    title,
                    updated_at_unix,
                    score,
                    matched_fields,
                    snippet,
                })
            },
        )?;

        let results: Vec<SessionSearchRow> = rows.collect::<Result<_, _>>()?;
        let total_usize = usize::try_from(total).unwrap_or(0);
        let next_offset = (offset + results.len() < total_usize).then_some(offset + results.len());

        Ok(QueryResult {
            results,
            next_offset,
            total_estimate: Some(total_usize),
        })
    }

    /// Build the AND-joined and OR-joined FTS5 MATCH strings for a query.
    ///
    /// The strings are identical for single-token queries, which lets the caller skip the fallback rerun.
    fn build_match_queries(query: &str) -> Option<(String, String)> {
        let prefixes: Vec<String> = query
            .split_whitespace()
            .flat_map(Self::sanitize_token)
            .map(Self::token_prefix)
            .collect();

        if prefixes.is_empty() {
            let fallback = query.trim();
            if fallback.is_empty() {
                return None;
            }
            let cleaned = fallback.replace('"', "");
            let phrase = format!("\"{cleaned}\" *");
            return Some((phrase.clone(), phrase));
        }

        Some((prefixes.join(" AND "), prefixes.join(" OR ")))
    }

    /// Split a query word on every stripped character instead of gluing: `session_picker.rs` searches as `session_picker` and `rs`.
    /// Gluing produced the never-indexed `session_pickerrs`.
    /// Fragments with no alphanumeric (`-`, `->`, `_`) are dropped: they tokenize to empty phrases; an empty phrase inside an AND matches nothing.
    fn sanitize_token(token: &str) -> impl Iterator<Item = &str> {
        token
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .filter(|part| part.chars().any(|c| c.is_ascii_alphanumeric()))
    }

    /// One quoted FTS5 prefix per token, stemmed on the query side only. Plural queries reach singular docs by searching the
    /// shorter stem (`sessions` becomes `session*`, `caches` becomes `cach*`). Words shorter than four letters, identifiers
    /// with digits/`_`/`-`, and words ending in `ss` (`pass`, `class`) stay exact.
    fn token_prefix(token: &str) -> String {
        let stem = if token.len() < 4 || !token.chars().all(|c| c.is_ascii_alphabetic()) {
            token
        } else {
            let lower = token.to_ascii_lowercase();
            if lower.ends_with("es") {
                // Stemming `caches` to `cach` still finds `cache`: the trailing `*` covers the dropped `e`
                match token.len().checked_sub(2).and_then(|n| token.get(..n)) {
                    Some(s) => s,
                    None => token,
                }
            } else if lower.ends_with('s') && !lower.ends_with("ss") {
                match token.len().checked_sub(1).and_then(|n| token.get(..n)) {
                    Some(s) => s,
                    None => token,
                }
            } else {
                token
            }
        };
        format!("\"{stem}\" *")
    }
}

/// True when `query` is a UUID or a hyphenated hex prefix long enough to be an intentional session-id search (not a normal keyword).
fn is_session_id_like(query: &str) -> bool {
    let q = query.trim();
    if uuid::Uuid::try_parse(q).is_ok() {
        return true;
    }
    let stripped: String = q.chars().filter(|&c| c != '-').collect();
    stripped.len() >= 8 && stripped.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_doc(id: &str, title: &str, content: &str) -> SessionDoc {
        SessionDoc {
            session_id: id.to_string(),
            cwd: "/test/workspace".to_string(),
            updated_at_unix: 1700000000,
            title: title.to_string(),
            content: content.to_string(),
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
        }
    }

    fn open(tmp: &TempDir) -> SessionSearchIndex {
        SessionSearchIndex::open_or_create(&tmp.path().join("session_search.sqlite")).unwrap()
    }

    const LEASE: Duration = Duration::from_secs(300);

    #[test]
    fn test_bootstrap_claim_is_single_flight_until_lease_expires() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        let peer = open(&tmp);
        let claim = |idx: &SessionSearchIndex, now: i64, token: &str| {
            idx.try_claim_bootstrap(now, LEASE, token).unwrap()
        };

        assert!(claim(&index, 1_000, "a"));
        assert!(!claim(&index, 1_010, "a"), "live claim is not re-claimable");
        assert!(
            !claim(&peer, 1_299, "b"),
            "live claim is not claimable by a peer"
        );
        assert!(claim(&peer, 1_301, "b"), "expired lease is claimable");
    }

    #[test]
    fn test_bootstrap_claim_release_and_refresh_are_owner_fenced() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        let claim = |now: i64, token: &str| index.try_claim_bootstrap(now, LEASE, token).unwrap();

        assert!(claim(1_000, "a"));
        assert!(index.refresh_bootstrap_claim(1_200, "a").unwrap());
        assert!(!claim(1_400, "b"), "refresh extends the lease");

        // A stale claimant (expired, taken over) can neither refresh nor release the successor's claim
        assert!(claim(1_501, "b"), "lease from 1_200 expires at 1_500");
        assert!(!index.refresh_bootstrap_claim(1_502, "a").unwrap());
        assert!(!index.release_bootstrap_claim("a").unwrap());
        assert!(!claim(1_503, "c"), "b's claim survives a's stale release");

        assert!(index.release_bootstrap_claim("b").unwrap());
        assert!(claim(1_504, "c"), "owner release frees the claim");
    }

    #[test]
    fn test_upgrade_drop_clears_bootstrap_claim() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        assert!(index.try_claim_bootstrap(1_000, LEASE, "a").unwrap());
        index.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
        drop(index);

        let reopened = open(&tmp);
        assert!(
            reopened.try_claim_bootstrap(1_001, LEASE, "b").unwrap(),
            "the upgrade wipe must clear the claim so the rebuild is not blocked"
        );
    }

    #[test]
    fn test_set_meta_if_claim_owner_is_fenced() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);

        assert!(!index.set_meta_if_claim_owner("k", "v", "a").unwrap());
        assert_eq!(index.get_meta("k").unwrap(), None, "no claim: no write");

        assert!(index.try_claim_bootstrap(1_000, LEASE, "a").unwrap());
        assert!(!index.set_meta_if_claim_owner("k", "v", "b").unwrap());
        assert_eq!(index.get_meta("k").unwrap(), None, "non-owner: no write");

        assert!(index.set_meta_if_claim_owner("k", "v1", "a").unwrap());
        assert_eq!(index.get_meta("k").unwrap().as_deref(), Some("v1"));
        assert!(index.set_meta_if_claim_owner("k", "v2", "a").unwrap());
        assert_eq!(
            index.get_meta("k").unwrap().as_deref(),
            Some("v2"),
            "owner writes take the update arm on conflict"
        );
    }

    #[test]
    fn test_bootstrap_claim_takes_over_garbage_and_future_stamps() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);

        index
            .set_meta(META_KEY_BOOTSTRAP_CLAIM, "not-a-number")
            .unwrap();
        assert!(index.try_claim_bootstrap(1_000, LEASE, "a").unwrap());

        // A future-dated stamp (clock rollback) must not hold forever.
        index
            .set_meta(META_KEY_BOOTSTRAP_CLAIM, &claim_stamp(9_999_999, "x"))
            .unwrap();
        assert!(index.try_claim_bootstrap(1_000, LEASE, "a").unwrap());
    }

    fn journal_mode(index: &SessionSearchIndex) -> String {
        index
            .db
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_open_or_create_uses_wal_on_local_fs() {
        // The `GROK_SQLITE_JOURNAL_MODE` env var would override the decision; skip if set
        if std::env::var("GROK_SQLITE_JOURNAL_MODE").is_ok() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        assert_eq!(journal_mode(&open(&tmp)), "wal");
    }

    #[test]
    fn test_network_mode_uses_fresh_per_host_truncate_db() {
        // Network mode opens a per-host sibling of the given path in rollback-journal mode, and the index is fully usable there
        // The legacy shared file is left untouched: a live old binary can flip it back to WAL at any time
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session_search.sqlite");

        let index =
            SessionSearchIndex::open_with_journal_mode(&path, JournalMode::Truncate).unwrap();
        assert_eq!(journal_mode(&index), "truncate");
        index
            .upsert_doc(&test_doc("s1", "NFS crash", "sigbus walIndexTryHdr"))
            .unwrap();
        let hits = index.query("sigbus", None, 10, 0, false).unwrap();
        assert_eq!(hits.results.len(), 1);
        drop(index);

        let eff = JournalMode::Truncate.effective_db_path(&path);
        assert_ne!(eff, path);
        let base = eff.display().to_string();
        assert!(!std::fs::exists(format!("{base}-wal")).unwrap());
        assert!(!std::fs::exists(format!("{base}-shm")).unwrap());
    }

    #[test]
    fn test_version_mismatch_drops_docs_and_preserves_unrelated_meta() {
        let tmp = TempDir::new().unwrap();
        {
            let index = open(&tmp);
            index
                .upsert_doc(&test_doc("s1", "Rust debugging", "borrow checker"))
                .unwrap();
            index.set_meta("last_bootstrap_at", "1700000000").unwrap();
            index.set_meta("last_upload_at", "1700000001").unwrap();
        }

        {
            // Guard against the drop branch firing on every open: a reopen at the current version must keep the docs
            let same_version = open(&tmp);
            assert_eq!(
                same_version.all_indexed_session_ids().unwrap(),
                vec!["s1".to_string()],
                "docs must survive a same-version reopen"
            );
            // Simulate a database written by an older schema version.
            same_version.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
            assert_eq!(
                same_version
                    .get_meta(META_KEY_SCHEMA_VERSION)
                    .unwrap()
                    .as_deref(),
                Some("3"),
                "version downgrade must take effect for the migration to fire"
            );
        }

        let reopened = open(&tmp);
        assert!(
            reopened.all_indexed_session_ids().unwrap().is_empty(),
            "stale docs must be dropped on version mismatch"
        );
        assert_eq!(
            reopened
                .get_meta(META_KEY_SCHEMA_VERSION)
                .unwrap()
                .as_deref(),
            Some(SCHEMA_VERSION),
            "schema version must be rewritten to current"
        );
        // The drop batch invalidates the completed-bootstrap marker (the dropped tables no longer reflect a completed bootstrap)
        // It leaves every other `meta` key alone
        assert_eq!(
            reopened.get_meta("last_bootstrap_at").unwrap(),
            None,
            "the completed-bootstrap marker must be invalidated by the drop"
        );
        assert_eq!(
            reopened.get_meta("last_upload_at").unwrap().as_deref(),
            Some("1700000001"),
            "unrelated meta keys must survive the drop"
        );
        // Recreated tables and FTS triggers must be functional end-to-end
        reopened
            .upsert_doc(&test_doc("s2", "Python profiling", "flamegraph"))
            .unwrap();
        let qr = reopened.query("python", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s2")
        );
    }

    #[test]
    fn test_newer_version_index_is_tolerated_not_dropped() {
        let tmp = TempDir::new().unwrap();
        {
            let index = open(&tmp);
            index
                .upsert_doc(&test_doc("s1", "Rust debugging", "borrow checker"))
                .unwrap();
            // Simulate an index owned by a newer grok generation that has completed a bootstrap
            index.set_meta(META_KEY_SCHEMA_VERSION, "5").unwrap();
            index.set_meta("last_bootstrap_at", "1700000000").unwrap();
        }

        let reopened = open(&tmp);
        assert_eq!(
            reopened.all_indexed_session_ids().unwrap(),
            vec!["s1".to_string()],
            "docs must survive an older binary opening a newer index"
        );
        assert_eq!(
            reopened
                .get_meta(META_KEY_SCHEMA_VERSION)
                .unwrap()
                .as_deref(),
            Some("5"),
            "the newer generation keeps ownership of the version row"
        );
        assert_eq!(
            reopened.get_meta("last_bootstrap_at").unwrap().as_deref(),
            Some("1700000000"),
            "no drop happened, so the newer index's bootstrap marker must survive"
        );
        // The tolerated index must stay fully usable for the older binary.
        let qr = reopened.query("borrow", None, 10, 0, false).unwrap();
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );
    }

    #[test]
    fn test_corrupt_version_row_drops_index() {
        let tmp = TempDir::new().unwrap();
        {
            let index = open(&tmp);
            index
                .upsert_doc(&test_doc("s1", "Rust debugging", "borrow checker"))
                .unwrap();
            index.set_meta(META_KEY_SCHEMA_VERSION, "garbage").unwrap();
        }

        let reopened = open(&tmp);
        assert!(
            reopened.all_indexed_session_ids().unwrap().is_empty(),
            "a corrupt version row must drop and rebuild"
        );
        assert_eq!(
            reopened
                .get_meta(META_KEY_SCHEMA_VERSION)
                .unwrap()
                .as_deref(),
            Some(SCHEMA_VERSION),
            "rebuild rewrites the current version"
        );
    }

    #[test]
    fn test_malformed_db_file_is_quarantined_and_recreated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session_search.sqlite");
        // Not a SQLite database, so opens fail with "file is not a database" (NOTADB)
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();

        // Drive the production entrypoint: it heals on open, then the op runs.
        with_index(&path, |index| {
            index.upsert_doc(&test_doc("s1", "after heal", "session search works again"))
        })
        .expect("with_index self-heals then upserts");
        let qr = with_index(&path, |index| index.query("works", None, 10, 0, false))
            .expect("query after heal");
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );

        // Original path is a real DB again; a quarantine sibling should exist.
        assert!(path.is_file());
        let quarantined: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("corrupt"))
            .collect();
        assert!(
            !quarantined.is_empty(),
            "expected a quarantined corrupt sibling, got {quarantined:?}"
        );
    }

    #[test]
    fn test_with_index_retries_op_once_after_heal() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session_search.sqlite");
        // A non-db file gives a real "unusable" error to inject into the op.
        let bogus = tmp.path().join("bogus.sqlite");
        std::fs::write(&bogus, b"not-sqlite").unwrap();

        // The first op attempt reports the DB unusable, as a mid-op corruption would
        // `with_index` heals (a no-op here, the DB is healthy) and retries the op exactly once, which then succeeds
        let calls = std::cell::Cell::new(0u32);
        let result = with_index(&path, |index| {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                match SessionSearchIndex::open_with_journal_mode(&bogus, JournalMode::Wal) {
                    Ok(_) => unreachable!("a non-sqlite file cannot open as a database"),
                    Err(e) => Err(e),
                }
            } else {
                index.upsert_doc(&test_doc("s1", "t", "retried body"))
            }
        });

        assert!(result.is_ok(), "op should succeed on the retry: {result:?}");
        assert_eq!(
            calls.get(),
            2,
            "op runs once, fails unusable, then runs once more"
        );

        let index = SessionSearchIndex::open_or_create(&path).unwrap();
        let qr = index.query("retried", None, 10, 0, false).unwrap();
        assert_eq!(qr.results.len(), 1, "the retried op's write is persisted");
    }

    /// Repro: the on-disk state left behind by a pre-ratchet binary that wiped the shared DB and ran its own bootstrap. Pins
    /// that the current binary's open drops the tables and deletes the marker together (see the drop batch in
    /// `open_or_create`). A surviving marker would suppress re-bootstrap over empty tables.
    #[test]
    fn test_upgrade_drop_invalidates_completed_bootstrap_marker() {
        let tmp = TempDir::new().unwrap();
        {
            let index = open(&tmp);
            index
                .upsert_doc(&test_doc("s1", "old-binary doc", "indexed by v3"))
                .unwrap();
            index.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
            index.set_meta("last_bootstrap_at", "1783393389").unwrap();
        }

        let reopened = open(&tmp);
        assert!(
            reopened.all_indexed_session_ids().unwrap().is_empty(),
            "v3 docs must be dropped on upgrade"
        );
        assert_eq!(
            reopened
                .get_meta(META_KEY_SCHEMA_VERSION)
                .unwrap()
                .as_deref(),
            Some(SCHEMA_VERSION),
            "upgrade must stamp the current version"
        );
        assert_eq!(
            reopened.get_meta("last_bootstrap_at").unwrap(),
            None,
            "the stale bootstrap marker must not survive the upgrade drop, \
             or the wiped index would be treated as fully bootstrapped"
        );

        // A subsequent bootstrap can repopulate and re-stamp the marker.
        reopened
            .upsert_doc(&test_doc("s2", "fresh doc", "indexed by v4"))
            .unwrap();
        reopened
            .set_meta("last_bootstrap_at", "1783393999")
            .unwrap();
        let qr = reopened.query("fresh", None, 10, 0, false).unwrap();
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s2")
        );
    }

    #[test]
    fn test_upsert_and_query() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc(
                "s1",
                "Rust debugging",
                "fix the borrow checker issue",
            ))
            .unwrap();

        let qr = index.query("rust", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );
        assert!(qr.results.first().is_some_and(|r| r.score > 0.0));
        assert!(
            qr.results
                .first()
                .is_some_and(|r| r.matched_fields.contains(&"title".to_string()))
        );
    }

    #[test]
    fn test_upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc("s1", "Old title", "old content"))
            .unwrap();
        index
            .upsert_doc(&test_doc("s1", "New title about kubernetes", "new content"))
            .unwrap();

        let old = index.query("old", None, 10, 0, false).unwrap();
        assert!(
            old.results.is_empty(),
            "old content should not be searchable"
        );

        let new = index.query("kubernetes", None, 10, 0, false).unwrap();
        assert_eq!(new.results.len(), 1);
    }

    #[test]
    fn test_delete_doc() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc("s1", "Delete me", "some content about python"))
            .unwrap();
        assert_eq!(index.all_indexed_session_ids().unwrap().len(), 1);

        index.delete_doc("s1").unwrap();
        assert!(index.all_indexed_session_ids().unwrap().is_empty());

        assert!(
            index
                .query("python", None, 10, 0, false)
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[test]
    fn test_insert_doc_if_absent_never_overwrites() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        let full = test_doc("s1", "Rust debugging", "borrow checker");
        index.upsert_doc(&full).unwrap();

        // Conflict arm: an existing (fuller) row must be left untouched.
        index
            .insert_doc_if_absent(&test_doc("s1", "placeholder", ""))
            .unwrap();
        assert_eq!(
            index.get_content_hash("s1").unwrap().as_deref(),
            Some(full.content_hash.as_str()),
            "existing row must not be downgraded to the placeholder"
        );
        let qr = index.query("borrow", None, 10, 0, false).unwrap();
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1"),
            "full content must remain FTS-queryable after the no-op insert"
        );

        // Insert arm: a new id must land and fire the FTS trigger.
        index
            .insert_doc_if_absent(&test_doc("s2", "Python profiling", ""))
            .unwrap();
        let qr = index.query("python", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s2")
        );
    }

    /// `/resume` search types a session UUID; CLI `--resume <id>` works because it looks up by id globally.
    /// FTS only indexed title and content, so pasting the id into search returned nothing.
    #[test]
    fn test_query_matches_session_id() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        let id = "019f870d-6976-7d73-a12a-52e9d4aebcd4";
        index
            .upsert_doc(&test_doc(
                id,
                "unrelated title",
                "body text with no identifier",
            ))
            .unwrap();

        let qr = index.query(id, None, 10, 0, false).unwrap();
        assert_eq!(qr.results.len(), 1, "full session id must match");
        assert_eq!(qr.results.first().map(|r| r.session_id.as_str()), Some(id));
        assert!(
            qr.results
                .first()
                .is_some_and(|r| r.matched_fields.iter().any(|f| f == "session_id"))
        );

        let prefix = index.query("019f870d-6976", None, 10, 0, false).unwrap();
        assert_eq!(prefix.results.len(), 1, "session id prefix must match");
        assert_eq!(
            prefix.results.first().map(|r| r.session_id.as_str()),
            Some(id)
        );

        let mut other_cwd = test_doc("019f870d-6976-7d73-a12a-ffffffffffff", "other", "unrelated");
        other_cwd.cwd = "/other".to_string();
        index.upsert_doc(&other_cwd).unwrap();
        let scoped = index
            .query(id, Some("/test/workspace"), 10, 0, false)
            .unwrap();
        assert_eq!(scoped.results.len(), 1);
        assert_eq!(
            scoped.results.first().map(|r| r.session_id.as_str()),
            Some(id)
        );
    }

    #[test]
    fn test_query_cwd_filter() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);

        let mut doc_a = test_doc("s1", "Rust project", "cargo build");
        doc_a.cwd = "/workspace/a".to_string();
        let mut doc_b = test_doc("s2", "Rust library", "cargo test");
        doc_b.cwd = "/workspace/b".to_string();
        index.upsert_doc(&doc_a).unwrap();
        index.upsert_doc(&doc_b).unwrap();

        let all = index.query("rust", None, 10, 0, false).unwrap();
        assert_eq!(all.results.len(), 2);

        let filtered = index
            .query("rust", Some("/workspace/a"), 10, 0, false)
            .unwrap();
        assert_eq!(filtered.results.len(), 1);
        assert_eq!(
            filtered.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );
    }

    #[test]
    fn test_query_pagination() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        for i in 0..5 {
            index
                .upsert_doc(&test_doc(
                    &format!("s{i}"),
                    &format!("Session {i}"),
                    &format!("rust content {i}"),
                ))
                .unwrap();
        }

        let page1 = index.query("rust", None, 2, 0, false).unwrap();
        assert_eq!(page1.results.len(), 2);
        assert_eq!(page1.total_estimate, Some(5));
        assert_eq!(page1.next_offset, Some(2));

        let page2 = index.query("rust", None, 2, 2, false).unwrap();
        assert_eq!(page2.results.len(), 2);
    }

    #[test]
    fn test_query_with_snippets() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc(
                "s1",
                "Debugging session",
                "the rust borrow checker was causing lifetime errors in the parser",
            ))
            .unwrap();

        let qr = index.query("borrow checker", None, 10, 0, true).unwrap();
        assert_eq!(qr.results.len(), 1);
        assert!(qr.results.first().is_some_and(|r| r.snippet.is_some()));
    }

    #[test]
    fn test_query_empty_string() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index.upsert_doc(&test_doc("s1", "Title", "body")).unwrap();

        let qr = index.query("", None, 10, 0, false).unwrap();
        assert!(qr.results.is_empty());
        assert_eq!(qr.total_estimate, Some(0));
    }

    #[test]
    fn test_query_special_chars_sanitized() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc("s1", "Title", "hello world"))
            .unwrap();

        // Special chars are stripped, leaving "hello"
        let qr = index.query("hello!!!", None, 10, 0, false).unwrap();
        assert_eq!(qr.results.len(), 1);
    }

    #[test]
    fn test_matched_fields_title_vs_content() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc(
                "s1",
                "kubernetes deployment",
                "unrelated body text",
            ))
            .unwrap();

        let qr = index.query("kubernetes", None, 10, 0, false).unwrap();
        assert_eq!(qr.results.len(), 1);
        assert!(
            qr.results
                .first()
                .is_some_and(|r| r.matched_fields.contains(&"title".to_string()))
        );
    }

    /// cwd is a filter dimension, not a search dimension.
    /// A term that only appears in the cwd must never cause a session to match.
    #[test]
    fn test_cwd_not_searchable() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        let mut doc = test_doc("s1", "unrelated title", "unrelated content");
        doc.cwd = "/Users/alice/workspace/supercalifragilistic".to_string();
        index.upsert_doc(&doc).unwrap();

        let qr = index
            .query("supercalifragilistic", None, 10, 0, false)
            .unwrap();
        assert!(
            qr.results.is_empty(),
            "cwd-only term must not match, got {} results",
            qr.results.len()
        );
    }

    #[test]
    fn test_query_filename_tokens_split() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc(
                "s1",
                "Fix list rendering",
                "the bug lives in session_picker.rs near the filter",
            ))
            .unwrap();

        // Pins splitting on stripped chars: gluing the fragments produced the never-indexed token `session_pickerrs`, so this query found nothing
        let qr = index
            .query("session_picker.rs", None, 10, 0, false)
            .unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );
    }

    #[test]
    fn test_query_and_first_with_or_fallback() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc(
                "s1",
                "Borrow both",
                "fix the borrow checker issue",
            ))
            .unwrap();
        index
            .upsert_doc(&test_doc("s2", "Borrow only", "borrow money from the bank"))
            .unwrap();
        index
            .upsert_doc(&test_doc("s3", "Tokio doc", "tokio runtime setup"))
            .unwrap();
        index
            .upsert_doc(&test_doc("s4", "Sqlite doc", "sqlite index tuning"))
            .unwrap();

        // AND has hits: only the doc matching every token is returned, so partial matches cannot dilute the result set
        let qr = index.query("borrow checker", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );

        // A separator-only word (`->`) must be dropped, not become an empty phrase that silently makes the whole AND match nothing
        let qr = index.query("fix -> borrow", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(1));
        assert_eq!(
            qr.results.first().map(|r| r.session_id.as_str()),
            Some("s1")
        );

        // No doc has both tokens: the OR rerun returns the partial matches
        let qr = index.query("tokio sqlite", None, 10, 0, false).unwrap();
        assert_eq!(qr.total_estimate, Some(2));
        let ids: Vec<&str> = qr.results.iter().map(|r| r.session_id.as_str()).collect();
        assert!(
            ids.contains(&"s3") && ids.contains(&"s4"),
            "OR fallback must return both partial matches: {ids:?}"
        );
    }

    #[test]
    fn test_query_plural_variants() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index
            .upsert_doc(&test_doc("s1", "Plural doc", "resumed sessions list"))
            .unwrap();
        index
            .upsert_doc(&test_doc("s2", "Singular doc", "resume the session flow"))
            .unwrap();

        // Plural query, singular doc: pins the query-side stem; without it `sessions*` cannot prefix-match `session`
        let qr = index.query("sessions", None, 10, 0, false).unwrap();
        let ids: Vec<&str> = qr.results.iter().map(|r| r.session_id.as_str()).collect();
        assert!(
            ids.contains(&"s2"),
            "singular doc must match a plural query: {ids:?}"
        );

        // Singular query, plural doc: pins the prefix-`*` coverage that makes an added plural variant unnecessary
        let qr = index.query("session", None, 10, 0, false).unwrap();
        let ids: Vec<&str> = qr.results.iter().map(|r| r.session_id.as_str()).collect();
        assert!(
            ids.contains(&"s1"),
            "plural doc must match a singular query: {ids:?}"
        );
    }

    #[test]
    fn test_query_pure_symbol_fallback() {
        let tmp = TempDir::new().unwrap();
        let index = open(&tmp);
        index.upsert_doc(&test_doc("s1", "Title", "body")).unwrap();

        // No indexable characters: the raw-phrase fallback must not error.
        let qr = index.query("…", None, 10, 0, false).unwrap();
        assert!(qr.results.is_empty());
        assert_eq!(qr.total_estimate, Some(0));
    }
}
