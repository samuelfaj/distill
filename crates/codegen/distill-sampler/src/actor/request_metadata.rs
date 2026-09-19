// Modified for Distill by Samuel Fajreldines, 2026.
//! Bounded detector metadata collected across one sampling request.

use tokio::sync::oneshot;

use distill_sampling_types::{ConversationResponse, SamplingError};

use crate::doom_loop::{MAX_COLLECTED_DOOM_LOOP_SIGNALS, MAX_DOOM_LOOP_SIGNAL_BYTES};
use crate::handle::{CollectedSamplingResult, DoomLoopRecoveryAttempt};
use crate::metrics::InferenceLatencyStats;
const MAX_DOOM_LOOP_RECOVERY_ATTEMPTS: usize = 16;

pub(crate) type SamplingResultWithMetrics =
    Result<(ConversationResponse, InferenceLatencyStats), SamplingError>;

pub(crate) fn merge_signal_labels(
    target: &mut Vec<String>,
    signals: impl IntoIterator<Item = impl AsRef<str>>,
) {
    for signal in signals {
        if target.len() >= MAX_COLLECTED_DOOM_LOOP_SIGNALS {
            break;
        }
        let signal = signal.as_ref();
        if signal.len() <= MAX_DOOM_LOOP_SIGNAL_BYTES
            && !target.iter().any(|existing| existing == signal)
        {
            target.push(signal.to_owned());
        }
    }
}

pub(crate) struct CompletionState {
    tx: Option<oneshot::Sender<CollectedSamplingResult>>,
    doom_loop_signals: Vec<String>,
    doom_loop_recovery_attempts: Vec<DoomLoopRecoveryAttempt>,
}

impl CompletionState {
    pub(crate) fn new(tx: Option<oneshot::Sender<CollectedSamplingResult>>) -> Self {
        Self {
            tx,
            doom_loop_signals: Vec::new(),
            doom_loop_recovery_attempts: Vec::new(),
        }
    }

    pub(crate) fn merge_doom_loop_signals(&mut self, signals: impl IntoIterator<Item = String>) {
        merge_signal_labels(&mut self.doom_loop_signals, signals);
    }

    pub(crate) fn record_recovery_attempt(
        &mut self,
        triggers: impl IntoIterator<Item = String>,
        aborted_at_chunk: Option<u64>,
    ) {
        if self.doom_loop_recovery_attempts.len() >= MAX_DOOM_LOOP_RECOVERY_ATTEMPTS {
            return;
        }
        let mut bounded = Vec::new();
        merge_signal_labels(&mut bounded, triggers);
        self.doom_loop_recovery_attempts
            .push(DoomLoopRecoveryAttempt {
                triggers: bounded,
                aborted_at_chunk,
            });
    }

    pub(crate) fn send(&mut self, result: SamplingResultWithMetrics, terminal_event_queued: bool) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(CollectedSamplingResult {
                result,
                terminal_event_queued,
                doom_loop_signals: std::mem::take(&mut self.doom_loop_signals),
                doom_loop_recovery_attempts: std::mem::take(&mut self.doom_loop_recovery_attempts),
            });
        }
    }
}

#[cfg(test)]
#[path = "request_metadata_tests.rs"]
mod tests;
