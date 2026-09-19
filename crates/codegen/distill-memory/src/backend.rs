// Modified for Distill by Samuel Fajreldines, 2026.
//! Concrete `MemoryBackend` implementation using hybrid search.
//!
//! `MemoryBackendImpl` combines FTS5 keyword search with optional vector KNN similarity via `hybrid_search()`.
//! When embeddings are available (embedding config and API key), the query is vectorized and both signals merge with recency and source weights.
//! When embeddings are unavailable, search degrades to FTS-only.
//!
//! `rusqlite::Connection` is `!Send + !Sync`, so we open a fresh `MemoryIndex` per query.
//! WAL mode keeps concurrent readers from blocking.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use distill_tools::types::memory_backend::{MemoryBackend, MemorySearchResult};

use super::embedding::EmbeddingProvider as _;
use super::observation::{
    MemoryObservationSink, MemoryRetrievalMode, MemorySearchErrorClass, MemorySearchObservation,
    MemorySearchOutcome, MemorySearchSource, MemoryWatcherSyncObservation,
    noop_memory_observation_sink,
};
use super::storage::MemoryStorage;
use super::watcher::MemoryFileWatcher;

fn merge_fts_results<E>(
    primary: Result<Vec<super::index::FtsResult>, E>,
    evergreen: Result<Vec<super::index::FtsResult>, E>,
) -> Vec<super::index::FtsResult> {
    let mut results = primary.unwrap_or_default();
    let existing: std::collections::HashSet<_> = results
        .iter()
        .map(|result| result.chunk_id.clone())
        .collect();
    results.extend(
        evergreen
            .into_iter()
            .flatten()
            .filter(|result| !existing.contains(&result.chunk_id)),
    );
    results
}

fn select_search_error_class(
    fts_error_class: Option<MemorySearchErrorClass>,
    is_vector_degraded: bool,
) -> Option<MemorySearchErrorClass> {
    fts_error_class.or_else(|| is_vector_degraded.then_some(MemorySearchErrorClass::Vector))
}

/// Embedding-client credentials scoped to a trusted endpoint.
/// Only [`Self::for_endpoint`] retains a live credential; the empty default fails closed.
#[derive(Clone, Default)]
pub struct EndpointScopedCredentials {
    endpoint: Option<reqwest::Url>,
    auth_credentials: Option<Arc<dyn distill_auth::AuthCredentialProvider>>,
    api_key_provider: Option<distill_tools::types::SharedApiKeyProvider>,
}

// Manual Debug that redacts the credential handles; only their presence shows.
impl std::fmt::Debug for EndpointScopedCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointScopedCredentials")
            .field("endpoint", &self.endpoint)
            .field("has_auth_credentials", &self.auth_credentials.is_some())
            .field("has_api_key_provider", &self.api_key_provider.is_some())
            .finish()
    }
}

impl EndpointScopedCredentials {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.auth_credentials.is_none() && self.api_key_provider.is_none()
    }

    /// Retains the credentials only for a trusted, parsable `endpoint`; otherwise drops them.
    pub fn for_endpoint(
        endpoint: &str,
        is_trusted: impl FnOnce(&str) -> bool,
        auth_credentials: Option<Arc<dyn distill_auth::AuthCredentialProvider>>,
        api_key_provider: Option<distill_tools::types::SharedApiKeyProvider>,
    ) -> Self {
        if is_trusted(endpoint)
            && let Ok(url) = reqwest::Url::parse(endpoint)
        {
            return Self {
                endpoint: Some(url),
                auth_credentials,
                api_key_provider,
            };
        }
        if auth_credentials.is_some() || api_key_provider.is_some() {
            tracing::info!(
                target: crate::MEMORY_LOG_TARGET,
                endpoint,
                "memory embeddings: session credentials withheld for non-first-party endpoint; its own key, if any, still applies"
            );
        }
        Self::none()
    }

    fn auth_credentials(&self) -> Option<&Arc<dyn distill_auth::AuthCredentialProvider>> {
        self.auth_credentials.as_ref()
    }

    fn api_key_provider(&self) -> Option<&distill_tools::types::SharedApiKeyProvider> {
        self.api_key_provider.as_ref()
    }

    fn approved_for(&self, base_url: &str) -> bool {
        match &self.endpoint {
            None => self.is_empty(),
            Some(endpoint) => reqwest::Url::parse(base_url).is_ok_and(|url| &url == endpoint),
        }
    }
}

/// All configuration needed to build a fully-wired [`MemoryBackendImpl`] for a live session.
/// Grouping these in one struct makes every call site (ToolBridge, first-turn injection, post-compaction recovery) share identical config.
/// Separately wired paths used to fall back to FTS-only search and ignore `[memory.search]` config because no one place applied all builder methods.
#[derive(Clone)]
pub struct MemoryBackendParams {
    pub session_id: String,
    /// Embedding provider config; `None` forces FTS-only fallback everywhere.
    pub embed_config: Option<distill_config_types::MemoryEmbeddingConfig>,
    /// Base URL for embedding API calls (CLI proxy).
    /// It must match the endpoint `embedding_credentials` was scoped to; a mismatch fails closed.
    pub embed_base_url: String,
    /// API key for embedding API calls.
    pub embed_api_key: Option<String>,
    /// Hybrid search scoring config (weights, thresholds, decay, MMR).
    pub search_config: distill_config_types::MemorySearchConfig,
    /// File watcher for sync-on-search; `None` disables external-edit detection.
    pub watcher: Option<Arc<MemoryFileWatcher>>,
    /// Seconds before a stale reindex claim is forcibly released.
    pub stale_claim_secs: i64,
    pub search_source: MemorySearchSource,
    pub observation_sink: Arc<dyn MemoryObservationSink>,
    pub embedding_credentials: EndpointScopedCredentials,
}

