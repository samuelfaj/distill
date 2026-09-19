// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-turn retry policy for 401s after an auth recovery attempt: recovery succeeded (resubmit), or
//! failed transiently on a credential-less request — parked on the uncharged path to wait for a token.

use distill_sampling_types::SentCredential;
use tokio_retry::strategy::ExponentialBackoff;

use super::RecoveredStore;
use crate::util::dual_clock::DualClock;
use distill_login::AuthManager;

/// One blind wait inside [`pace_uncharged_resubmit`]: `notify_waiters` stores no permit and the
/// adoption paths never notify, so re-check wire-validity and poll auth.json every slice.
const PACE_WAIT_SLICE: std::time::Duration = std::time::Duration::from_secs(1);

/// Pace an uncharged resubmit: hold the escalating `delay`, releasing early only once a wire-valid
/// session token exists (the send is no longer doomed). Wakes without a token change re-arm.
pub(crate) async fn pace_uncharged_resubmit(
    store: RecoveredStore,
    auth_manager: Option<&AuthManager>,
    delay: std::time::Duration,
) {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    match (store, auth_manager) {
        // Only a Distill authority stamps the token on the wire: elsewhere an early
        // release would fire unpaced doomed sends straight into the runaway guard.
        (RecoveredStore::SessionToken, Some(am))
            if ActiveAuthBackend::default().is_xai_authority() =>
        {
            let started = tokio::time::Instant::now();
            loop {
                if am.current_wire_valid().is_some() {
                    break;
                }
                // Parked turns suppress the refresh dispatches that adopt disk, so
                // poll for a token another process wrote (`grok login` elsewhere).
                if am.pick_up_sibling_token() {
                    continue;
                }
                let remaining = delay.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                if am
                    .wait_for_token_refresh(remaining.min(PACE_WAIT_SLICE))
                    .await
                {
                    break;
                }
            }
        }
        _ => tokio::time::sleep(delay).await,
    }
}

/// Compact `2h3m` / `4m7s` / `12s` rendering for turn-failure messages.
pub(crate) fn human_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    if mins < 60 {
        return format!("{mins}m{}s", total_secs % 60);
    }
    format!("{}h{}m", mins / 60, mins % 60)
}

/// Decision for one post-recovery 401 (see [`AuthRetrySchedule::on_recovered_401`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthRetryDecision {
    /// No credential was on the wire, so no slot is charged: resubmit after the escalating
    /// `delay`; `resubmit` is the 1-indexed count since the last successful response.
    UnchargedResubmit {
        resubmit: u32,
        delay: std::time::Duration,
    },
    /// Charged one escalating slot: back off `delay`, then resubmit.
    Backoff {
        attempt: u32,
        delay: std::time::Duration,
    },
    /// Per-incident budget exhausted by credentialed 401s; fail the turn.
    Exhausted,
    /// Runaway guard: `rejections` credential-less rejections with no success in between — fail the turn.
    RunawayGuard { rejections: u32 },
}

/// The budget is per-incident (successes and suspend boundaries reset it, the latter capped) and only credentialed rejections charge it.
/// Delays must be 1s/2s/4s. `ExponentialBackoff::from_millis(base)` raises `base` to the attempt number, so the base must stay small.
/// `from_millis(1000)` yields 1s, then 16m40s, then 11.57 days of silent hang (a past field incident).
pub(crate) struct AuthRetrySchedule {
    delays: std::iter::Take<ExponentialBackoff>,
    /// Slots charged this incident.
    attempt: u32,
    /// 401s seen this incident, total and the subset that provably carried a credential.
    /// Feeds the exhaustion message so "real credential rejected" and "budget exhausted" cannot be conflated.
    incident_rejections: u32,
    incident_authenticated: u32,
    /// Stamped by the incident's first charged 401; cleared by resets.
    incident_started: Option<DualClock>,
    /// Uncharged fail-closed rejections since the last successful response (survives suspend resets).
    uncharged_resubmits: u32,
    /// Escalating pace for uncharged resubmits; survives suspend resets, only a success re-arms it.
    uncharged_delays: ExponentialBackoff,
    /// Suspend-triggered resets since the last successful response.
    suspend_resets: u32,
}

impl AuthRetrySchedule {
    /// Consecutive credentialed post-recovery 401s tolerated per incident before the turn fails.
    pub(crate) const MAX_RETRIES: u32 = 3;
    /// Uncharged rejections tolerated without a success in between. Burn rate: ~1 per 16-min
    /// sleep cycle (>13 h lid-closed survival), or ~1 per pace step awake (≥ ~45 min to fail).
    pub(crate) const MAX_UNCHARGED_RESUBMITS: u32 = 50;
    /// Suspend resets tolerated without an intervening successful response (about 8 sleep cycles of a continuously failing incident).
    /// Beyond this the budget stops resetting and is allowed to exhaust.
    pub(crate) const MAX_SUSPEND_RESETS: u32 = 8;
    /// Cap on the escalating uncharged pace; a landing token wakes the wait early, so it costs no recovery latency.
    pub(crate) const UNCHARGED_PACE_CAP: std::time::Duration = std::time::Duration::from_secs(60);
    /// Wall-vs-monotonic drift beyond which the machine must have slept:
    /// well below a real sleep cycle (minutes), well above NTP step jitter.
    const SUSPEND_DRIFT_MIN: std::time::Duration = std::time::Duration::from_secs(30);

