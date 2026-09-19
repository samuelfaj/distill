// Modified for Distill by Samuel Fajreldines, 2026.
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

#[derive(Debug, thiserror::Error)]
pub(crate) enum MemoryInitializationError {
    #[error("memory storage initialization failed: {0}")]
    Storage(#[source] std::io::Error),
    #[error("memory storage initialization task failed: {0}")]
    TaskJoin(#[source] tokio::task::JoinError),
}

async fn run_v2_initialization_blocking<F, T>(initialize: F) -> Result<T, MemoryInitializationError>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(initialize)
        .await
        .map_err(MemoryInitializationError::TaskJoin)?
        .map_err(MemoryInitializationError::Storage)
}

/// Initialize a session's configured memory storage without running v2's
/// filesystem and SQLite setup on the single-threaded actor runtime.
///
/// Legacy initialization remains inline to preserve its existing behavior.
pub(crate) async fn initialize_memory_storage(
    storage: crate::session::memory::MemoryStorage,
) -> Result<(), MemoryInitializationError> {
    if storage.mode().is_legacy() {
        return storage
            .ensure_initialized()
            .map_err(MemoryInitializationError::Storage);
    }
    run_v2_initialization_blocking(move || storage.ensure_initialized()).await
}

pub(crate) struct CaptureWorker {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio_util::task::AbortOnDropHandle<()>,
}

impl CaptureWorker {
    fn new(cancel: tokio_util::sync::CancellationToken, task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            cancel,
            task: tokio_util::task::AbortOnDropHandle::new(task),
        }
    }

    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    async fn cancel_and_join(mut self) {
        self.cancel.cancel();
        if let Err(error) = (&mut self.task).await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "memory-v2 capture worker join failed");
        }
    }

    async fn join_finished(mut self) {
        debug_assert!(self.task.is_finished());
        if let Err(error) = (&mut self.task).await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "memory-v2 finished capture worker join failed");
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct V2DreamWorkers {
    cancel: RefCell<tokio_util::sync::CancellationToken>,
    tasks: RefCell<Vec<tokio_util::task::AbortOnDropHandle<()>>>,
}

impl Default for V2DreamWorkers {
    fn default() -> Self {
        Self {
            cancel: RefCell::new(tokio_util::sync::CancellationToken::new()),
            tasks: RefCell::new(Vec::new()),
        }
    }
}

impl V2DreamWorkers {
    pub(crate) fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.borrow().clone()
    }

    pub(crate) fn track(&self, handle: tokio::task::JoinHandle<()>) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio_util::task::AbortOnDropHandle::new(handle));
    }

    pub(crate) async fn cancel_and_join(&self) {
        self.cancel.borrow().cancel();
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        for task in tasks {
            if let Err(error) = task.await {
                tracing::warn!(error = %error, "memory-v2 Dream worker join failed");
            }
        }
        *self.cancel.borrow_mut() = tokio_util::sync::CancellationToken::new();
    }
}

impl Drop for V2DreamWorkers {
    fn drop(&mut self) {
        self.cancel.get_mut().cancel();
        self.tasks.get_mut().clear();
    }
}

