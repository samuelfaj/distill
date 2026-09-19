// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-platform system **sleep/wake** (suspend/resume) notifications.
//!
//! The motivating use case: an OIDC token refresh that is *in flight when the
//! laptop sleeps* can lose its rotated successor token (the server processes
//! the request, rotates/revokes the old refresh token, and the response is
//! lost across the suspend). On wake the client is holding a dead refresh
//! token and the user is forced to re-login. See
//! `distill-shell`'s `AuthManager` sleep gate, which consumes these events to
//! avoid *starting* a refresh just before sleep. An in-flight refresh is
//! deliberately left to finish, never aborted (dropping it could discard a
//! rotated-token response and cause the very revocation this guards against);
//! instead, its [`PowerEvent::WillSleep`] handler may block briefly (bounded)
//! to hold off the suspend until that in-flight refresh completes — see the
//! callback contract below.
//!
//! This crate exposes a single tiny abstraction — [`SystemPowerListener`] —
//! with per-OS implementations behind `#[cfg]` and a no-op fallback:
//!
//! | OS      | Mechanism                                                            |
//! |---------|----------------------------------------------------------------------|
//! | macOS   | IOKit `IORegisterForSystemPower` on a dedicated `CFRunLoop` thread    |
//! | Windows | `PowerRegisterSuspendResumeNotification` (`DEVICE_NOTIFY_CALLBACK`)   |
//! | Linux   | logind D-Bus `PrepareForSleep` signal + a `delay` inhibitor lock      |
//! | other   | no-op (returns `None` from [`SystemPowerListener::start`])            |
//!
//! The callback fires from a platform event thread/callback, so it must be
//! `Send + Sync`. It should return promptly, but a [`PowerEvent::WillSleep`]
//! handler *may* block for a short, bounded time to hold off sleep: the per-OS
//! implementations acknowledge the transition only **after** the callback
//! returns (macOS calls `IOAllowPowerChange`; Linux releases its `delay`
//! inhibitor), so blocking there delays the suspend itself. Keep any such block
//! within the OS budget — macOS allows ~30 s after `kIOMessageSystemWillSleep`;
//! Linux logind's `InhibitDelayMaxSec` defaults to 5 s — or the OS proceeds to
//! sleep anyway. `DidWake` handlers must stay cheap and non-blocking.

#![deny(clippy::indexing_slicing)]

/// A system power transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// Because the idle-sleep negotiation can be vetoed (by any power client), a `WillSleep` is not a guarantee that sleep
    /// follows: it may be succeeded by a [`Self::DidWake`] without an intervening suspend. Handlers must therefore be
    /// idempotent and safe to "cancel" via `DidWake`.
    WillSleep,
    /// The system resumed from sleep, or a previously announced sleep was
    /// cancelled (macOS `kIOMessageSystemWillNotSleep` after a vetoed
    /// idle-sleep query). Both mean "not sleeping (anymore)".
    DidWake,
}

/// Boxed user callback invoked on each [`PowerEvent`].
pub type PowerCallback = Box<dyn Fn(PowerEvent) + Send + Sync + 'static>;

/// A coarse, synchronously-queryable system power state (see [`current_power_state`]). Code that starts irreversible
/// network work — notably a one-time-use OIDC refresh-token exchange — should avoid doing so during a dark wake, because
/// the machine may re-sleep mid-request and lose the response that carries the rotated token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Full / user wake: display (graphics) capability present — a user is (or
    /// can be) present. Safe to start irreversible work.
    FullWake,
    /// Dark wake: CPU (and usually network/disk) up for background or
    /// maintenance work, but display off and no user. The system may re-sleep
    /// at any moment with no warning.
    DarkWake,
    /// State could not be determined: an unsupported OS, or the platform query failed / returned a transitional sample.
    /// Callers should treat this as "no signal" and fall back to their existing behavior — never block on it.
    Unknown,
}

/// Cheap, non-blocking, and never panics. Returns [`PowerState::Unknown`] on platforms without a real implementation
/// (currently everything except macOS) or when the platform query fails. Unlike [`SystemPowerListener`], this needs no
/// running listener — on macOS it is a single connection-less IOKit call.
pub fn current_power_state() -> PowerState {
    imp::current_power_state()
}

