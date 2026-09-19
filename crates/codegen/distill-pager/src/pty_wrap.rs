//! Local PTY wrapper: the engine behind `grok wrap` (see [`crate::wrap_cmd`]).
//!
//! Spawns a command inside a local pseudo-terminal and pipes its output through `crate::wrap_filter::Osc52Filter`.
//! The filter intercepts OSC 52 clipboard sequences, making "copy" work for programs that cannot reach the user's clipboard (containers, SSH).
//! Copy works even under terminals without OSC 52 support.
//! The filter also answers the private host clipboard image request OSC (see [`crate::wrap_clipboard_image`]).
//! It reports DEC private mode changes to `crate::wrap_restore::ModeTracker`.
//!
//! This module owns the PTY setup, the writer/stdin/resize threads, and the exit paths.
//! The exit paths (drop guard, termination-signal thread) restore the outer terminal when the child dies with modes still latched.

use anyhow::Result;
use std::io::Write;
use std::sync::Arc;

use crate::theme::system_appearance::SystemAppearance;
use crate::wrap_filter::Osc52Filter;
use crate::wrap_restore::ModeTracker;

/// Sets the OSC 52 sink markers and, when known, the local appearance (`LC_*` survives SSH).
fn apply_wrap_child_env(
    cmd: &mut portable_pty::CommandBuilder,
    appearance: Option<SystemAppearance>,
) {
    cmd.env("GROK_OSC52_SINK", "1");
    cmd.env("LC_GROK_OSC52_SINK", "1");
    if let Some(appearance) = appearance {
        let value = appearance.as_env_value();
        cmd.env("GROK_APPEARANCE", value);
        cmd.env("LC_GROK_APPEARANCE", value);
    }
}

/// Run an arbitrary command inside a local PTY with OSC 52 output filtering. This is the engine behind `grok wrap`:
/// it spawns `program` (with `args`) attached to a local pseudo-terminal. Size changes of the outer terminal are
/// forwarded to the child. All other output passes through unchanged.
pub(crate) fn run_wrapped_command(program: &str, args: &[String]) -> Result<i32> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Open PTY pair.
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Build the command.
    let mut cmd = CommandBuilder::new(program);
    args.iter().for_each(|arg| cmd.arg(arg));

    apply_wrap_child_env(&mut cmd, crate::theme::system_appearance::detect_desktop());

    // Not session-scoped: this is the wrapped process itself.
    #[allow(clippy::disallowed_methods)]
    let mut child = pair.slave.spawn_command(cmd)?;
    // Drop the slave so we get EOF when child exits.
    drop(pair.slave);

    // Obtain reader from the master PTY. Confining `write_all` to a single owner thread avoids that Handles are
    // intentionally detached: `grok wrap` is short-lived and exits with the child.
    let mut pty_reader = pair.master.try_clone_reader()?;

    // We deliberately do NOT block SIGWINCH here
    // The resize handler (`sigwinch_loop`) installs a real signal handler via `signal-hook`, which must be free to run when the signal is delivered
    // Blocking it and waiting via `sigwait` looks correct but silently fails on macOS (see `sigwinch_loop`)

    // Tracks the DEC private modes / kitty pushes flowing through the output filter, so every exit path can reset exactly what the child left latched
    // (A connection drop kills the child before its reset bytes arrive.)
    let tracker = Arc::new(ModeTracker::new());

    // Switch to raw mode so keystrokes pass through unchanged.
    crossterm::terminal::enable_raw_mode()?;
    let _restore_guard = TerminalRestoreGuard {
        tracker: Arc::clone(&tracker),
    };

    // Terminating signals (external kill, terminal-close HUP) bypass Drop, so handle them explicitly: forward to the
    // child, restore, exit 128+N. Handlers are installed here on the main thread so no signal can slip through before
    // the loop thread gets scheduled.
    #[cfg(unix)]
    let child_reaped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(unix)]
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        let child_pid = child.process_id();
        let tracker = Arc::clone(&tracker);
        let child_reaped = Arc::clone(&child_reaped);
        match signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM]) {
            Ok(signals) => {
                std::thread::spawn(move || {
                    terminate_signal_loop(signals, tracker, child_pid, child_reaped)
                });
            }
            Err(e) => tracing::debug!("failed to install wrap termination handler: {e}"),
        }
    }

    let (write_tx, write_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut writer = pair.master.take_writer()?;
        std::thread::spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                // `write_all` runs only on this thread, so libc never interleaves chunks from two writers
                // EIO here almost always means the slave has closed (child exited); stop
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    // Forward local stdin to the writer thread
    let stdin_tx = write_tx.clone();
    let _stdin_handle = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else {
                        break;
                    };
                    if stdin_tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Unix only: there is no SIGWINCH on Windows. On Windows `pair.master` is kept alive inside `pair` until this
    // function returns (after `child.wait`), so the ConPTY stays open for the read loop. The OSC 52 clipboard bridge
    // works identically there; only live resize is unavailable.
    #[cfg(unix)]
    {
        let master = pair.master;
        std::thread::spawn(move || {
            sigwinch_loop(master);
        });
    }

    // Output forwarding with OSC 52 filtering: the PTY reader feeds the filter, which feeds stdout. The worker then
    // enqueues the bracketed-paste frame on the writer thread, plus a newline so ICANON slaves deliver it without
    // another key.
    {
        let mut stdout = std::io::stdout().lock();
        let mut filter = Osc52Filter::new()
            .with_wrap_image_handler(move || {
                let tx = write_tx.clone();
                std::thread::spawn(move || {
                    let mut bytes = crate::wrap_filter::host_clipboard_image_frame();
                    bytes.push(b'\n');
                    let _ = tx.send(bytes);
                });
            })
            .with_mode_tracker(Arc::clone(&tracker));
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else {
                        break;
                    };
                    let filtered = filter.feed(chunk);
                    if !filtered.is_empty() {
                        if stdout.write_all(&filtered).is_err() {
                            break;
                        }
                        let _ = stdout.flush();
                    }
                }
            }
        }
    }

    // Wait for child and extract exit code.
    let status = child.wait()?;
    // Reaped: the pid is recyclable from here on, so the signal thread must no longer forward to it
    #[cfg(unix)]
    child_reaped.store(true, std::sync::atomic::Ordering::SeqCst);
    let code = status.exit_code() as i32;

    Ok(code)
}

