// Modified for Distill by Samuel Fajreldines, 2026.
//! Repository/worktree discovery helpers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Git dir from a worktree's `.git` file (a pointer, not a directory). Regular
/// repos return the `.git` directory itself.
pub(crate) fn find_worktree_git_dir(worktree_path: &Path) -> Result<PathBuf> {
    let git_path = worktree_path.join(".git");

    if git_path.is_file() {
        // Worktree: .git is a file containing "gitdir: <path>"
        let content = std::fs::read_to_string(&git_path)
            .with_context(|| format!("failed to read .git file at {}", git_path.display()))?;

        let raw = content
            .strip_prefix("gitdir: ")
            .ok_or_else(|| anyhow::anyhow!("invalid .git file format: {}", content.trim()))?
            .trim();

        // git may write a relative pointer. Resolve against the worktree dir —
        // otherwise index lookups join it against the CWD and break (mirrors
        // `read_worktree_gitdir` in api.rs).
        let raw_path = Path::new(raw);
        let resolved = if raw_path.is_relative() {
            worktree_path.join(raw_path)
        } else {
            raw_path.to_path_buf()
        };
        Ok(dunce::canonicalize(&resolved).unwrap_or(resolved))
    } else if git_path.is_dir() {
        // Regular repository
        Ok(git_path)
    } else {
        anyhow::bail!(
            "no .git file or directory found at {}",
            worktree_path.display()
        )
    }
}

/// Working-directory root for a path, for both regular repos and worktrees.
/// A subdirectory resolves to its repo or worktree root, not itself.
pub(crate) fn find_worktree_root(path: &Path) -> Result<PathBuf> {
    let repo = gix::discover(path)
        .with_context(|| format!("failed to discover git repo at {}", path.display()))?;

    // workdir() returns the working directory root for both repos and worktrees
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("bare repository has no working directory"))?;

    Ok(work_dir.to_path_buf())
}

/// Get the HEAD commit hash using gix.
pub(crate) fn get_head_commit(path: &Path) -> Result<String> {
    let repo = gix::discover(path)
        .with_context(|| format!("failed to discover git repo at {}", path.display()))?;

    let head = repo
        .head()
        .context("failed to get HEAD")?
        .peel_to_commit()
        .context("failed to peel HEAD to commit")?;

    Ok(head.id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use distill_test_utils::git::{git_commit_all, init_git_repo};

    #[test]
    fn test_find_worktree_git_dir_resolves_relative_gitdir() {
        // A worktree `.git` file can hold a RELATIVE `gitdir:` pointer; it must
        // resolve against the worktree dir, not be returned as-is (which would
        // break index copy for relative-gitdir worktrees).
        let temp = TempDir::new().unwrap();
        let worktree = temp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let real_git = temp.path().join("repo/.git/worktrees/wt");
        std::fs::create_dir_all(&real_git).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();

        let resolved = find_worktree_git_dir(&worktree).unwrap();
        assert_eq!(resolved, dunce::canonicalize(&real_git).unwrap());
    }

    #[test]
    fn test_find_worktree_git_dir_regular_repo() {
        distill_test_utils::require_git!();
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path());

        let git_dir = find_worktree_git_dir(temp.path()).unwrap();
        assert!(git_dir.ends_with(".git"));
    }

    #[test]
    fn test_get_head_commit() {
        distill_test_utils::require_git!();
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path());

        // Create a commit
        std::fs::write(temp.path().join("file.txt"), "content").unwrap();
        git_commit_all(temp.path(), "initial");

        let commit = get_head_commit(temp.path()).unwrap();
        assert_eq!(commit.len(), 40); // SHA-1 hex string
        assert!(commit.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