/// Ask the OS not to sleep until the returned guard is dropped. Motivating case: a macOS dark wake re-sleeps within
/// seconds without any sleep notification, so a `WillSleep` handler cannot protect work started there. [`SleepAssertion`]
/// releases from `Drop` (a leaked assertion pins the machine awake); visible in `pmset -g assertions`.
#[must_use = "the assertion is released as soon as the guard is dropped"]
pub fn hold_awake(reason: &str) -> Option<SleepAssertion> {
    imp::hold_awake(reason).map(|inner| SleepAssertion { _inner: inner })
}

/// RAII guard from [`hold_awake`]; releases the OS assertion on drop.
#[derive(Debug)]
pub struct SleepAssertion {
    _inner: imp::Assertion,
}

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod imp;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    use super::PowerCallback;

    pub(crate) struct Listener;

    impl Listener {
        pub(crate) fn start(_callback: PowerCallback) -> Option<Self> {
            None
        }
    }

    pub(crate) fn current_power_state() -> super::PowerState {
        super::PowerState::Unknown
    }

    /// Never constructed (`hold_awake` always returns `None`); present so the
    /// cross-platform `SleepAssertion` has a field type on every target.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct Assertion;

    pub(crate) fn hold_awake(_reason: &str) -> Option<Assertion> {
        None
    }
}

/// On macOS/Windows, dropping it stops the listener and releases its OS resources. On Linux the worker parks on a
/// blocking logind signal and cannot be cleanly interrupted, so it runs (with its D-Bus connection + sleep-delay
/// inhibitor) until process exit — see the `linux` module. Intended as a process-lifetime singleton.
pub struct SystemPowerListener {
    // Kept for its `Drop`; the field is read on platforms with a real impl.
    #[allow(dead_code)]
    inner: imp::Listener,
}

impl SystemPowerListener {
    /// Callers should treat `None` as "no power notifications" and degrade gracefully (the dependent feature simply does not
    /// engage). `callback` is invoked from a platform event thread, so it must be `Send + Sync`, cheap, and non-blocking.
    pub fn start<F>(callback: F) -> Option<Self>
    where
        F: Fn(PowerEvent) + Send + Sync + 'static,
    {
        imp::Listener::start(Box::new(callback)).map(|inner| Self { inner })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `start` + `drop` must be clean on every platform: no panic and no hang
    /// (the latter exercises the macOS run-loop teardown). On Linux/Windows CI
    /// `start` may return `None` (no system bus / unsupported) — also fine.
    #[test]
    fn start_and_drop_is_clean() {
        // Bind and let it drop at end of scope rather than calling `drop()`: on platforms where the listener owns no `Drop` type
        // (e.g. Linux, whose worker is detached and runs until process exit) an explicit `drop()` trips `clippy::drop_non_drop`.
        let _listener = SystemPowerListener::start(|_event| {});
    }
}

#[cfg(all(test, target_os = "macos"))]
mod assertion_tests {
    /// Proves the FFI really registers with the OS, not merely that it links: take an assertion, look for it in `pmset -g
    /// assertions`, drop it, and confirm it went away. A wrong symbol or ABI would compile and silently protect nothing.
    #[test]
    fn hold_awake_registers_and_releases_a_real_assertion() {
        let name = format!("distill-system-power selftest {}", std::process::id());
        let listed = || -> String {
            std::process::Command::new("/usr/bin/pmset")
                .args(["-g", "assertions"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        if listed().is_empty() {
            return; // no pmset (sandboxed CI) — nothing to assert against
        }

        // The OS can refuse (sandboxed runners deny the powerd service);
        // refusal is a supported outcome, not a test failure.
        let Some(held) = super::hold_awake(&name) else {
            return;
        };
        assert!(
            listed().contains(&name),
            "assertion should be visible to the OS while held"
        );
        drop(held);
        assert!(
            !listed().contains(&name),
            "assertion must be released on drop, or the machine cannot sleep"
        );
    }
}
