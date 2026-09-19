// Modified for Distill by Samuel Fajreldines, 2026.
//! Owner-only enforcement for the database file and its journal siblings.
//!
//! SQLite creates files at umask defaults, so the store pre-creates the database 0600 before SQLite's first open and re-tightens modes on every load.
//! On Windows the files keep the default profile ACLs.
//! Nothing here follows a symlink.
//! A link planted at the store path would otherwise redirect both SQLite's create and the chmod to an attacker-chosen target.

use std::path::{Path, PathBuf};

/// Refuse to operate through anything but a regular file (or nothing yet).
/// `O_CREAT|O_EXCL` alone is not enough: it fails `EEXIST` on a symlink, even a dangling one.
/// An open that then follows the link would create the database at the link's target.
pub(crate) fn require_regular_file(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => {
            tracing::error!(
                path = %path.display(),
                "workspace store path is not a regular file; refusing to open"
            );
            Err(std::io::Error::other(format!(
                "workspace store path {} is not a regular file",
                path.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create the file 0600 via `O_CREAT|O_EXCL`.
/// A concurrent creator winning the race is fine: it used the same mode, and the tighten pass follows.
/// What exists must still be a regular file (see [`require_regular_file`]).
pub(crate) fn create_owner_only(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => require_regular_file(path),
        Err(e) => Err(e),
    }
}

/// Re-assert owner-only mode on an existing file (a restored or hand-copied 0644 database is tightened on load).
/// Missing files are fine; a non-regular file or any other failure is a hard error, failing closed on a private store.
/// Same pattern as `distill_shell_base::util::secure_file`; depending on that crate is too heavy.
pub(crate) fn tighten_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !metadata.is_file() {
            tracing::error!(
                path = %path.display(),
                "workspace store file is not a regular file; refusing to chmod through it"
            );
            return Err(std::io::Error::other(format!(
                "workspace store path {} is not a regular file",
                path.display()
            )));
        }
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o777 != 0o600 {
            permissions.set_mode(0o600);
            if let Err(e) = std::fs::set_permissions(path, permissions) {
                tracing::warn!(path = %path.display(), error = %e, "failed to tighten workspace store file mode");
                return Err(e);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Journal sibling path, derived from the effective per-host path: `workspace.db` becomes `workspace.db-wal`.
pub(crate) fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
