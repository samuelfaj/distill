//! Per-turn `memory.tar.gz` build and upload.
//!
//! The workspace memory dir is tarred+gzipped on the blocking pool (seconds
//! on large dirs), deduplicated per cwd across concurrent turn ends, and
//! uploaded under every turn's own prefix so each restorable turn has a blob.

use super::trace::{UploadFailure, record_upload_failure, upload_small_artifact};
use super::turn::{PromptTraceContext, UploadWait};

#[cfg(test)]
use super::trace::apply_test_archive_fault;

/// A failed memory-archive build still gets a manifest entry and the shared
/// failure-episode accounting, so the manifest cannot vacuously report
/// `fully_uploaded` with an ingest-expected artifact silently absent.
fn record_memory_archive_failure(ctx: &PromptTraceContext, err_msg: &str) {
    record_upload_failure(
        ctx,
        UploadFailure {
            artifact: "memory_archive",
            reason: "archive_failed",
            error: err_msg,
            ..Default::default()
        },
    );
    super::manifest::record_artifact(
        &ctx.artifact_tracker,
        "memory.tar.gz",
        super::manifest::ArtifactResult::Failed {
            reason: "archive_failed",
            error: Some(err_msg),
        },
    );
}

/// Upper bound on the detached post-deadline memory-archive upload: this path
/// bypasses the durable queue, so without it a stuck connection pins the
/// archive bytes and the task forever.
const DETACHED_MEMORY_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A finished memory-archive build, shared by every turn end that joined it.
type MemoryArchiveResult = Result<std::sync::Arc<Vec<u8>>, String>;

/// Receiver side of an in-flight build: `None` until the build publishes.
type MemoryArchiveBuild = tokio::sync::watch::Receiver<Option<MemoryArchiveResult>>;

/// In-flight memory-archive builds keyed by cwd. Same-cwd turn ends join the in-flight build instead of stacking duplicate tar+gzip work, and each joiner uploads the shared bytes under its own turn prefix — every restorable turn gets a `memory.tar.gz` blob. Keyed by cwd (known before the expensive workspace discovery); worktrees sharing a memory dir may build it concurrently, bounded by live sessions.
static MEMORY_ARCHIVE_BUILDS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, MemoryArchiveBuild>>,
> = std::sync::LazyLock::new(Default::default);

/// Join the in-flight build for `cwd` or start one. The builder task owns publishing and map cleanup, so it completes even if every waiter is dropped (e.g. all turns hit their flush deadline). `MemoryStorage::new` runs git2 workspace discovery and the build tars+gzips the whole memory dir — seconds on large workspaces — so both stay on the blocking pool.
/// `memory_root` overrides the default `~/.grok/memory` root for tests.
fn join_or_start_memory_archive_build(
    cwd: String,
    memory_root: Option<std::path::PathBuf>,
) -> MemoryArchiveBuild {
    let mut builds = MEMORY_ARCHIVE_BUILDS.lock();
    if let Some(rx) = builds.get(&cwd) {
        return rx.clone();
    }
    let (tx, rx) = tokio::sync::watch::channel(None);
    builds.insert(cwd.clone(), rx.clone());
    tokio::spawn(async move {
        let build_cwd = cwd.clone();
        let result = match tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            apply_test_archive_fault()?;
            let storage = crate::session::memory::MemoryStorage::new(
                std::path::Path::new(&build_cwd),
                memory_root.as_deref(),
            );
            crate::session::memory::archive::build_memory_archive(&storage)
        })
        .await
        {
            Ok(Ok(bytes)) => Ok(std::sync::Arc::new(bytes)),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(e) => Err(format!("archive build task join failed: {e}")),
        };
        MEMORY_ARCHIVE_BUILDS.lock().remove(&cwd);
        let _ = tx.send(Some(result));
    });
    rx
}

/// Wait for an in-flight build to publish its result.
async fn await_memory_archive(build: &mut MemoryArchiveBuild) -> MemoryArchiveResult {
    match build.wait_for(|v| v.is_some()).await {
        Ok(published) => match published.as_ref() {
            Some(result) => result.clone(),
            None => Err("archive build published nothing".to_string()),
        },
        // Sender dropped without publishing: the builder task panicked.
        Err(_) => Err("archive build task dropped before publishing".to_string()),
    }
}

