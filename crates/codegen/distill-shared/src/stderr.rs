// Modified for Distill by Samuel Fajreldines, 2026.
//! Serialized access to the TUI's stderr writer.

use parking_lot::{Mutex, MutexGuard};
use std::sync::OnceLock;
use std::time::Duration;

static STDERR_OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn stderr_output_lock() -> &'static Mutex<()> {
    STDERR_OUTPUT_LOCK.get_or_init(|| Mutex::new(()))
}

pub fn stderr_lock() -> MutexGuard<'static, ()> {
    stderr_output_lock().lock()
}

/// When [`distill_tty_utils::redirect_native_stderr`] has been called, this writes to the dup'd fd that points at the real
/// terminal. Otherwise falls back to normal stderr. Event-loop-thread code must enqueue on the pager's `EscapeWriter`;
/// this helper is for startup/teardown/suspend paths and non-loop threads.
pub fn with_locked_stderr<T>(f: impl FnOnce(&mut std::fs::File) -> T) -> T {
    let _guard = stderr_lock();
    f(&mut tui_stderr_file())
}

/// [`with_locked_stderr`] that gives up after `timeout` (a holder may be wedged in a tty write); `None` means `f` never ran.
pub fn try_with_locked_stderr_for<T>(
    timeout: Duration,
    f: impl FnOnce(&mut std::fs::File) -> T,
) -> Option<T> {
    let _guard = stderr_output_lock().try_lock_for(timeout)?;
    Some(f(&mut tui_stderr_file()))
}

fn tui_stderr_file() -> std::fs::File {
    distill_tty_utils::dup_tui_stderr().unwrap_or_else(|_| {
        // Fallback: try_clone stderr to get an independently-owned File
        // This path is hit if redirect_native_stderr was never called or fd dup fails
        let stderr = std::io::stderr();
        let stderr_file: std::fs::File;
        #[cfg(unix)]
        {
            use std::os::unix::io::{AsRawFd, FromRawFd};
            // SAFETY: stderr fd (2) is valid; from_raw_fd takes ownership
            // of the dup'd copy, not the original.
            let fd = unsafe { libc::dup(stderr.as_raw_fd()) };
            stderr_file = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        #[cfg(not(unix))]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle};
            // SAFETY: stderr handle is valid; DuplicateHandle gives us
            // an independent copy.
            let handle = stderr.as_raw_handle();
            let temp = unsafe { std::fs::File::from_raw_handle(handle) };
            stderr_file = temp.try_clone().expect("dup stderr fallback");
            std::mem::forget(temp);
        }
        stderr_file
    })
}

#[cfg(test)]
#[path = "stderr_tests.rs"]
mod tests;