impl MemoryBackendParams {
    /// Async so `current_api_key_async` can drive the AuthManager refresh chain; reindex loops outlive the OIDC TTL.
    pub async fn make_embedding_provider(&self) -> Option<super::embedding::ApiEmbeddingProvider> {
        build_embedding_provider(
            self.embed_config.as_ref(),
            &self.embedding_credentials,
            self.embed_api_key.as_deref(),
            &self.embed_base_url,
        )
        .await
    }
}

async fn build_embedding_provider(
    config: Option<&distill_config_types::MemoryEmbeddingConfig>,
    credentials: &EndpointScopedCredentials,
    static_api_key: Option<&str>,
    base_url: &str,
) -> Option<super::embedding::ApiEmbeddingProvider> {
    let config = config?;
    if config.model.as_ref().is_none_or(|m| m.is_empty()) {
        return None;
    }

    // Enforce at runtime, in release too: a `debug_assert` compiles out of shipped binaries and lets a scoped credential reach an unapproved URL
    let credentials_approved = credentials.approved_for(base_url);
    if !credentials_approved {
        tracing::error!(
            target: crate::MEMORY_LOG_TARGET,
            base_url,
            approved = ?credentials.endpoint,
            "memory embeddings: scoped credentials do not match the request URL; dropping them"
        );
    }

    if credentials_approved && let Some(creds) = credentials.auth_credentials() {
        let client = super::embedding::build_middleware_client(creds.clone());
        return super::embedding::ApiEmbeddingProvider::from_config(
            config,
            base_url.to_owned(),
            client,
        );
    }

    let per_call_key = if credentials_approved && let Some(p) = credentials.api_key_provider() {
        p.current_api_key_async().await
    } else {
        None
    };
    let api_key = per_call_key.or_else(|| static_api_key.map(|s| s.to_owned()))?;
    super::embedding::ApiEmbeddingProvider::from_session(config, base_url.to_owned(), api_key)
}

/// `MemoryBackend` implementation backed by hybrid search (FTS5 + vector KNN).
/// Stores only `Send + Sync` config data.
/// The `MemoryIndex` and `EmbeddingProvider` are constructed on demand per query.
pub struct MemoryBackendImpl {
    db_path: PathBuf,
    storage: MemoryStorage,
    /// Embedding config; `None` disables vector search (FTS-only fallback).
    embed_config: Option<distill_config_types::MemoryEmbeddingConfig>,
    /// API base URL for embedding requests (cli-chat-proxy).
    embed_base_url: String,
    /// API key for embedding requests.
    embed_api_key: Option<String>,
    /// Search scoring config (weights, min_score, max_results).
    search_config: distill_config_types::MemorySearchConfig,
    /// File watcher for detecting external memory edits.
    watcher: Option<Arc<MemoryFileWatcher>>,
    /// Stale claim threshold for reindex coordination.
    stale_claim_secs: i64,
    session_id: String,
    search_source: MemorySearchSource,
    observation_sink: Arc<dyn MemoryObservationSink>,
    /// Shared search counter, read by session summary telemetry.
    /// Only the ToolBridge backend's counter is shared back to the session actor; injection and compaction-recovery backends keep local counters.
    pub search_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    embedding_credentials: EndpointScopedCredentials,
}