/// Upload memory .md files as `memory.tar.gz` alongside the per-turn trace.
/// Only runs when session registry is enabled via remote settings or config.toml.
/// With [`UploadWait::Defer`] the build wait is bounded by the flush deadline (the `UploadWait` contract): past it the turn records a miss while the build finishes detached and uploads best-effort.
pub(crate) async fn upload_memory_state(ctx: &PromptTraceContext, wait: UploadWait) {
    if !ctx.session_registry_enabled {
        tracing::debug!("memory upload skipped: session_registry_enabled=false");
        super::manifest::skip_artifact(
            &ctx.artifact_tracker,
            "memory.tar.gz",
            "session_registry_disabled",
        );
        return;
    }
    if ctx.memory_mode == Some(crate::config::MemoryMode::V2) {
        // The current replay archive contract understands the legacy layout
        // only. Uploading a partial v2 snapshot (just its generated manifests)
        // would silently produce an unrestorable memory state.
        tracing::debug!("memory upload skipped: v2 archive format is not supported");
        super::manifest::skip_artifact(
            &ctx.artifact_tracker,
            "memory.tar.gz",
            "memory_v2_archive_unsupported",
        );
        return;
    }
    let mut build =
        join_or_start_memory_archive_build(ctx.session_info.cwd.clone(), /*memory_root*/ None);
    match wait {
        UploadWait::Confirm => {
            let result = await_memory_archive(&mut build).await;
            upload_built_memory_archive(ctx, result, wait).await;
        }
        UploadWait::Defer { deadline } => {
            match tokio::time::timeout_at(deadline, await_memory_archive(&mut build)).await {
                Ok(result) => upload_built_memory_archive(ctx, result, wait).await,
                Err(_) => {
                    // Nothing durable exists at the deadline; the detached
                    // finish may still record and upload later (last write
                    // wins in the tracker, the blob listing is ground truth).
                    tracing::debug!(
                        artifact = "memory_archive",
                        reason = "timed_out_before_enqueue",
                        session_id = %ctx.session_info.id.0,
                        turn_number = ctx.turn_number,
                        "memory archive build missed the flush deadline; finishing detached"
                    );
                    super::manifest::record_artifact(
                        &ctx.artifact_tracker,
                        "memory.tar.gz",
                        super::manifest::ArtifactResult::Failed {
                            reason: "timed_out_before_enqueue",
                            error: None,
                        },
                    );
                    let ctx = ctx.clone();
                    super::turn::spawn_linked_upload_task(
                        "memory_archive_detached",
                        // Telemetry-only span label; PromptTraceContext
                        // carries no prompt id, so the turn stands in.
                        format!("turn_{}", ctx.turn_number),
                        ctx.session_info.id.0.clone(),
                        async move {
                            let result = await_memory_archive(&mut build).await;
                            // Confirm awaits the real upload result; the
                            // timeout bounds the direct attempt (this path
                            // has no durable-queue retries).
                            if tokio::time::timeout(
                                DETACHED_MEMORY_UPLOAD_TIMEOUT,
                                upload_built_memory_archive(&ctx, result, UploadWait::Confirm),
                            )
                            .await
                            .is_err()
                            {
                                super::manifest::record_artifact(
                                    &ctx.artifact_tracker,
                                    "memory.tar.gz",
                                    super::manifest::ArtifactResult::Failed {
                                        reason: "direct_upload_timed_out",
                                        error: None,
                                    },
                                );
                            }
                        },
                    );
                }
            }
        }
    }
}

/// Record or upload a finished memory-archive build. The upload honors the same [`UploadWait`] contract as the sibling turn artifacts: durable queue accept (or a deadline-bounded direct attempt) on `Defer`, an awaited direct upload on `Confirm`.
async fn upload_built_memory_archive(
    ctx: &PromptTraceContext,
    result: MemoryArchiveResult,
    wait: UploadWait,
) {
    let archive = match result {
        Ok(a) => a,
        Err(e) => {
            record_memory_archive_failure(ctx, &e);
            return;
        }
    };
    if archive.len() < 30 {
        // An empty tar.gz is ~29 bytes. Nothing to upload, but the manifest
        // still needs a terminal record (and a Defer-timeout's provisional
        // failure must not stand when the late build produced nothing).
        super::manifest::skip_artifact(&ctx.artifact_tracker, "memory.tar.gz", "empty_archive");
        return;
    }
    let prefix = ctx.gcs_config.gcs_prefix.as_deref().unwrap_or("");
    let gcs_path = format!("{prefix}/memory.tar.gz");
    upload_small_artifact(
        ctx,
        &archive,
        &gcs_path,
        "application/gzip",
        "memory_archive",
        wait,
    )
    .await;
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
