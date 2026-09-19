// Modified for Distill by Samuel Fajreldines, 2026.
use std::time::Duration;

use distill_sampling_types::SentCredential;

use super::{AuthRetryDecision, AuthRetrySchedule};
use crate::util::dual_clock::DualClock;

/// `now` shifted `wall_ahead` on the wall clock only, the pattern a suspend leaves behind (monotonic pauses, wall keeps advancing).
fn after_suspend(base: DualClock, wall_ahead: Duration) -> DualClock {
    DualClock {
        mono: base.mono,
        wall: base.wall + wall_ahead,
    }
}

/// Pins the exact schedule.
/// Guards against the `from_millis(1000)` mistake, where the 1000 became the exponent base and real delays came out as 1s, 16m40s, and 11.57 days.
#[test]
fn schedule_is_one_two_four_seconds_then_exhausted() {
    let mut schedule = AuthRetrySchedule::new();
    let steps: Vec<_> = (0..3)
        .map(|_| schedule.on_recovered_401(SentCredential::Sent))
        .collect();
    assert_eq!(
        steps,
        vec![
            AuthRetryDecision::Backoff {
                attempt: 1,
                delay: Duration::from_secs(1)
            },
            AuthRetryDecision::Backoff {
                attempt: 2,
                delay: Duration::from_secs(2)
            },
            AuthRetryDecision::Backoff {
                attempt: 3,
                delay: Duration::from_secs(4)
            },
        ],
    );
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Sent),
        AuthRetryDecision::Exhausted,
    );
    assert_eq!(schedule.incident_counts(), (4, 4));
}

/// `SentCredential::Unknown` charges the budget like an authenticated 401, failing closed toward terminating.
/// It is not reported as a proven credential rejection.
#[test]
fn unknown_credential_charges_but_is_not_counted_authenticated() {
    let mut schedule = AuthRetrySchedule::new();
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Unknown),
        AuthRetryDecision::Backoff {
            attempt: 1,
            delay: Duration::from_secs(1)
        },
    );
    assert_eq!(schedule.incident_counts(), (1, 0));
}

/// Expected escalating pace for the `i`-th (1-indexed) uncharged resubmit:
/// 1s, 2s, 4s, … capped at [`AuthRetrySchedule::UNCHARGED_PACE_CAP`].
fn expected_uncharged_delay(i: u32) -> Duration {
    (Duration::from_millis(500) * 2u32.pow(i.min(20))).min(AuthRetrySchedule::UNCHARGED_PACE_CAP)
}

/// Rule: a credential-less 401 never consumes a budget slot; only the runaway guard bounds it.
#[test]
fn missing_credential_never_charges_until_runaway_guard() {
    let mut schedule = AuthRetrySchedule::new();
    for i in 1..=AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS {
        assert_eq!(
            schedule.on_recovered_401(SentCredential::Missing),
            AuthRetryDecision::UnchargedResubmit {
                resubmit: i,
                delay: expected_uncharged_delay(i)
            },
        );
    }
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Missing),
        AuthRetryDecision::RunawayGuard {
            rejections: AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS + 1
        },
    );
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Sent),
        AuthRetryDecision::Backoff {
            attempt: 1,
            delay: Duration::from_secs(1)
        },
        "the credentialed budget must be untouched throughout"
    );
}

/// A success resets everything: the escalating delays, the attempt numbering, and the runaway counter all restart.
/// A 200 proves the session is not running away, so a productive multi-day turn can never accumulate into the guard.
#[test]
fn success_resets_budget_and_uncharged_counter() {
    let mut schedule = AuthRetrySchedule::new();
    schedule.on_recovered_401(SentCredential::Sent);
    schedule.on_recovered_401(SentCredential::Sent);
    for _ in 0..AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS {
        schedule.on_recovered_401(SentCredential::Missing);
    }
    schedule.reset_on_success();
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Missing),
        AuthRetryDecision::UnchargedResubmit {
            resubmit: 1,
            delay: Duration::from_secs(1)
        },
    );
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Sent),
        AuthRetryDecision::Backoff {
            attempt: 1,
            delay: Duration::from_secs(1)
        },
    );
}

