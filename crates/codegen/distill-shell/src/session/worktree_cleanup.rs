// Modified for Distill by Samuel Fajreldines, 2026.
//! Must not stat or walk a Grove dest: `Path::exists` / `remove_dir_all` can hang or delete retained backing when dest is mounted or inconclusive.

use std::path::{Path, PathBuf};

use distill_workspace::session::git::find_git_root_from_path;
use distill_workspace::worktree::remove_jj_workspace;

const WORKTREE_LOG: &str = "xai_worktree";

/// Best-effort teardown of a dest created by a resume that later failed.
#[tracing::instrument(skip_all)]
pub(crate) async fn cleanup_worktree_on_failure(source_cwd: &str, worktree_path: &str) {
    let wt = Path::new(worktree_path);
    let is_jj = find_git_root_from_path(Path::new(source_cwd))
        .ok()
        .is_some_and(|root| distill_workspace::session::git::detect_vcs_kind(&root).is_jj());
    if is_jj {
        if let Err(e) = remove_jj_workspace(worktree_path).await {
            tracing::warn!(
                target: WORKTREE_LOG,
                error = %e,
                "failed to clean up jj workspace after failure"
            );
        }
        return;
    }

    let wt_path = wt.to_path_buf();
    let span = tracing::Span::current();
    match tokio::task::spawn_blocking(move || {
        span.in_scope(|| distill_fast_worktree::remove_worktree(&wt_path))
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(
                target: WORKTREE_LOG,
                error = %e,
                path = %wt.display(),
                "remove_worktree failed during resume cleanup; leaving dest for daemon recovery"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: WORKTREE_LOG,
                error = %e,
                path = %wt.display(),
                "remove_worktree task panicked during resume cleanup; leaving dest"
            );
        }
    }

    // `remove_stale_worktree_registration` stats the dest
    // Only run it when the mount table says dest is unmounted (safe to stat)
    if !distill_fast_worktree::dest_is_known_unmounted(wt) {
        return;
    }
    let Ok(root) = find_git_root_from_path(Path::new(source_cwd)) else {
        return;
    };
    let wt_path = PathBuf::from(wt);
    let _ = tokio::task::spawn_blocking(move || {
        distill_fast_worktree::remove_stale_worktree_registration(&root, &wt_path)
    })
    .await;
}