pub(crate) struct SessionMemory {
    /// Mode resolved when the session was spawned. Kept even while memory is
    /// disabled so toggles, telemetry, and trace uploads cannot switch roots.
    pub configured_mode: Option<crate::config::MemoryMode>,
    /// Rollout and kill switches resolved once at session spawn.
    pub v2_config: crate::config::MemoryV2Config,
    /// Storage layout resolved at spawn. Retained while disabled so re-enabling
    /// restores the pinned mode and any configured root override.
    pub configured_storage: Option<crate::session::memory::MemoryStorage>,
    /// `--no-memory` / `GROK_MEMORY=0`: memory stays off for the whole process.
    pub process_disabled: bool,
    /// The effective TOML started this session with `[memory] enabled = false`.
    /// The `/memory` toggle can still enable memory for the session.
    pub config_opt_out: bool,
    /// Whether enabling v2 carries curated legacy `MEMORY.md` files into the v2 scopes.
    /// Resolved at spawn from the root override and the maintenance rollout gate.
    pub v2_legacy_carryover: bool,
    /// A `/memory` toggle changed v2 state while a turn was running, so the system prompt's
    /// `<memory>` section is stale. Applied before the next turn is promoted.
    pub prompt_sync_pending: AtomicBool,
    /// Memory storage handle for writing flush output (None when memory disabled).
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub storage: RefCell<Option<crate::session::memory::MemoryStorage>>,
    /// Whether to write a session summary to memory on session end.
    pub save_on_end: bool,
    /// Legacy backend parameters. `None` when memory is disabled or v2 is selected.
    pub backend_params: Option<crate::session::memory::MemoryBackendParams>,
    /// First-turn memory injection behavior resolved from local and remote config.
    pub initial_injection_config: crate::config::MemoryInitialInjectionConfig,
    /// Per-process latch: the first-turn injection decision already ran in this session segment.
    /// Cross-segment idempotency comes from `conversation_has_memory_context`, not this flag.
    pub context_injected: AtomicBool,
    pub flush_config: crate::config::MemoryFlushConfig,
    /// When `true`, auto-compact checks are suppressed during the legacy
    /// in-context memory flush. Memory-v2 extraction uses its own worker latch.
    pub is_flushing: Arc<AtomicBool>,
    /// Cooperatively-cancelled, joinable memory-v2 worker. The wrapper also
    /// cancels and aborts on drop as a teardown backstop.
    pub capture_worker: RefCell<Option<CaptureWorker>>,
    /// Owns every capture-triggered Dream task until session teardown.
    pub dream_workers: V2DreamWorkers,
    /// Class of the most recent capture failure reported by this process, so
    /// `/flush` can attribute a still-failing queue correctly. The durable
    /// queue only keeps the sanitized error text, so a failure inherited from
    /// an earlier process has no class here.
    pub last_capture_failure:
        RefCell<Option<distill_telemetry::memory_telemetry::MemoryV2FailureClass>>,
    /// The compaction count at which the last flush ran (once-per-cycle guard).
    pub last_flush_compaction: AtomicU64,
    pub flush_count: AtomicU64,
    /// Content from the most recent successful flush, used for delta prompts.
    /// Wrapped in `RefCell` because `SessionActor` is single-threaded (LocalSet).
    pub last_flush_content: RefCell<Option<String>>,
    pub flush_success_count: AtomicU64,
    pub flush_error_count: AtomicU64,
    /// Counts model-initiated `memory_search` tool calls.
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub search_counter: RefCell<Option<Arc<AtomicU64>>>,
    /// Counts first-turn memory context injections.
    pub injection_count: AtomicU64,
    /// Counts post-compaction memory re-injection searches.
    pub compaction_recovery_count: AtomicU64,
    /// Total memory chunks added across all sources.
    pub chunks_added: Arc<AtomicU64>,
    /// Handle to the startup reindex+embed task, taken and awaited by the launch dream. `None`
    /// once taken, or when memory was not indexed at launch.
    pub init_reindex_handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// autoDream consolidation config.
    pub dream_config: crate::config::MemoryDreamConfig,
    pub dream_count: AtomicU64,
    pub dream_success_count: AtomicU64,
    pub dream_error_count: AtomicU64,
    pub token_totals: MemoryV2TokenTotals,
}

#[derive(Default)]
pub(crate) struct MemoryV2TokenTotals {
    pub capture_prompt_tokens: AtomicU64,
    pub capture_completion_tokens: AtomicU64,
    pub capture_cost_usd_ticks: AtomicU64,
    pub dream_prompt_tokens: AtomicU64,
    pub dream_completion_tokens: AtomicU64,
    pub dream_cost_usd_ticks: AtomicU64,
    pub injected_bytes: AtomicU64,
}

impl SessionMemory {
    pub(crate) fn is_enabled(&self) -> bool {
        self.storage.borrow().is_some()
    }

    pub(crate) fn mode(&self) -> Option<crate::config::MemoryMode> {
        self.configured_mode
            .or_else(|| self.storage.borrow().as_ref().map(|storage| storage.mode()))
    }

    pub(crate) fn uses_legacy_pipeline(&self) -> bool {
        self.is_enabled()
            && self
                .mode()
                .is_some_and(crate::config::MemoryMode::is_legacy)
    }

    pub(crate) fn can_capture_v2(&self) -> bool {
        self.is_enabled()
            && self.mode().is_some_and(crate::config::MemoryMode::is_v2)
            && self.v2_config.can_capture()
    }

    pub(crate) fn can_expose_v2(&self) -> bool {
        self.is_enabled()
            && self.mode().is_some_and(crate::config::MemoryMode::is_v2)
            && self.v2_config.can_expose_memory()
    }

