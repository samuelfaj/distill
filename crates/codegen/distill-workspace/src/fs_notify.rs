// Modified for Distill by Samuel Fajreldines, 2026.
//! Bridges [`distill_fsnotify`] into the workspace: the [`WorkspaceEvent::FsChanged`] producer for an
//! exposed root, and the codebase-graph refresh that follows a git HEAD change.
//!
//! The producer shares the OS watcher per canonical root ([`distill_fsnotify::shared`]) and never
//! calls `shutdown()` on it; the watcher lives exactly as long as the producer task holds its `Arc`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::task::AbortOnDropHandle;
use distill_codebase_graph::{FileEvent, FileEventKind, IndexManagerHandle};
use distill_fsnotify::{FsConfig, FsEvent, FsEventKind};
use distill_workspace_types::WorkspaceEvent;

#[cfg(test)]
#[path = "fs_notify_tests.rs"]
mod tests;

/// Most paths one `FsChanged` carries. A watcher batch is unbounded (a tarball extracted into the
/// root); the hub closes a socket on any inbound frame over 8 MiB, and at ~100 bytes a path this
/// keeps a frame two orders of magnitude under that. A `Renamed` batch is at most a `[from, to]`
/// pair and is never split.
const FS_CHANGED_PATHS_PER_FRAME: usize = 1024;

/// Identity mapping onto the wire type; unknown future variants of the `#[non_exhaustive]` source
/// enum fall back to `Modified`.
fn to_workspace_event_kind(kind: FsEventKind) -> distill_workspace_types::FsEventKind {
    match kind {
        FsEventKind::Created => distill_workspace_types::FsEventKind::Created,
        FsEventKind::Modified => distill_workspace_types::FsEventKind::Modified,
        FsEventKind::Removed => distill_workspace_types::FsEventKind::Removed,
        FsEventKind::Renamed => distill_workspace_types::FsEventKind::Renamed,
        _ => distill_workspace_types::FsEventKind::Modified,
    }
}

/// Watch `root` and broadcast each settle window's changed paths as one [`WorkspaceEvent::FsChanged`].
///
/// Watcher init failure (root gone, inotify exhausted) is logged once at warn and the task exits;
/// the workspace keeps serving without change events. The task holds the only strong reference to
/// the shared OS watcher and cannot end on its own, so its handle aborts it on drop: whoever owns
/// the handle owns the watch, and letting go of the handle — `shutdown` or not — releases it.
pub(crate) fn spawn_fs_change_producer(
    root: PathBuf,
    events_tx: broadcast::Sender<WorkspaceEvent>,
) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(async move {
        // OS-watcher init walks the tree and blocks until every watch is armed. The watcher
        // reports paths under the canonical root (`shared` canonicalizes what it watches), so
        // that, not `root` as given, is what the paths are made relative to.
        let init_root = root.clone();
        let init = tokio::task::spawn_blocking(move || {
            let source = distill_fsnotify::shared(init_root.clone(), FsConfig::default())?;
            Ok::<_, distill_fsnotify::FsNotifyError>((
                source,
                dunce::canonicalize(&init_root).unwrap_or(init_root),
            ))
        })
        .await
        .map_err(|join| join.to_string())
        .and_then(|init| init.map_err(|e| e.to_string()));
        let (source, watch_root) = match init {
            Ok(init) => init,
            Err(error) => {
                tracing::warn!(
                    root = %root.display(),
                    error,
                    "fs change producer: watcher init failed; no FsChanged events for this root"
                );
                return;
            }
        };
        // `source` outlives the loop: dropping the last `Arc` tears down the shared OS watcher.
        forward_fs_changes(source.subscribe(), &events_tx, &watch_root).await;
    }))
}

