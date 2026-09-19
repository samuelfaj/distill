//! Decides whether teardown runs the kitty pop fence and records what happened.
//!
//! The fence needs the key-event source to itself and a terminal still reading output; every other case, including a
//! fence whose query never got out, gets the legacy crossterm drain instead.

use std::time::Duration;

use crate::app::event_loop;
use crate::app::reader_thread::ReaderJoin;
use crate::terminal::{PopFence, PopFenceOutcome, pop_fence};

/// neovim's field-tested bound; paid only when a terminal that answered DA1 at startup has since gone silent.
pub(crate) const POP_FENCE_TIMEOUT: Duration = Duration::from_secs(1);

/// Quiet window of the legacy drain, which also catches late probe replies and focus/mouse reports.
const LEGACY_DRAIN_QUIET: Duration = Duration::from_millis(10);

pub(crate) struct TeardownFence {
    pub(crate) reader: ReaderJoin,
    pub(crate) writer_timed_out: bool,
    pub(crate) flags_pushed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum FenceDecision {
    Run,
    /// A live crossterm reader could steal the reply.
    SkipReaderAlive,
    /// The terminal stopped reading output and would not answer.
    SkipWriterWedged,
    SkipNoFlags,
}

impl TeardownFence {
    /// A wedged writer outranks a live reader: nothing would answer even with stdin free.
    pub(crate) fn decide(&self) -> FenceDecision {
        if self.writer_timed_out {
            return FenceDecision::SkipWriterWedged;
        }
        if self.reader == ReaderJoin::TimedOut {
            return FenceDecision::SkipReaderAlive;
        }
        if !self.flags_pushed {
            return FenceDecision::SkipNoFlags;
        }
        FenceDecision::Run
    }

    pub(crate) fn run(self) -> TeardownFenceReport {
        use PopFenceOutcome::{QueryFailed, Unsupported};

        let decision = self.decide();
        let fence = match decision {
            FenceDecision::Run => Some(pop_fence::run(POP_FENCE_TIMEOUT)),
            FenceDecision::SkipReaderAlive
            | FenceDecision::SkipWriterWedged
            | FenceDecision::SkipNoFlags => None,
        };
        // A fence that never read still owes the residue the legacy drain
        if fence.is_none_or(|pop| matches!(pop.outcome, Unsupported | QueryFailed)) {
            let _ = event_loop::drain_pending_events(LEGACY_DRAIN_QUIET, |_| false);
        }
        TeardownFenceReport {
            decision,
            fence,
            reader: self.reader,
        }
    }
}

pub(crate) struct TeardownFenceReport {
    pub(crate) decision: FenceDecision,
    pub(crate) fence: Option<PopFence>,
    pub(crate) reader: ReaderJoin,
}

impl TeardownFenceReport {
    /// Direct write: after a failed startup the ACP forwarder has no sender, so a buffered entry would never leave the process.
    pub(crate) fn record(&self) {
        let duration_ms = self
            .fence
            .map(|fence| u64::try_from(fence.elapsed.as_millis()).unwrap_or(u64::MAX));
        crate::unified_log::write_direct_info(
            "teardown.kitty_pop_fence",
            Some(serde_json::json!({
                "decision": <&'static str>::from(self.decision),
                "outcome": self.fence.map(|fence| <&'static str>::from(fence.outcome)),
                "residue_bytes": self.fence.map(|fence| fence.residue_bytes),
                "duration_ms": duration_ms,
                "reader_join": <&'static str>::from(self.reader),
            })),
        );
    }
}

#[cfg(test)]
#[path = "teardown_fence_tests.rs"]
mod tests;
