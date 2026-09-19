// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-turn aggregation for doom-loop detection and recovery telemetry.

use std::collections::{HashMap, HashSet};

const MAX_TRIGGER_LABELS: usize = 64;
const MAX_TRIGGER_LABEL_BYTES: usize = 256;

/// Fold `new` trigger labels into `current`, keeping the tightest (lowest-threshold) raw label overall.
pub(crate) fn merge_tightest_trigger(current: Option<String>, new: &[String]) -> Option<String> {
    distill_sampling_types::doom_loop::DoomLoopSignal::tightest(
        current
            .iter()
            .map(String::as_str)
            .chain(new.iter().map(String::as_str)),
    )
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DoomLoopDetectionSummary {
    pub(crate) detector_kinds: Vec<String>,
    pub(crate) channels: Vec<String>,
    pub(crate) tightest_tail_threshold: Option<u32>,
    pub(crate) max_exact_sequence_tokens: Option<u32>,
    pub(crate) max_exact_repeat_count: Option<u32>,
}

/// Telemetry-only detector and recovery tally. It never influences recovery.
#[derive(Debug, Default, Clone)]
pub(crate) struct DoomLoopTurnTally {
    /// Unique raw trigger labels observed this turn, in first-seen order.
    pub(crate) triggers: Vec<String>,
    /// Resamples this turn (doomed attempts discarded).
    pub(crate) attempts: u32,
    /// Whether any response this turn was accepted with confident signals.
    pub(crate) accepted_after_budget: bool,
    /// Tightest raw trigger label observed this turn.
    pub(crate) top_trigger: Option<String>,
    recovery_attempts_by_request: HashMap<String, u32>,
    counted_recovery_attempts: HashSet<(String, u32)>,
    stamped_recovery_attempts: HashSet<(String, u32)>,
    accepted_requests: HashSet<String>,
    stamped_accepted_requests: HashSet<String>,
}

pub(crate) fn reconcile_request_metadata(
    tally: &mut DoomLoopTurnTally,
    request_owned: bool,
    request_id: &str,
    signals: &[String],
    attempts: &[distill_sampler::DoomLoopRecoveryAttempt],
) -> Vec<(Vec<String>, Option<u64>)> {
    if !request_owned {
        return Vec::new();
    }
    tally.merge_all_triggers(signals);
    attempts
        .iter()
        .enumerate()
        .filter_map(|(index, attempt)| {
            let attempt_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            tally
                .record_recovery_attempt(request_id, attempt_number, &attempt.triggers)
                .then_some((attempt.triggers.clone(), attempt.aborted_at_chunk))
        })
        .collect()
}

impl DoomLoopTurnTally {
    /// Fold raw detector labels into this turn's `triggers` list.
    pub(crate) fn merge_all_triggers(&mut self, triggers: &[String]) {
        for trigger in triggers {
            if self.triggers.len() >= MAX_TRIGGER_LABELS {
                break;
            }
            if trigger.len() <= MAX_TRIGGER_LABEL_BYTES && !self.triggers.contains(trigger) {
                self.triggers.push(trigger.clone());
            }
        }
    }

    /// Fold recovery-action labels into both `triggers` and the legacy tightest `top_trigger` used by recovery telemetry.
    pub(crate) fn merge_recovery_triggers(&mut self, triggers: &[String]) {
        self.merge_all_triggers(triggers);
        let current = self.top_trigger.take();
        self.top_trigger = merge_tightest_trigger(current, triggers);
    }

    pub(crate) fn record_recovery_attempt(
        &mut self,
        request_id: &str,
        attempt: u32,
        triggers: &[String],
    ) -> bool {
        let key = (request_id.to_owned(), attempt);
        if !self.counted_recovery_attempts.insert(key) {
            return false;
        }
        let count = self
            .recovery_attempts_by_request
            .entry(request_id.to_owned())
            .or_default();
        *count = (*count).max(attempt);
        self.attempts = self
            .recovery_attempts_by_request
            .values()
            .copied()
            .sum::<u32>();
        self.merge_recovery_triggers(triggers);
        true
    }

    pub(crate) fn mark_recovery_attempt_stamped(&mut self, request_id: &str, attempt: u32) -> bool {
        self.stamped_recovery_attempts
            .insert((request_id.to_owned(), attempt))
    }

    pub(crate) fn recovery_attempt_count(&self, request_id: &str) -> u32 {
        self.recovery_attempts_by_request
            .get(request_id)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn mark_accepted_request(&mut self, request_id: &str) -> bool {
        let inserted = self.accepted_requests.insert(request_id.to_owned());
        self.accepted_after_budget |= inserted;
        inserted
    }

    pub(crate) fn mark_accepted_request_stamped(&mut self, request_id: &str) -> bool {
        self.stamped_accepted_requests.insert(request_id.to_owned())
    }

    pub(crate) fn detected(&self) -> bool {
        !self.triggers.is_empty()
    }

    pub(crate) fn detection_summary(&self) -> DoomLoopDetectionSummary {
        use distill_sampling_types::doom_loop::{DoomLoopSignal, DoomLoopSignalKind};

        let mut summary = DoomLoopDetectionSummary::default();
        for raw in &self.triggers {
            let signal = DoomLoopSignal::parse(raw);
            let channel = match signal.channel.as_str() {
                "thinking" => "thinking",
                "response" => "response",
                _ => "other",
            };
            if !summary.channels.iter().any(|existing| existing == channel) {
                summary.channels.push(channel.to_owned());
            }
            match signal.kind {
                DoomLoopSignalKind::TailRepetition(threshold) => {
                    if !summary
                        .detector_kinds
                        .iter()
                        .any(|kind| kind == "tail_repetition")
                    {
                        summary.detector_kinds.push("tail_repetition".to_owned());
                    }
                    summary.tightest_tail_threshold = Some(
                        summary
                            .tightest_tail_threshold
                            .map_or(threshold, |current| current.min(threshold)),
                    );
                }
                DoomLoopSignalKind::ExactRepetition {
                    sequence_tokens,
                    repeat_count,
                } => {
                    if !summary
                        .detector_kinds
                        .iter()
                        .any(|kind| kind == "exact_repetition")
                    {
                        summary.detector_kinds.push("exact_repetition".to_owned());
                    }
                    summary.max_exact_sequence_tokens = Some(
                        summary
                            .max_exact_sequence_tokens
                            .map_or(sequence_tokens, |current| current.max(sequence_tokens)),
                    );
                    summary.max_exact_repeat_count = Some(
                        summary
                            .max_exact_repeat_count
                            .map_or(repeat_count, |current| current.max(repeat_count)),
                    );
                }
                DoomLoopSignalKind::LowLogprob => {
                    if !summary
                        .detector_kinds
                        .iter()
                        .any(|kind| kind == "low_logprob")
                    {
                        summary.detector_kinds.push("low_logprob".to_owned());
                    }
                }
                DoomLoopSignalKind::Unknown(_) => {
                    if !summary.detector_kinds.iter().any(|kind| kind == "unknown") {
                        summary.detector_kinds.push("unknown".to_owned());
                    }
                }
            }
        }
        summary
    }

    /// True when recovery acted this turn.
    pub(crate) fn fired(&self) -> bool {
        self.attempts > 0 || self.accepted_after_budget
    }
}

#[cfg(test)]
#[path = "doom_loop_telemetry_tests.rs"]
mod tests;
