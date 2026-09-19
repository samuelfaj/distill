// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared terminal-probe primitives: write a query, and raw-fd poll/read stdin until a terminator or deadline.
//! XTVERSION uses only `write_query`; its reply is handled by the event loop's response filter.
//!
//! Safety invariants (timed-read paths):
//! - Sole stdin reader: must run before crossterm's reader thread exists, or after that thread has been joined (both compete for stdin).
//! - Keystrokes typed inside the read window are consumed and dropped; no portable re-injection exists (TIOCSTI is blocked), an accepted loss.

use std::io::Write;
use std::time::Duration;

/// Bounds the reply buffer against terminals that stream without a terminator.
#[cfg(unix)]
pub(crate) const MAX_PROBE_RESPONSE: usize = 256;

/// Hard cap on post-deadline consumption of an in-flight reply.
#[cfg(unix)]
const LATE_REPLY_GRACE: Duration = Duration::from_millis(100);

/// Per-byte quiet window during the grace period.
#[cfg(unix)]
const LATE_REPLY_QUIET_MS: i32 = 25;

/// Write a probe query via the shared stderr lock; `false` if the TUI fd is not a TTY or the write fails.
pub(crate) fn write_query(query: &[u8]) -> bool {
    use std::io::IsTerminal;

    let write_result: std::io::Result<()> = distill_shared::stderr::with_locked_stderr(|stderr| {
        // fd 2 is /dev/null-redirected; the TTY check must run on the dup'd render fd inside the lock, not on std::io::stderr()
        if !stderr.is_terminal() {
            return Err(std::io::Error::other("TUI output is not a TTY"));
        }
        stderr.write_all(query)?;
        stderr.flush()
    });
    write_result.is_ok()
}

/// [`write_query`] under one `deadline`: both the stderr lock wait (a wedged writer thread may hold it) and the write are
/// bounded, so a terminal that stopped reading cannot hang teardown. `false` means the caller must not wait for a reply.
#[cfg(unix)]
pub(crate) fn write_query_until(query: &[u8], deadline: std::time::Instant) -> bool {
    use std::io::IsTerminal;
    use std::os::unix::io::AsRawFd;

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    distill_shared::stderr::try_with_locked_stderr_for(remaining, |stderr| {
        stderr.is_terminal() && write_all_until(stderr.as_raw_fd(), query, deadline)
    })
    .unwrap_or(false)
}

/// Nonblocking write of all of `buf` before `deadline`; `false` on the deadline or any error other than `EINTR`/`EAGAIN`.
#[cfg(unix)]
fn write_all_until(fd: i32, buf: &[u8], deadline: std::time::Instant) -> bool {
    use std::io::ErrorKind;

    let Some(_nonblocking) = NonblockingGuard::set(fd) else {
        return false;
    };
    let mut rest = buf;
    while !rest.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: pfd is a valid pollfd struct with a valid fd.
        let ready = unsafe { libc::poll(&mut pfd, 1, remaining_millis_ceil(remaining)) };
        if ready < 0 {
            if last_errno_is_eintr() {
                continue;
            }
            return false;
        }
        // A `poll` timeout may still be short of the deadline; the loop head decides
        if ready == 0 {
            continue;
        }
        // SAFETY: rest is a valid, initialized buffer of the given length for the duration of the call.
        let written = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if written < 0 {
            match std::io::Error::last_os_error().kind() {
                ErrorKind::Interrupted | ErrorKind::WouldBlock => continue,
                _ => return false,
            }
        }
        let Ok(count) = usize::try_from(written) else {
            return false;
        };
        let Some(unwritten) = rest.get(count..) else {
            return false;
        };
        rest = unwritten;
    }
    true
}

/// `O_NONBLOCK` on `fd` until drop. File-status flags live on the open file description, which the dup'd render fd shares
/// with `TUI_STDERR_FD` and usually with fds 0/1/2, so the flag must be gone before the drain's blocking reads; the stderr
/// lock held by the caller and the already joined writer thread mean nobody else writes meanwhile.
#[cfg(unix)]
struct NonblockingGuard {
    fd: i32,
    original_flags: i32,
}