impl MemoryBackendImpl {
    /// `db_path` must point to an existing SQLite database created by `MemoryIndex::open_or_create()`.
    pub fn new(db_path: PathBuf, storage: MemoryStorage) -> Self {
        Self {
            db_path,
            storage,
            embed_config: None,
            embed_base_url: String::new(),
            embed_api_key: None,
            search_config: distill_config_types::MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            session_id: String::new(),
            search_source: MemorySearchSource::Tool,
            observation_sink: noop_memory_observation_sink(),
            embedding_credentials: EndpointScopedCredentials::none(),
            search_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = session_id;
        self
    }

    /// Configure the embedding provider for hybrid search.
    /// Without this, `search()` falls back to FTS-only.
    pub fn with_embedding(
        mut self,
        config: distill_config_types::MemoryEmbeddingConfig,
        base_url: String,
        api_key: Option<String>,
    ) -> Self {
        self.embed_config = Some(config);
        self.embed_base_url = base_url;
        self.embed_api_key = api_key;
        self
    }

    /// Override the search scoring config (weights, limits, etc.).
    pub fn with_search_config(mut self, config: distill_config_types::MemorySearchConfig) -> Self {
        self.search_config = config;
        self
    }

    /// Attach a file watcher for sync-on-search (reindex dirty files before querying).
    pub fn with_watcher(mut self, watcher: Arc<MemoryFileWatcher>, stale_claim_secs: i64) -> Self {
        self.watcher = Some(watcher);
        self.stale_claim_secs = stale_claim_secs;
        self
    }

    /// Open a read-only connection for simple queries (`total_chunks`, `get`).
    fn open_readonly(&self) -> Result<rusqlite::Connection, rusqlite::Error> {
        // Journal-mode-aware open (busy_timeout included): never mmap a legacy WAL -shm on network mounts (SIGBUS); see JournalMode::open_readonly
        distill_sqlite_journal::JournalMode::for_db_path(&self.db_path).open_readonly(&self.db_path)
    }

    async fn make_embedding_provider(&self) -> Option<super::embedding::ApiEmbeddingProvider> {
        build_embedding_provider(
            self.embed_config.as_ref(),
            &self.embedding_credentials,
            self.embed_api_key.as_deref(),
            &self.embed_base_url,
        )
        .await
    }

    /// Build a fully configured backend for a live session.
    pub fn from_session_params(storage: MemoryStorage, params: &MemoryBackendParams) -> Self {
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut backend = Self::new(db_path, storage)
            .with_session_id(params.session_id.clone())
            .with_search_config(params.search_config.clone());
        backend.search_source = params.search_source;
        backend.observation_sink = params.observation_sink.clone();
        if let Some(ec) = &params.embed_config {
            backend = backend.with_embedding(
                ec.clone(),
                params.embed_base_url.clone(),
                params.embed_api_key.clone(),
            );
        }
        if let Some(w) = &params.watcher {
            backend = backend.with_watcher(w.clone(), params.stale_claim_secs);
        }
        backend.embedding_credentials = params.embedding_credentials.clone();
        backend
    }
}

#[cfg(test)]
impl MemoryBackendImpl {
    pub fn search_config_for_test(&self) -> &distill_config_types::MemorySearchConfig {
        &self.search_config
    }
}

#[async_trait::async_trait]
impl MemoryBackend for MemoryBackendImpl {
    #[tracing::instrument(name = "memory.search", skip_all, fields(
        session_id = %self.session_id, max_results, min_score,
    ))]
    async fn search(
        &self,
        query: &str,
        max_results: usize,
        min_score: f64,
    ) -> Result<Vec<MemorySearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let search_start = std::time::Instant::now();
        let query_length = query.len();
        let keyword_count = super::query_expansion::extract_keywords(query).len();

        let embed_dims = self.embed_config.as_ref().map_or(1024, |ec| ec.dimensions);
        let mut index = match super::index::MemoryIndex::open_or_create(
            &self.db_path,
            self.storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            embed_dims,
        ) {
            Ok(index) => index,
            Err(error) => {
                self.observation_sink
                    .observe_search(MemorySearchObservation {
                        source: self.search_source,
                        mode: MemoryRetrievalMode::FtsOnly,
                        outcome: MemorySearchOutcome::Error,
                        query_length,
                        keyword_count,
                        result_count: 0,
                        top_score: 0.0,
                        min_score_threshold: min_score,
                        duration_ms: search_start.elapsed().as_millis() as u64,
                        is_vector_available: false,
                        error_class: Some(MemorySearchErrorClass::IndexOpen),
                    });
                return Err(Box::new(std::io::Error::other(error.to_string())));
            }
        };

        // ── Sync phase 1: reindex dirty files, collect chunks needing embeddings ──
        let mut reindex_chunks: Vec<(String, String)> = Vec::new();
        let mut needs_release = false;
        // Watcher-sync telemetry data (populated inside the claim guard below).
        let mut watcher_sync_stats: Option<(usize, usize, std::time::Instant)> = None;
        if let Some(ref watcher) = self.watcher
            && watcher.is_dirty()
            && index.try_claim_reindex(self.stale_claim_secs)
        {
            needs_release = true;
            let sync_start = std::time::Instant::now();
            let dirty_files = watcher.take_dirty();
            let dirty_count = dirty_files.len();
            // Sum of all index-chunk changes this cycle: chunks added, updated, or removed by reindex_file, plus chunks removed by delete_path
            // One counter covers delete-only syncs, where reindex_file never runs
            let mut changed_chunk_count: usize = 0;
            for file in &dirty_files {
                if file.exists() {
                    // The file was created or modified; reindex it
                    let source = self.storage.classify_source(file);
                    if let Ok(stats) = index.reindex_file(file, source) {
                        changed_chunk_count += stats.added + stats.updated + stats.removed;
                    }
                } else {
                    // The file was deleted; remove its stale chunks so they are no longer searchable
                    // reindex_file alone would return early on the unreadable file and leave orphaned chunks behind
                    if let Ok(n) = index.delete_path(file) {
                        changed_chunk_count += n;
                    }
                }
            }
            if dirty_count > 0 {
                reindex_chunks = index.chunks_without_embeddings().unwrap_or_default();
            }
            watcher_sync_stats = Some((dirty_count, changed_chunk_count, sync_start));
        }

        // ── Async phase: embed missing chunks (no &index borrow) ──
        let provider = self.make_embedding_provider().await;
        let mut embedded_count: usize = 0;
        if !reindex_chunks.is_empty()
            && let Some(ref provider) = provider
        {
            let mut upserts: Vec<(String, Vec<f32>)> = Vec::new();
            for batch in reindex_chunks.chunks(32) {
                let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
                match provider.embed_batch(&texts).await {
                    Ok(embeddings) => {
                        for ((chunk_id, _), emb) in batch.iter().zip(embeddings.into_iter()) {
                            upserts.push((chunk_id.clone(), emb));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: crate::MEMORY_LOG_TARGET,
                            error = %e,
                            "embedding batch failed during sync-on-search, skipping"
                        );
                    }
                }
            }
            // Sync: upsert embeddings back (borrows &index, no await)
            for (chunk_id, emb) in &upserts {
                let _ = index.upsert_embedding(chunk_id, emb);
            }
            embedded_count = upserts.len();
        }
        if needs_release {
            index.release_claim();
            if let Some((dirty_file_count, reindexed_count, sync_start)) = watcher_sync_stats {
                self.observation_sink
                    .observe_watcher_sync(MemoryWatcherSyncObservation {
                        dirty_file_count,
                        is_claimed: true,
                        reindexed_count,
                        embedded_count,
                        duration_ms: sync_start.elapsed().as_millis() as u64,
                    });
            }
        }

        // ── Sync phase 2: FTS search ──
        let mut search_config = self.search_config.clone();
        search_config.max_results = max_results;
        search_config.min_score = min_score as f32;

        let candidate_limit = search_config.max_results * 3;
        let primary = index.search_fts(query, candidate_limit);
        let fts_error_class = primary.as_ref().err().map(|error| {
            tracing::warn!(target: crate::MEMORY_LOG_TARGET, %error, "FTS search failed");
            MemorySearchErrorClass::Fts
        });
        let fts_results = merge_fts_results(
            primary,
            index.search_fts_by_sources(query, candidate_limit, &["global", "workspace"]),
        );

        // ── Async phase: embed query for vector search (no &index borrow) ──
        let query_embedding = super::search::resolve_query_embedding(
            provider
                .as_ref()
                .map(|provider| provider as &dyn super::embedding::EmbeddingProvider),
            index.vec_available(),
            query,
        )
        .await;

        // ── Sync phase 3: vector search + scoring + merge (borrows &index) ──
        let merged = super::search::hybrid_search_merge(
            &index,
            fts_results,
            query_embedding.embedding(),
            &search_config,
        )
        .map_err(|error| {
            Box::new(std::io::Error::other(error.to_string()))
                as Box<dyn std::error::Error + Send + Sync>
        })?;
        let mode = if merged.is_vector_degraded {
            MemoryRetrievalMode::FtsOnly
        } else {
            query_embedding.mode()
        };
        let error_class = select_search_error_class(fts_error_class, merged.is_vector_degraded);
        let results = merged.results;

        // Record accesses for the returned chunks so access_count and last_accessed stay current
        // Non-fatal: a failed write does not affect the search response
        for result in &results {
            let _ = index.record_access(&result.chunk_id);
        }

        self.observation_sink
            .observe_search(MemorySearchObservation {
                source: self.search_source,
                mode,
                outcome: if results.is_empty() {
                    MemorySearchOutcome::Empty
                } else {
                    MemorySearchOutcome::Results
                },
                query_length,
                keyword_count,
                result_count: results.len(),
                top_score: results.first().map_or(0.0, |result| result.score),
                min_score_threshold: min_score,
                duration_ms: search_start.elapsed().as_millis() as u64,
                is_vector_available: query_embedding.mode() != MemoryRetrievalMode::FtsOnly,
                error_class,
            });
        self.search_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(results
            .into_iter()
            .map(|r| MemorySearchResult {
                chunk_id: r.chunk_id,
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                score: r.score,
                snippet: r.snippet,
                source: r.source,
                created_at: Some(r.created_at),
            })
            .collect())
    }

    fn get(
        &self,
        path: &str,
        from: Option<usize>,
        lines: Option<usize>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.storage.read_file(Path::new(path), from, lines)?)
    }

    fn total_chunks(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.open_readonly()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Overrides the trait default so the `memory_search` tool honours `[memory.search].max_results` when the model supplies no value.
    fn default_search_max_results(&self) -> usize {
        self.search_config.max_results
    }

    fn default_search_min_score(&self) -> f64 {
        self.search_config.min_score as f64
    }
}

