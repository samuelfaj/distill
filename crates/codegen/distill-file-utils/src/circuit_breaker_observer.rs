// Modified for Distill by Samuel Fajreldines, 2026.
//! Tracing-based observer for the storage circuit breaker.

use std::sync::Arc;
use distill_circuit_breaker::{BreakerState, Observer, Outcome};

/// `Observer` that emits `tracing` events matching the legacy breaker so existing analytics keep firing.
/// Route on the **new** state only — `(old, new)` tuples mis-label `Open -> HalfOpen`.
/// Failures are traced; successes are dropped so steady state does not dominate log volume.
pub(crate) struct TracingObserver {
    name: &'static str,
}

impl TracingObserver {
    pub(crate) fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self { name })
    }
}

impl Observer for TracingObserver {
    fn on_state_change(&self, old: BreakerState, new: BreakerState, reason: &str) {
        match new {
            BreakerState::Open => tracing::warn!(
                target: "circuit_breaker",
                breaker = self.name,
                ?old,
                ?new,
                reason,
                "circuit breaker opened"
            ),
            BreakerState::HalfOpen => tracing::debug!(
                target: "circuit_breaker",
                breaker = self.name,
                ?old,
                ?new,
                reason,
                "circuit breaker half-open"
            ),
            BreakerState::Closed => tracing::info!(
                target: "circuit_breaker",
                breaker = self.name,
                ?old,
                ?new,
                reason,
                "circuit breaker closed"
            ),
        }
    }

    fn on_probe_admission(&self, allowed: bool) {
        tracing::debug!(
            target: "circuit_breaker",
            breaker = self.name,
            allowed,
            "circuit breaker probe admission"
        );
    }

    fn on_outcome(&self, outcome: Outcome, state: BreakerState) {
        if let Outcome::Failure = outcome {
            tracing::trace!(
                target: "circuit_breaker",
                breaker = self.name,
                ?state,
                "circuit breaker outcome failure"
            );
        }
    }
}

#[cfg(test)]
#[path = "circuit_breaker_observer_tests.rs"]
mod tests;
