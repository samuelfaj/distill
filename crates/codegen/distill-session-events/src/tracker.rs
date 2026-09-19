use std::cell::{Cell, RefCell};
use std::path::Path;
use std::time::Instant;

use crate::log::EventWriter;
use crate::types::{CancellationCategory, Event, RedirectKind, TurnOutcomeLabel};

/// The tool call currently running, kept so a cancel can report it.
/// `dispatch_duration_ms` was measured when the call was dispatched, so a cancel reports that instead of timing the call itself.
#[derive(Debug, Clone)]
struct ActiveTool {
    tool_name: String,
    tool_call_id: String,
    dispatch_duration_ms: u64,
}

/// One session's event state. It is `!Send` and lives on the session actor.
/// Background tasks emit through the `Clone + Send + Sync` handle from `writer()`.
pub struct EventTracker {
    writer: EventWriter,
    turn_ended_emitted: Cell<bool>,
    active_tool: RefCell<Option<ActiveTool>>,
    turn_tool_count: Cell<u32>,
    /// The next real user prompt consumes it once, via `take_prior_interrupt_category`, to tag
    /// `UserItem::prior_turn_interrupt`. `begin_turn` must not clear it: it has to survive into that next turn. An
    /// interjection does not cancel the turn, so it is never recorded here (see `Event::Interjected`).
    prior_interrupt_category: Cell<Option<CancellationCategory>>,
    /// `CancelThenSend` means nothing was queued; `QueuedAfterCancel` means a prompt sat queued behind the aborted turn.
    /// `cancel_running_task` sets it, and the next user `turn_started` consumes it into `Event::TurnStarted::redirect_kind`.
    /// Like `prior_interrupt_category` it survives `begin_turn` so it reaches that next turn.
    prior_redirect_kind: Cell<Option<RedirectKind>>,
    /// Set by the cancel path only when a turn was aborted mid-stream with no tool in flight. Nothing else then tells the
    /// model it was interrupted: no dangling tool call gets repaired and no permission tool result is written. The next real
    /// user prompt consumes it and frames that query with the interrupt envelope.
    pending_interrupt_reminder: Cell<bool>,
}

impl std::fmt::Debug for EventTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active_tool = self.active_tool.borrow();
        f.debug_struct("EventTracker")
            .field("writer", &self.writer)
            .field("turn_ended_emitted", &self.turn_ended_emitted.get())
            .field("turn_tool_count", &self.turn_tool_count.get())
            .field("active_tool", &*active_tool)
            .field(
                "prior_interrupt_category",
                &self.prior_interrupt_category.get(),
            )
            .field("prior_redirect_kind", &self.prior_redirect_kind.get())
            .field(
                "pending_interrupt_reminder",
                &self.pending_interrupt_reminder.get(),
            )
            .finish()
    }
}

impl EventTracker {
    pub fn new(session_dir: &Path) -> Self {
        Self {
            writer: EventWriter::open(session_dir),
            turn_ended_emitted: Cell::new(false),
            active_tool: RefCell::new(None),
            turn_tool_count: Cell::new(0),
            prior_interrupt_category: Cell::new(None),
            prior_redirect_kind: Cell::new(None),
            pending_interrupt_reminder: Cell::new(false),
        }
    }

    /// Clone the writer for background tasks.
    pub fn writer(&self) -> EventWriter {
        self.writer.clone()
    }

    pub fn emit(&self, event: Event) {
        self.writer.emit(event);
    }

    pub fn begin_turn(&self) {
        self.turn_ended_emitted.set(false);
        self.turn_tool_count.set(0);
    }

    /// Emits `turn_ended` at most once per turn; later calls do nothing.
    pub fn emit_turn_ended(
        &self,
        outcome: TurnOutcomeLabel,
        category: Option<CancellationCategory>,
        context: Option<serde_json::Value>,
    ) {
        if self.turn_ended_emitted.replace(true) {
            return;
        }
        self.emit(Event::TurnEnded {
            outcome,
            cancellation_category: category,
            cancellation_context: context,
        });
    }

    /// Marks a tool as active so a cancel can report it.
    /// `dispatch_duration_ms` was measured when the call was dispatched, so a cancel reports it instead of timing the call again.
    pub fn tool_started(&self, tool_name: String, tool_call_id: String, dispatch_duration_ms: u64) {
        let is_new = self.active_tool.borrow().is_none();
        *self.active_tool.borrow_mut() = Some(ActiveTool {
            tool_name,
            tool_call_id,
            dispatch_duration_ms,
        });
        // A repeat call for the same tool (e.g. after a reauth retry) only refreshes the duration.
        if is_new {
            self.turn_tool_count.set(self.turn_tool_count.get() + 1);
        }
    }

    pub fn tool_count_this_turn(&self) -> u32 {
        self.turn_tool_count.get()
    }

    pub fn has_active_tool(&self) -> bool {
        self.active_tool.borrow().is_some()
    }

    pub fn tool_finished(&self) {
        *self.active_tool.borrow_mut() = None;
    }

