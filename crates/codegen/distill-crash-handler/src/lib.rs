// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-platform crash handler with startup crash detection.
//!
//! - **Unix**: SIGBUS/SIGSEGV/SIGABRT via `sigaction(2)`. SIGABRT capture
//!   means `panic = "abort"` builds (every shipped release) leave a crash
//!   report when a Rust panic aborts the process.
//! - **Windows**: access violations via `SetUnhandledExceptionFilter`.
//!   SIGABRT capture is Unix-only — `abort()` on Windows does not route
//!   through the unhandled-exception filter.
//!
//! # Usage
//!
//! Call [`check_previous_crash`] first to detect crashes from the previous
//! session, then [`install`] early in `main()`, before any async runtime or
//! thread spawning. `check_previous_crash` must run before `install` because
//! `install` opens `last-crash.bin` with `O_TRUNC`.
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//!
//! let crash_dir = PathBuf::from("/home/user/.myapp/crash");
//!
//! if let Some(report) = distill_crash_handler::check_previous_crash(&crash_dir) {
//!     eprintln!("Application crashed during your last session.");
//!     eprintln!("  Signal: {}", report.signal_name);
//!     eprintln!("  Report: {}", report.report_path.display());
//! }
//!
//! distill_crash_handler::install(distill_crash_handler::CrashHandlerConfig {
//!     app_version: "0.1.0".to_string(),
//!     crash_dir: crash_dir.clone(),
//! });
//! ```

#![deny(clippy::indexing_slicing)]

pub mod format;
mod handler;
pub mod symbolicate;
pub mod terminal;

use std::path::{Path, PathBuf};

pub use symbolicate::ResolvedFrame;

const MAX_HISTORY: usize = 5;

/// Configuration for the crash handler.
pub struct CrashHandlerConfig {
    /// Application version string (e.g. "0.1.169-alpha.2").
    pub app_version: String,
    /// Directory where crash dumps are written.
    /// Created if it does not exist.
    pub crash_dir: PathBuf,
}

/// Information about a crash from the previous session.
#[derive(Debug)]
pub struct CrashReport {
    /// Human-readable signal name (e.g. "SIGBUS (Bus error)").
    pub signal_name: &'static str,
    /// The `si_code` from `siginfo_t`.
    pub si_code: i32,
    /// The faulting memory address.
    pub faulting_address: u64,
    /// Unix timestamp of the crash.
    pub timestamp: u64,
    /// Application version at crash time.
    pub app_version: String,
    /// Symbolicated backtrace frames.
    pub backtrace: Vec<ResolvedFrame>,
    /// Path to the saved human-readable crash report.
    pub report_path: PathBuf,
}

/// Install the crash handler for SIGBUS, SIGSEGV, and SIGABRT (Unix; Windows: access violations).
/// Call early in `main()`, before any async runtime or thread spawning. Creates `crash_dir` if needed.
/// Returns `true` on success; unsupported platforms are a no-op returning `false`.
pub fn install(config: CrashHandlerConfig) -> bool {
    handler::install(&config.crash_dir, &config.app_version)
}

/// Install a minimal SIGSEGV/SIGBUS/SIGABRT handler that only restores the terminal.
/// No crash reporting (no file I/O, no stack walking). If [`install`] is called later, it replaces these.
/// No-op on unsupported platforms.
pub fn install_terminal_restore_only() {
    handler::install_terminal_restore_only()
}

/// Upgrade SIGSEGV/SIGBUS/SIGABRT handlers to include terminal escape code
/// restoration. Call when TUI modes are enabled.
pub fn enable_terminal_escape_restore() {
    handler::enable_terminal_escape_restore()
}

/// Downgrade SIGSEGV/SIGBUS/SIGABRT handlers to termios-only restoration.
/// Call when TUI modes are disabled.
pub fn disable_terminal_escape_restore() {
    handler::disable_terminal_escape_restore()
}

/// Check for a crash from the previous session.
/// Reads `crash_dir/last-crash.bin`, symbolicates, writes a report, and archives it.
/// `Some` if a valid crash file was found.
pub fn check_previous_crash(crash_dir: &Path) -> Option<CrashReport> {
    let crash_file = crash_dir.join("last-crash.bin");
    let data = std::fs::read(&crash_file).ok()?;
    let blob = format::CrashBlob::parse(&data)?;

    let frames = symbolicate::resolve_frames(&blob);
    let report_text = symbolicate::format_report(&blob, &frames);

    // Write the human-readable report (owner-only when the OS supports it).
    let report_path = crash_dir.join("last-crash-report.txt");
    let _ = write_owner_only(&report_path, report_text.as_bytes());

    // Archive to history/ (keep last MAX_HISTORY).
    archive_report(crash_dir, &report_text, blob.timestamp);

    // Remove the binary blob so it's not re-processed.
    let _ = std::fs::remove_file(&crash_file);

    Some(CrashReport {
        signal_name: symbolicate::signal_name(blob.signal),
        si_code: blob.si_code,
        faulting_address: blob.si_addr,
        timestamp: blob.timestamp,
        app_version: blob.app_version,
        backtrace: frames,
        report_path,
    })
}

/// Write `contents` with owner-only permissions when the platform allows it.
/// Crash reports may include source paths and backtraces; under `$GROK_HOME` they must not be world-readable.
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // mode() only applies on create — force owner-only before writing so a
        // preexisting 0644 file never holds sensitive content while world-readable.
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

fn archive_report(crash_dir: &Path, report_text: &str, timestamp: u64) {
    let history_dir = crash_dir.join("history");
    let _ = std::fs::create_dir_all(&history_dir);

    let filename = format!("crash-{}.txt", timestamp);
    let _ = write_owner_only(&history_dir.join(&filename), report_text.as_bytes());

    // Prune old reports beyond MAX_HISTORY.
    if let Ok(mut entries) = std::fs::read_dir(&history_dir) {
        let mut files: Vec<PathBuf> = entries
            .by_ref()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        files.sort();
        if files.len() > MAX_HISTORY
            && let Some(old_files) = files
                .len()
                .checked_sub(MAX_HISTORY)
                .and_then(|n| files.get(..n))
        {
            for old in old_files {
                let _ = std::fs::remove_file(old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_previous_crash_returns_none_when_no_file() {
        let dir = PathBuf::from("/tmp/distill-crash-handler-test-nonexistent");
        assert!(check_previous_crash(&dir).is_none());
    }

    #[cfg(unix)]
    fn unique_tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "distill-crash-handler-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_creates_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_tmp_dir("create-0600");
        let path = dir.join("report.txt");
        write_owner_only(&path, b"secret").expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "new file must be owner-only");
        assert_eq!(std::fs::read(&path).expect("read"), b"secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_tightens_preexisting_0644() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_tmp_dir("tighten-0644");
        let path = dir.join("report.txt");
        std::fs::write(&path, b"old").expect("seed");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).expect("set 0644");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o644
        );

        write_owner_only(&path, b"new-secret").expect("overwrite");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "overwrite must tighten to owner-only");
        assert_eq!(std::fs::read(&path).expect("read"), b"new-secret");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