/// Re-broadcast each `FilesChanged` batch as one `FsChanged` until the source closes. The
/// watcher's settle window is the coalescing unit: a checkout is one frame, not one per file,
/// split only past [`FS_CHANGED_PATHS_PER_FRAME`]. Paths are confined to `root` first
/// ([`confine_to_root`]); a batch with nothing left inside it sends no frame.
async fn forward_fs_changes(
    mut rx: broadcast::Receiver<FsEvent>,
    events_tx: &broadcast::Sender<WorkspaceEvent>,
    root: &Path,
) {
    loop {
        match rx.recv().await {
            Ok(FsEvent::FilesChanged { paths, kind }) => {
                let (mut paths, kind) = confine_to_root(paths, to_workspace_event_kind(kind), root);
                while !paths.is_empty() {
                    let rest = paths.split_off(paths.len().min(FS_CHANGED_PATHS_PER_FRAME));
                    let _ = events_tx.send(WorkspaceEvent::FsChanged { paths, kind });
                    paths = rest;
                }
            }
            Ok(other) => {
                tracing::trace!(?other, "fs change producer: event variant not bridged");
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    lagged = n,
                    "fs change producer lagged; some events were dropped"
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Make `paths` relative to `root` and drop the ones outside it. A rename across the root boundary
/// is reported as the half inside: `Removed` for a file that left the root, `Created` for one that
/// arrived, since a lone-path `Renamed` would not say which. A rename the watcher saw by one path
/// stays a `Renamed` when that path is inside.
fn confine_to_root(
    paths: Vec<PathBuf>,
    kind: distill_workspace_types::FsEventKind,
    root: &Path,
) -> (Vec<PathBuf>, distill_workspace_types::FsEventKind) {
    let relative = |path: &PathBuf| {
        path.strip_prefix(root)
            .ok()
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    let confined = match (kind, paths.as_slice()) {
        (distill_workspace_types::FsEventKind::Renamed, [from, to]) => {
            match (relative(from), relative(to)) {
                (Some(from), Some(to)) => (vec![from, to], kind),
                (Some(from), None) => (vec![from], distill_workspace_types::FsEventKind::Removed),
                (None, Some(to)) => (vec![to], distill_workspace_types::FsEventKind::Created),
                (None, None) => (Vec::new(), kind),
            }
        }
        _ => (paths.iter().filter_map(relative).collect(), kind),
    };
    let dropped = paths.len() - confined.0.len();
    if dropped > 0 {
        tracing::debug!(
            dropped,
            root = %root.display(),
            "fs change producer: dropped paths outside the watch root"
        );
    }
    confined
}

const GIT_DIFF_REBUILD_THRESHOLD: usize = 500;

fn parse_diff_name_status_line(
    line: &str,
    repo_root: &Path,
) -> Option<distill_codebase_graph::FileEvent> {
    let mut parts = line.splitn(3, '\t');
    let status = parts.next()?.trim();
    let path = parts.next()?;

    match status.chars().next()? {
        'A' => Some(distill_codebase_graph::FileEvent::created(repo_root.join(path))),
        'D' => Some(distill_codebase_graph::FileEvent::removed(repo_root.join(path))),
        'R' | 'C' => {
            let new_path = parts.next()?;
            Some(distill_codebase_graph::FileEvent::renamed(
                repo_root.join(path),
                repo_root.join(new_path),
            ))
        }
        _ => Some(distill_codebase_graph::FileEvent::modified(
            repo_root.join(path),
        )),
    }
}

/// After a HEAD change, diff `ORIG_HEAD..HEAD` and send targeted graph events, or rebuild if too many files changed.
/// Emits `CodebaseIndexUpdated` after the update; skips it if the index actor channel is closed.
pub(crate) async fn refresh_codebase_graph_after_head_change(
    idx: &distill_codebase_graph::IndexManagerHandle,
    repo_root: &Path,
    events_tx: &broadcast::Sender<WorkspaceEvent>,
) {
    let mut diff_cmd = tokio::process::Command::new("git");
    diff_cmd
        .args(["diff", "--name-status", "ORIG_HEAD", "HEAD"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null());
    distill_tools::util::detach_command(&mut diff_cmd);
    diff_cmd.envs(distill_tools::util::pager_env());
    let diff_output = diff_cmd.output().await;

    // `None` means the update failed entirely (channel closed); skip the event so subscribers are not misled
    let files_updated: Option<u64>;

    match diff_output {
        Ok(output) if output.status.success() => {
            let changed: Vec<_> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| parse_diff_name_status_line(l, repo_root))
                .collect();

            let count = changed.len();
            if count > GIT_DIFF_REBUILD_THRESHOLD {
                tracing::debug!(
                    "git_refresh: {count} changed files exceeds threshold, falling back to rebuild"
                );
                files_updated = match idx.rebuild() {
                    Ok(()) => Some(count as u64),
                    Err(e) => {
                        tracing::debug!("git_refresh: rebuild failed: {:?}", e);
                        None
                    }
                };
            } else if let Err(e) = idx.send_events(changed) {
                tracing::debug!("git_refresh: failed to send graph events: {:?}", e);
                files_updated = None;
            } else {
                tracing::debug!("git_refresh: sent {count} changed files to codebase graph");
                files_updated = Some(count as u64);
            }
        }
        _ => {
            tracing::debug!("git_refresh: git diff failed, falling back to rebuild");
            files_updated = match idx.rebuild() {
                Ok(()) => Some(0),
                Err(e) => {
                    tracing::debug!("git_refresh: rebuild fallback also failed: {:?}", e);
                    None
                }
            };
        }
    }

    if let Some(count) = files_updated {
        let _ = events_tx.send(WorkspaceEvent::CodebaseIndexUpdated {
            files_indexed: count,
        });
    }
}

/// One [`FileEvent`] per covering index for an `FsChanged` batch, each carrying every path of the
/// batch that index covers; paths no index covers are dropped.
///
/// A `Renamed` batch is `[from, to]` when the watcher saw both halves, or one path when it saw one
/// (notify's `From`/`To`/`Any` rename modes). A pair whose halves share an index goes to it whole,
/// and the graph splits it into Removed(from) + Created(to); halves under different indexes go to
/// each as its own Removed / Created, since an index never learns of a file outside its root from
/// a pair sent elsewhere. A lone path is a `Renamed` its index re-indexes.
pub(crate) fn codebase_graph_events_for_batch(
    paths: Vec<PathBuf>,
    kind: distill_workspace_types::FsEventKind,
    mut covering: impl FnMut(&Path) -> Option<Arc<IndexManagerHandle>>,
) -> Vec<(Arc<IndexManagerHandle>, FileEvent)> {
    let graph_kind = match kind {
        distill_workspace_types::FsEventKind::Created => FileEventKind::Created,
        distill_workspace_types::FsEventKind::Modified => FileEventKind::Modified,
        distill_workspace_types::FsEventKind::Removed => FileEventKind::Removed,
        distill_workspace_types::FsEventKind::Renamed => FileEventKind::Renamed,
    };
    if graph_kind == FileEventKind::Renamed {
        let mut paths = paths.into_iter();
        let (Some(from), to) = (paths.next(), paths.next()) else {
            return Vec::new();
        };
        let (from_idx, to_idx) = (covering(&from), to.as_deref().and_then(&mut covering));
        return match (from_idx, to, to_idx) {
            (Some(idx), Some(to), Some(to_idx)) if Arc::ptr_eq(&idx, &to_idx) => {
                vec![(idx, FileEvent::new(vec![from, to], graph_kind))]
            }
            (from_idx, Some(to), to_idx) => from_idx
                .map(|idx| (idx, FileEvent::new(vec![from], FileEventKind::Removed)))
                .into_iter()
                .chain(to_idx.map(|idx| (idx, FileEvent::new(vec![to], FileEventKind::Created))))
                .collect(),
            (from_idx, None, _) => from_idx
                .map(|idx| vec![(idx, FileEvent::new(vec![from], graph_kind))])
                .unwrap_or_default(),
        };
    }
    let mut groups: Vec<(Arc<IndexManagerHandle>, Vec<PathBuf>)> = Vec::new();
    for path in paths {
        let Some(idx) = covering(&path) else { continue };
        match groups
            .iter_mut()
            .find(|(group_idx, _)| Arc::ptr_eq(group_idx, &idx))
        {
            Some((_, group)) => group.push(path),
            None => groups.push((idx, vec![path])),
        }
    }
    groups
        .into_iter()
        .map(|(idx, paths)| (idx, FileEvent::new(paths, graph_kind)))
        .collect()
}
