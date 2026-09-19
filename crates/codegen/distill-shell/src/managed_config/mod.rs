// Modified for Distill by Samuel Fajreldines, 2026.
//! Sync `managed_config.toml` + `requirements.toml` from the deployment-config endpoint per principal;
//! evicted on identity switch and logout so config never crosses principals (`GROK_MANAGED_CONFIG=0` skips the sweep).

mod policy;
mod response;
mod store;
mod supervisor;

pub use policy::ManagedPolicyRefusal;
pub use response::ManagedConfigError;
// Test seams: pub only so the integration suites can link them.
#[doc(hidden)]
pub use store::MANAGED_ARTIFACT_FILES;
pub use store::{
    classify_auth_mode, clear_orphan, current_serving_identity, has_principal, is_fetch_enabled,
};
pub(crate) use store::{resolve_deployment_id, resolve_deployment_key};
pub(crate) use supervisor::policy_repair_pending;
#[doc(hidden)]
pub use supervisor::{ManagedConfigRefresher, spawn_refresh_supervisor, take_refresh_supervisor};
pub use supervisor::{
    ManagedConfigSync, SESSION_START_AUTH_DEADLINE, SESSION_START_SYNC_DEADLINE, SetupOutcome,
    SetupReport, ensure_managed_policy_present, fetch_setup_report, post_login_sync, run_setup,
    start_refresh_supervisor, sync,
};

/// Absorbs a healthy in-flight apply without letting a wedged holder stall start.
const GATE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchProfile {
    Personal,
    Managed,
}

static PUBLISHED_PROFILE: std::sync::Mutex<Option<LaunchProfile>> = std::sync::Mutex::new(None);

fn observe_startup_profile() -> LaunchProfile {
    if !cfg!(test) && store::managed_principal_present() {
        LaunchProfile::Managed
    } else {
        LaunchProfile::Personal
    }
}

/// Sample the launch profile once. A later observation may only escalate
/// Personal → Managed, so pager and bootstrap agree and a managed start is
/// never left on the shorter personal connect budget.
pub fn startup_profile() -> LaunchProfile {
    let observed = observe_startup_profile();
    let mut published = PUBLISHED_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
    match *published {
        Some(LaunchProfile::Managed) => LaunchProfile::Managed,
        Some(LaunchProfile::Personal) if observed == LaunchProfile::Managed => {
            *published = Some(LaunchProfile::Managed);
            LaunchProfile::Managed
        }
        Some(existing) => existing,
        None => {
            *published = Some(observed);
            observed
        }
    }
}

/// Test seam: drop the published sample so the next call observes fresh.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_startup_profile_for_tests() {
    *PUBLISHED_PROFILE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Fail-closed session-start gate for managed principals.
pub fn managed_policy_gate() -> Result<(), ManagedPolicyRefusal> {
    managed_policy_gate_with_lock_wait(GATE_LOCK_WAIT)
}

/// Test seam injecting the bounded lock wait; production stays on [`GATE_LOCK_WAIT`].
#[doc(hidden)]
pub fn managed_policy_gate_with_lock_wait(
    lock_wait: std::time::Duration,
) -> Result<(), ManagedPolicyRefusal> {
    // Unit tests would hit the host's real home.
    if cfg!(test) {
        return Ok(());
    }
    if !store::managed_principal_present() {
        return Ok(());
    }
    let snapshot = locked_gate_snapshot(&crate::util::distill_home::distill_home(), lock_wait)?;
    policy::managed_policy_gate_decision(snapshot)
}

fn locked_gate_snapshot(
    home: &std::path::Path,
    lock_wait: std::time::Duration,
) -> Result<policy::GateSnapshot, ManagedPolicyRefusal> {
    let lock_file = match store::try_gate_lock(home) {
        store::GateLockAttempt::Acquired(lock_file) => lock_file,
        // No `block_in_place`: bootstrap runs inside a `LocalSet`, where tokio panics on it even on a multi-thread runtime.
        // Plain blocking is safe here: no task ever holds this flock across an await, and the wait is bounded by `lock_wait`.
        store::GateLockAttempt::Contended(lock_file) => {
            store::wait_for_gate_lock(&lock_file, home, lock_wait)?;
            lock_file
        }
        store::GateLockAttempt::Unavailable => {
            return Err(ManagedPolicyRefusal::LockUnavailable {
                home: home.to_path_buf(),
            });
        }
    };
    let _lock = lock_file;
    Ok(store::gate_snapshot_locked(home))
}

#[cfg(test)]
mod tests;