    pub(crate) fn new() -> Self {
        Self {
            delays: ExponentialBackoff::from_millis(2)
                .factor(500)
                .max_delay(std::time::Duration::from_secs(10))
                .take(Self::MAX_RETRIES as usize),
            attempt: 0,
            incident_rejections: 0,
            incident_authenticated: 0,
            incident_started: None,
            uncharged_resubmits: 0,
            uncharged_delays: ExponentialBackoff::from_millis(2)
                .factor(500)
                .max_delay(Self::UNCHARGED_PACE_CAP),
            suspend_resets: 0,
        }
    }

    /// Parked on the uncharged path. Parked turns must not drive refreshes — see the
    /// re-park arm in `handle_sampling_failure`.
    pub(crate) fn is_parked(&self) -> bool {
        self.uncharged_resubmits > 0
    }

    /// Decision for one post-recovery 401. Charges a slot only when the
    /// rejected request carried a credential (or its provenance is unknown
    /// — fail closed toward terminating).
    pub(crate) fn on_recovered_401(&mut self, credential: SentCredential) -> AuthRetryDecision {
        self.on_recovered_401_at(credential, DualClock::now())
    }

    /// Clock-injected twin of [`Self::on_recovered_401`] for tests.
    fn on_recovered_401_at(
        &mut self,
        credential: SentCredential,
        now: DualClock,
    ) -> AuthRetryDecision {
        if credential.is_missing() {
            self.uncharged_resubmits += 1;
            if self.uncharged_resubmits > Self::MAX_UNCHARGED_RESUBMITS {
                return AuthRetryDecision::RunawayGuard {
                    rejections: self.uncharged_resubmits,
                };
            }
            return AuthRetryDecision::UnchargedResubmit {
                resubmit: self.uncharged_resubmits,
                // Unbounded iterator (`max_delay`-capped): `next()` always yields.
                delay: self
                    .uncharged_delays
                    .next()
                    .unwrap_or(Self::UNCHARGED_PACE_CAP),
            };
        }
        self.incident_started.get_or_insert(now);
        self.incident_rejections += 1;
        if credential == SentCredential::Sent {
            self.incident_authenticated += 1;
        }
        match self.delays.next() {
            Some(delay) => {
                self.attempt += 1;
                AuthRetryDecision::Backoff {
                    attempt: self.attempt,
                    delay,
                }
            }
            None => AuthRetryDecision::Exhausted,
        }
    }

    /// Close the open incident if it spans a suspend (wall elapsed outgrew monotonic elapsed by [`Self::SUSPEND_DRIFT_MIN`]).
    /// Separate wakes are independent 401 events.
    /// Capped at [`Self::MAX_SUSPEND_RESETS`] per success-free stretch so a fault that persists across wakes exhausts instead of retrying forever.
    pub(crate) fn reset_if_incident_spans_suspend(&mut self) -> bool {
        self.reset_if_incident_spans_suspend_at(DualClock::now())
    }

    /// Clock-injected twin of [`Self::reset_if_incident_spans_suspend`].
    fn reset_if_incident_spans_suspend_at(&mut self, now: DualClock) -> bool {
        let Some(started) = self.incident_started else {
            return false;
        };
        if self.suspend_resets >= Self::MAX_SUSPEND_RESETS {
            return false;
        }
        let (awake, total) = started.elapsed_between(now);
        if total.saturating_sub(awake) < Self::SUSPEND_DRIFT_MIN {
            return false;
        }
        self.reset_incident_keeping_park();
        self.suspend_resets += 1;
        true
    }

    /// Close the charged incident but keep the park: the evidence proves nothing about the missing
    /// credential, and un-parking would re-dispatch recovery (see the re-park arm).
    pub(crate) fn reset_incident_keeping_park(&mut self) {
        let (uncharged, resets) = (self.uncharged_resubmits, self.suspend_resets);
        let delays = self.uncharged_delays.clone();
        *self = Self::new();
        self.uncharged_resubmits = uncharged;
        self.uncharged_delays = delays;
        self.suspend_resets = resets;
    }

    /// A successful model response ends every open incident.
    /// Restart the escalating schedule and clear the success-free-stretch counters (uncharged rejections, suspend resets).
    pub(crate) fn reset_on_success(&mut self) {
        *self = Self::new();
    }

    /// `(rejections, authenticated)` seen this incident, for the exhaustion message.
    pub(crate) fn incident_counts(&self) -> (u32, u32) {
        (self.incident_rejections, self.incident_authenticated)
    }

    /// Uncharged fail-closed rejections since the last successful response.
    pub(crate) fn uncharged_rejections(&self) -> u32 {
        self.uncharged_resubmits
    }
}

#[cfg(test)]
#[path = "auth_retry_tests.rs"]
mod tests;
