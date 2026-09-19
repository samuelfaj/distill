// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared git-repo dir-chain primitive.
//!
//! One `git2` discovery and one walk from cwd up to the root.
//! The folder-trust gate reuses it across the many repo-local config marker checks it runs back-to-back.
//! Lives in its own module (rather than `discovery`) because it is a generic repo-walk primitive, not agent-definition discovery.
//! `distill-workspace` consumes it cross-crate.

use std::path::{Path, PathBuf};

/// Git worktree root for `cwd` plus the cwd-to-root chain, from one `git2` discovery and one walk.
/// Shared so the folder-trust gate and loaders cannot drift, and so startup does not repeat the walk.
/// Outside a git repo `git_root` is `None` and `dirs` is just `[cwd]`.
#[derive(Debug, Clone)]
pub struct RepoDirChain {
    /// Git worktree root (`workdir`), or `None` when `cwd` is not inside a repo.
    pub git_root: Option<PathBuf>,
    /// `cwd` up to and including `git_root`, cwd-first (`[cwd]` with no repo).
    pub dirs: Vec<PathBuf>,
}

impl RepoDirChain {
    /// Resolve the chain for `cwd`: ONE `git2` discovery and ONE upward walk.
    pub fn resolve(cwd: &Path) -> Self {
        let git_root = git2::Repository::discover(cwd)
            .ok()
            .and_then(|repo| repo.workdir().map(|p| p.to_path_buf()))
            // Home-is-a-git-repo: a walk up to $HOME must not treat the whole home subtree as one repo.
            // Otherwise home-level `.grok`/plugins would look repo-local. Drop it so cwd is probed as no-repo.
            // Home is compared canonically to match the symlink handling below.
            .filter(|root| !is_home_dir(root));

        let mut dirs = Vec::new();
        if let Some(ref root) = git_root {
            // Canonicalize only for the stop test so a symlinked cwd still halts at the worktree root.
            // Pushed dirs keep their original spelling. Do not reduce this to `starts_with`: a mid-chain absolute symlink would walk past the root.
            let root_canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.clone());
            let mut current = Some(cwd.to_path_buf());
            while let Some(dir) = current {
                let dir_canonical = dunce::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                let parent = dir.parent().map(|p| p.to_path_buf());
                dirs.push(dir);
                if dir_canonical == root_canonical {
                    break;
                }
                current = parent;
            }
        } else {
            dirs.push(cwd.to_path_buf());
        }

        Self { git_root, dirs }
    }
}

#[derive(Debug, Clone)]
pub struct StartupProjectSources {
    pub chain: RepoDirChain,
    workspace_user_dir: Option<PathBuf>,
}

impl StartupProjectSources {
    pub fn resolve(cwd: &Path) -> Self {
        Self::with_workspace_user(
            cwd,
            crate::prompt::workspace_user::optional_workspace_user_dir(),
        )
    }

    pub fn with_workspace_user(cwd: &Path, workspace_user_dir: Option<PathBuf>) -> Self {
        let chain = RepoDirChain::resolve(cwd);
        let workspace_user_dir = workspace_user_dir.filter(|user_dir| {
            let canonical = canonical_or_raw(user_dir);
            chain
                .dirs
                .iter()
                .all(|dir| canonical_or_raw(dir) != canonical)
        });
        Self {
            chain,
            workspace_user_dir,
        }
    }

    pub fn skill_dirs(&self) -> impl Iterator<Item = &Path> {
        self.chain
            .dirs
            .iter()
            .map(PathBuf::as_path)
            .chain(self.workspace_user_dir.as_deref())
    }