    /// Cancels the in-flight tool and emits `ToolCompleted(cancelled)`.
    /// `cancel_running_task()` calls this before `turn_ended`.
    /// A tool cancelled while still dispatching was never marked active, so it gets no `tool_completed` row.
    pub fn cancel_active_tool(&self) {
        if let Some(tool) = self.active_tool.borrow_mut().take() {
            self.emit(Event::ToolCompleted {
                tool_name: tool.tool_name,
                duration_ms: tool.dispatch_duration_ms,
                outcome: crate::types::ToolOutcome::Cancelled,
                tool_call_id: tool.tool_call_id,
                source: crate::types::ToolCompletedSource::Shell,
                rewriting_hook: None,
            });
        }
    }

    /// Records why a user interrupt cancelled this turn, for tagging the next real user prompt.
    /// A later cause overwrites an earlier one.
    pub fn set_prior_interrupt_category(&self, category: CancellationCategory) {
        self.prior_interrupt_category.set(Some(category));
    }

    /// Take (and clear) the recorded prior-turn interrupt cause.
    pub fn take_prior_interrupt_category(&self) -> Option<CancellationCategory> {
        self.prior_interrupt_category.take()
    }

    /// Records how the turn after a mid-turn abort starts (`CancelThenSend` or `QueuedAfterCancel`).
    /// A later abort overwrites an earlier one.
    pub fn set_prior_redirect_kind(&self, kind: RedirectKind) {
        self.prior_redirect_kind.set(Some(kind));
    }

    /// Take (and clear) the recorded prior-turn redirect kind.
    pub fn take_prior_redirect_kind(&self) -> Option<RedirectKind> {
        self.prior_redirect_kind.take()
    }

    /// Turns on the one-shot interrupt envelope for the next real user prompt.
    /// Only the cancel path sets it, and only when no tool was in flight, because the model would otherwise never learn it was interrupted.
    pub fn set_pending_interrupt_reminder(&self) {
        self.pending_interrupt_reminder.set(true);
    }

    /// Take (and clear) the pending interrupt-envelope flag.
    pub fn take_pending_interrupt_reminder(&self) -> bool {
        self.pending_interrupt_reminder.replace(false)
    }

    /// Emits `PhaseChanged(PermissionPrompt)` and then `PermissionRequested`.
    /// Returns the `Instant` that `permission_resolved()` uses to compute `wait_ms`.
    pub fn permission_requested(&self, tool_name: &str) -> Instant {
        self.emit(Event::PhaseChanged {
            phase: crate::types::Phase::PermissionPrompt,
        });
        self.emit(Event::PermissionRequested {
            tool_name: tool_name.to_string(),
        });
        Instant::now()
    }

    pub fn permission_resolved(
        &self,
        tool_name: &str,
        decision: crate::types::PermissionDecision,
        start: Instant,
    ) {
        self.emit(Event::PermissionResolved {
            tool_name: tool_name.to_string(),
            decision,
            wait_ms: start.elapsed().as_millis() as u64,
        });
        self.emit(Event::PhaseChanged {
            phase: crate::types::Phase::ToolExecution,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_interrupt_markers_are_one_shot_and_survive_begin_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let t = EventTracker::new(dir.path());

        // Defaults: nothing recorded.
        assert_eq!(t.take_prior_interrupt_category(), None);
        assert!(t.take_prior_redirect_kind().is_none());
        assert!(!t.take_pending_interrupt_reminder());

        // Cancel cause is consumed exactly once.
        t.set_prior_interrupt_category(CancellationCategory::MidTurnAbort);
        assert_eq!(
            t.take_prior_interrupt_category(),
            Some(CancellationCategory::MidTurnAbort)
        );
        assert_eq!(t.take_prior_interrupt_category(), None);

        // Interrupt-reminder flag is consumed exactly once.
        t.set_pending_interrupt_reminder();
        assert!(t.take_pending_interrupt_reminder());
        assert!(!t.take_pending_interrupt_reminder());

        // Redirect kind is consumed exactly once.
        t.set_prior_redirect_kind(RedirectKind::QueuedAfterCancel);
        assert!(matches!(
            t.take_prior_redirect_kind(),
            Some(RedirectKind::QueuedAfterCancel)
        ));
        assert!(t.take_prior_redirect_kind().is_none());

        // `begin_turn` runs at the start of a turn, before the next real user prompt consumes the markers
        // It must not clear these cross-turn markers; it only resets the per-turn counters
        // A regression here would silently drop the `prior_turn_interrupt` tag and the `redirect_kind`
        t.set_prior_interrupt_category(CancellationCategory::PermissionRejected);
        t.set_prior_redirect_kind(RedirectKind::CancelThenSend);
        t.set_pending_interrupt_reminder();
        t.begin_turn();
        assert_eq!(
            t.take_prior_interrupt_category(),
            Some(CancellationCategory::PermissionRejected),
            "begin_turn must preserve the cross-turn interrupt cause"
        );
        assert!(
            matches!(
                t.take_prior_redirect_kind(),
                Some(RedirectKind::CancelThenSend)
            ),
            "begin_turn must preserve the cross-turn redirect kind"
        );
        assert!(
            t.take_pending_interrupt_reminder(),
            "begin_turn must preserve the pending interrupt reminder"
        );
    }
}
