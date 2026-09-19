// Modified for Distill by Samuel Fajreldines, 2026.
//! Memory system shim.
//!
//! The memory "core engine" now lives in the standalone `distill-memory` crate.
//! This module re-exports that crate's public API under the historical `crate::session::memory::*` paths.
//!
//! Only `hooks` stays here: it is session glue (depends on `crate::sampling` and `crate::session::helpers::session_compact`).

pub(crate) mod capture_transcript;
pub mod hooks;
pub(crate) mod v2_capture;

pub use distill_memory::{
    EndpointScopedCredentials, MemoryBackendImpl, MemoryBackendParams, MemoryIndex, MemoryScope,
    MemorySearchSource, MemoryStorage, V2ManifestBudget, V2MemoryAccessPolicy, V2MemoryScope,
    archive, backend, chunker, dream, dream_lock, embed_missing_chunks, embedding, index,
    init_sqlite_vec, mmr, noop_memory_observation_sink, query_expansion, regenerate_scope_manifest,
    schema, search, storage, text_utils, v2, watcher,
};
