// Modified for Distill by Samuel Fajreldines, 2026.
mod acp_fs;
pub use acp_fs::AcpSessionFs;

mod ext_fs;
pub use ext_fs::{
    FsDeleteFileReq, FsExistsData, FsExistsReq, FsListData, FsListNode, FsListReq, FsReadFileData,
    FsReadFileReq, FsWriteFileReq,
};

// Client-facing read-only fs ops (`workspace.client_fs_*`)
// Not re-exported: the `ClientFs*` types live in `distill_workspace_types::rpc::fs` and would collide with the shell-facing `ext_fs` names above
pub(crate) mod client_fs;

// Shared filesystem core: paginated listing and binary-safe ranged reads, used by `client_fs`, `ext_fs`, and the shell-local `session::file_system`
mod walk;
pub use walk::{
    ChunkPayload, ListOptions, ListPage, ListedEntry, MAX_LIST_COLLECT, MAX_READ_BYTES,
    clamp_read_length, encode_chunk, list_directory_paged, read_range,
};
// Re-exported so shell-side fs ops can name the shared read encoding.
pub use distill_workspace_types::rpc::fs::FsReadEncoding;

pub mod adapter;
pub use adapter::AcpFsAdapter;

mod codebase_index;
pub use codebase_index::{CodebaseIndexManager, get_index_cache_path};

mod fs;
pub use fs::{AsyncFileSystem, AsyncFsWrapper, FsError, bytes_to_string};

mod local_fs;
pub use local_fs::LocalFs;

mod mock_fs;
pub use mock_fs::MockFs;

mod file_tree;
pub use file_tree::{ListContentsLimits, list_contents, list_contents_multi};

// Shared subprocess teardown for the session-triggered git/jj status spawns.
mod process;

// `core.fsmonitor` pin resolution for the session-triggered git status.
mod fsmonitor;
pub use fsmonitor::{FsmonitorOverride, probe_fsmonitor_override};

mod git_status;
pub use git_status::{git_status, git_status_short_pinned};

mod jj_status;
pub use jj_status::jj_status;

mod attach_file;
pub use attach_file::{FileReference, render_embedded_resource, render_file_reference};

pub use distill_fuzzy_file_search::{
    FuzzyFileMatcher, FuzzyFileMatcherDaemon, FuzzyMatchResult, FuzzyMatcherDaemonResults,
    FuzzyMatcherStatus,
};

mod index;
pub use index::{FileEntry, FileIndex, FileIndexDelta, SegmentId, StringInterner, WalkOptions};

mod content;
pub use content::{
    ContentMatch, ContentMatchFile, ContentSearchBatch, ContentSearchData, ContentSearchParams,
    content_search_streaming,
};

use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

// These types are canonical in distill-workspace-types; the re-export keeps existing import paths working
pub use distill_workspace_types::rpc::search::{ClientId, ContentSearchRequest, TargetClientId};

impl From<ContentSearchRequest> for ContentSearchParams {
    fn from(req: ContentSearchRequest) -> Self {
        let pattern = if req.is_regex {
            req.pattern
        } else if req.whole_word {
            format!("\\b{}\\b", regex::escape(&req.pattern))
        } else {
            req.pattern
        };

        let literal = !req.is_regex && !req.whole_word;
        let globs: Vec<String> = req
            .include_globs
            .into_iter()
            .chain(req.exclude_globs.into_iter().map(|g| format!("!{g}")))
            .collect();

        Self {
            pattern,
            case_insensitive: req.case_insensitive,
            literal,
            globs,
            max_files: req.max_files,
            max_matches: req.max_matches,
            respect_gitignore: req.respect_gitignore,
        }
    }
}

const DEFAULT_SEARCH_TIMEOUT_SECS: u64 = 30;
const DEFAULT_TOP_K: usize = 1000;

pub type FuzzySearchId = String;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FuzzySearchData {
    pub matches: Vec<FuzzyMatchResult>,
    pub total: usize,
    pub done: bool,
    pub generation: usize,
}

/// Result of one fuzzy-search poll tick (see [`WorkspaceHandle::fuzzy_poll`]).
/// Consumed in-process, so it carries non-`Deserialize` match results directly rather than over RPC.
#[derive(Debug, Clone)]
pub enum FuzzyPollOutcome {
    /// The query was superseded by a newer change; stop polling.
    Stale,
    /// The search no longer exists; stop polling.
    Closed,
    /// The search exists but produced no new results this tick; keep polling.
    Pending,
    /// New results, with paths already absolutized against the search root.
    Update(FuzzySearchData),
}

pub struct FuzzySearchContext {
    pub daemon: FuzzyFileMatcherDaemon,
    pub created_at: Instant,
    pub last_activity: Instant,
    pub hidden: bool,
    pub min_generation: usize,
    pub has_query: bool,
    pub query_version: usize,
    /// The root path for this search (used to convert relative paths to absolute).
    pub root: PathBuf,
    /// Used by the relay to route notifications to session subscribers.
    pub session_id: Option<String>,
    /// Extracted from `_meta.clientId` in the open request.
    pub target_client_id: TargetClientId,
}