    pub fn instruction_dirs(&self) -> Vec<&Path> {
        let mut dirs: Vec<&Path> = self.chain.dirs.iter().rev().map(PathBuf::as_path).collect();
        if self.chain.git_root.is_some()
            && let Some(user_dir) = self.workspace_user_dir.as_deref()
        {
            dirs.insert(1.min(dirs.len()), user_dir);
        }
        dirs
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` canonicalizes to the user's home directory.
/// It stays local (not reused from `distill-workspace`, which depends on THIS crate) to keep the dep edge one-way.
/// It backs the guard in [`RepoDirChain::resolve`] that drops a $HOME git root.
fn is_home_dir(path: &Path) -> bool {
    let Some(home) = distill_dirs::home_dir() else {
        return false;
    };
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(path) == canon(&home)
}

/// Existing `<dir>/<subdir>` directories under each dir of a precomputed cwd-to-git-root chain ([`RepoDirChain::dirs`]).
/// Results are in chain order: cwd-first, then each `subdirs` entry in order.
/// The project plugin/agent dir walkers share this body so the byte-identical double-loop lives in one place.
pub(crate) fn existing_subdirs_along(chain_dirs: &[PathBuf], subdirs: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in chain_dirs {
        for subdir in subdirs {
            let candidate = dir.join(subdir);
            if candidate.is_dir() {
                found.push(candidate);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// RAII guard: set an env var, restore the prior value (or unset) on drop.
    /// A test then never leaves process-global env pointing at a dropped tempdir.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn resolve_in_repo_yields_cwd_to_root_chain() {
        // A git-init'd tmp with a 2-deep subdir: the chain is cwd to root inclusive, cwd-first, in the dirs' original spelling
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let chain = RepoDirChain::resolve(&nested);
        assert_eq!(
            chain.dirs,
            vec![
                nested.clone(),
                tmp.path().join("a"),
                tmp.path().to_path_buf(),
            ]
        );
        // `git_root` is the canonical worktree root (git2's `workdir`)
        // Compare by canonical form so a `/tmp` to `/private/tmp` symlink doesn't fail the test
        let root = chain.git_root.expect("inside a repo");
        assert_eq!(
            dunce::canonicalize(&root).unwrap(),
            dunce::canonicalize(tmp.path()).unwrap()
        );
    }

    #[test]
    fn resolve_outside_repo_is_cwd_only() {
        // A non-git tmp: no discovery hit, so the chain is just `[cwd]` and there is no git root
        // Only assert the no-repo shape when the temp dir is genuinely outside any repo
        // A dev/CI checkout may place $TMPDIR inside a larger git worktree
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        if git2::Repository::discover(&plain).is_err() {
            let chain = RepoDirChain::resolve(&plain);
            assert_eq!(chain.dirs, vec![plain]);
            assert_eq!(chain.git_root, None);
        }
    }

    #[test]
    #[serial(home_env)]
    fn resolve_treats_home_git_repo_as_no_repo() {
        // Home-is-a-git-repo (dotfiles in $HOME): discovery walks up to $HOME, but the guard drops that root
        // A subdir then resolves as no-repo (probe cwd only) instead of spanning the whole home subtree
        // Pin HOME and USERPROFILE: distill_dirs::home_dir reads USERPROFILE on Windows
        let tmp = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(tmp.path()).unwrap();
        git2::Repository::init(&home).unwrap();
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _userprofile_guard = EnvVarGuard::set("USERPROFILE", &home);
        let sub = home.join("proj");
        std::fs::create_dir_all(&sub).unwrap();

        let chain = RepoDirChain::resolve(&sub);
        assert_eq!(chain.git_root, None, "a home-dir git root must be dropped");
        assert_eq!(chain.dirs, vec![sub]);
    }

    #[test]
    #[serial(home_env)]
    fn resolve_keeps_non_home_git_root() {
        // The guard matches $HOME exactly: a git root that is NOT $HOME still resolves normally
        // Pin both HOME and USERPROFILE so Windows home_dir() sees the unrelated tempdir too
        let home = tempfile::tempdir().unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.path());
        let _userprofile_guard = EnvVarGuard::set("USERPROFILE", home.path());
        let repo = tempfile::tempdir().unwrap();
        git2::Repository::init(repo.path()).unwrap();
        let sub = repo.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();

        let chain = RepoDirChain::resolve(&sub);
        let root = chain.git_root.expect("a non-home git root must be kept");
        assert_eq!(
            dunce::canonicalize(&root).unwrap(),
            dunce::canonicalize(repo.path()).unwrap()
        );
    }
}