#[cfg(unix)]
impl NonblockingGuard {
    fn set(fd: i32) -> Option<Self> {
        let original_flags = file_status_flags(fd)?;
        if original_flags & libc::O_NONBLOCK == 0 {
            // SAFETY: F_SETFL with a flag word derived from F_GETFL on the same fd.
            let set = unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) };
            if set < 0 {
                return None;
            }
        }
        Some(NonblockingGuard { fd, original_flags })
    }
}

#[cfg(unix)]
impl Drop for NonblockingGuard {
    fn drop(&mut self) {
        loop {
            // SAFETY: restores the flag word read by F_GETFL on the same fd.
            if unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.original_flags) } >= 0 {
                return;
            }
            if last_errno_is_eintr() {
                continue;
            }
            // tracing never takes the stderr lock, so this is safe while the caller still holds it
            let error = std::io::Error::last_os_error();
            tracing::warn!(fd = self.fd, %error, "could not restore tty file-status flags");
            return;
        }
    }
}

#[cfg(unix)]
fn file_status_flags(fd: i32) -> Option<i32> {
    // SAFETY: F_GETFL takes no argument and only reads the descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    (flags >= 0).then_some(flags)
}

/// Rounded up, so a `poll` timeout never expires before the deadline itself.
#[cfg(unix)]
fn remaining_millis_ceil(remaining: Duration) -> i32 {
    let has_fraction = !remaining.subsec_nanos().is_multiple_of(1_000_000);
    let millis = remaining.as_millis() + u128::from(has_fraction);
    millis.min(i32::MAX as u128) as i32
}

/// The descriptor crossterm reads key events from (`tty_fd`): stdin when it is a terminal, else the controlling tty.
/// A drain must read that same source; `grok </dev/null` would otherwise hit EOF at once and leave the reply to the shell.
#[cfg(unix)]
pub(crate) enum TtyInput {
    Stdin,
    ControllingTty(std::os::unix::io::OwnedFd),
}

#[cfg(unix)]
impl TtyInput {
    pub(crate) fn as_raw_fd(&self) -> i32 {
        use std::os::unix::io::AsRawFd;

        match self {
            TtyInput::Stdin => libc::STDIN_FILENO,
            TtyInput::ControllingTty(fd) => fd.as_raw_fd(),
        }
    }
}

/// `None` when neither stdin nor `/dev/tty` is a terminal.
#[cfg(unix)]
pub(crate) fn tty_input_fd() -> Option<TtyInput> {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        return Some(TtyInput::Stdin);
    }
    // Opened read+write like crossterm's `tty_fd`, so this succeeds in exactly the cases crossterm had an event source
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => Some(TtyInput::ControllingTty(std::os::unix::io::OwnedFd::from(
            tty,
        ))),
        Err(error) => {
            tracing::debug!(%error, "no controlling tty to drain");
            None
        }
    }
}

/// `Some` even for a partial read, so a half-read reply is never left for the EventStream.
/// `None` only when nothing arrived or stdin errored before any byte.
#[cfg(unix)]
pub(crate) fn read_tty_reply(
    timeout: Duration,
    mut is_terminated: impl FnMut(&[u8], u8) -> bool,
) -> Option<Vec<u8>> {
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let start = std::time::Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(64);

    loop {
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return finish_after_deadline(fd, buf, is_terminated);
        };
        let remaining_ms = remaining.as_millis().min(i32::MAX as u128) as i32;

        match poll_read_byte(fd, remaining_ms) {
            PollRead::Byte(byte) => {
                buf.push(byte);
                if buf.len() >= MAX_PROBE_RESPONSE || is_terminated(&buf, byte) {
                    return Some(buf);
                }
            }
            // Re-entry recomputes the deadline, so EINTR cannot extend it.
            PollRead::Interrupted => continue,
            PollRead::Timeout => return finish_after_deadline(fd, buf, is_terminated),
            PollRead::Error => return if buf.is_empty() { None } else { Some(buf) },
        }
    }
}

