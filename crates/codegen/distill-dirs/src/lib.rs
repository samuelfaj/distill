// Modified for Distill by Samuel Fajreldines, 2026.
//! Home-directory resolution generally: USERPROFILE-first `home_dir`, plus
//! Distill home (`$DISTILL_HOME` or `<home>/.distill`, with legacy profile reuse). Shared by `distill-config`
//! and `distill-fast-worktree`.
//!
//! Which function to call:
//! - [`distill_home`]: the usual choice, a cached, created path to build on.
//! - [`user_distill_home`]: `None` instead of a cwd fallback when no home resolves.
//! - [`default_distill_home`]: the `<home>/.distill` default, ignoring `$GROK_HOME`, so callers can detect an override.
//! - [`resolve_distill_home`]: a fresh, uncached resolve.
//! - [`resolve_distill_home_with_source`]: [`resolve_distill_home`] plus where the path came from.
//! - [`home_dir`]: the home directory itself, for sibling dot dirs (`~/.claude`, `~/.agents`, ...).
//!
//! TODO: collapse these getters by threading the path through config as an
//! explicit value.

#![deny(clippy::indexing_slicing)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where a resolved Distill home came from, so "why did Distill pick this
/// directory?" is answerable in diagnostics without re-reading the
/// environment at the asking site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistillHomeSource {
    /// A non-empty `$DISTILL_HOME` or legacy `$GROK_HOME` override.
    EnvOverride,
    /// `<home>/.distill` derived from the home directory.
    HomeDefault,
}

/// The user's home directory via [`std::env::home_dir`]: `HOME` on Unix, `USERPROFILE` on Windows.
/// Not `dirs::home_dir()`: on Windows `dirs` ignores a redirected `USERPROFILE`.
/// Every home-anchored path must come from this one function.
#[allow(deprecated, clippy::disallowed_methods)] // the one sanctioned std::env::home_dir call
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// `<home>/.distill`, canonicalized via `dunce` (not `std::fs::canonicalize`,
/// which yields Windows `\\?\` verbatim paths).
fn distill_home_in(home: &Path) -> PathBuf {
    dunce::canonicalize(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .join(".distill")
}

/// Explicit home verbatim when non-empty; otherwise reuse an existing legacy profile
/// until a Distill profile exists.
/// Used as-is (not canonicalized) so literal prefix checks and symlink guards still see original components.
fn resolve_distill_home_from(
    distill_home_env: Option<&OsStr>,
    os_home: Option<&Path>,
) -> Option<(PathBuf, DistillHomeSource)> {
    if let Some(env) = distill_home_env.filter(|env| !env.is_empty()) {
        return Some((PathBuf::from(env), DistillHomeSource::EnvOverride));
    }
    os_home.map(|home| {
        let current = distill_home_in(home);
        let legacy = home.join(".grok");
        // Existing users retain their accounts and sessions after the rename.
        let path = if !current.exists() && legacy.is_dir() {
            legacy
        } else {
            current
        };
        (path, DistillHomeSource::HomeDefault)
    })
}

/// Resolve the Distill home from the environment (fresh, no cache); `None` if neither resolves.
pub fn resolve_distill_home() -> Option<PathBuf> {
    resolve_distill_home_with_source().map(|(home, _)| home)
}

/// [`resolve_distill_home`] plus the [`DistillHomeSource`] the path came from.
pub fn resolve_distill_home_with_source() -> Option<(PathBuf, DistillHomeSource)> {
    resolve_distill_home_from(
        std::env::var_os("DISTILL_HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("GROK_HOME"))
            .as_deref(),
        home_dir().as_deref(),
    )
}

/// The default `<home>/.distill`, used when `$GROK_HOME` is unset.
pub fn default_distill_home() -> PathBuf {
    distill_home_in(&home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

/// The Distill home, created if missing and cached for the process; falls back to
/// [`default_distill_home`] when neither `$GROK_HOME` nor a home resolves.
pub fn distill_home() -> PathBuf {
    static DISTILL_HOME: OnceLock<PathBuf> = OnceLock::new();
    DISTILL_HOME
        .get_or_init(|| {
            let home = resolve_distill_home().unwrap_or_else(default_distill_home);
            if let Err(err) = std::fs::create_dir_all(&home) {
                tracing::warn!(path = %home.display(), %err, "failed to create Distill home");
            }
            home
        })
        .clone()
}

/// Like [`distill_home`], but `None` when no home resolves (no cwd fallback).
pub fn user_distill_home() -> Option<PathBuf> {
    resolve_distill_home().is_some().then(distill_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn env_wins_over_os_home() {
        let resolved =
            resolve_distill_home_from(Some(OsStr::new("/custom/home")), Some(Path::new("/home/u")));
        assert_eq!(
            resolved,
            Some((
                PathBuf::from("/custom/home"),
                DistillHomeSource::EnvOverride
            ))
        );
    }

    #[test]
    fn env_used_verbatim_even_when_it_exists() {
        // A real, existing dir whose canonical form differs (macOS symlinks
        // `/var` -> `/private/var`): the env value must come back unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_distill_home_from(Some(tmp.path().as_os_str()), None);
        assert_eq!(
            resolved,
            Some((tmp.path().to_path_buf(), DistillHomeSource::EnvOverride))
        );
    }

    #[test]
    fn empty_env_falls_through_to_os_home() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_distill_home_from(Some(&OsString::new()), Some(tmp.path()));
        assert_eq!(
            resolved,
            Some((
                dunce::canonicalize(tmp.path()).unwrap().join(".distill"),
                DistillHomeSource::HomeDefault
            ))
        );
    }

    #[test]
    fn default_distill_home_has_no_verbatim_prefix() {
        // The reason we canonicalize via dunce: std::fs::canonicalize yields
        // `\\?\` verbatim paths on Windows that break git and byte-exact
        // comparisons. No-op assertion on Unix.
        let home = default_distill_home();
        assert!(!home.to_string_lossy().starts_with(r"\\?\"));
        assert!(home.ends_with(".distill"));
    }

    #[test]
    fn existing_profile_is_reused_until_distill_profile_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(tmp.path()).unwrap();
        let legacy = home.join(".grok");
        std::fs::create_dir(&legacy).unwrap();
        assert_eq!(
            resolve_distill_home_from(None, Some(&home)).unwrap().0,
            legacy
        );
        let current = home.join(".distill");
        std::fs::create_dir(&current).unwrap();
        assert_eq!(
            resolve_distill_home_from(None, Some(&home)).unwrap().0,
            current
        );
    }

    #[test]
    fn none_when_nothing_resolves() {
        assert_eq!(
            resolve_distill_home_from(/* distill_home_env */ None, /* os_home */ None),
            None
        );
    }
}