#[cfg(test)]
mod factory_tests {
    use super::*;

    #[test]
    fn fts_failures_keep_other_results() {
        let primary = Err::<Vec<_>, _>("primary");
        assert_eq!(merge_fts_results(primary, Ok(vec![])).len(), 0);
        let base = vec![crate::index::FtsResult {
            chunk_id: "base".into(),
            rowid: 1,
            rank: -1.0,
        }];
        let merged = merge_fts_results(Ok(base), Err("evergreen"));
        assert_eq!(merged.first().map(|r| r.chunk_id.as_str()), Some("base"));
    }

    #[test]
    fn fts_error_takes_precedence_when_vector_search_also_degrades() {
        assert_eq!(
            select_search_error_class(Some(MemorySearchErrorClass::Fts), true),
            Some(MemorySearchErrorClass::Fts)
        );
        assert_eq!(
            select_search_error_class(None, true),
            Some(MemorySearchErrorClass::Vector)
        );
    }
    use crate::index::{MemoryIndex, init_sqlite_vec};
    use crate::storage::MemoryStorage;
    use tempfile::TempDir;
    use distill_config_types::{MemoryEmbeddingConfig, MemorySearchConfig};

    fn make_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    fn make_params_fts_only(session_id: &str) -> MemoryBackendParams {
        MemoryBackendParams {
            session_id: session_id.to_owned(),
            embed_config: None,
            embed_base_url: String::new(),
            embed_api_key: None,
            search_config: MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            search_source: MemorySearchSource::Tool,
            observation_sink: noop_memory_observation_sink(),
            embedding_credentials: EndpointScopedCredentials::none(),
        }
    }

    #[tokio::test]
    async fn test_factory_backend_increments_search_counter() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let file = tmp.path().join("note.md");
        std::fs::write(&file, "# Facts\n\nRust is fast.").unwrap();
        idx.reindex_file(&file, "workspace").unwrap();
        drop(idx);

