// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-session memory for Grok.
//!
//! Two isolated pipelines. They do not share files, search, flush, or Dream.
//! See the crate `AGENTS.md` before changing either path.
//!
//! - **Legacy:** markdown under `~/.grok/memory/` (tree below).
//! - **v2:** `~/.grok/memory-v2/` topics, observation inbox, and generated
//!   `MEMORY.md`. See `v2.rs`. v2 never reads or writes the legacy tree.
//!
//! ## Legacy data layout
//!
//! ```text
//! ~/.grok/memory/
//!   ├── MEMORY.md                         # Global curated knowledge
//!   └── {workspace_hash}/                 # Per-workspace (blake3(cwd)[..16])
//!       ├── MEMORY.md                     # Project-level curated knowledge
//!       └── sessions/
//!           └── YYYY-MM-DD-{slug}-{sid8}.md  # Session logs
//! ```
//!
//! ## Feature Flag
//!
//! Resolve enablement through `MemoryConfig::resolve_settings`.
//! `GROK_MEMORY`, `[memory] enabled`, and `[memory_v2] enabled` all participate.
//! When disabled, this crate is not initialized by the host.

#![deny(clippy::indexing_slicing)]

pub mod archive;
pub mod backend;
pub mod chunker;
pub mod dream;
pub mod dream_lock;
pub mod embedding;
pub mod flush;
pub mod index;
pub mod mmr;
pub mod observation;
pub mod query_expansion;
pub mod schema;
pub mod search;
pub mod storage;
mod storage_v2;
pub mod text_utils;
pub mod v2;
mod v2_access;
pub mod v2_capture;
pub mod v2_carryover;
mod v2_clock;
pub mod v2_consolidation;
mod v2_maintenance;
pub mod watcher;

pub use backend::{EndpointScopedCredentials, MemoryBackendImpl, MemoryBackendParams};
pub use index::{MemoryIndex, init_sqlite_vec};
pub use observation::*;
pub use storage::{MemoryScope, MemoryStorage, SaveRememberNoteError};
pub use v2::{
    MAX_MANUAL_OBSERVATION_BYTES, V2Manifest, V2ManifestBudget, V2MemoryScope, V2StorageError,
    ensure_scope_initialized, ensure_scope_initialized_with_journal_mode,
    regenerate_scope_manifest, render_scope_manifest,
};
pub use v2_access::{V2AccessError, V2MemoryAccessPolicy, V2PathClass};
pub use v2_capture::{
    CaptureCursors, CaptureJob, CaptureLease, CaptureOutcomeDraft, CaptureRange, CaptureWorkState,
    ClaimRequest, CommitResult, MAX_ALIASES, MAX_BODY_BYTES, MAX_KEYWORDS, MAX_OBSERVATIONS,
    MAX_STATEMENT_BYTES, MAX_TERM_BYTES, MAX_TOPIC_BYTES, ObservationDraft, ObservationType,
    V2CaptureError, V2CaptureStore,
};
pub use v2_carryover::{
    V2CarryoverError, V2CarryoverOutcome, V2CarryoverReport, carry_over_legacy_memory,
    default_legacy_memory_root, legacy_memory_file,
};
pub use v2_clock::{SharedV2Clock, SystemV2Clock, V2Clock, system_v2_clock};
pub use v2_consolidation::{
    ClaimedObservation, ConsolidationInput, ConsolidationLease, ConsolidationResult,
    ConsolidationStatus, DreamClaimRequest, DreamEligibility, DreamEligibilityConfig,
    DreamTriggerDisposition, TopicOperation, V2ConsolidationError, V2ConsolidationStore,
};
pub use v2_maintenance::{
    DreamLeaseState, ForgetReason, ForgetRequest, ForgetResult, GcResult, MAX_FORGET_FILE_BYTES,
    RetentionPolicy, V2MaintenanceError, V2MaintenanceStore, V2ScopeStatus,
};

pub(crate) const MEMORY_LOG_TARGET: &str = "distill_memory";

/// Embed all chunks that don't have embeddings yet.
/// Call after reindex, flush writes, or session-end writes.
pub async fn embed_missing_chunks(
    index: &MemoryIndex,
    provider: &dyn embedding::EmbeddingProvider,
) -> usize {
    let chunks = match index.chunks_without_embeddings() {
        Ok(c) if c.is_empty() => return 0,
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: MEMORY_LOG_TARGET,
                error = %e,
                "failed to query chunks without embeddings"
            );
            return 0;
        }
    };

    let total = chunks.len();
    let mut embedded = 0;

    // 32 is the provider's typical max batch size
    for batch in chunks.chunks(32) {
        let texts: Vec<&str> = batch.iter().map(|(_, text)| text.as_str()).collect();
        match provider.embed_batch(&texts).await {
            Ok(embeddings) => {
                for ((chunk_id, _), embedding) in batch.iter().zip(embeddings.iter()) {
                    if let Err(e) = index.upsert_embedding(chunk_id, embedding) {
                        tracing::warn!(
                            target: MEMORY_LOG_TARGET,
                            chunk_id,
                            error = %e,
                            "failed to upsert embedding"
                        );
                    } else {
                        embedded += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: MEMORY_LOG_TARGET,
                    error = %e,
                    batch_size = texts.len(),
                    "embedding batch failed, skipping"
                );
            }
        }
    }

    if embedded > 0 {
        tracing::info!(
            target: MEMORY_LOG_TARGET,
            embedded,
            total,
            "embedded missing chunks"
        );
    }
    embedded
}
