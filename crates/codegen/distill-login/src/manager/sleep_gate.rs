// Modified for Distill by Samuel Fajreldines, 2026.
//! Keeps an [`AuthManager`] token refresh from straddling a system sleep.
//!
//! A refresh that straddles a suspend can lose its rotated successor token, leaving a revoked refresh token on disk and forcing re-login.
//! Two layers guard against that straddle:
//!
//! 1. The gate `refresh_chain` consults *defers* a refresh that has not started yet.
//!    An in-flight refresh is never aborted: dropping it could discard a response carrying the rotated token, the very revocation we guard against.
//!    See [`AuthManager::refresh_chain`].
//! 2. When sleep becomes imminent while a refresh is in flight, [`AuthManager::set_system_sleep_imminent`] **holds the OS sleep acknowledgment**.
//!    The hold lasts until the refresh drains or [`SLEEP_ACK_MAX_WAIT`] elapses, so the exchange finishes *before* the machine suspends.
//!    macOS delays `IOAllowPowerChange` and Linux holds its `delay` inhibitor, both via the blocking power-listener callback.
//!
//! Split out of `manager.rs` so the manager stays scannable.
//! It is self-contained: the [`SleepGate`] type, the [`InFlightGuard`], and a small `impl AuthManager` block driving them from OS power events.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};

use parking_lot::RwLock;

use super::AuthManager;
use distill_shell_base::util::dual_clock::DualClock;

/// Max lifetime of the "system sleep imminent" gate.
/// A wake event normally clears it; this is the safety bound so a *missed* wake event can never permanently block token refresh.
/// The bound is generous compared to the OS pre-sleep window (macOS ~30 s, Linux ~5 s); it only needs to outlast the sleep transition.
pub(super) const SLEEP_GATE_MAX: StdDuration = StdDuration::from_secs(120);

/// Max time a token refresh may stay deferred for **dark wake** before one is forced through, mirroring [`SLEEP_GATE_MAX`]. A normal dark wake lasts seconds and recurs interspersed with full wakes, so this rarely fires.
/// It rescues a machine that reports a *continuous* dark wake, e.g. an interactive Mac with no display, whose system video capability is never set.
/// Such a machine would otherwise defer every refresh forever and reach the same logged-out state this guard prevents. Bounded on two clocks (see [`DualClock`]) so it also survives the machine sleeping between dark wakes.
pub(super) const DARK_WAKE_DEFER_MAX: StdDuration = StdDuration::from_secs(120);

/// Upper bound on how long a `WillSleep` transition holds the OS sleep acknowledgment while in-flight IdP refreshes drain. **macOS** allows ~30 s before `IOAllowPowerChange` is forced, so we use most of it.
/// A straddled exchange can need ~15 s of awake time to complete (in-call retries included). Losing its response past the assumed ~60 s rotation grace revokes the token family, and every session then demands `/login`.
/// **Linux** logind's `InhibitDelayMaxSec` defaults to 5 s, so we stay under it. The hold releases the moment the in-flight count drains; a healthy round-trip is ~1 s.
#[cfg(target_os = "macos")]
pub(super) const SLEEP_ACK_MAX_WAIT: StdDuration = StdDuration::from_secs(20);
#[cfg(not(target_os = "macos"))]
pub(super) const SLEEP_ACK_MAX_WAIT: StdDuration = StdDuration::from_secs(3);

/// A gate `refresh_chain` consults to avoid *starting* an IdP refresh just before sleep. It only *defers* a refresh that has not started; an in-flight one is left to finish (see [`AuthManager::refresh_chain`]).
/// The raise timestamp is a [`DualClock`] so the [`SLEEP_GATE_MAX`] backstop survives the sleep itself.
/// A gate raised just before a long sleep never auto-expires on the monotonic clock alone; that bug let an expired token reach the server and 401. The gate therefore expires once *either* clock passes the bound.
#[derive(Default)]
pub(super) struct SleepGate {
    pub(super) raised_at: RwLock<Option<DualClock>>,
}

impl SleepGate {
    pub(super) fn raise(&self) {
        *self.raised_at.write() = Some(DualClock::now());
        distill_telemetry::unified_log::warn("auth.sleep.gate_set", None, None);
    }