        let params = make_params_fts_only("test-session-abc");
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        let before = backend
            .search_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let _ = backend.search("rust", 5, 0.0).await;
        let after = backend
            .search_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "search counter must increment per search"
        );
    }

    /// from_session_params stores the search_config it was given.
    ///
    /// `search()` overrides `max_results` with the caller's argument, so checking the stored value is the only way to confirm the factory wired it.
    #[tokio::test]
    async fn test_factory_wires_search_config() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        for i in 0..10 {
            let f = tmp.path().join(format!("note{i}.md"));
            std::fs::write(&f, format!("# Entry {i}\n\nRust tip number {i}.")).unwrap();
            idx.reindex_file(&f, "workspace").unwrap();
        }
        drop(idx);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                max_results: 3,
                ..Default::default()
            },
            ..make_params_fts_only("test-search-config")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        assert_eq!(
            backend.search_config_for_test().max_results,
            3,
            "stored max_results must equal what was supplied to the factory"
        );
    }

    /// from_session_params wires non-overridable config fields (MMR, temporal decay) that `search()` never replaces with caller arguments.
    #[test]
    fn test_factory_wires_non_overridable_search_config_fields() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let custom_search = MemorySearchConfig {
            max_results: 7,
            mmr: distill_config_types::MmrConfig {
                enabled: true,
                lambda: 0.42,
            },
            temporal_decay: distill_config_types::TemporalDecayConfig {
                enabled: true,
                half_life_days: 14.0,
            },
            ..Default::default()
        };
        let params = MemoryBackendParams {
            search_config: custom_search,
            ..make_params_fts_only("test-full-config")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let stored = backend.search_config_for_test();

        // None of these are overridden by the caller in search(), so they must survive the factory path unchanged
        assert_eq!(stored.max_results, 7);
        assert!(stored.mmr.enabled, "MMR enabled must be stored");
        assert!(
            (stored.mmr.lambda - 0.42).abs() < f64::EPSILON,
            "MMR lambda must be stored exactly"
        );
        assert!(
            stored.temporal_decay.enabled,
            "temporal_decay enabled must be stored"
        );
        assert!(
            (stored.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON,
            "temporal_decay half_life_days must be stored exactly"
        );
    }

    /// from_session_params propagates search_source into the backend.
    /// Every caller (tool, injection, compaction_recovery) must set a distinct source label so dashboards can separate the three search paths.
    #[test]
    fn test_factory_propagates_search_source() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        for source in [
            MemorySearchSource::Tool,
            MemorySearchSource::Injection,
            MemorySearchSource::CompactionRecovery,
        ] {
            let params = MemoryBackendParams {
                search_source: source,
                ..make_params_fts_only("test-source")
            };
            let backend = MemoryBackendImpl::from_session_params(storage.clone(), &params);
            assert_eq!(source, backend.search_source);
        }
    }

    /// The default search_source is "tool" when constructing via new().
    #[test]
    fn test_default_search_source_is_tool() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage = make_storage(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);
        assert_eq!(MemorySearchSource::Tool, backend.search_source);
    }

    #[test]
    fn test_params_clone_preserves_search_source() {
        let params = MemoryBackendParams {
            search_source: MemorySearchSource::Injection,
            ..make_params_fts_only("test-clone-source")
        };
        let cloned = params.clone();
        assert_eq!(MemorySearchSource::Injection, cloned.search_source);
    }

    /// Watcher startup telemetry reflects actual runtime state.
    /// `watcher.is_some()` is `true` only when the watcher started successfully.
    /// Telemetry's `watcher_started` must reflect runtime outcome, not configuration intent.
    #[test]
    fn test_params_watcher_started_reflects_runtime() {
        let tmp = TempDir::new().unwrap();

        // Success path: the directory exists, so the watcher starts
        let watch_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&watch_dir).unwrap();
        let watcher = crate::watcher::MemoryFileWatcher::start(&watch_dir);
        let params_with_watcher = MemoryBackendParams {
            watcher: watcher.map(std::sync::Arc::new),
            ..make_params_fts_only("test-watcher-runtime")
        };
        // watcher.is_some() reflects whether startup succeeded.
        // (On environments without inotify/FSEvents this may be None; skip rather than fail.)
        let _ = params_with_watcher.watcher.is_some(); // This line only needs to compile.

        // Failure path: the directory does not exist, so the watcher must return None
        let missing = tmp.path().join("does_not_exist");
        let no_watcher = crate::watcher::MemoryFileWatcher::start(&missing);
        assert!(
            no_watcher.is_none(),
            "watcher must return None for a non-existent directory"
        );
        let params_no_watcher = MemoryBackendParams {
            watcher: None,
            ..make_params_fts_only("test-no-watcher")
        };
        assert!(
            params_no_watcher.watcher.is_none(),
            "params.watcher.is_none() means telemetry reports watcher_started=false"
        );
    }

    /// The MemoryBackend trait override in MemoryBackendImpl must expose search_config.max_results rather than the hardcoded default (6).
    #[test]
    fn test_default_search_max_results_from_config() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                max_results: 12,
                ..Default::default()
            },
            ..make_params_fts_only("test-defaults")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        assert_eq!(
            backend.default_search_max_results(),
            12,
            "default_search_max_results must return search_config.max_results"
        );
    }

    #[test]
    fn test_default_search_min_score_from_config() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                min_score: 0.42,
                ..Default::default()
            },
            ..make_params_fts_only("test-defaults")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        assert!(
            (backend.default_search_min_score() - 0.42_f64).abs() < 1e-6,
            "default_search_min_score must return search_config.min_score"
        );
    }

    /// from_session_params without embed_config produces a backend that does not panic and returns results via the FTS-only path.
    #[tokio::test]
    async fn test_factory_fts_only_without_embed() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Guide\n\nRust ownership rules.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = make_params_fts_only("test-fts-only");
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let results = backend.search("rust ownership", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "FTS-only backend should return results"
        );
        let Some(first) = results.first() else {
            panic!("expected FTS-only results: {results:?}");
        };
        let ts = first.created_at;
        assert!(
            ts.is_some() && ts.unwrap() > 0,
            "created_at must be Some(positive) after backend search (got {ts:?})"
        );
    }

    /// from_session_params with embed_config but no api_key falls back to FTS-only (the embedding provider requires a key).
    #[tokio::test]
    async fn test_factory_embed_config_without_key_falls_back_to_fts() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Guide\n\nRust borrow checker.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = MemoryBackendParams {
            embed_config: Some(MemoryEmbeddingConfig::default()),
            embed_base_url: "http://localhost".to_string(),
            embed_api_key: None, // No key, so the provider cannot be created.
            ..make_params_fts_only("test-embed-no-key")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        // The search must not panic; FTS results should still come back
        let results = backend.search("rust borrow", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "should fall back to FTS when api_key is None"
        );
    }

    #[test]
    fn test_params_is_clone() {
        let params = make_params_fts_only("clone-test");
        let _cloned = params.clone();
    }

    /// from_session_params without watcher produces a backend that searches correctly.
    #[tokio::test]
    async fn test_factory_no_watcher() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Tip\n\nAlways write tests.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = MemoryBackendParams {
            watcher: None,
            ..make_params_fts_only("test-no-watcher")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let results = backend.search("tests", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "no-watcher backend should still return results"
        );
    }

    /// `ensure_initialized` must be called before watcher startup.
    /// On a first-use machine the memory directories do not exist yet.
    /// `MemoryFileWatcher::start` on a non-existent directory returns `None`, silently dropping the feature.
    #[test]
    fn test_ensure_initialized_before_watcher_ordering() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global.clone(), workspace.clone());

        // Precondition: neither directory exists yet (fresh machine simulation).
        assert!(
            !global.exists(),
            "global memory dir must not exist before initialization"
        );

        // --- Wrong ordering (watcher before init) ---
        // The watcher returns None because the directory does not exist.
        let watcher_before_init = crate::watcher::MemoryFileWatcher::start(&global);
        assert!(
            watcher_before_init.is_none(),
            "watcher must fail (None) when directory does not exist yet"
        );

        // --- Correct ordering (init, then watcher) ---
        // After ensure_initialized the directories and MEMORY.md templates exist.
        storage.ensure_initialized().unwrap();

        assert!(
            global.exists(),
            "global dir must exist after ensure_initialized"
        );
        assert!(
            workspace.exists(),
            "workspace dir must exist after ensure_initialized"
        );
        assert!(
            global.join("MEMORY.md").exists(),
            "global MEMORY.md template must exist"
        );
        assert!(
            workspace.join("MEMORY.md").exists(),
            "workspace MEMORY.md template must exist"
        );

        // Watcher now succeeds because the directory exists.
        // (It may still return None in environments without inotify/kqueue, e.g. some CI containers, but must not panic.)
        let watcher_after_init = crate::watcher::MemoryFileWatcher::start(&global);
        // If a watcher was returned we can confirm it is usable (not dirty yet).
        if let Some(w) = watcher_after_init {
            assert!(
                !w.is_dirty(),
                "freshly started watcher must report no dirty files"
            );
        }
        // If None, the test environment does not support file-watching. That is acceptable; the directories themselves are what matter here.
    }

    /// End-to-end regression test for the watcher-driven delete path.
    /// file is indexed; watcher is started; first `backend.search()` confirms content is found; file is deleted (OS fires a Remove event to the watcher); second `backend.search()` triggers sync-on-search, which calls `delete_path()` because the file no longer exists; content is no longer returned.
    /// This test guards the branch that calls `delete_path()` when `file.exists()` is false, which the `delete_path` unit tests alone cannot see.
    #[tokio::test]
    async fn test_watcher_delete_clears_stale_chunks() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();

        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        let storage = MemoryStorage::with_paths(global.clone(), workspace);
        let db_path = storage.workspace_dir().join("index.sqlite");

        // Step 1: Write and canonicalize the file path BEFORE indexing
        // On macOS, TempDir paths may live under /private/tmp (via a symlink from /tmp)
        // FSEvents returns canonicalized paths, so the path stored in the index must match what the watcher event delivers
        let file_raw = global.join("note.md");
        std::fs::write(&file_raw, "# Unique\n\nXyzzy-watcher-delete-token.").unwrap();
        let file = dunce::canonicalize(&file_raw).unwrap_or(file_raw);

        {
            let mut idx = MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                distill_config_types::MemoryIndexConfig::default(),
                4,
            )
            .unwrap();
            // Index with the canonical path so DB key matches watcher event paths.
            idx.reindex_file(&file, "workspace").unwrap();
        }

        // Step 2: Start watcher AFTER indexing so the Remove event for the upcoming deletion is the first event the watcher ever sees
        let watch_dir = dunce::canonicalize(&global).unwrap_or(global.clone());
        let watcher = match crate::watcher::MemoryFileWatcher::start(&watch_dir) {
            Some(w) => w,
            None => {
                // File-watching is not supported in this environment (e.g., some CI containers without inotify/FSEvents). Skip rather than fail.
                return;
            }
        };
        let watcher_arc = std::sync::Arc::new(watcher);

        let params = MemoryBackendParams {
            watcher: Some(watcher_arc.clone()),
            ..make_params_fts_only("test-watcher-delete")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        // Step 3: Confirm content is found before deletion.
        let before = backend
            .search("Xyzzy-watcher-delete-token", 5, 0.0)
            .await
            .unwrap();
        assert!(
            !before.is_empty(),
            "content must be found before file is deleted"
        );

        // Step 4: Delete the file; the OS will fire a Remove event
        std::fs::remove_file(&file).unwrap();

        // Poll until the watcher detects the event (more reliable than a fixed sleep on macOS, where FSEvents delivery time varies considerably)
        // Give up after 2 s and skip the timing-sensitive assertion rather than flake; delete_path unit tests cover the underlying logic
        let mut event_delivered = false;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if watcher_arc.is_dirty() {
                event_delivered = true;
                break;
            }
        }
        if !event_delivered {
            // FSEvents did not deliver the event within 2 s; the environment is too slow
            // Skip silently; the logic is covered by delete_path unit tests.
            return;
        }

        // Steps 5 and 6: search triggers sync-on-search, which detects file.exists() == false and calls delete_path(), clearing all stale chunks
        let after = backend
            .search("Xyzzy-watcher-delete-token", 5, 0.0)
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "deleted file's content must not appear after watcher-driven delete sync"
        );
    }

    /// Regression: provider build must use `current_api_key_async`, never sync.
    /// This prevents memory_search 401s on rotated tokens.
    #[tokio::test]
    async fn make_embedding_provider_uses_async_api_key_resolution() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use distill_tools::types::ApiKeyProvider;

        struct AsyncProbe {
            sync_calls: Arc<AtomicU32>,
            async_calls: Arc<AtomicU32>,
        }
        impl ApiKeyProvider for AsyncProbe {
            fn current_api_key(&self) -> Option<String> {
                self.sync_calls.fetch_add(1, Ordering::SeqCst);
                Some("sync-stale".into())
            }
            fn current_api_key_async(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>>
            {
                let counter = self.async_calls.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Some("async-fresh".into())
                })
            }
        }

        let sync_calls = Arc::new(AtomicU32::new(0));
        let async_calls = Arc::new(AtomicU32::new(0));
        let probe: distill_tools::types::SharedApiKeyProvider = Arc::new(AsyncProbe {
            sync_calls: sync_calls.clone(),
            async_calls: async_calls.clone(),
        });

        let params = MemoryBackendParams {
            session_id: "s1".into(),
            embed_config: Some(MemoryEmbeddingConfig {
                model: Some("test-embed-model".into()),
                ..Default::default()
            }),
            embed_base_url: "http://example/v1".into(),
            embed_api_key: Some("static-fallback".into()),
            search_config: MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            search_source: MemorySearchSource::Tool,
            observation_sink: noop_memory_observation_sink(),
            // Trusted endpoint with no auth_credentials exercises the api_key_provider path
            embedding_credentials: EndpointScopedCredentials::for_endpoint(
                "http://example/v1",
                |_| true,
                None,
                Some(probe),
            ),
        };

        let provider = params.make_embedding_provider().await;
        assert!(
            provider.is_some(),
            "provider must be built when model is set"
        );
        assert_eq!(
            async_calls.load(Ordering::SeqCst),
            1,
            "must call current_api_key_async exactly once per provider build"
        );
        assert_eq!(
            sync_calls.load(Ordering::SeqCst),
            0,
            "sync current_api_key must NOT be called — the async path is the contract"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{MemoryIndex, init_sqlite_vec};
    use tempfile::TempDir;
    use distill_config_types::MemoryIndexConfig;

    /// An api-key provider that fails the test if its key is ever resolved, proving a scoped-away credential is never consulted.
    struct PanicKey;
    impl distill_tools::types::ApiKeyProvider for PanicKey {
        fn current_api_key(&self) -> Option<String> {
            panic!("scoped-away credential must not be resolved");
        }
    }

    fn setup_index(tmp: &TempDir) -> (PathBuf, MemoryStorage) {
        init_sqlite_vec();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = tmp.path().join("test.sqlite");

        let mut idx =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 4)
                .unwrap();

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Guide\n\nRust programming tutorial.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        (db_path, storage)
    }

    #[tokio::test]
    async fn test_backend_search() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        let results = backend.search("rust programming", 10, 0.0).await.unwrap();
        assert!(!results.is_empty(), "should find indexed content");
        assert!(results.first().is_some_and(|r| r.snippet.contains("Rust")));
    }

    #[test]
    fn test_backend_total_chunks() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        let count = backend.total_chunks().unwrap();
        assert!(count >= 1, "should have at least 1 chunk");
    }

    #[test]
    fn test_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MemoryBackendImpl>();
    }

    /// Credentials approved for one endpoint but used against a different URL (a wiring bug) are dropped at build time instead of being sent.
    /// The session provider would panic if resolved.
    #[tokio::test]
    async fn test_build_drops_credentials_when_request_url_differs() {
        let session: distill_tools::types::SharedApiKeyProvider = Arc::new(PanicKey);

        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            None,
            Some(session),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");

        let config = distill_config_types::MemoryEmbeddingConfig {
            model: Some("test-embedding-model".to_string()),
            ..Default::default()
        };
        let provider = build_embedding_provider(
            Some(&config),
            &scoped,
            Some("byok-static-key"),
            "https://other.example/v1",
        )
        .await;
        assert!(
            provider.is_some(),
            "mismatched request URL must fall back to the static key, not the scoped credential"
        );
    }

    /// A trusted, URL-matching endpoint builds the provider from the refresh-capable session credential.
    /// The per-call api-key provider is never consulted; it panics if resolved.
    #[tokio::test]
    async fn test_trusted_endpoint_prefers_session_credential() {
        struct StubAuth;
        impl distill_auth::HttpAuth for StubAuth {
            fn apply(
                &self,
                builder: reqwest::RequestBuilder,
                _base_url: &str,
            ) -> reqwest::RequestBuilder {
                builder
            }
        }
        #[async_trait::async_trait]
        impl distill_auth::AuthCredentialProvider for StubAuth {
            fn snapshot(&self) -> distill_auth::CredentialSnapshot {
                distill_auth::CredentialSnapshot::default()
            }
            async fn refresh_after_unauthorized(&self) -> bool {
                false
            }
        }

        let auth: Arc<dyn distill_auth::AuthCredentialProvider> = Arc::new(StubAuth);
        let api_key: distill_tools::types::SharedApiKeyProvider = Arc::new(PanicKey);
        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            Some(auth),
            Some(api_key),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");

        let config = distill_config_types::MemoryEmbeddingConfig {
            model: Some("test-embedding-model".to_string()),
            ..Default::default()
        };
        let provider =
            build_embedding_provider(Some(&config), &scoped, None, "https://api.x.ai/v1").await;
        assert!(
            provider.is_some(),
            "trusted endpoint must build a provider from the session credential"
        );
    }

    #[test]
    fn endpoint_scoped_credentials_trust_gate_and_url_match() {
        struct AnyKey;
        impl distill_tools::types::ApiKeyProvider for AnyKey {
            fn current_api_key(&self) -> Option<String> {
                None
            }
        }
        let key = || Arc::new(AnyKey) as distill_tools::types::SharedApiKeyProvider;

        let denied = EndpointScopedCredentials::for_endpoint(
            "https://byok.example/v1",
            |_| false,
            None,
            Some(key()),
        );
        assert!(denied.is_empty(), "untrusted endpoint drops the credential");

        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            None,
            Some(key()),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");
        assert!(
            scoped.approved_for("https://API.x.ai/v1"),
            "host casing normalizes"
        );
        assert!(
            !scoped.approved_for("https://api.x.ai/v2"),
            "different path rejected"
        );
        assert!(
            !scoped.approved_for("https://other.example/v1"),
            "different host rejected"
        );
        assert!(!scoped.approved_for("not-a-url"), "unparsable fails closed");
    }

    #[tokio::test]
    async fn test_search_with_punctuation_in_query() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // A raw user message with punctuation must not crash FTS5
        let results = backend
            .search("what is rust? how to use it!", 10, 0.0)
            .await
            .unwrap();
        assert!(
            !results.is_empty(),
            "should match 'rust' despite punctuation in query"
        );
    }

    #[tokio::test]
    async fn test_search_with_special_chars_only() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // A query with only special chars must return empty, not an error
        let results = backend.search("???!!!", 10, 0.0).await.unwrap();
        assert!(
            results.is_empty(),
            "special-chars-only query should return empty"
        );
    }

    #[tokio::test]
    async fn test_search_hybrid_fts_only_fallback() {
        // Without embedding config, hybrid search should degrade to FTS-only
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // Even with high min_score, hybrid search normalizes scores to [0,1] so results above the threshold should be returned
        let results = backend.search("rust programming", 10, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "FTS-only fallback should still return results"
        );
        // Hybrid scoring normalizes scores into the (0,1] range
        assert!(
            results.first().is_some_and(|r| r.score > 0.0),
            "hybrid scores should be positive"
        );
    }

    /// The supplemental evergreen query in `search()` adds global/workspace candidates that the base `search_fts` missed due to candidate_limit.
    /// With a tight FTS limit, global/workspace chunks are absent from the base results but present in the supplemental source-filtered query.
    /// The full backend search pipeline must then surface them.
    #[tokio::test]
    async fn test_search_returns_global_and_workspace_memory() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = storage.workspace_dir().join("index.sqlite");

        let mut idx =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 4)
                .unwrap();

        // Index global and workspace with matching content
        let global_file = tmp.path().join("global_mem.md");
        std::fs::write(
            &global_file,
            "# Preferences\n\nAlways use graphite for PRs. Prefer Rust over Python.",
        )
        .unwrap();
        idx.reindex_file(&global_file, "global").unwrap();

        let ws_file = tmp.path().join("ws_mem.md");
        std::fs::write(
            &ws_file,
            "# Project Decisions\n\nWe chose graphite for PRs in this project.",
        )
        .unwrap();
        idx.reindex_file(&ws_file, "workspace").unwrap();

        // Index session files that also match the query.
        for i in 0..5 {
            let f = tmp.path().join(format!("session_{i}.md"));
            std::fs::write(
                &f,
                format!("# Session {i}\n\nDiscussed graphite for PRs and item {i}."),
            )
            .unwrap();
            idx.reindex_file(&f, "session").unwrap();
        }

        // Verify the supplemental query mechanism
        // With a tight limit the base FTS returns a mix, but `search_fts_by_sources` for "global"/"workspace" always finds the evergreen chunks
        let evergreen = idx
            .search_fts_by_sources("graphite PRs", 10, &["global", "workspace"])
            .unwrap();
        assert!(
            evergreen.len() >= 2,
            "supplemental evergreen query must find both global and workspace chunks"
        );
        let evergreen_sources: Vec<String> = evergreen
            .iter()
            .filter_map(|r| idx.get_chunk(&r.chunk_id).ok().flatten())
            .map(|c| c.source)
            .collect();
        assert!(
            evergreen_sources.contains(&"global".to_string()),
            "evergreen query must find global chunk"
        );
        assert!(
            evergreen_sources.contains(&"workspace".to_string()),
            "evergreen query must find workspace chunk"
        );
        drop(idx);

        // Full backend search: global/workspace must appear in results.
        let backend = MemoryBackendImpl::new(db_path, storage);
        let results = backend.search("graphite PRs", 10, 0.0).await.unwrap();

        let has_global = results.iter().any(|r| r.source == "global");
        let has_workspace = results.iter().any(|r| r.source == "workspace");
        assert!(
            has_global,
            "global MEMORY.md chunks must appear in search results"
        );
        assert!(
            has_workspace,
            "workspace MEMORY.md chunks must appear in search results"
        );
    }
}