    /// Why memory is off, or `None` while it is on. `/memory on` refuses unless this is
    /// `SessionToggle` or `ConfigOptOut`; the `/memory` modal offers its turn-on hint under the same rule.
    pub(crate) fn disabled_reason(
        &self,
    ) -> Option<crate::extensions::notification::MemoryDisabledReason> {
        use crate::extensions::notification::MemoryDisabledReason;
        if self.is_enabled() {
            return None;
        }
        let v2_restricted = self.mode() == Some(crate::config::MemoryMode::V2)
            && (self.v2_config.rollout == crate::config::MemoryV2Rollout::Off
                || !self.v2_config.file_writes_enabled);
        Some(if self.process_disabled {
            MemoryDisabledReason::ProcessDisabled
        } else if v2_restricted {
            MemoryDisabledReason::RolloutRestricted
        } else if self.configured_storage.is_none() {
            MemoryDisabledReason::NotConfigured
        } else if self.config_opt_out {
            MemoryDisabledReason::ConfigOptOut
        } else {
            MemoryDisabledReason::SessionToggle
        })
    }

    /// Clone the storage out of the `RefCell`, dropping the borrow immediately.
    pub(crate) fn storage(&self) -> Option<crate::session::memory::MemoryStorage> {
        self.storage.borrow().clone()
    }

