//! Teardown fence after the kitty keyboard pop: a DA1 query whose reply proves the terminal has applied the pop.
//!
//! A terminal handles its output in order, so every key report it emitted before applying `CSI < u` is already in stdin
//! when the `CSI ? … c` reply arrives; draining up to the reply consumes release events that would otherwise reach the
//! user's shell as keystrokes (fish reads `ESC [ 100 ; 5 : 3 u` as Ctrl-D and exits). Reads use the raw fd because
//! crossterm filters DA1 replies out of its event stream.
//!
//! Caller-enforced preconditions: raw mode on, no other stdin reader alive, flags pushed on this screen. The fence never pops.

use std::time::Duration;

#[cfg(unix)]
const QUERY: &[u8] = b"\x1b[c";

/// DA2 replies (`CSI >`) and key reports lack the private marker.
#[cfg(any(unix, test))]
const DA1_REPLY_INTRO: &[u8] = b"\x1b[?";

/// Why the fence stopped. `Replied` is the only outcome that proves the terminal applied the pop.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PopFenceOutcome {
    Replied,
    /// Also a read error; both are bounded.
    TimedOut,
    /// No key-event source, or the query did not reach the terminal inside the deadline; nothing was read.
    QueryFailed,
    Unsupported,
}

/// `residue_bytes` is what stdin held ahead of the reply (key reports, typeahead); non-zero means a leak was prevented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PopFence {
    pub outcome: PopFenceOutcome,
    pub residue_bytes: usize,
    pub elapsed: Duration,
}

/// Write `CSI c` on the render fd (the fd and lock the pop used) and drain the key-event source until a complete DA1 reply.
/// One deadline bounds the lock, the write and the read, so a terminal that stopped reading cannot hold the quit.
#[cfg(unix)]
pub fn run(timeout: Duration) -> PopFence {
    use crate::terminal::probe::{DrainEnd, drain_tty_until, tty_input_fd, write_query_until};

    let started = std::time::Instant::now();
    let deadline = started + timeout;
    let query_failed = || PopFence {
        outcome: PopFenceOutcome::QueryFailed,
        residue_bytes: 0,
        elapsed: started.elapsed(),
    };
    // Resolved first: a reply nobody can read for must never be requested
    let Some(input) = tty_input_fd() else {
        return query_failed();
    };
    if !write_query_until(QUERY, deadline) {
        return query_failed();
    }
    let stats = drain_tty_until(input.as_raw_fd(), deadline, da1_reply_len);
    let (outcome, residue_bytes) = match stats.end {
        DrainEnd::Terminated => (
            PopFenceOutcome::Replied,
            stats.bytes.saturating_sub(stats.tail_len),
        ),
        DrainEnd::Deadline | DrainEnd::Error => (PopFenceOutcome::TimedOut, stats.bytes),
    };
    PopFence {
        outcome,
        residue_bytes,
        elapsed: started.elapsed(),
    }
}

#[cfg(not(unix))]
pub fn run(_timeout: Duration) -> PopFence {
    PopFence {
        outcome: PopFenceOutcome::Unsupported,
        residue_bytes: 0,
        elapsed: Duration::ZERO,
    }
}

/// `Some(len)` iff `tail` ends with a complete DA1 reply `ESC [ ? [0-9;]* c`, `len` being that reply's byte length.
/// DA2, kitty flag reports (`… u`) and CSI-u key events never match, so a key report cannot end the drain; kitty's trailing
/// `;` (`ESC [ ? 6 2 ; c`) is admitted by the parameter class.
#[cfg(any(unix, test))]
fn da1_reply_len(tail: &[u8]) -> Option<usize> {
    let (final_byte, before_final) = tail.split_last()?;
    if *final_byte != b'c' {
        return None;
    }
    let params_len = before_final
        .iter()
        .rev()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
        .count();
    let intro = before_final.get(..before_final.len() - params_len)?;
    intro
        .ends_with(DA1_REPLY_INTRO)
        .then_some(DA1_REPLY_INTRO.len() + params_len + 1)
}

#[cfg(test)]
#[path = "pop_fence_tests.rs"]
mod tests;