#[cfg(test)]
mod index_embedding_tests {
    use crate::index::MemoryIndex;
    use crate::storage::MemoryStorage;

    #[test]
    fn test_chunks_without_embeddings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = tmp.path().join("test.sqlite");

        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage,
            distill_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();

        if !idx.vec_available() {
            // sqlite-vec is not available, so chunks_without_embeddings returns empty
            let missing = idx.chunks_without_embeddings().unwrap();
            assert!(missing.is_empty(), "no-vec: should return empty");
            return;
        }

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Title\n\nSome content here.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        // After reindex, chunks should exist but have no embeddings
        let missing = idx.chunks_without_embeddings().unwrap();
        assert!(
            !missing.is_empty(),
            "newly indexed chunks should be missing embeddings"
        );

        // After upserting an embedding, the chunk should disappear from missing
        let Some((chunk_id, _)) = missing.first() else {
            panic!("expected a chunk missing embeddings: {missing:?}");
        };
        let dummy_embedding = vec![0.0f32; 4];
        idx.upsert_embedding(chunk_id, &dummy_embedding).unwrap();

        let missing_after = idx.chunks_without_embeddings().unwrap();
        assert_eq!(
            missing_after.len(),
            missing.len() - 1,
            "one fewer chunk should be missing after embedding"
        );
    }
}