/// Rule: an Api 5xx closes the charged incident but must not un-park.
#[test]
fn reset_incident_keeping_park_preserves_uncharged_state() {
    let mut schedule = AuthRetrySchedule::new();
    schedule.on_recovered_401(SentCredential::Missing);
    schedule.on_recovered_401(SentCredential::Missing);
    schedule.on_recovered_401(SentCredential::Sent);
    schedule.reset_incident_keeping_park();
    assert!(schedule.is_parked(), "the park must survive");
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Missing),
        AuthRetryDecision::UnchargedResubmit {
            resubmit: 3,
            delay: expected_uncharged_delay(3)
        },
        "uncharged counter and pace must survive"
    );
    assert_eq!(
        schedule.on_recovered_401(SentCredential::Sent),
        AuthRetryDecision::Backoff {
            attempt: 1,
            delay: Duration::from_secs(1)
        },
        "the charged incident restarts"
    );
}

/// Rule: the uncharged counter survives a suspend reset (the guard exists to span sleep cycles).
#[test]
fn suspend_reset_preserves_uncharged_counter() {
    let mut schedule = AuthRetrySchedule::new();
    let start = DualClock::now();
    schedule.on_recovered_401_at(SentCredential::Missing, start);
    schedule.on_recovered_401_at(SentCredential::Missing, start);
    schedule.on_recovered_401_at(SentCredential::Sent, start);

    let woke = after_suspend(start, Duration::from_secs(16 * 60));
    assert!(schedule.reset_if_incident_spans_suspend_at(woke));
    assert_eq!(
        schedule.on_recovered_401_at(SentCredential::Missing, woke),
        AuthRetryDecision::UnchargedResubmit {
            resubmit: 3,
            delay: expected_uncharged_delay(3)
        },
    );
    assert_eq!(
        schedule.on_recovered_401_at(SentCredential::Sent, woke),
        AuthRetryDecision::Backoff {
            attempt: 1,
            delay: Duration::from_secs(1)
        },
        "post-suspend 401 starts a fresh incident instead of exhausting"
    );
}

/// Suspend resets are capped until a success: a fault that persists across wakes must eventually exhaust instead of retrying forever.
/// A success restores the cap.
#[test]
fn suspend_resets_cap_without_success_and_rearm_on_success() {
    let mut schedule = AuthRetrySchedule::new();
    let mut now = DualClock::now();
    for _ in 0..AuthRetrySchedule::MAX_SUSPEND_RESETS {
        schedule.on_recovered_401_at(SentCredential::Sent, now);
        now = after_suspend(now, Duration::from_secs(16 * 60));
        assert!(schedule.reset_if_incident_spans_suspend_at(now));
    }
    schedule.on_recovered_401_at(SentCredential::Sent, now);
    now = after_suspend(now, Duration::from_secs(16 * 60));
    assert!(
        !schedule.reset_if_incident_spans_suspend_at(now),
        "reset {} must be refused: the budget is now allowed to exhaust",
        AuthRetrySchedule::MAX_SUSPEND_RESETS + 1
    );

    schedule.reset_on_success();
    schedule.on_recovered_401_at(SentCredential::Sent, now);
    now = after_suspend(now, Duration::from_secs(16 * 60));
    assert!(
        schedule.reset_if_incident_spans_suspend_at(now),
        "a success re-arms the suspend-reset cap"
    );
}

/// No suspend, no reset: wall drift below the suspend threshold (NTP jitter) and a schedule with no open incident are both no-ops.
#[test]
fn suspend_reset_requires_open_incident_and_real_drift() {
    let mut schedule = AuthRetrySchedule::new();
    let start = DualClock::now();
    assert!(
        !schedule
            .reset_if_incident_spans_suspend_at(after_suspend(start, Duration::from_secs(3600))),
        "no open incident: nothing to reset"
    );
    schedule.on_recovered_401_at(SentCredential::Sent, start);
    assert!(
        !schedule.reset_if_incident_spans_suspend_at(after_suspend(start, Duration::from_secs(5))),
        "5s wall drift is NTP-jitter territory, not a suspend"
    );
    assert_eq!(
        schedule.on_recovered_401_at(SentCredential::Sent, start),
        AuthRetryDecision::Backoff {
            attempt: 2,
            delay: Duration::from_secs(2)
        },
        "the failed reset checks must not charge the budget"
    );
}

