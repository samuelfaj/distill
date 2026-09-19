//! One instant captured on two clocks, so elapsed time stays honest across a system suspend without trusting the wall clock alone.

use std::time::{Duration, Instant, SystemTime};

/// `Instant` pauses while the machine sleeps (macOS `mach_absolute_time`, Linux `CLOCK_MONOTONIC`).
/// `SystemTime` keeps advancing through sleep but jumps with NTP steps and manual changes.
/// The difference between the two elapsed spans bounds the suspended time.
#[derive(Clone, Copy)]
pub struct DualClock {
    /// Monotonic; pauses during sleep. Bounds elapsed *awake* time.
    pub mono: Instant,
    /// Wall clock; advances through sleep. Bounds elapsed *real* time.
    pub wall: SystemTime,
}

impl DualClock {
    pub fn now() -> Self {
        Self {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// Elapsed on each clock as `(monotonic, wall)`.
    /// Wall elapsed clamps to zero if the clock ran backwards (NTP step) so a backward jump can never fabricate a suspend or inflate a duration.
    pub fn elapsed_between(&self, now: DualClock) -> (Duration, Duration) {
        (
            now.mono.saturating_duration_since(self.mono),
            now.wall.duration_since(self.wall).unwrap_or(Duration::ZERO),
        )
    }

    /// [`Self::elapsed_between`] against the live clocks.
    pub fn elapsed(&self) -> (Duration, Duration) {
        self.elapsed_between(Self::now())
    }

    /// `(awake, total, suspended)` durations since this instant.
    pub fn elapsed_split(&self) -> (Duration, Duration, Duration) {
        let (awake, total) = self.elapsed();
        (awake, total, total.saturating_sub(awake))
    }
}

#[cfg(test)]
#[path = "dual_clock_tests.rs"]
mod tests;
