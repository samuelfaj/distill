//! Lane order for parent messages delivered at a safe point, and the wake signal that lets a
//! pending Interject abort a blocking wait tool.
//!
//! Ordering guarantee: the delivered batch is every Pending slot bound to the running turn;
//! Interject messages precede Steer messages; each class keeps admission order; a message
//! admitted after a safe point is delivered at a later one.
//!
//! Signal contract: `ParentInterjectSignal` is written only by `MessageDeliveryState` under the
//! `State` lock and read by spawned tool futures; `note_wait_aborted` is the one tool-side write
//! and carries the aborting turn's epoch, so a late note from a finished turn never counts for a
//! later turn's drain.

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::parent_message::ParentDeliveryMessage;
use crate::session::TurnEpoch;

pub(super) fn order_for_delivery(
    messages: &[ParentDeliveryMessage],
) -> impl Iterator<Item = &ParentDeliveryMessage> {
    let interjects = messages
        .iter()
        .filter(|message| message.content().is_interject());
    let steers = messages
        .iter()
        .filter(|message| !message.content().is_interject());
    interjects.chain(steers)
}

/// Read-only projection of the lifecycle: whether an Interject slot is Pending for the running
/// turn, plus the turn whose wait tool aborted for one and has not been drained yet.
///
/// `has_pending` is a pure flag: no other memory is published through it, so `Relaxed` suffices.
#[derive(Default)]
pub(super) struct ParentInterjectSignal {
    has_pending: AtomicBool,
    wait_aborted_turn: Mutex<Option<TurnEpoch>>,
}

impl ParentInterjectSignal {
    pub(super) fn is_pending(&self) -> bool {
        self.has_pending.load(Ordering::Relaxed)
    }

    pub(super) fn set_pending(&self, has_pending: bool) {
        self.has_pending.store(has_pending, Ordering::Relaxed);
    }

    pub(super) fn note_wait_aborted(&self, turn: TurnEpoch) {
        let mut marked = self.wait_aborted_turn.lock();
        // A late note from a finished turn must not overwrite a live turn's mark.
        if marked.is_none_or(|earlier| earlier <= turn) {
            *marked = Some(turn);
        }
    }

    /// Consumes the mark; it counts only when it was noted for `turn`.
    pub(super) fn take_wait_aborted(&self, turn: TurnEpoch) -> bool {
        self.wait_aborted_turn.lock().take() == Some(turn)
    }

    pub(super) fn clear_wait_aborted(&self) {
        *self.wait_aborted_turn.lock() = None;
    }
}

#[cfg(test)]
#[path = "parent_interject_tests.rs"]
mod tests;