/// Hard-expired OIDC manager (nothing wire-valid) for pace tests; tempdir must outlive it.
fn expired_manager_for_pace() -> (
    tempfile::TempDir,
    std::sync::Arc<distill_login::AuthManager>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = std::sync::Arc::new(distill_login::AuthManager::new(
        dir.path(),
        distill_login::GrokComConfig::default(),
    ));
    am.hot_swap(distill_login::GrokAuth {
        key: "expired-key".into(),
        auth_mode: distill_login::AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..distill_login::GrokAuth::test_default()
    });
    (dir, am)
}

/// Rule: a token landing mid-wait wakes the pace early. Non-Grok-authority
/// builds hold the full delay — the resolver stamps nothing to rescue.
#[tokio::test(start_paused = true)]
async fn pace_early_wake_on_token_landing_returns_promptly() {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    let (_dir, am) = expired_manager_for_pace();
    let started = tokio::time::Instant::now();
    let waker = am.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waker.hot_swap(distill_login::GrokAuth {
            key: "fresh-key".into(),
            auth_mode: distill_login::AuthMode::Oidc,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..distill_login::GrokAuth::test_default()
        });
        waker.refresh_notifier().notify_waiters();
    });
    let delay = Duration::from_secs(15);
    super::pace_uncharged_resubmit(super::RecoveredStore::SessionToken, Some(&am), delay).await;
    let elapsed = started.elapsed();
    if ActiveAuthBackend::default().is_xai_authority() {
        assert!(
            elapsed < Duration::from_secs(1),
            "token landed at 100ms; pace must return promptly, took {elapsed:?}"
        );
    } else {
        assert!(
            elapsed >= delay,
            "a token the resolver will not stamp must not release the pace, took {elapsed:?}"
        );
    }
}

/// Rule: early release matches the resolver's stamping predicate — an already-wire-valid
/// token is not slept on, but a non-Grok authority (which stamps nothing) never releases early.
#[tokio::test(start_paused = true)]
async fn pace_release_matches_resolver_authority_predicate() {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    let (_dir, am) = expired_manager_for_pace();
    // A cursor.com issuer keeps `current_wire_valid()` `Some` under both builds,
    // so only the authority predicate separates the branches.
    am.hot_swap(distill_login::GrokAuth {
        key: "fresh-key".into(),
        auth_mode: distill_login::AuthMode::Oidc,
        oidc_issuer: Some("https://cursor.com".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..distill_login::GrokAuth::test_default()
    });
    let started = tokio::time::Instant::now();
    super::pace_uncharged_resubmit(
        super::RecoveredStore::SessionToken,
        Some(&am),
        AuthRetrySchedule::UNCHARGED_PACE_CAP,
    )
    .await;
    let elapsed = started.elapsed();
    if ActiveAuthBackend::default().is_xai_authority() {
        assert!(
            elapsed < Duration::from_secs(1),
            "a wire-valid token must end the pace immediately, took {elapsed:?}"
        );
    } else {
        assert!(
            elapsed >= AuthRetrySchedule::UNCHARGED_PACE_CAP,
            "a token the resolver will not stamp must not release the pace, took {elapsed:?}"
        );
    }
}

/// Rule: a token landed without a notify (adoption paths) is observed within
/// a poll slice. Non-Grok-authority builds hold the full delay.
#[tokio::test(start_paused = true)]
async fn pace_wakes_on_token_adopted_without_notify() {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    let (_dir, am) = expired_manager_for_pace();
    let waker = am.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No notify: what the disk-adoption and config-watcher paths do.
        waker.hot_swap(distill_login::GrokAuth {
            key: "adopted-key".into(),
            auth_mode: distill_login::AuthMode::Oidc,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..distill_login::GrokAuth::test_default()
        });
    });
    let started = tokio::time::Instant::now();
    super::pace_uncharged_resubmit(
        super::RecoveredStore::SessionToken,
        Some(&am),
        AuthRetrySchedule::UNCHARGED_PACE_CAP,
    )
    .await;
    let elapsed = started.elapsed();
    if ActiveAuthBackend::default().is_xai_authority() {
        assert!(
            elapsed < Duration::from_secs(5),
            "an adopted token must end the pace within a poll slice, took {elapsed:?}"
        );
    } else {
        assert!(
            elapsed >= AuthRetrySchedule::UNCHARGED_PACE_CAP,
            "a token the resolver will not stamp must not release the pace, took {elapsed:?}"
        );
    }
}

