//! Session-close timing: a closed table of `session_end.*` spans plus a per-session [`SessionEndTimer`] that feeds the `SessionEndTimings` event.
//!
//! Leaf phases carry a duration field.
//! Wrapper phases (`Memory`, `Hooks`, `Workflows`, `Feedback`, `SessionFlush`) are span-only aggregates of their leaves.
//! Summing every `*_ms` therefore double-counts.
//! Adding a leaf [`Phase`] wires it to a field in [`phase_event_slot`] (exhaustive) or it stays span-only.
//!
//! The timer also records `total_ms`, the wall-clock from its creation (session end starts) to emit.
//! So `total_ms - sum(leaves)` is time spent between the measured steps.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::region::Region;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    SessionFlush,
    Workflows,
    WorkflowsDrain,
    WorkflowsPersist,
    Hooks,
    HooksDispatch,
    HooksStop,
    Memory,
    MemorySave,
    /// Intentionally unpopulated; retained because downstream metrics consumers still read this field.
    MemoryConsolidate,
    Feedback,
    FeedbackDrain,
    BackgroundTasksSave,
}

crate::startup::span_table!(fn phase_span, fn phase_span_under(Phase) {
    SessionFlush => "session_end.session_flush",
    Workflows => "session_end.workflows",
    WorkflowsDrain => "session_end.workflows_drain",
    WorkflowsPersist => "session_end.workflows_persist",
    Hooks => "session_end.hooks",
    HooksDispatch => "session_end.hooks_dispatch",
    HooksStop => "session_end.hooks_stop",
    Memory => "session_end.memory",
    MemorySave => "session_end.memory_save",
    MemoryConsolidate => "session_end.memory_consolidate",
    Feedback => "session_end.feedback",
    FeedbackDrain => "session_end.feedback_drain",
    BackgroundTasksSave => "session_end.background_tasks_save",
});

pub fn span(phase: Phase) -> Region {
    Region::from_span(phase_span(phase))
}

pub fn child(phase: Phase, parent: &tracing::Span) -> Region {
    Region::from_span(phase_span_under(phase, parent))
}

pub fn join_span() -> Region {
    Region::from_span(tracing::info_span!(
        parent: None,
        "session_end.worker_join",
        outcome = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        notice_shown = tracing::field::Empty,
    ))
}

pub fn record_join(span: &Region, outcome: &str, elapsed_ms: u64, notice_shown: bool) {
    let s = span.span();
    s.record("outcome", outcome);
    s.record("elapsed_ms", elapsed_ms);
    s.record("notice_shown", notice_shown);
}

#[derive(Debug)]
pub struct SessionEndTimer {
    start: Instant,
    phases: Mutex<Vec<(Phase, u64)>>,
}

impl Default for SessionEndTimer {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            phases: Mutex::default(),
        }
    }
}

pub type SharedSessionEndTimer = Arc<SessionEndTimer>;

impl SessionEndTimer {
    pub fn new_shared() -> SharedSessionEndTimer {
        Arc::new(Self::default())
    }

    /// Wall-clock since the timer was created (session-end teardown start).
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn record(&self, phase: Phase, elapsed: Duration) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let mut phases = self
            .phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = phases.iter_mut().find(|(p, _)| *p == phase) {
            slot.1 = ms;
        } else {
            phases.push((phase, ms));
        }
    }

    pub fn write_event_phases(&self, event: &mut crate::events::SessionEndTimings) {
        for (phase, ms) in self
            .phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            if let Some(slot) = phase_event_slot(event, *phase) {
                *slot = Some(*ms);
            }
        }
    }
}

fn phase_event_slot(
    event: &mut crate::events::SessionEndTimings,
    phase: Phase,
) -> Option<&mut Option<u64>> {
    match phase {
        Phase::MemorySave => Some(&mut event.memory_save_ms),
        Phase::MemoryConsolidate => Some(&mut event.memory_consolidate_ms),
        Phase::HooksDispatch => Some(&mut event.hooks_dispatch_ms),
        Phase::HooksStop => Some(&mut event.hooks_stop_ms),
        Phase::WorkflowsDrain => Some(&mut event.workflows_drain_ms),
        Phase::WorkflowsPersist => Some(&mut event.workflows_persist_ms),
        Phase::FeedbackDrain => Some(&mut event.feedback_drain_ms),
        Phase::BackgroundTasksSave => Some(&mut event.background_tasks_save_ms),
        Phase::SessionFlush | Phase::Workflows | Phase::Hooks | Phase::Memory | Phase::Feedback => {
            None
        }
    }
}

#[must_use = "bind the guard for the phase's scope; dropping it immediately records ~0ms"]
pub struct TimedPhase {
    region: Region,
    timer: SharedSessionEndTimer,
    phase: Phase,
    start: Instant,
    done: bool,
}

impl TimedPhase {
    pub fn span(&self) -> &tracing::Span {
        self.region.span()
    }

    pub fn close(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if !self.done {
            self.timer.record(self.phase, self.start.elapsed());
            self.done = true;
        }
    }
}

impl Drop for TimedPhase {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn timed_child(
    timer: &SharedSessionEndTimer,
    phase: Phase,
    parent: &tracing::Span,
) -> TimedPhase {
    TimedPhase {
        region: child(phase, parent),
        timer: Arc::clone(timer),
        phase,
        start: Instant::now(),
        done: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_event_phases_maps_every_leaf_and_skips_wrappers() {
        let timer = SessionEndTimer::default();
        // Distinct value per leaf so a mis-wired slot (HooksStop writing hooks_dispatch_ms) fails
        timer.record(Phase::MemorySave, Duration::from_millis(1));
        timer.record(Phase::MemoryConsolidate, Duration::from_millis(2));
        timer.record(Phase::HooksDispatch, Duration::from_millis(3));
        timer.record(Phase::HooksStop, Duration::from_millis(4));
        timer.record(Phase::WorkflowsDrain, Duration::from_millis(5));
        timer.record(Phase::WorkflowsPersist, Duration::from_millis(6));
        timer.record(Phase::FeedbackDrain, Duration::from_millis(7));
        timer.record(Phase::BackgroundTasksSave, Duration::from_millis(8));
        // Wrappers are span-only: recording one must not populate any field.
        timer.record(Phase::Memory, Duration::from_millis(99));
        timer.record(Phase::SessionFlush, Duration::from_millis(99));
        let mut event = crate::events::SessionEndTimings::default();
        timer.write_event_phases(&mut event);
        assert_eq!(event.memory_save_ms, Some(1));
        assert_eq!(event.memory_consolidate_ms, Some(2));
        assert_eq!(event.hooks_dispatch_ms, Some(3));
        assert_eq!(event.hooks_stop_ms, Some(4));
        assert_eq!(event.workflows_drain_ms, Some(5));
        assert_eq!(event.workflows_persist_ms, Some(6));
        assert_eq!(event.feedback_drain_ms, Some(7));
        assert_eq!(event.background_tasks_save_ms, Some(8));
    }
}