    pub(super) fn lower(&self, reason: &str) {
        let prev = self.raised_at.write().take();
        let (mono_ms, wall_ms) = prev
            .map(|r| {
                let (mono, wall) = r.elapsed();
                (mono.as_millis() as u64, wall.as_millis() as u64)
            })
            .unwrap_or((0, 0));
        distill_telemetry::unified_log::info(
            "auth.sleep.gate_cleared",
            None,
            Some(serde_json::json!({
                "reason": reason,
                "was_raised": prev.is_some(),
                "mono_elapsed_ms": mono_ms,
                "wall_elapsed_ms": wall_ms,
            })),
        );
    }

    /// A stale gate (a missed or late wake event) is lazily lowered here so it can never permanently block refresh. This read can therefore have a side effect.
    /// The gate expires once *either* clock passes [`SLEEP_GATE_MAX`] (see [`DualClock`]).
    /// Without the wall-clock check, a gate raised before a long sleep would never auto-expire; the monotonic clock pauses while the machine sleeps.
    pub(super) fn is_gated(&self) -> bool {
        // Copy out so the read guard drops before the write lock below (parking_lot is not reentrant)
        let raised_at = *self.raised_at.read();
        let Some(raise) = raised_at else {
            return false;
        };
        let (mono, wall) = raise.elapsed();
        if mono < SLEEP_GATE_MAX && wall < SLEEP_GATE_MAX {
            return true;
        }
        // The gate is stale (a missed or late wake) `sleep_straddle` means the monotonic clock is still under the bound but real (wall-clock) time is not In that case the machine slept through the gate without delivering a wake event
        // This is the case the wall-clock check exists to catch, so it is logged explicitly rather than folded into the generic expiry
        let sleep_straddle = mono < SLEEP_GATE_MAX;
        {
            // Re-check under the write lock: a `WillSleep` can raise a *fresh* gate between the read above and here
            // Clearing that fresh gate would start a refresh into the very suspend it announces
            let mut guard = self.raised_at.write();
            match *guard {
                Some(current) if current.mono == raise.mono => *guard = None,
                Some(_fresh_raise) => return true,
                None => return false,
            }
        }
        distill_telemetry::unified_log::info(
            "auth.sleep.gate_cleared",
            None,
            Some(serde_json::json!({
                "reason": "auto_expiry",
                "sleep_straddle": sleep_straddle,
                "mono_elapsed_ms": mono.as_millis() as u64,
                "wall_elapsed_ms": wall.as_millis() as u64,
            })),
        );
        false
    }
}

/// RAII counter for in-flight IdP refreshes.
/// Increments on construction and decrements on drop so the count stays balanced even if the refresh future is cancelled or panics.
/// When the count returns to zero it wakes any sleep-imminent waiter parked in [`AuthManager::hold_sleep_ack_until_refresh_drains`].
pub(super) struct InFlightGuard<'a>(&'a AuthManager);

impl<'a> InFlightGuard<'a> {
    pub(super) fn new(mgr: &'a AuthManager) -> Self {
        mgr.begin_refresh_in_flight();
        Self(mgr)
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.end_refresh_in_flight();
    }
}

impl AuthManager {
    /// Report a system power transition (`true` means sleep is imminent, `false` means the machine woke). Safe to call from any thread.
    pub fn set_system_sleep_imminent(&self, imminent: bool) {
        if imminent {
            // Raise the gate first: a refresh that re-checks it right before its IdP call (see `refresh_chain`) then backs out
            // Then hold the OS sleep acknowledgment until any refresh already in flight drains, so it finishes before the machine suspends
            self.sleep_gate.raise();
            self.hold_sleep_ack_until_refresh_drains(SLEEP_ACK_MAX_WAIT);
        } else {
            self.sleep_gate.lower("wake");
            // `DidWake` fires for dark wakes too; clearing unconditionally would reset the defer budget every cycle and it could never exhaust
            if !self.is_dark_wake() {
                self.end_dark_wake_defer_run();
            }
            // Restart the proactive-refresh loop; its monotonic timer did not advance during the suspend (see [`AuthManager::notify_wake`])
            self.notify_wake();
        }
    }