/// That pattern is POSIX-correct but silently fails on macOS. SIGWINCH's default disposition is "ignore", and macOS
/// discards a blocked default-ignore signal rather than leaving it pending for `sigwait`. The handler never woke,
/// so the inner PTY was never resized and the remote TUI kept rendering at the original size.
#[cfg(unix)]
fn sigwinch_loop(master: Box<dyn portable_pty::MasterPty + Send>) {
    use portable_pty::PtySize;

    let mut signals = match signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]) {
        Ok(signals) => signals,
        Err(e) => {
            tracing::debug!("failed to install SIGWINCH handler: {e}");
            return;
        }
    };

    for _ in signals.forever() {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

/// Guard that restores terminal state when dropped (including on panic). The tracker's run-once claim is shared
/// with the termination-signal thread so the restore never runs twice.
struct TerminalRestoreGuard {
    tracker: Arc<ModeTracker>,
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        restore_terminal(&self.tracker);
    }
}

/// Idempotently restore the outer terminal: emit resets for the latched modes (only while stdout is still a TTY),
/// then leave raw mode. An exit racing the winner would kill the process mid-restore, keeping raw mode and leaving
/// the resets partial.
fn restore_terminal(tracker: &ModeTracker) {
    use std::io::IsTerminal;

    if !tracker.begin_restore() {
        wait_restore_done(tracker, std::time::Duration::from_millis(100));
        return;
    }
    let bytes = crate::wrap_restore::restore_bytes(tracker.snapshot());
    if !bytes.is_empty() && std::io::stdout().is_terminal() {
        write_stdout_unlocked(&bytes);
    }
    let _ = crossterm::terminal::disable_raw_mode();
    tracker.finish_restore();
}

/// Bounded wait for a claimed restore to complete. An unbounded wait here would reintroduce the hang (a wrap
/// process that never exits) the unlocked write exists to avoid.
fn wait_restore_done(tracker: &ModeTracker, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if tracker.restore_done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Best-effort write of restore bytes to stdout, bypassing Rust's stdout lock. Taking the lock there would trade a
/// broken terminal for a wrap process that never exits. Accepted: this only arises on the signal path of a process
/// that exits immediately after.
#[cfg(unix)]
fn write_stdout_unlocked(bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: plain write(2) on fd 1 with an in-bounds slice.
        let rc = unsafe {
            libc::write(
                1,
                match bytes.get(written..) {
                    Some(rest) => rest.as_ptr() as *const libc::c_void,
                    None => break,
                },
                bytes.len() - written,
            )
        };
        if rc > 0 {
            written += rc as usize;
        } else if rc < 0
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        } else {
            break;
        }
    }
}

