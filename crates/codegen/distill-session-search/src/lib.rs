//! Full-text search over local grok sessions, backed by a rebuildable SQLite FTS5 cache under `<grok home>/sessions/session_search.sqlite`.
//!
//! Layering, bottom up:
//!
//! - [`fts`] owns the schema, the BM25 query, and the fenced `meta` writes.
//! - `recovery` classifies an unusable database file, quarantines it, and recreates an empty one; `db` adds the open-and-retry helpers on top.
//! - `doc` turns a session plus its extracted text into an index document.
//! - `bootstrap` runs the full reindex, held to one process at a time by a lease.
//! - `manager` debounces per-session upserts and answers queries.
//!
//! The crate never reads the session store directly: a caller supplies a [`SessionSource`] and a [`ContentExtractor`].
//! That keeps the `updates.jsonl` wire format owned by the store instead of duplicated here.

#![deny(clippy::indexing_slicing)]

mod bootstrap;
mod db;
mod doc;
pub mod fts;
mod manager;
mod recovery;
mod source;

pub use manager::{
    SearchIndexManager, SearchIndexStatus, SessionSearchRequest, SessionSearchResponse,
    evict_session, execute_search,
};
pub use source::{ContentExtractor, IndexableSession, SessionSource, SessionSourceFactory};