    /// Mark an IdP refresh as starting.
    /// Paired with [`Self::end_refresh_in_flight`] via [`InFlightGuard`]; see [`Self::hold_sleep_ack_until_refresh_drains`].
    fn begin_refresh_in_flight(&self) {
        self.refresh_in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// Mark an IdP refresh as finished. When the count returns to zero, wake any sleep-ack waiter under the same lock it parks on.
    /// A held OS sleep ack is then released the moment the exchange finishes rather than after the full timeout. `fetch_sub` returns the *previous* value, so `== 1` means the count just dropped to zero.
    /// Notifying with no waiter parked is cheap and harmless.
    fn end_refresh_in_flight(&self) {
        if self.refresh_in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _drain = self.refresh_drain_lock.lock();
            self.refresh_drain_cv.notify_all();
        }
    }

    /// Block the calling thread until in-flight IdP refreshes drain or `max` elapses. The caller is the OS power-listener callback, so blocking delays the macOS `IOAllowPowerChange` ack or the Linux `delay`-inhibitor release.
    /// A refresh on the wire when sleep is requested would otherwise straddle the suspend and, on a long sleep, lose its rotated successor token. Losing it revokes the refresh-token family and forces re-login.
    /// We never abort the refresh; we briefly delay the suspend so it can finish first. Bounded by `max` (see [`SLEEP_ACK_MAX_WAIT`]) so a hung refresh can't hold the machine awake past the OS pre-sleep budget.
    fn hold_sleep_ack_until_refresh_drains(&self, max: StdDuration) {
        let in_flight = self.refresh_in_flight.load(Ordering::SeqCst);
        if in_flight == 0 {
            return;
        }
        distill_telemetry::unified_log::warn(
            "auth.sleep.refresh_in_flight_at_suspend",
            None,
            Some(serde_json::json!({ "in_flight": in_flight })),
        );
        let started = Instant::now();
        {
            let mut drain = self.refresh_drain_lock.lock();
            // Loop on the atomic (the authoritative predicate) under the lock
            // A notify that races the park, or a spurious wake, can then neither lose the signal nor over-wait
            // `InFlightGuard::drop` notifies when the count hits zero
            while self.refresh_in_flight.load(Ordering::SeqCst) > 0 {
                let Some(remaining) = max.checked_sub(started.elapsed()) else {
                    break;
                };
                if remaining.is_zero() {
                    break;
                }
                let _ = self.refresh_drain_cv.wait_for(&mut drain, remaining);
            }
        }
        let remaining = self.refresh_in_flight.load(Ordering::SeqCst);
        distill_telemetry::unified_log::info(
            "auth.sleep.refresh_drain",
            None,
            Some(serde_json::json!({
                "in_flight_at_start": in_flight,
                "in_flight_remaining": remaining,
                "drained": remaining == 0,
                "waited_ms": started.elapsed().as_millis() as u64,
                "max_wait_ms": max.as_millis() as u64,
            })),
        );
    }

    pub fn is_sleep_gated(&self) -> bool {
        self.sleep_gate.is_gated()
    }

    /// See [`distill_system_power::PowerState`] for what a dark wake is and why an IdP refresh must avoid one. `refresh_chain` gates on [`Self::should_defer_for_dark_wake`], which wraps this with a deferral bound.
    /// If the OS power listener was never started (headless or server), we skip the query: dark wake is no concern there.
    /// A screenless Mac can also read as a permanent dark wake (its video capability is never set), which would block refresh forever. It is read **before** the `power_listener_started` check. A headless run never starts the listener, and this ordering lets it drive the dark-wake paths against a real binary.
    pub fn is_dark_wake(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = *self.dark_wake_override.lock() {
            return forced;
        }
        match std::env::var("GROK_AUTH_FORCE_DARK_WAKE").ok().as_deref() {
            Some("1") => return true,
            Some("0") => return false,
            _ => {}
        }
        if !self.power_listener_started.load(Ordering::Acquire) {
            return false;
        }
        matches!(
            distill_system_power::current_power_state(),
            distill_system_power::PowerState::DarkWake
        )
    }

    /// Ends the current dark-wake deferral run so the next one starts with a fresh [`DARK_WAKE_DEFER_MAX`] budget.
    pub(super) fn end_dark_wake_defer_run(&self) {
        *self.dark_wake_defer_since.write() = None;
    }

