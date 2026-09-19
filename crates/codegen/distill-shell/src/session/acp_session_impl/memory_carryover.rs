// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-start carry-over of legacy curated `MEMORY.md` files into the v2 scopes.
//!
//! Runs before the first turn so the injected index already lists the carried
//! topics. A failure is logged and never disables memory.

use std::path::Path;
use std::sync::Arc;

use distill_memory::{
    V2CarryoverOutcome, V2MemoryAccessPolicy, V2MemoryScope, carry_over_legacy_memory,
    legacy_memory_file,
};
use distill_telemetry::memory_telemetry::{
    MemoryV2CarryoverCompleted, MemoryV2CarryoverOutcome, MemoryV2Scope,
};

use crate::session::memory::MemoryStorage;

pub(crate) async fn carry_over_legacy_memory_into_v2(
    storage: &MemoryStorage,
    legacy_root: &Path,
    access: Arc<V2MemoryAccessPolicy>,
) {
    let Some(workspace_dir_name) = storage
        .workspace_dir()
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
    else {
        return;
    };
    let mut scopes = vec![(storage.global_dir().to_path_buf(), V2MemoryScope::Global)];
    if !storage.is_ephemeral() {
        scopes.push((
            storage.workspace_dir().to_path_buf(),
            V2MemoryScope::Workspace,
        ));
    }
    for (scope_dir, scope) in scopes {
        let source = legacy_memory_file(legacy_root, scope, &workspace_dir_name);
        let access = access.clone();
        let result = tokio::task::spawn_blocking(move || {
            carry_over_legacy_memory(&scope_dir, &source, &access, &distill_memory::SystemV2Clock)
        })
        .await
        .map_err(|join_error| join_error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()));
        let telemetry_scope = match scope {
            V2MemoryScope::Global => MemoryV2Scope::Global,
            V2MemoryScope::Workspace => MemoryV2Scope::Workspace,
        };
        match result {
            Ok(V2CarryoverOutcome::Imported(report)) => {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    scope = ?scope,
                    topics_created = report.topics_created,
                    topics_appended = report.topics_appended,
                    sections_skipped = report.sections_skipped,
                    "MEMORY_CARRYOVER: carried legacy notes into memory-v2"
                );
                distill_telemetry::session_ctx::log_event(MemoryV2CarryoverCompleted {
                    scope: telemetry_scope,
                    outcome: MemoryV2CarryoverOutcome::Imported,
                    topics_created: report.topics_created,
                    topics_appended: report.topics_appended,
                    sections_skipped: report.sections_skipped,
                    bytes_written: report.bytes_written,
                });
            }
            Ok(
                V2CarryoverOutcome::MissingSource
                | V2CarryoverOutcome::NothingToCarry
                | V2CarryoverOutcome::Unchanged,
            ) => {}
            Err(error) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    scope = ?scope,
                    error = %error,
                    "MEMORY_CARRYOVER: legacy carry-over failed; memory stays enabled"
                );
                distill_telemetry::session_ctx::log_event(MemoryV2CarryoverCompleted {
                    scope: telemetry_scope,
                    outcome: MemoryV2CarryoverOutcome::Failed,
                    ..MemoryV2CarryoverCompleted::default()
                });
            }
        }
    }
}
