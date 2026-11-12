// Modified for Distill by Samuel Fajreldines, 2026.
//! The patch one prompt turn made, for the UltraCode turn review.
//!
//! Before the turn's first tool call the harness writes the tracked files as a
//! tree through a temporary index and lists the untracked paths. At turn end
//! the same procedure, plus the untracked paths that are new since, gives the
//! end tree, and the diff between the two trees is the turn's own work even on
//! a tree that was already dirty.
//!
//! The user's index, refs and working tree are never written: every `git add`
//! goes to a temporary index file. It does add blob objects to the object
//! store, which nothing references and `git gc` prunes.

use super::evidence::{CapturedChanges, extract_changed_files, truncate_diff};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Budget for the end-of-turn capture; the turn is over, but a wedged git
/// must not hold the review forever.
const TURN_PATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// First line of a patch taken against HEAD because the turn snapshot was
/// missing.
pub(crate) const EARLIER_CHANGES_NOTE: &str = "# Note: the harness could not snapshot the tree before this turn, so this diff is against HEAD and may include earlier changes that this turn did not make.\n";

/// The tree before the turn's first tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnSnapshot {
    /// The session directory is not inside a git work tree.
    NoRepository,
    Tree {
        tree: String,
        /// Untracked, non-ignored paths, relative to the repository root.
        untracked: BTreeSet<String>,
    },
    /// The tree could not be written within the budget; the turn patch falls
    /// back to a diff against HEAD.
    Unavailable { untracked: Option<BTreeSet<String>> },
}

/// What the turn changed, as the reviewer reads it.
#[derive(Debug, Clone)]
pub(crate) struct TurnPatch {
    pub changes: CapturedChanges,
    /// The diff is against HEAD, so it may hold changes made before the turn.
    pub may_include_earlier: bool,
}

enum GitError {
    /// git ran and refused (for example: not a repository).
    Exit,
    /// git could not be run.
    Spawn,
}

fn git(cwd: &Path, index: Option<&Path>) -> Command {
    let mut command = Command::new(crate::util::subprocess::git_bin());
    command
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
        ]);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

async fn git_stdout(cwd: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = git(cwd, index)
        .args(args)
        .output()
        .await
        .map_err(|_| GitError::Spawn)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Exit)
    }
}