    /// Whether `refresh_chain` should defer this refresh because the system is in a dark wake, bounded so deferral can never be indefinite.
    /// Tracks when the current unbroken run of dark-wake deferrals began (on two clocks; see [`DualClock`]). While inside the [`DARK_WAKE_DEFER_MAX`] budget it returns `true` (defer).
    /// Once either clock passes the bound it forces one refresh through (`false`) and resets the clock. A machine stuck reporting a continuous dark wake thus refreshes periodically instead of deferring forever and logging the user out. A full wake clears the run (here, or eagerly in [`Self::set_system_sleep_imminent`]).
    pub fn should_defer_for_dark_wake(&self) -> bool {
        // Sample the power state before taking the lock; it's an FFI read with no ordering relationship to the budget
        // Then hold one write guard for the whole decision
        // A read-then-write would let two concurrent callers both start a run and restart the budget indefinitely
        let dark = self.is_dark_wake();
        let mut run = self.dark_wake_defer_since.write();
        if !dark {
            // Full wake (or no signal): end any deferral run in progress.
            *run = None;
            return false;
        }
        let Some(raise) = *run else {
            // First deferral of this dark-wake run: start the budget clock.
            *run = Some(DualClock::now());
            return true;
        };
        let (mono, wall) = raise.elapsed();
        if mono < DARK_WAKE_DEFER_MAX && wall < DARK_WAKE_DEFER_MAX {
            return true;
        }
        // Budget exhausted: force this refresh through and reset the clock
        // A still-continuous dark wake then defers afresh, up to DARK_WAKE_DEFER_MAX, before the next forced refresh
        *run = None;
        drop(run);
        distill_telemetry::unified_log::warn(
            "auth.dark_wake.defer_budget_exhausted",
            None,
            Some(serde_json::json!({
                "mono_elapsed_ms": mono.as_millis() as u64,
                "wall_elapsed_ms": wall.as_millis() as u64,
            })),
        );
        false
    }

    /// Force the [`AuthManager::is_dark_wake`] result in tests.
    #[cfg(test)]
    pub fn set_dark_wake_for_test(&self, dark: bool) {
        *self.dark_wake_override.lock() = Some(dark);
    }

    /// Test hook: simulate an IdP refresh entering flight (mirrors [`InFlightGuard::new`]).
    #[cfg(test)]
    pub fn test_enter_refresh_in_flight(&self) {
        self.begin_refresh_in_flight();
    }

    /// Test hook: simulate an in-flight IdP refresh finishing (mirrors [`InFlightGuard`]'s drop), waking a sleep-ack waiter.
    #[cfg(test)]
    pub fn test_exit_refresh_in_flight(&self) {
        self.end_refresh_in_flight();
    }

    /// Test hook: run the bounded sleep-ack hold directly so tests can pass a short bound instead of [`SLEEP_ACK_MAX_WAIT`].
    #[cfg(test)]
    pub fn test_hold_sleep_ack(&self, max: StdDuration) {
        self.hold_sleep_ack_until_refresh_drains(max);
    }

    /// Start the OS power listener so sleep and wake events drive the gate.
    /// Idempotent and a no-op where the listener is unavailable.
    /// Call only from local, interactive entrypoints, never headless or server ones.
    pub fn start_system_power_listener(self: &Arc<Self>) {
        // Claim the one-time startup so concurrent or duplicate calls don't double-register
        if self
            .power_listener_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Weak ref so the manager and the listener never form an Arc cycle
        let weak = Arc::downgrade(self);
        let listener = distill_system_power::SystemPowerListener::start(move |event| {
            if let Some(this) = weak.upgrade() {
                let imminent = matches!(event, distill_system_power::PowerEvent::WillSleep);
                this.set_system_sleep_imminent(imminent);
            }
        });
        let available = listener.is_some();
        if available {
            *self.power_listener.lock() = listener;
        } else {
            // Unavailable (unsupported OS, no logind, or a registration failure): release the guard so a later call can retry
            self.power_listener_started.store(false, Ordering::Release);
        }
        distill_telemetry::unified_log::info(
            "auth.sleep.power_listener_init",
            None,
            Some(serde_json::json!({ "available": available })),
        );
    }
}