/// Rule: a token another process writes to auth.json mid-park releases the pace within a
/// slice (parked turns suppress the dispatches that adopt disk); non-Grok authority sleeps full.
#[tokio::test(start_paused = true)]
async fn pace_adopts_token_written_to_auth_json_mid_park() {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    let (_dir, am) = expired_manager_for_pace();
    let path = am.auth_json_path().to_path_buf();
    let scope = ActiveAuthBackend::default().scope_key(&distill_login::GrokComConfig::default());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Disk write only — no hot_swap, no notify — what an external login does.
        let landed = distill_login::GrokAuth {
            key: "disk-landed-key".into(),
            auth_mode: distill_login::AuthMode::Oidc,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..distill_login::GrokAuth::test_default()
        };
        let store = std::collections::BTreeMap::from([(scope, landed)]);
        std::fs::write(&path, serde_json::to_string(&store).expect("serialize"))
            .expect("write auth.json");
    });
    let started = tokio::time::Instant::now();
    super::pace_uncharged_resubmit(
        super::RecoveredStore::SessionToken,
        Some(&am),
        AuthRetrySchedule::UNCHARGED_PACE_CAP,
    )
    .await;
    let elapsed = started.elapsed();
    if ActiveAuthBackend::default().is_xai_authority() {
        assert!(
            elapsed < Duration::from_secs(5),
            "a token landed on disk must end the pace within a poll slice, took {elapsed:?}"
        );
        assert_eq!(
            am.current_wire_valid().map(|a| a.key),
            Some("disk-landed-key".into()),
            "the pace must adopt the disk token so the resubmit carries it"
        );
    } else {
        assert!(
            elapsed >= AuthRetrySchedule::UNCHARGED_PACE_CAP,
            "a token the resolver will not stamp must not release the pace, took {elapsed:?}"
        );
    }
}

/// Rule: a notify without a token change re-arms for the remaining window — notify
/// bursts cannot turn parked resubmits into back-to-back sends.
#[tokio::test(start_paused = true)]
async fn pace_spurious_notify_holds_the_full_delay() {
    let (_dir, am) = expired_manager_for_pace();
    let started = tokio::time::Instant::now();
    let waker = am.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No token change: fire the notify alone.
        waker.refresh_notifier().notify_waiters();
    });
    let delay = Duration::from_secs(4);
    super::pace_uncharged_resubmit(super::RecoveredStore::SessionToken, Some(&am), delay).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= delay,
        "spurious notify must not shorten the pace, took {elapsed:?}"
    );
}

/// Rule: non-session stores sleep the full delay — nothing in the `AuthManager` to wait on.
#[tokio::test(start_paused = true)]
async fn pace_provider_store_sleeps_the_delay() {
    let (_dir, am) = expired_manager_for_pace();
    let started = tokio::time::Instant::now();
    let delay = Duration::from_secs(2);
    super::pace_uncharged_resubmit(super::RecoveredStore::AuthProvider, Some(&am), delay).await;
    assert_eq!(
        started.elapsed(),
        delay,
        "provider store must sleep the delay exactly"
    );
}
