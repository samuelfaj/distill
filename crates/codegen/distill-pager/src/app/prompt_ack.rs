//! Bounded wait for the agent's first acknowledgment of a sent prompt.
//!
//! A `session/prompt` RPC has no deadline of its own, so this bounds only the *acknowledgment*: the first
//! `x.ai/queue/changed`, `session/update`, or turn end that names the prompt id. Shared by the TUI reconcile
//! (`dispatch::reconcile_overdue_prompt_acks`) and the headless runner.
//!
//! Invariant: a [`PromptAckWatch`] exists on an agent only while `current_prompt_id == watch.prompt_id`
//! and the pane is `TurnRunning` or `TurnCancelling`; every turn-end path clears it.

use std::time::{Duration, Instant};

use serde::Serialize;

const PROMPT_ACK_TIMEOUT_ENV: &str = "GROK_PROMPT_ACK_TIMEOUT_SECS";
/// Status-line notice ("waiting for the agent to accept…") before the hard deadline.
pub(crate) const PROMPT_ACK_SOFT_NOTICE: Duration = Duration::from_secs(10);
/// Sized above the shell's first-prompt worst case, which acknowledges only after its whole
/// preamble: the templated delivery-tools prefix wait (60 s + 10 s), a settings refresh (up to
/// 16.5 s), an auth refresh, and two actor round trips. Resumed sessions acknowledge in under a second.
pub(crate) const DEFAULT_PROMPT_ACK_TIMEOUT: Duration = Duration::from_secs(120);
/// Clamp floor; the watch can be shortened but never disabled.
pub(crate) const MIN_PROMPT_ACK_TIMEOUT_SECS: u64 = 5;
/// Clamp ceiling; keeps `armed_at + hard` from overflowing `Instant` on absurd input.
pub(crate) const MAX_PROMPT_ACK_TIMEOUT_SECS: u64 = 3600;

/// Resolved soft/hard deadlines, measured from the arm instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromptAckDeadlines {
    pub(crate) soft: Duration,
    pub(crate) hard: Duration,
}

impl PromptAckDeadlines {
    /// Read once per entry point; not cached, so tests and forks see their own environment.
    pub(crate) fn from_process_env() -> Self {
        Self::from_env(std::env::var(PROMPT_ACK_TIMEOUT_ENV).ok().as_deref())
    }

    /// `None`, `0`, or an unparsable value keeps the default; anything else is clamped into the bounds.
    pub(crate) fn from_env(env: Option<&str>) -> Self {
        let hard = match env.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
            None | Some(0) => DEFAULT_PROMPT_ACK_TIMEOUT,
            Some(secs) => Duration::from_secs(
                secs.clamp(MIN_PROMPT_ACK_TIMEOUT_SECS, MAX_PROMPT_ACK_TIMEOUT_SECS),
            ),
        };
        PromptAckDeadlines {
            soft: PROMPT_ACK_SOFT_NOTICE.min(hard / 2),
            hard,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckStage {
    Armed,
    SoftNoticed,
}

/// One sent prompt awaiting its first acknowledgment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptAckWatch {
    prompt_id: String,
    armed_at: Instant,
    stage: AckStage,
}

/// What the reconcile must do after a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptAckOutcome {
    Waiting,
    SoftNotice { waited: Duration },
    Expired { waited: Duration },
}

/// Which acknowledgment disarmed a watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AckSignal {
    QueueChanged,
    SessionUpdate,
    TurnEnded,
}

impl PromptAckWatch {
    pub(crate) fn new(prompt_id: impl Into<String>, now: Instant) -> Self {
        PromptAckWatch {
            prompt_id: prompt_id.into(),
            armed_at: now,
            stage: AckStage::Armed,
        }
    }

    pub(crate) fn prompt_id(&self) -> &str {
        &self.prompt_id
    }

    pub(crate) fn is_soft_noticed(&self) -> bool {
        self.stage == AckStage::SoftNoticed
    }

    pub(crate) fn waited(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.armed_at)
    }

    pub(crate) fn hard_deadline(&self, deadlines: &PromptAckDeadlines) -> Instant {
        self.armed_at + deadlines.hard
    }

    /// Advances the stage; the soft notice fires once, expiry is reported on every poll past the hard deadline.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        deadlines: &PromptAckDeadlines,
    ) -> PromptAckOutcome {
        let waited = self.waited(now);
        if waited >= deadlines.hard {
            return PromptAckOutcome::Expired { waited };
        }
        if waited >= deadlines.soft && self.stage == AckStage::Armed {
            self.stage = AckStage::SoftNoticed;
            return PromptAckOutcome::SoftNotice { waited };
        }
        PromptAckOutcome::Waiting
    }
}

/// Whether a `x.ai/queue/changed` payload proves the shell holds `prompt_id` (queued or running).
pub(crate) fn queue_changed_acks(
    changed: &crate::app::prompt_queue::QueueChanged,
    prompt_id: &str,
) -> bool {
    changed.running_prompt_id.as_deref() == Some(prompt_id)
        || changed.entries.iter().any(|entry| entry.id == prompt_id)
}

#[cfg(test)]
#[path = "prompt_ack_tests.rs"]
mod tests;