/// Without a signal path (no Unix signals), the restore only runs on the main-thread drop path after the read loop released the lock.
/// The ordinary locked stdout is therefore safe here.
#[cfg(not(unix))]
fn write_stdout_unlocked(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

/// Without this thread a signal death would skip `Drop` entirely, leaking raw mode and every latched mode. The
/// snapshot taken here can therefore include an enable the terminal never received. Deferring the report until
/// after the write would miss resets instead, and puts a per-CSI buffer on the hot output path.
#[cfg(unix)]
fn terminate_signal_loop(
    mut signals: signal_hook::iterator::Signals,
    tracker: Arc<ModeTracker>,
    child_pid: Option<u32>,
    child_reaped: Arc<std::sync::atomic::AtomicBool>,
) {
    if let Some(signal) = signals.forever().next() {
        // Skip the forward once the child is reaped: its pid is recyclable and the kill could hit a bystander
        // The check narrows the reuse window but cannot close it (a reap can land between it and the kill)
        // Every signal-forwarding wrapper accepts that residual window
        if let Some(pid) = child_pid
            && !child_reaped.load(std::sync::atomic::Ordering::SeqCst)
        {
            // Forward first so the child can run its own teardown while we restore
            // Its late output goes to a PTY we are abandoning
            // SAFETY: kill(2) has no memory-safety preconditions; pid is positive (never the 0/-1 broadcast forms)
            unsafe { libc::kill(pid as libc::pid_t, signal) };
        }
        restore_terminal(&tracker);
        std::process::exit(128 + signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_restore_done_returns_immediately_when_finished() {
        let tracker = ModeTracker::new();
        assert!(tracker.begin_restore());
        tracker.finish_restore();
        assert!(wait_restore_done(
            &tracker,
            std::time::Duration::from_millis(100)
        ));
    }

    #[test]
    fn wait_restore_done_times_out_when_winner_never_finishes() {
        let tracker = ModeTracker::new();
        assert!(tracker.begin_restore());
        // No finish_restore: the bounded wait must give up, not hang.
        assert!(!wait_restore_done(
            &tracker,
            std::time::Duration::from_millis(5)
        ));
    }

    fn env_str(cmd: &portable_pty::CommandBuilder, key: &str) -> Option<String> {
        cmd.get_env(key)
            .and_then(|value| value.to_str().map(str::to_owned))
    }

    #[test]
    fn apply_wrap_child_env_dark_overrides_parent_light_on_both_names() {
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.env("GROK_APPEARANCE", "light");
        cmd.env("LC_GROK_APPEARANCE", "light");
        apply_wrap_child_env(&mut cmd, Some(SystemAppearance::Dark));
        assert_eq!(env_str(&cmd, "GROK_APPEARANCE").as_deref(), Some("dark"));
        assert_eq!(env_str(&cmd, "LC_GROK_APPEARANCE").as_deref(), Some("dark"));
        assert_eq!(env_str(&cmd, "GROK_OSC52_SINK").as_deref(), Some("1"));
        assert_eq!(env_str(&cmd, "LC_GROK_OSC52_SINK").as_deref(), Some("1"));
    }

    #[test]
    fn apply_wrap_child_env_light_overrides_parent_dark_on_both_names() {
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.env("GROK_APPEARANCE", "dark");
        cmd.env("LC_GROK_APPEARANCE", "dark");
        apply_wrap_child_env(&mut cmd, Some(SystemAppearance::Light));
        assert_eq!(env_str(&cmd, "GROK_APPEARANCE").as_deref(), Some("light"));
        assert_eq!(
            env_str(&cmd, "LC_GROK_APPEARANCE").as_deref(),
            Some("light")
        );
    }

    #[test]
    fn apply_wrap_child_env_none_does_not_stamp_from_parent_snapshot() {
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.env("GROK_APPEARANCE", "dark");
        cmd.env_remove("LC_GROK_APPEARANCE");
        apply_wrap_child_env(&mut cmd, None);
        assert_eq!(env_str(&cmd, "GROK_APPEARANCE").as_deref(), Some("dark"));
        assert_eq!(env_str(&cmd, "LC_GROK_APPEARANCE"), None);
        assert_eq!(env_str(&cmd, "GROK_OSC52_SINK").as_deref(), Some("1"));
    }
}
