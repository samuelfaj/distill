//! Owner of the stdin reader thread that `event_loop::run` spawns.
//!
//! Teardown joins it before the kitty pop fence reads stdin, so the fence is the only stdin reader. The join is bounded
//! and never `join`s a thread that has not finished (it may be blocked in `read`); a straggler is detached and reported.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::app::event_loop::TimedInputEvent;

/// The reader notices a dropped `input_rx` within one [`POLL_TIMEOUT`]; ten of them cover a loaded machine.
pub(crate) const READER_JOIN_GRACE: Duration = Duration::from_millis(250);

// Bounds how long a tty handoff (external editor / pager) waits for this thread to park
// The pause flag is only observed between `poll()` calls, so the timeout is the handoff latency
// A `poll()` timeout does NOT wake the main loop (only a successful `send` does), so the idle loop still parks (no metronome tick)
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

/// Outcome of [`ReaderThread::join_within`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ReaderJoin {
    Joined,
    /// Detached at the deadline: another stdin reader may be alive.
    TimedOut,
    /// Never spawned (startup failed before the event loop).
    Absent,
}

pub(crate) struct ReaderThread {
    handle: Option<JoinHandle<()>>,
}

impl ReaderThread {
    pub(crate) fn detached() -> Self {
        ReaderThread { handle: None }
    }

    /// The thread owns the sole strong sender and exits once `tx` is closed. `paused` is set around tty handoffs so it
    /// stops touching stdin; it acknowledges through `parked`.
    pub(crate) fn spawn(
        tx: UnboundedSender<TimedInputEvent>,
        paused: Arc<AtomicBool>,
        parked: Arc<AtomicBool>,
    ) -> Self {
        let handle = std::thread::spawn(move || {
            let mut consecutive_event_errors: u32 = 0;
            loop {
                // Shutdown observed within one poll cycle in every state (idle or paused); the send() break below covers close-while-sending
                if tx.is_closed() {
                    break;
                }
                // While a tty handoff owns stdin, do not read(): the child (e.g. the editor) must keep its bytes.
                // Re-check soon without touching stdin
                if paused.load(Ordering::Acquire) {
                    // Signal the handoff that the reader is no longer in crossterm.
                    parked.store(true, Ordering::Release);
                    std::thread::sleep(POLL_TIMEOUT);
                    continue;
                }
                // Active path: this thread owns crossterm again this iteration.
                parked.store(false, Ordering::Release);
                // poll() then read() (not a bare blocking read) so the pause flag and a dropped receiver are observed within POLL_TIMEOUT
                let event = match crossterm::event::poll(POLL_TIMEOUT) {
                    Ok(true) => crossterm::event::read(),
                    Ok(false) => continue,
                    Err(e) => Err(e),
                };
                match event {
                    Ok(ev) => {
                        consecutive_event_errors = 0;
                        let timed = TimedInputEvent::now(ev);
                        if tx.send(timed).is_err() {
                            break; // event loop has shut down
                        }
                    }
                    Err(e) => {
                        // VTE terminals / SSH PTYs can emit garbage that crossterm's parser rejects
                        // Skip transient errors rather than kill the TUI (ratatui#1275), bailing only if they never stop
                        consecutive_event_errors += 1;
                        if consecutive_event_errors >= 50 {
                            tracing::error!(
                                "crossterm read returned {consecutive_event_errors} \
                                 consecutive errors, exiting reader: {e}"
                            );
                            break;
                        }
                        tracing::warn!("crossterm read error (skipping): {e}");
                    }
                }
            }
        });
        ReaderThread {
            handle: Some(handle),
        }
    }

    /// Never `join`s a thread that is still running: blocked in `read`, it would hang teardown.
    pub(crate) fn join_within(self, grace: Duration) -> ReaderJoin {
        let Some(handle) = self.handle else {
            return ReaderJoin::Absent;
        };
        let deadline = Instant::now() + grace;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    grace_ms = grace.as_millis() as u64,
                    "input reader thread still running at teardown; detaching"
                );
                drop(handle);
                return ReaderJoin::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if handle.join().is_err() {
            tracing::warn!("input reader thread panicked");
        }
        ReaderJoin::Joined
    }
}

#[cfg(test)]
#[path = "reader_thread_tests.rs"]
mod tests;