    pub(crate) fn try_acquire_flush_lock(&self) -> bool {
        self.is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn release_flush_lock(&self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn track_capture_worker(
        &self,
        cancel: tokio_util::sync::CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) {
        debug_assert!(!self.capture_worker_is_running());
        self.capture_worker
            .replace(Some(CaptureWorker::new(cancel, task)));
    }

    pub(crate) fn capture_worker_is_running(&self) -> bool {
        self.capture_worker
            .borrow()
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    pub(crate) async fn join_finished_capture_worker(&self) {
        let worker = {
            let mut slot = self.capture_worker.borrow_mut();
            if slot.as_ref().is_some_and(CaptureWorker::is_finished) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(worker) = worker {
            worker.join_finished().await;
        }
    }

    pub(crate) async fn stop_capture_worker(&self) {
        let worker = self.capture_worker.borrow_mut().take();
        if let Some(worker) = worker {
            worker.cancel_and_join().await;
        }
    }

    pub(crate) fn record_capture_failure(
        &self,
        failure_class: Option<distill_telemetry::memory_telemetry::MemoryV2FailureClass>,
    ) {
        self.last_capture_failure.replace(failure_class);
    }

    pub(crate) fn last_capture_failure(
        &self,
    ) -> Option<distill_telemetry::memory_telemetry::MemoryV2FailureClass> {
        *self.last_capture_failure.borrow()
    }

    /// Record a flush result and increment the appropriate counter.
    /// "written" increments success, "error" increments error.
    /// Anything else ("nothing_to_store", "rejected") increments only the total flush count.
    pub(crate) fn record_flush_result(&self, outcome: &str) {
        self.flush_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match outcome {
            "written" => {
                self.flush_success_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            "error" => {
                self.flush_error_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub(crate) fn record_dream_result(&self, success: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.dream_count.fetch_add(1, Relaxed);
        if success {
            self.dream_success_count.fetch_add(1, Relaxed);
        } else {
            self.dream_error_count.fetch_add(1, Relaxed);
        }
    }

    /// Record a neutral dream outcome (nothing to consolidate / skipped).
    pub(crate) fn record_dream_neutral(&self) {
        self.dream_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn record_capture_usage(
        &self,
        usage: &distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
    ) {
        add_model_usage(
            usage,
            &self.token_totals.capture_prompt_tokens,
            &self.token_totals.capture_completion_tokens,
            &self.token_totals.capture_cost_usd_ticks,
        );
    }

    pub(crate) fn record_dream_usage(
        &self,
        usage: &distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
    ) {
        add_model_usage(
            usage,
            &self.token_totals.dream_prompt_tokens,
            &self.token_totals.dream_completion_tokens,
            &self.token_totals.dream_cost_usd_ticks,
        );
    }

    pub(crate) fn record_injected_bytes(&self, bytes: u64) {
        self.token_totals
            .injected_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Open (or create) the memory index for the current workspace.
    pub(crate) fn open_index(
        &self,
        storage: &crate::session::memory::MemoryStorage,
    ) -> Option<crate::session::memory::MemoryIndex> {
        if !self.uses_legacy_pipeline() {
            return None;
        }
        let embed_dims = self
            .backend_params
            .as_ref()
            .and_then(|p| p.embed_config.as_ref())
            .map_or(1024, |c| c.dimensions);
        let db_path = storage.workspace_dir().join("index.sqlite");
        crate::session::memory::MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            Default::default(),
            embed_dims,
        )
        .ok()
    }

    /// Await the startup reindex+embed task if it is still tracked; `None` returns at once.
    pub(crate) async fn await_init_reindex(&self) {
        let handle = self.init_reindex_handle.borrow_mut().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Reindex a file and embed new chunks when embedding is configured.
    pub(crate) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        let Some(storage) = self.storage.borrow().clone() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let _ = index.reindex_file(path, source);
            if let Some(ref params) = self.backend_params
                && let Some(provider) = params.make_embedding_provider().await
            {
                crate::session::memory::embed_missing_chunks(&index, &provider).await;
            }
        }
    }

    /// Remove chunks for the given file paths from the search index.
    /// Used after dream consolidation deletes processed session files so that stale chunks don't linger in the index.
    /// Best-effort: errors are logged but don't propagate.
    pub(crate) fn delete_paths_from_index(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(storage) = self.storage.borrow().clone() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let mut total_removed = 0usize;
            for path in paths {
                match index.delete_path(path) {
                    Ok(n) => total_removed += n,
                    Err(e) => {
                        tracing::warn!(
                            target: distill_telemetry::memory_log::TARGET,
                            path = %path.display(),
                            error = %e,
                            "DREAM_CLEANUP: failed to remove chunks from index"
                        );
                    }
                }
            }
            if total_removed > 0 {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    chunks_removed = total_removed,
                    files = paths.len(),
                    "DREAM_CLEANUP: removed stale chunks from index"
                );
            }
        }
    }

    /// Collect telemetry counters for session-end summary.
    pub(crate) fn telemetry_snapshot(&self) -> MemoryTelemetry {
        use std::sync::atomic::Ordering::Relaxed;
        MemoryTelemetry {
            flush_count: self.flush_count.load(Relaxed),
            flush_success_count: self.flush_success_count.load(Relaxed),
            flush_error_count: self.flush_error_count.load(Relaxed),
            tool_search_count: self
                .search_counter
                .borrow()
                .as_ref()
                .map_or(0, |c| c.load(Relaxed)),
            injection_count: self.injection_count.load(Relaxed),
            compaction_recovery_count: self.compaction_recovery_count.load(Relaxed),
            chunks_added: self.chunks_added.load(Relaxed),
            dream_count: self.dream_count.load(Relaxed),
            dream_success_count: self.dream_success_count.load(Relaxed),
            dream_error_count: self.dream_error_count.load(Relaxed),
            capture_prompt_tokens: self.token_totals.capture_prompt_tokens.load(Relaxed),
            capture_completion_tokens: self.token_totals.capture_completion_tokens.load(Relaxed),
            capture_cost_usd_ticks: self.token_totals.capture_cost_usd_ticks.load(Relaxed),
            dream_prompt_tokens: self.token_totals.dream_prompt_tokens.load(Relaxed),
            dream_completion_tokens: self.token_totals.dream_completion_tokens.load(Relaxed),
            dream_cost_usd_ticks: self.token_totals.dream_cost_usd_ticks.load(Relaxed),
            injected_bytes: self.token_totals.injected_bytes.load(Relaxed),
        }
    }
}

fn add_model_usage(
    usage: &distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
    prompt_tokens: &AtomicU64,
    completion_tokens: &AtomicU64,
    cost_usd_ticks: &AtomicU64,
) {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(tokens) = usage.prompt_tokens {
        prompt_tokens.fetch_add(u64::from(tokens), Relaxed);
    }
    if let Some(tokens) = usage.completion_tokens {
        completion_tokens.fetch_add(u64::from(tokens), Relaxed);
    }
    if let Some(ticks) = usage
        .cost_usd_ticks
        .and_then(|ticks| u64::try_from(ticks).ok())
    {
        cost_usd_ticks.fetch_add(ticks, Relaxed);
    }
}

#[must_use]
#[cfg(test)]
pub(crate) struct FlushLockGuard {
    is_flushing: Arc<AtomicBool>,
}

#[cfg(test)]
impl FlushLockGuard {
    fn try_acquire(is_flushing: Arc<AtomicBool>) -> Option<Self> {
        is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
            .then_some(Self { is_flushing })
    }
}

#[cfg(test)]
impl Drop for FlushLockGuard {
    fn drop(&mut self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Snapshot of memory telemetry counters for session-end logging.
pub(crate) struct MemoryTelemetry {
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub compaction_recovery_count: u64,
    pub chunks_added: u64,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
    pub capture_prompt_tokens: u64,
    pub capture_completion_tokens: u64,
    pub capture_cost_usd_ticks: u64,
    pub dream_prompt_tokens: u64,
    pub dream_completion_tokens: u64,
    pub dream_cost_usd_ticks: u64,
    pub injected_bytes: u64,
}

#[cfg(test)]
#[path = "memory_state_tests.rs"]
mod tests;
