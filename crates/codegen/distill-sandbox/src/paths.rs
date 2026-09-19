// Modified for Distill by Samuel Fajreldines, 2026.
//! Filesystem path tables for sandbox profiles.

use std::path::{Path, PathBuf};

// ── Grok state directory ────────────────────────────────────────────────────

/// Grok state directory (`$GROK_HOME` or `~/.grok`).
pub(crate) fn distill_home() -> PathBuf {
    distill_config::distill_home()
}

/// On-disk JSONL audit log under the sessions directory.
///
/// Strict writes it via the sessions directory grant (not a file grant on the grok home parent, which would follow a planted symlink).
pub(crate) fn sandbox_events_log_path() -> PathBuf {
    distill_home().join("sessions").join("sandbox-events.jsonl")
}

// ── Device files & directories ──────────────────────────────────────────────

/// Device files that need write access for normal tool operation. These are individual files (use `allow_file`, not
/// `allow_path`). Directory nodes under `/dev` belong in [`DEVICE_DIRS`].
#[cfg(all(feature = "enforce", unix))]
pub(crate) const DEVICE_FILES: &[&str] = &[
    "/dev/null",    // output sink — used by virtually every CLI tool
    "/dev/zero",    // zero source — used by memory allocators
    "/dev/random",  // entropy — used by crypto/TLS
    "/dev/urandom", // entropy — used by crypto/TLS
    "/dev/tty",     // controlling terminal — used by git, ssh, gpg
    "/dev/ptmx",    // PTY allocation — used by terminal spawning
];

/// Device directories that need write access (use `allow_path`, not `allow_file`).
#[cfg(all(feature = "enforce", unix))]
pub(crate) const DEVICE_DIRS: &[&str] = &[
    "/dev/pts", // PTY slaves (Linux)
    "/dev/fd",  // fd table (symlink to /proc/self/fd on Linux; a directory)
];

// ── Temporary directories ───────────────────────────────────────────────────

/// Temporary directories that need write access.
///
/// On macOS, programs use both `/tmp` (symlink to `/private/tmp`) and `/private/var/folders/` (the real `TMPDIR` / `NSTemporaryDirectory()`).
pub(crate) fn temp_writable_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];

    // macOS: /tmp is a symlink to /private/tmp, but the real TMPDIR is under /private/var/folders
    // Also include /private/tmp since Seatbelt may resolve the symlink.
    if cfg!(target_os = "macos") {
        for p in ["/private/tmp", "/private/var/tmp", "/private/var/folders"] {
            let pb = PathBuf::from(p);
            if pb.exists() && pb.is_dir() {
                paths.push(pb);
            }
        }
    }

    // Respect $TMPDIR if it points somewhere else (e.g. custom Linux setups).
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let pb = PathBuf::from(&tmpdir);
        if pb.exists() && pb.is_dir() && !paths.contains(&pb) {
            paths.push(pb);
        }
    }

    paths
}

// ── Essential writable paths ────────────────────────────────────────────────

/// Writable directory paths for the workspace profile (full `distill_home()` and temp).
/// Device files are handled separately via `allow_file` in `to_capability_set_with_config`.
pub(crate) fn essential_writable_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = vec![workspace.to_path_buf(), distill_home()];
    paths.extend(temp_writable_paths());
    paths
}

/// Writable directory paths for the strict profile (workspace, sessions, and temp). Strict may read `distill_home()` but
/// must not write the parent. Events JSONL lives under `sessions/`.
pub(crate) fn essential_writable_paths_strict(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = vec![workspace.to_path_buf(), distill_home().join("sessions")];
    paths.extend(temp_writable_paths());
    paths
}

/// Writable directory paths for the read-only profile (minimal: just ~/.grok + temp).
/// Device files are handled separately via `allow_file` in `to_capability_set_with_config`.
pub(crate) fn essential_writable_paths_minimal() -> Vec<PathBuf> {
    let mut paths = vec![distill_home()];
    paths.extend(temp_writable_paths());
    paths
}
