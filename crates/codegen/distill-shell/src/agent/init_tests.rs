// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{
    AgentConfig, BootstrapError, PREFETCH_RUNS, StartupPrefetch, apply_post_gate_settings,
    bootstrap_with_cancel, hold_bootstrap_gate_for_tests, startup_settings_deadline,
};
use crate::managed_config::LaunchProfile;
use distill_login::{AuthManager, GrokComConfig};
use tokio_util::sync::CancellationToken;

#[test]
fn startup_settings_deadline_selects_by_profile() {
    assert_eq!(
        startup_settings_deadline(LaunchProfile::Managed),
        crate::http::MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE,
        "a managed principal must get the longer kill-switch wait",
    );
    assert_eq!(
        startup_settings_deadline(LaunchProfile::Personal),
        crate::http::STARTUP_SETTINGS_WAIT_DEADLINE,
        "a personal launch must get the shorter wait",
    );
}

// Resets the process-global `STARTUP_STATE` (aborting its owner), so it must serialize with the settings_get tests that own it.
#[test]
#[serial_test::serial(remote_sig_disarm, startup_settings)]
fn post_gate_pass_spends_at_most_one_settings_budget() {
    crate::agent::remote_config::settings_get::reset_startup_settings_for_tests();
    let runs_before = PREFETCH_RUNS.with(std::cell::Cell::get);

    let mut cfg = AgentConfig::default();
    assert!(
        cfg.remote_settings.is_none(),
        "an absent prefetch result is the state under test"
    );
    apply_post_gate_settings(
        &mut cfg,
        StartupPrefetch::ClientSupplied,
        LaunchProfile::Personal,
        &CancellationToken::new(),
        None,
        None,
    );
    assert_eq!(
        PREFETCH_RUNS.with(std::cell::Cell::get),
        runs_before + 1,
        "the fallback prefetch never ran: the counter is dead or the wiring lost the fetch"
    );

    let mut cfg = AgentConfig::default();
    apply_post_gate_settings(
        &mut cfg,
        StartupPrefetch::Ran,
        LaunchProfile::Personal,
        &CancellationToken::new(),
        None,
        None,
    );
    assert_eq!(
        PREFETCH_RUNS.with(std::cell::Cell::get),
        runs_before + 1,
        "the post-gate pass spent a second settings retry budget"
    );
}

// Resets the process-global `STARTUP_STATE` (aborting its owner), so it must serialize with the settings_get tests that own it.
#[test]
#[serial_test::serial(remote_sig_disarm, startup_settings)]
fn supplied_settings_skip_the_getter() {
    crate::agent::remote_config::settings_get::reset_startup_settings_for_tests();
    let runs_before = PREFETCH_RUNS.with(std::cell::Cell::get);
    let mut cfg = AgentConfig {
        remote_settings: Some(Default::default()),
        ..AgentConfig::default()
    };
    let outcome = super::ensure_remote_settings_side_effects(
        &mut cfg,
        LaunchProfile::Personal,
        &CancellationToken::new(),
        None,
        None,
    );
    assert!(
        matches!(outcome, Ok(StartupPrefetch::ClientSupplied)),
        "supplied settings must not run the getter, got {outcome:?}"
    );
    assert_eq!(
        PREFETCH_RUNS.with(std::cell::Cell::get),
        runs_before,
        "supplied settings must not spend a settings budget"
    );
}

#[test]
fn cancelled_bootstrap_returns_before_side_effects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = match bootstrap_with_cancel(&AgentConfig::default(), &auth, None, &cancel, None) {
        Err(err) => err,
        Ok(_) => panic!("a pre-cancelled token must not run bootstrap"),
    };
    assert!(matches!(err, BootstrapError::Cancelled), "got {err}");
}

#[test]
fn second_bootstrap_bails_when_cancelled_while_the_gate_is_held() {
    let _held = hold_bootstrap_gate_for_tests();
    let entered = Arc::new(AtomicBool::new(false));
    let entered_worker = entered.clone();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let handle = std::thread::spawn(move || {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        entered_worker.store(true, Ordering::SeqCst);
        bootstrap_with_cancel(&AgentConfig::default(), &auth, None, &worker_cancel, None)
    });
    let started = std::time::Instant::now();
    while !entered.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(entered.load(Ordering::SeqCst), "waiter never started");
    std::thread::sleep(Duration::from_millis(40));
    cancel.cancel();
    let err = match handle.join().expect("waiter thread") {
        Err(err) => err,
        Ok(_) => panic!("cancelled waiter must not run bootstrap beside the holder"),
    };
    assert!(matches!(err, BootstrapError::Cancelled), "got {err}");
}
