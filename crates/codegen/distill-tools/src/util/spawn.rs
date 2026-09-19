// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-platform child-process lifecycle helpers for `tokio::process::Command`.
//!
//! The spawn-lifecycle primitives are re-exported from the lightweight
//! [`distill_tty_utils`] crate; this module adds the logging reap wrapper
//! (tracing is unavailable there).

pub use distill_tty_utils::{
    ProcessGroup, ProcessScope, detach_command, detach_search_command, global_process_scope,
    new_process_group,
};

/// Reap an already-killed search child, bounded by
/// [`distill_tty_utils::KILL_REAP_TIMEOUT`]; on `None` warn and leave the corpse
/// to tokio's orphan reaper.
pub async fn reap_killed_search_child(
    child: &mut tokio::process::Child,
) -> Option<std::process::ExitStatus> {
    let status = distill_tty_utils::reap_killed_bounded(child, distill_tty_utils::KILL_REAP_TIMEOUT).await;
    if status.is_none() {
        tracing::warn!(
            reap_timeout_secs = distill_tty_utils::KILL_REAP_TIMEOUT.as_secs(),
            "killed search child not reaped (bound expired — likely uninterruptible kernel I/O — or wait failed); abandoning to the orphan reaper"
        );
    }
    status
}