impl FuzzySearchContext {
    pub fn new(
        root: &Path,
        hidden: bool,
        session_id: Option<String>,
        target_client_id: TargetClientId,
    ) -> Self {
        let matcher = FuzzyFileMatcher::new(root);
        let daemon = FuzzyFileMatcherDaemon::new(matcher, DEFAULT_TOP_K);
        daemon.restart_walk(hidden);

        Self {
            daemon,
            created_at: Instant::now(),
            last_activity: Instant::now(),
            hidden,
            min_generation: 0,
            has_query: false,
            query_version: 0,
            root: root.to_path_buf(),
            session_id,
            target_client_id,
        }
    }

    pub fn is_stale(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }
}

pub struct FuzzySearchManager {
    searches: HashMap<FuzzySearchId, FuzzySearchContext>,
    timeout: Duration,
}

impl FuzzySearchManager {
    pub fn new(timeout: Duration) -> Self {
        Self {
            searches: HashMap::new(),
            timeout,
        }
    }

    pub fn open(
        &mut self,
        root: &Path,
        request_id: Option<String>,
        hidden: bool,
        session_id: Option<String>,
        target_client_id: TargetClientId,
    ) -> FuzzySearchId {
        self.cleanup_stale();
        let search_id = request_id.unwrap_or_else(|| Uuid::now_v7().to_string());

        let context = FuzzySearchContext::new(root, hidden, session_id, target_client_id);
        self.searches.insert(search_id.clone(), context);
        search_id
    }

    /// Used for routing notifications to session subscribers.
    pub fn get_session_id(&self, search_id: &str) -> Option<String> {
        self.searches
            .get(search_id)
            .and_then(|ctx| ctx.session_id.clone())
    }

    /// Used for routing notifications to the correct client via relay.
    pub fn get_target_client_id(&self, search_id: &str) -> TargetClientId {
        self.searches
            .get(search_id)
            .map(|ctx| ctx.target_client_id.clone())
            .unwrap_or_default()
    }

    /// Used to convert relative paths to absolute paths in results.
    pub fn get_root(&self, search_id: &str) -> Option<PathBuf> {
        self.searches.get(search_id).map(|ctx| ctx.root.clone())
    }

    pub fn change(
        &mut self,
        search_id: &str,
        query: &str,
        dirs_only: bool,
    ) -> Option<(usize, bool, usize)> {
        let ctx = self.searches.get_mut(search_id)?;
        ctx.last_activity = Instant::now();

        // Rewalk on empty query to refresh index when picker opens.
        if query.is_empty() {
            ctx.daemon.restart_walk(ctx.hidden);
        }

        ctx.daemon.set_query(query, dirs_only);
        ctx.min_generation += 1;
        ctx.has_query = !query.is_empty();
        ctx.query_version += 1;
        Some((ctx.min_generation, ctx.has_query, ctx.query_version))
    }

    pub fn is_current_query(&self, search_id: &str, query_version: usize) -> bool {
        self.searches
            .get(search_id)
            .is_some_and(|ctx| ctx.query_version == query_version)
    }

    pub fn get_results(&mut self, search_id: &str) -> Option<FuzzySearchData> {
        let ctx = self.searches.get_mut(search_id)?;
        ctx.last_activity = Instant::now();

        let results = ctx.daemon.get();

        Some(FuzzySearchData {
            matches: results.topk.to_vec(),
            total: results.num_items,
            done: results.status.done,
            generation: results.generation,
        })
    }

    pub fn get_results_filtered(
        &mut self,
        search_id: &str,
        min_gen: usize,
        has_query: bool,
    ) -> Option<FuzzySearchData> {
        let ctx = self.searches.get_mut(search_id)?;
        ctx.last_activity = Instant::now();

        let results = ctx.daemon.get();

        if results.generation < min_gen {
            return None;
        }

        // Skip intermediate states: empty results or unscored defaults while scanning
        if has_query && !results.status.done {
            if results.topk.is_empty() {
                return None;
            }
            let all_unscored = results
                .topk
                .iter()
                .all(|m| m.score == 0 && m.indices.is_empty());
            if all_unscored {
                return None;
            }
        }

        Some(FuzzySearchData {
            matches: results.topk.to_vec(),
            total: results.num_items,
            done: results.status.done,
            generation: results.generation,
        })
    }

    pub fn close(&mut self, search_id: &str) -> bool {
        self.searches.remove(search_id).is_some()
    }

    pub fn cleanup_stale(&mut self) {
        let timeout = self.timeout;
        self.searches.retain(|_, ctx| !ctx.is_stale(timeout));
    }

    pub fn active_count(&self) -> usize {
        self.searches.len()
    }
}

impl Default for FuzzySearchManager {
    fn default() -> Self {
        Self::new(Duration::from_secs(DEFAULT_SEARCH_TIMEOUT_SECS))
    }
}