async fn git_line(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = git_stdout(cwd, None, args).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

async fn toplevel(root: &Path) -> Result<PathBuf, GitError> {
    git_line(root, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
}

async fn untracked_paths(top: &Path) -> Option<BTreeSet<String>> {
    let out = git_stdout(top, None, &["ls-files", "--others", "--exclude-standard", "-z"])
        .await
        .ok()?;
    Some(
        out.split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
    )
}

/// Writes the working tree's tracked files, plus `extra` untracked paths, as a
/// tree through a temporary copy of the user's index.
async fn write_tree(top: &Path, extra: &[&String]) -> Option<String> {
    let dir = tempfile::tempdir().ok()?;
    let index = dir.path().join("index");
    let real_index = git_line(top, &["rev-parse", "--git-path", "index"]).await.ok()?;
    // A copy keeps paths the user staged but never committed tracked.
    match tokio::fs::copy(top.join(real_index), &index).await {
        Ok(_) => {}
        // No index yet: a repository without staged files.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    git_stdout(top, Some(&index), &["add", "-u"]).await.ok()?;
    // A path removed since it was listed would fail the whole `add`.
    let mut present = Vec::with_capacity(extra.len());
    for path in extra {
        if tokio::fs::symlink_metadata(top.join(path.as_str())).await.is_ok() {
            present.push(path.as_str());
        }
    }
    if !present.is_empty() {
        let list = dir.path().join("pathspecs");
        let mut body = Vec::new();
        for path in present {
            body.extend_from_slice(path.as_bytes());
            body.push(0);
        }
        tokio::fs::write(&list, body).await.ok()?;
        let from_file = format!("--pathspec-from-file={}", list.display());
        git_stdout(
            top,
            Some(&index),
            &["--literal-pathspecs", "add", &from_file, "--pathspec-file-nul"],
        )
        .await
        .ok()?;
    }
    let tree = String::from_utf8_lossy(&git_stdout(top, Some(&index), &["write-tree"]).await.ok()?)
        .trim()
        .to_owned();
    (!tree.is_empty()).then_some(tree)
}

/// Snapshot of the tree before a turn changes it, within `budget`.
pub(crate) async fn capture_turn_snapshot(root: &Path, budget: Duration) -> TurnSnapshot {
    let capture = async {
        let top = match toplevel(root).await {
            Ok(top) => top,
            Err(GitError::Exit) => return TurnSnapshot::NoRepository,
            Err(GitError::Spawn) => return TurnSnapshot::Unavailable { untracked: None },
        };
        let untracked = untracked_paths(&top).await;
        match (write_tree(&top, &[]).await, untracked) {
            (Some(tree), Some(untracked)) => TurnSnapshot::Tree { tree, untracked },
            (_, untracked) => TurnSnapshot::Unavailable { untracked },
        }
    };
    match tokio::time::timeout(budget, capture).await {
        Ok(snapshot) => snapshot,
        Err(_) => {
            tracing::debug!("turn snapshot exceeded its budget");
            TurnSnapshot::Unavailable { untracked: None }
        }
    }
}

/// The diff from `snapshot` to the working tree now, including untracked
/// files created since. Without a snapshot tree the diff is against HEAD and
/// says it may include earlier changes.
pub(crate) async fn capture_turn_patch(
    root: &Path,
    snapshot: &TurnSnapshot,
) -> Result<TurnPatch, String> {
    let capture = async {
        let (base, before, may_include_earlier) = match snapshot {
            TurnSnapshot::NoRepository => return Err("not a git repository".to_owned()),
            TurnSnapshot::Tree { tree, untracked } => (Some(tree.clone()), Some(untracked), false),
            TurnSnapshot::Unavailable { untracked } => (None, untracked.as_ref(), true),
        };
        let top = toplevel(root)
            .await
            .map_err(|_| "not a git repository".to_owned())?;
        let now = untracked_paths(&top)
            .await
            .ok_or_else(|| "could not list untracked files".to_owned())?;
        let new: Vec<&String> = now
            .iter()
            .filter(|path| before.is_none_or(|before| !before.contains(*path)))
            .collect();
        let end = write_tree(&top, &new)
            .await
            .ok_or_else(|| "could not write the end-of-turn tree".to_owned())?;
        let base = match base {
            Some(tree) => tree,
            None => match git_line(&top, &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]).await {
                Ok(tree) => tree,
                // No commit yet: everything is new.
                Err(_) => git_line(&top, &["hash-object", "-t", "tree", "--stdin"])
                    .await
                    .map_err(|_| "could not resolve a base tree".to_owned())?,
            },
        };
        let diff = git_stdout(
            &top,
            None,
            &["diff", "--no-color", "--no-ext-diff", "--no-textconv", &base, &end],
        )
        .await
        .map_err(|_| "git diff failed".to_owned())?;
        let diff = String::from_utf8_lossy(&diff).into_owned();
        let changed_files = extract_changed_files(&diff);
        let mut body = if may_include_earlier && !diff.is_empty() {
            EARLIER_CHANGES_NOTE.to_owned()
        } else {
            String::new()
        };
        body.push_str(&truncate_diff(diff));
        Ok(TurnPatch {
            changes: CapturedChanges {
                diff: body,
                changed_files,
            },
            may_include_earlier,
        })
    };
    tokio::time::timeout(TURN_PATCH_TIMEOUT, capture)
        .await
        .unwrap_or_else(|_| Err("capturing the turn patch timed out".to_owned()))
}

#[cfg(test)]
#[path = "turn_snapshot_tests.rs"]
mod tests;