/// Deadline expiry: an in-flight reply (ESC byte seen; replies are DCS/CSI/OSC, plain keystrokes aren't) is consumed until quiet.
/// That keeps its tail from reaching the EventStream as typed garbage.
/// Otherwise return immediately to avoid eating keystrokes at a silent terminal.
#[cfg(unix)]
fn finish_after_deadline(
    fd: i32,
    mut buf: Vec<u8>,
    mut is_terminated: impl FnMut(&[u8], u8) -> bool,
) -> Option<Vec<u8>> {
    if buf.is_empty() {
        return None;
    }
    if !buf.contains(&0x1b) {
        return Some(buf);
    }
    let grace_start = std::time::Instant::now();
    while grace_start.elapsed() < LATE_REPLY_GRACE {
        match poll_read_byte(fd, LATE_REPLY_QUIET_MS) {
            PollRead::Byte(byte) => {
                buf.push(byte);
                if buf.len() >= MAX_PROBE_RESPONSE || is_terminated(&buf, byte) {
                    break;
                }
            }
            PollRead::Interrupted => continue,
            PollRead::Timeout | PollRead::Error => break,
        }
    }
    Some(buf)
}

/// Where [`drain_tty_until`] stopped.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrainEnd {
    Terminated,
    Deadline,
    /// Nothing is read after this.
    Error,
}

/// `bytes` counts every consumed byte, terminator included; `tail_len` is the length the predicate reported (0 unless `Terminated`).
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DrainStats {
    pub bytes: usize,
    pub end: DrainEnd,
    pub tail_len: usize,
}

/// A terminator must fit in the retained tail or it can never match; 64 covers every accepted DA1 reply, the longest real
/// one being xterm's 35-byte `ESC [ ? 64;1;2;6;9;15;16;17;18;21;22;28 c`.
#[cfg(unix)]
const DRAIN_TAIL_BYTES: usize = 64;

/// Read-and-discard `fd` until `is_terminated(tail)` returns `Some(len)` or `deadline` passes. Unlike [`read_tty_reply`]
/// nothing caps the byte count, so any amount of key reports or typeahead may precede the terminator.
#[cfg(unix)]
pub(crate) fn drain_tty_until(
    fd: i32,
    deadline: std::time::Instant,
    mut is_terminated: impl FnMut(&[u8]) -> Option<usize>,
) -> DrainStats {
    let mut tail: Vec<u8> = Vec::with_capacity(DRAIN_TAIL_BYTES);
    let mut bytes = 0usize;
    let end = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break DrainEnd::Deadline;
        }
        match poll_read_byte(fd, remaining_millis_ceil(remaining)) {
            PollRead::Byte(byte) => {
                bytes += 1;
                if tail.len() == DRAIN_TAIL_BYTES {
                    tail.remove(0);
                }
                tail.push(byte);
                if let Some(tail_len) = is_terminated(&tail) {
                    return DrainStats {
                        bytes,
                        end: DrainEnd::Terminated,
                        tail_len,
                    };
                }
            }
            PollRead::Interrupted => continue,
            // `poll` may wake short of the deadline; only the deadline itself ends the drain
            PollRead::Timeout if std::time::Instant::now() >= deadline => break DrainEnd::Deadline,
            PollRead::Timeout => continue,
            PollRead::Error => break DrainEnd::Error,
        }
    };
    DrainStats {
        bytes,
        end,
        tail_len: 0,
    }
}

#[cfg(unix)]
enum PollRead {
    Byte(u8),
    Interrupted,
    Timeout,
    Error,
}

/// One EINTR-retrying poll-then-read step for a single byte.
#[cfg(unix)]
fn poll_read_byte(fd: i32, timeout_ms: i32) -> PollRead {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd is a valid pollfd struct with a valid fd.
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ret == 0 {
        return PollRead::Timeout;
    }
    if ret < 0 {
        return if last_errno_is_eintr() {
            PollRead::Interrupted
        } else {
            PollRead::Error
        };
    }

    loop {
        let mut byte = [0u8; 1];
        // SAFETY: byte is a valid buffer of length 1.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return PollRead::Byte(byte[0]);
        }
        if n < 0 && last_errno_is_eintr() {
            continue;
        }
        return PollRead::Error;
    }
}

#[cfg(unix)]
fn last_errno_is_eintr() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

#[cfg(all(test, unix))]
#[path = "probe_tests.rs"]
mod tests;
