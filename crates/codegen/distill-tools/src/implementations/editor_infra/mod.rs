// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared editor infrastructure used by the default distill tools.
pub mod file_operation_lock;
pub use file_operation_lock::{FileOperationLockGuard, FileOperationLockManager};

use std::path::{Path, PathBuf};

use crate::types::resources::SharedResources;

/// Serialize concurrent mutations of `paths` within this toolset (write, search_replace, apply_patch).
/// Paths are canonicalized so two spellings of one file share a lock, then acquired in sorted order so
/// two multi-file callers cannot deadlock. Hold the returned guards across the read-check-write.
/// The manager lives in this toolset's resources, so a subagent (own toolset) does not share it.
pub async fn lock_paths(
    resources: &SharedResources,
    paths: &[&Path],
) -> Vec<FileOperationLockGuard> {
    let manager = resources
        .lock()
        .await
        .get_or_default::<FileOperationLockManager>()
        .clone();
    let mut keys: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths {
        keys.push(crate::types::file_read_state::key_for(path).await);
    }
    keys.sort();
    keys.dedup();
    let mut guards = Vec::with_capacity(keys.len());
    for key in &keys {
        guards.push(manager.wait_for_lock(&key.to_string_lossy()).await);
    }
    guards
}
/// Default `block_until_ms` when the model omits the field on blocking shell
/// tools. Shared so ACP UI labels and tool implementations stay aligned.
pub const DEFAULT_BLOCK_UNTIL_MS: u64 = 30_000;
