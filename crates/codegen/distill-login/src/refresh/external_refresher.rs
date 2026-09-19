// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

use crate::ExternalRefreshError;
use crate::error::RefreshTokenFailedReason;
use crate::manager::{RefreshReason, refresh_failure_backoff};
use crate::model::CredentialGeneration;

use super::AuthSnapshot;
use super::ExternalCommandRunner;
use super::RefreshOutcome;
use super::TokenRefresher;

/// Escalate non-timeout `PreRequest` run failures to `PermanentFailure` after this many consecutive strikes.
/// A permanent verdict blocks unattended retry for `PERMANENT_FAILURE_TTL`, which on a headless box is a multi-minute window of turns sent with no credential at all.
/// A one-off provider blip (a briefly unreachable token authority, a spawn hiccup) must therefore stay transient and be retried. For an interactive-only provider that is exactly the wrong message, so those stay single-strike permanent and the client routes straight into the provider's login flow.
const MAX_CONSECUTIVE_RUN_FAILURES: u32 = 5;

/// Spacing between runs once no sendable token is left.
/// Every call is then a genuine attempt to get one, so the ladder's cooldown does not apply; this only folds a burst of concurrent pre-flights (startup makes several within a millisecond) into a single run.
const TOKENLESS_RUN_SPACING: Duration = Duration::from_secs(1);

/// How long after the `n`th consecutive failed run the binary is left alone while a sendable token is cached.
/// Every `auth()` in the early-invalidation window enters `refresh_chain` (each turn's pre-flight, every tool bearer lookup, the proactive loop), so without this the strike budget is spent at call rate, not wall-clock rate: a busy turn burns five strikes in seconds.
/// The cooldown makes the ladder time-based regardless of how often callers ask. It is the jitter-free [`refresh_failure_backoff`] schedule, so the proactive loop's wake (the same schedule plus jitter) always lands at or after the cooldown and gets a real run.
fn run_cooldown(strikes: u32) -> Duration {
    refresh_failure_backoff(strikes)
}

/// Consecutive non-timeout `PreRequest` run failures, when the last one landed, and the credential issuance they accrued against.
#[derive(Default)]
struct StrikeLadder {
    /// Issuance the strikes belong to (`None` when nothing is cached).
    /// A different issuance (login, hot-swap, sibling adoption — including a re-issue of the same opaque bearer with renewed metadata) starts a fresh ladder, so it neither inherits strikes nor sits out a cooldown it did not earn.
    generation: Option<CredentialGeneration>,
    strikes: u32,
    last_failure: Option<Instant>,
}

impl StrikeLadder {
    /// Re-scope to `generation`, dropping the strikes if the issuance changed.
    fn scope_to(&mut self, generation: Option<CredentialGeneration>) {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
    }

    /// Time left before the next run is allowed.
    /// With a sendable token cached the ladder's cooldown applies; without one only [`TOKENLESS_RUN_SPACING`] does, so a strike shortly before hard expiry cannot suppress the last refresh that could still put a credential on the wire.
    fn remaining_wait(&self, now: Instant, has_sendable_token: bool) -> Option<Duration> {
        let last = self.last_failure?;
        let wait = if has_sendable_token {
            run_cooldown(self.strikes)
        } else {
            TOKENLESS_RUN_SPACING
        };
        wait.checked_sub(now.saturating_duration_since(last))
            .filter(|d| !d.is_zero())
    }

    /// Record a failed run against the current issuance; returns the strike count.
    fn strike(&mut self, now: Instant) -> u32 {
        self.strikes += 1;
        self.last_failure = Some(now);
        self.strikes
    }
}

/// Refreshes by re-running the operator's external auth binary via the async external-command runner.
/// Returns data only; mutation lives in `refresh_chain` (honors the [`TokenRefresher`] no-mutation contract).
pub struct ExternalBinaryRefresher {
    runner: Arc<dyn ExternalCommandRunner>,
    snapshot: Arc<dyn AuthSnapshot>,
    command: String,
    ladder: Mutex<StrikeLadder>,
}

impl ExternalBinaryRefresher {
    pub fn new(
        runner: Arc<dyn ExternalCommandRunner>,
        snapshot: Arc<dyn AuthSnapshot>,
        command: String,
    ) -> Self {
        Self {
            runner,
            snapshot,
            command,
            ladder: Mutex::new(StrikeLadder::default()),
        }
    }

    /// The cached issuance the ladder scopes to, expired or not.
    fn cached_generation(&self) -> Option<CredentialGeneration> {
        self.snapshot
            .current()
            .or_else(|| self.snapshot.expired_auth())
            .map(|a| a.generation())
    }

    /// A permanent verdict; the reason is non-sticky so a flaky binary still recovers without the user once the TTL passes.
    fn record_permanent(&self, message: &str) -> RefreshOutcome {
        tracing::warn!(%message, "auth: external binary refresh failed permanently");
        // Reset so the next TTL window gets the full strike budget.
        *self.ladder.lock() = StrikeLadder::default();
        // No token key in the binary flow; the caller scopes the verdict.
        RefreshOutcome::permanent(RefreshTokenFailedReason::ProviderInteractiveRequired, None)
    }

    /// A failed run over a credential the server already rejected: single-strike permanent, no ladder.
    fn fail_rejected(&self, message: &str) -> RefreshOutcome {
        distill_telemetry::unified_log::warn(
            "auth: external binary refresh failed",
            None,
            Some(serde_json::json!({
                "message": message,
                "reason": format!("{:?}", RefreshReason::ServerRejected),
                "consecutive_failures": 1,
                "next_run_in_ms": serde_json::Value::Null,
            })),
        );
        self.record_permanent(message)
    }

    /// A failed unattended run: a strike on the ladder, transient until the budget is spent.
    fn fail_pre_request(&self, message: String) -> RefreshOutcome {
        let strikes = {
            let mut ladder = self.ladder.lock();
            ladder.scope_to(self.cached_generation());
            ladder.strike(Instant::now())
        };
        let escalates = strikes >= MAX_CONSECUTIVE_RUN_FAILURES;
        distill_telemetry::unified_log::warn(
            "auth: external binary refresh failed",
            None,
            Some(serde_json::json!({
                "message": &message,
                "reason": format!("{:?}", RefreshReason::PreRequest),
                "consecutive_failures": strikes,
                // After the escalating strike the next attempt is gated by the verdict TTL, not the cooldown
                "next_run_in_ms": (!escalates).then(|| run_cooldown(strikes).as_millis() as u64),
            })),
        );
        if escalates {
            self.record_permanent(&message)
        } else {
            RefreshOutcome::transient(message)
        }
    }
}

#[async_trait::async_trait]
impl TokenRefresher for ExternalBinaryRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {
        tracing::debug!(?reason, "auth: external binary refresh starting");
        // See MAX_CONSECUTIVE_RUN_FAILURES: only unattended pre-request renewals get the transient ladder.
        // A new reason must pick a side here rather than inherit one.
        let ladder_applies = match reason {
            RefreshReason::PreRequest => true,
            RefreshReason::ServerRejected => false,
        };
        if ladder_applies {
            let has_sendable_token = self.snapshot.has_sendable_token();
            let mut ladder = self.ladder.lock();
            ladder.scope_to(self.cached_generation());
            if let Some(remaining) = ladder.remaining_wait(Instant::now(), has_sendable_token) {
                let message = format!(
                    "external binary refresh backing off after {} failed run(s); next run in {:.0?}",
                    ladder.strikes, remaining
                );
                drop(ladder);
                distill_telemetry::unified_log::debug(
                    "auth: external binary refresh cooling down",
                    None,
                    None,
                );
                return RefreshOutcome::transient(message);
            }
        }
        match self.runner.run_external_command(&self.command).await {
            Ok(auth) => {
                distill_telemetry::unified_log::info(
                    "auth: external binary refresh succeeded",
                    None,
                    None,
                );
                *self.ladder.lock() = StrikeLadder::default();
                RefreshOutcome::success(auth)
            }
            // A timeout is the contract's interactive-required signal (conforming providers decline a headless `GROK_AUTH_EXPIRED=1` run fast; only one waiting on a human outlives the budget), so it stays a single-strike permanent verdict whatever the ladder says.
            Err(ExternalRefreshError::TimedOut) => {
                distill_telemetry::unified_log::warn(
                    "auth: external binary refresh timed out",
                    None,
                    None,
                );
                self.record_permanent("external binary timed out")
            }
            Err(ExternalRefreshError::Failed(message)) if ladder_applies => {
                self.fail_pre_request(message)
            }
            Err(ExternalRefreshError::Failed(message)) => self.fail_rejected(&message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GrokAuth;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Runner that yields scripted results in order, then `Failed`.
    struct FakeRunner {
        results: std::sync::Mutex<Vec<Result<GrokAuth, ExternalRefreshError>>>,
        calls: AtomicU32,
    }
    impl FakeRunner {
        fn new(results: Vec<Result<GrokAuth, ExternalRefreshError>>) -> Self {
            Self {
                results: std::sync::Mutex::new(results),
                calls: AtomicU32::new(0),
            }
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait::async_trait]
    impl ExternalCommandRunner for FakeRunner {
        async fn run_external_command(
            &self,
            _command: &str,
        ) -> Result<GrokAuth, ExternalRefreshError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                return Err(ExternalRefreshError::Failed("scripted failure".into()));
            }
            results.remove(0)
        }
    }

    /// Snapshot with a settable cached credential and send-horizon state.
    struct FakeSnapshot {
        cached: parking_lot::Mutex<Option<GrokAuth>>,
        sendable: AtomicBool,
    }
    impl FakeSnapshot {
        /// A cached credential still inside the buffer but sendable: the common proactive-refresh situation.
        fn sendable(key: &str) -> Arc<Self> {
            Arc::new(Self {
                cached: parking_lot::Mutex::new(Some(GrokAuth {
                    key: key.into(),
                    ..GrokAuth::test_default()
                })),
                sendable: AtomicBool::new(true),
            })
        }
        fn set_cached(&self, auth: Option<GrokAuth>) {
            *self.cached.lock() = auth;
        }
        fn set_sendable(&self, sendable: bool) {
            self.sendable.store(sendable, Ordering::SeqCst);
        }
    }
    impl AuthSnapshot for FakeSnapshot {
        fn current(&self) -> Option<GrokAuth> {
            None
        }
        fn expired_auth(&self) -> Option<GrokAuth> {
            self.cached.lock().clone()
        }
        fn read_disk_auth(&self) -> Option<GrokAuth> {
            None
        }
        fn is_expired(&self) -> bool {
            true
        }
        fn has_sendable_token(&self) -> bool {
            self.sendable.load(Ordering::SeqCst)
        }
    }

    fn refresher(
        runner: &Arc<FakeRunner>,
        snapshot: &Arc<FakeSnapshot>,
    ) -> ExternalBinaryRefresher {
        ExternalBinaryRefresher::new(runner.clone(), snapshot.clone(), "auth-binary".into())
    }

    fn failed() -> Result<GrokAuth, ExternalRefreshError> {
        Err(ExternalRefreshError::Failed("mint blip".into()))
    }

    fn fresh(key: &str) -> Result<GrokAuth, ExternalRefreshError> {
        Ok(GrokAuth {
            key: key.into(),
            ..GrokAuth::test_default()
        })
    }

    /// Step the paused tokio clock past the cooldown the ladder is currently enforcing.
    async fn wait_out_cooldown(strikes: u32) {
        tokio::time::advance(run_cooldown(strikes) + Duration::from_millis(1)).await;
    }

    /// A timed-out run is the contract's interactive-required signal and must stay a single-strike permanent verdict.
    /// It is NON-sticky: it has to age out via the TTL, never lock an external-binary user out forever.
    #[tokio::test]
    async fn external_binary_timeout_is_single_strike_non_sticky_permanent() {
        let runner = Arc::new(FakeRunner::new(vec![Err(ExternalRefreshError::TimedOut)]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(
                    error.reason,
                    RefreshTokenFailedReason::ProviderInteractiveRequired
                );
                assert!(
                    !error.reason.is_sticky(),
                    "external-binary failure must age out, not strand the user forever",
                );
            }
            other => panic!("a timed-out binary run must be a permanent failure, got {other:?}"),
        }
        assert_eq!(runner.calls(), 1, "the single run gets the whole 7s budget");
    }

    /// A `ServerRejected` refresh means a user-facing 401 is already in hand.
    /// A failed run must stay single-strike permanent so the turn surfaces the provider-login remedy instead of self-healing "wait it out" advice.
    #[tokio::test]
    async fn external_binary_server_rejected_failure_is_single_strike_permanent() {
        let runner = Arc::new(FakeRunner::new(vec![failed()]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(
                    error.reason,
                    RefreshTokenFailedReason::ProviderInteractiveRequired
                );
                assert!(!error.reason.is_sticky());
            }
            other => {
                panic!("a failed run over a rejected credential must be permanent, got {other:?}")
            }
        }
        assert_eq!(runner.calls(), 1);
    }

    /// A `ServerRejected` run ignores the ladder's cooldown: the 401 recovery needs a real verdict now, not a "still cooling down" transient.
    #[tokio::test(start_paused = true)]
    async fn server_rejected_run_bypasses_the_cooldown() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { .. }
        ));
        assert!(matches!(
            refresher.refresh(RefreshReason::ServerRejected).await,
            RefreshOutcome::PermanentFailure { .. }
        ));
        assert_eq!(
            runner.calls(),
            2,
            "the rejected-credential run must not be skipped"
        );
    }

    /// A non-timeout `PreRequest` run failure proves nothing about the credential.
    /// It must stay transient below the strike budget, so a one-off mint blip cannot cost a headless devbox a `PERMANENT_FAILURE_TTL` window of turns sent with no credential.
    #[tokio::test(start_paused = true)]
    async fn external_binary_run_failure_is_transient_below_the_strike_budget() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        for attempt in 1..MAX_CONSECUTIVE_RUN_FAILURES {
            match refresher.refresh(RefreshReason::PreRequest).await {
                RefreshOutcome::TransientFailure { .. } => {}
                other => panic!("attempt {attempt} must stay transient, got {other:?}"),
            }
            wait_out_cooldown(attempt).await;
        }
        assert_eq!(runner.calls(), MAX_CONSECUTIVE_RUN_FAILURES - 1);
    }

    /// Calls inside the cooldown do not run the binary and do not count as strikes.
    /// The ladder therefore measures wall-clock time, not how many pre-flights a busy turn makes.
    #[tokio::test(start_paused = true)]
    async fn calls_inside_the_cooldown_neither_run_nor_strike() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { .. }
        ));
        // A burst of pre-flights well past the strike budget, all inside strike 1's cooldown.
        for _ in 0..(MAX_CONSECUTIVE_RUN_FAILURES * 4) {
            match refresher.refresh(RefreshReason::PreRequest).await {
                RefreshOutcome::TransientFailure { message } => {
                    assert!(message.contains("backing off"), "message={message}")
                }
                other => panic!("a cooling-down call must be a transient no-op, got {other:?}"),
            }
        }
        assert_eq!(
            runner.calls(),
            1,
            "the binary ran once; the burst was absorbed"
        );
        assert_eq!(refresher.ladder.lock().strikes, 1);

        // Once the cooldown passes the next call runs again and lands strike 2.
        wait_out_cooldown(1).await;
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { .. }
        ));
        assert_eq!(runner.calls(), 2);
        assert_eq!(refresher.ladder.lock().strikes, 2);
    }

    /// Once no sendable token is left the ladder's cooldown must not hold the binary back: that run is the only way a request still carries a credential.
    /// Only the short burst spacing applies.
    #[tokio::test(start_paused = true)]
    async fn cooldown_does_not_suppress_a_run_once_the_token_is_no_longer_sendable() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let snapshot = FakeSnapshot::sendable("k");
        let refresher = refresher(&runner, &snapshot);
        // Strikes 1..3 while the token is sendable: the cooldown climbs to 20 s.
        for strike in 1..=3 {
            assert!(matches!(
                refresher.refresh(RefreshReason::PreRequest).await,
                RefreshOutcome::TransientFailure { .. }
            ));
            if strike < 3 {
                wait_out_cooldown(strike).await;
            }
        }
        assert_eq!(runner.calls(), 3);
        // 2 s into strike 3's 20 s cooldown the token crosses the send horizon.
        tokio::time::advance(Duration::from_secs(2)).await;
        snapshot.set_sendable(false);
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { .. }
        ));
        assert_eq!(
            runner.calls(),
            4,
            "a tokenless call runs despite the cooldown"
        );
        // A burst right after is still folded into that one run …
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { message } if message.contains("backing off")
        ));
        assert_eq!(runner.calls(), 4);
        // … and the spacing, not the ladder's cooldown, decides when the next one may run.
        tokio::time::advance(TOKENLESS_RUN_SPACING + Duration::from_millis(1)).await;
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::PermanentFailure { .. }
        ));
        assert_eq!(runner.calls(), 5, "the fifth strike escalates as usual");
    }

    /// The ladder is scoped to the cached credential: strikes accrued against one token must not carry over to a token installed by login, hot-swap, or sibling adoption.
    #[tokio::test(start_paused = true)]
    async fn a_new_credential_starts_a_fresh_ladder() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let snapshot = FakeSnapshot::sendable("old");
        let refresher = refresher(&runner, &snapshot);
        // Four strikes against "old": one more would escalate, and its cooldown is 40 s.
        for strike in 1..MAX_CONSECUTIVE_RUN_FAILURES {
            assert!(matches!(
                refresher.refresh(RefreshReason::PreRequest).await,
                RefreshOutcome::TransientFailure { .. }
            ));
            if strike < MAX_CONSECUTIVE_RUN_FAILURES - 1 {
                wait_out_cooldown(strike).await;
            }
        }
        assert_eq!(
            refresher.ladder.lock().strikes,
            MAX_CONSECUTIVE_RUN_FAILURES - 1
        );

        // Another process installs a new credential; this refresher sees it on its next call.
        let new_cred = GrokAuth {
            key: "new".into(),
            ..GrokAuth::test_default()
        };
        snapshot.set_cached(Some(new_cred.clone()));
        match refresher.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::TransientFailure { message } => assert!(
                !message.contains("backing off"),
                "a new credential must not sit out the old one's cooldown: {message}"
            ),
            other => panic!("a new credential must get a fresh budget, got {other:?}"),
        }
        assert_eq!(
            runner.calls(),
            MAX_CONSECUTIVE_RUN_FAILURES,
            "the new credential's run happened"
        );
        let ladder = refresher.ladder.lock();
        assert_eq!(ladder.generation, Some(new_cred.generation()));
        assert_eq!(
            ladder.strikes, 1,
            "strike 1 of the new ladder, not strike 5 of the old"
        );
    }

    /// The scope is the issuance, not the bearer string: an authority re-issuing the same opaque token with renewed metadata is a new credential and gets a fresh ladder.
    #[tokio::test(start_paused = true)]
    async fn a_reissued_bearer_with_renewed_metadata_starts_a_fresh_ladder() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let snapshot = FakeSnapshot::sendable("same-bearer");
        let refresher = refresher(&runner, &snapshot);
        for strike in 1..MAX_CONSECUTIVE_RUN_FAILURES {
            assert!(matches!(
                refresher.refresh(RefreshReason::PreRequest).await,
                RefreshOutcome::TransientFailure { .. }
            ));
            if strike < MAX_CONSECUTIVE_RUN_FAILURES - 1 {
                wait_out_cooldown(strike).await;
            }
        }
        assert_eq!(
            refresher.ladder.lock().strikes,
            MAX_CONSECUTIVE_RUN_FAILURES - 1
        );

        // Same key, later expiry and mint time: a different issuance.
        let reissued = GrokAuth {
            key: "same-bearer".into(),
            create_time: chrono::Utc::now(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(15)),
            ..GrokAuth::test_default()
        };
        snapshot.set_cached(Some(reissued.clone()));
        match refresher.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::TransientFailure { message } => assert!(
                !message.contains("backing off"),
                "a re-issued credential must not inherit the old cooldown: {message}"
            ),
            other => panic!(
                "a re-issued credential must not escalate on its first failure, got {other:?}"
            ),
        }
        let ladder = refresher.ladder.lock();
        assert_eq!(ladder.generation, Some(reissued.generation()));
        assert_eq!(ladder.strikes, 1);
    }

    /// A `PreRequest` timeout is the interactive-required signal on the path where the ladder would otherwise apply: single-strike permanent, nothing recorded on the ladder.
    #[tokio::test(start_paused = true)]
    async fn pre_request_timeout_is_single_strike_permanent_and_leaves_no_strike() {
        let runner = Arc::new(FakeRunner::new(vec![Err(ExternalRefreshError::TimedOut)]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        match refresher.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(
                    error.reason,
                    RefreshTokenFailedReason::ProviderInteractiveRequired
                );
                assert!(!error.reason.is_sticky());
            }
            other => {
                panic!("a PreRequest timeout must be permanent on the first run, got {other:?}")
            }
        }
        assert_eq!(runner.calls(), 1);
        let ladder = refresher.ladder.lock();
        assert_eq!(ladder.strikes, 0, "a timeout is a verdict, not a strike");
        assert!(
            ladder.last_failure.is_none(),
            "no cooldown is armed after a verdict"
        );
    }

    /// A timeout after accumulated strikes escalates immediately and resets the ladder, so the next TTL window starts clean.
    #[tokio::test(start_paused = true)]
    async fn timeout_after_accumulated_strikes_is_permanent_and_resets_the_ladder() {
        let runner = Arc::new(FakeRunner::new(vec![
            failed(),
            failed(),
            Err(ExternalRefreshError::TimedOut),
        ]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        for strike in 1..=2 {
            assert!(matches!(
                refresher.refresh(RefreshReason::PreRequest).await,
                RefreshOutcome::TransientFailure { .. }
            ));
            wait_out_cooldown(strike).await;
        }
        assert_eq!(refresher.ladder.lock().strikes, 2);
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::PermanentFailure { .. }
        ));
        let ladder = refresher.ladder.lock();
        assert_eq!(ladder.strikes, 0, "the verdict resets the ladder");
        assert!(ladder.last_failure.is_none());
    }

    /// A `ServerRejected` failure must not touch the ladder at all: it never counts toward the `PreRequest` budget and never arms a cooldown.
    #[tokio::test(start_paused = true)]
    async fn server_rejected_failure_does_not_touch_the_ladder() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::TransientFailure { .. }
        ));
        wait_out_cooldown(1).await;
        assert!(matches!(
            refresher.refresh(RefreshReason::ServerRejected).await,
            RefreshOutcome::PermanentFailure { .. }
        ));
        // The verdict reset the ladder; the rejected run itself left no strike behind.
        let ladder = refresher.ladder.lock();
        assert_eq!(ladder.strikes, 0);
        assert!(ladder.last_failure.is_none());
    }

    /// The cooldown and the proactive loop's failure backoff are one schedule, so the loop's wake never lands inside the cooldown.
    #[test]
    fn proactive_backoff_never_undercuts_the_cooldown() {
        for n in 1..=12 {
            assert!(
                crate::manager::proactive_failure_backoff(n) >= run_cooldown(n),
                "strike {n}: the proactive wake must land at or after the cooldown"
            );
        }
        assert_eq!(run_cooldown(0), Duration::ZERO);
    }

    /// The cooldown doubles per strike, so the budget stretches across the early-invalidation buffer instead of the first few seconds of it.
    #[test]
    fn cooldown_doubles_per_strike() {
        assert_eq!(run_cooldown(1), Duration::from_secs(5));
        assert_eq!(run_cooldown(2), Duration::from_secs(10));
        assert_eq!(run_cooldown(4), Duration::from_secs(40));
        // Time from strike 1 to the escalating strike: the sum of the intervening cooldowns.
        let span: Duration = (1..MAX_CONSECUTIVE_RUN_FAILURES).map(run_cooldown).sum();
        assert_eq!(span, Duration::from_secs(75));
        assert_eq!(
            run_cooldown(u32::MAX),
            crate::manager::BACKOFF_INTERVAL,
            "capped at the proactive loop's ceiling"
        );
    }

    /// Consecutive run failures escalate to the same non-sticky permanent verdict on the final strike.
    /// A genuinely broken provider still backs off for the TTL instead of retrying forever.
    #[tokio::test(start_paused = true)]
    async fn external_binary_run_failures_escalate_on_the_final_strike() {
        let runner = Arc::new(FakeRunner::new(vec![]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        for strike in 1..MAX_CONSECUTIVE_RUN_FAILURES {
            match refresher.refresh(RefreshReason::PreRequest).await {
                RefreshOutcome::TransientFailure { .. } => {}
                other => panic!("pre-strike attempts must stay transient, got {other:?}"),
            }
            wait_out_cooldown(strike).await;
        }
        match refresher.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(
                    error.reason,
                    RefreshTokenFailedReason::ProviderInteractiveRequired
                );
                assert!(!error.reason.is_sticky());
            }
            other => panic!("the final strike must escalate to permanent, got {other:?}"),
        }
        // The escalation resets the budget: the next window starts transient and with no cooldown.
        match refresher.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::TransientFailure { .. } => {}
            other => panic!("the next TTL window gets a fresh budget, got {other:?}"),
        }
        assert_eq!(runner.calls(), MAX_CONSECUTIVE_RUN_FAILURES + 1);
    }

    /// A success resets the strike ladder, so isolated blips spread across a long-lived refresher never accumulate into an escalation.
    #[tokio::test(start_paused = true)]
    async fn external_binary_success_resets_the_strike_budget() {
        let runner = Arc::new(FakeRunner::new(vec![
            failed(),
            failed(),
            fresh("ext-fresh"),
            failed(),
            failed(),
        ]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        for strike in 1..=2 {
            assert!(matches!(
                refresher.refresh(RefreshReason::PreRequest).await,
                RefreshOutcome::TransientFailure { .. }
            ));
            wait_out_cooldown(strike).await;
        }
        assert!(matches!(
            refresher.refresh(RefreshReason::PreRequest).await,
            RefreshOutcome::Success(_)
        ));
        // Two more failures are strikes 1 and 2 of a fresh ladder, not 3 and 4 of the old one.
        for strike in 1..=2 {
            match refresher.refresh(RefreshReason::PreRequest).await {
                RefreshOutcome::TransientFailure { .. } => {}
                other => panic!("post-success failures must restart the ladder, got {other:?}"),
            }
            assert_eq!(refresher.ladder.lock().strikes, strike);
            wait_out_cooldown(strike).await;
        }
        assert_eq!(runner.calls(), 5);
    }

    #[tokio::test]
    async fn external_binary_success_returns_fresh_token() {
        let runner = Arc::new(FakeRunner::new(vec![fresh("ext-fresh")]));
        let refresher = refresher(&runner, &FakeSnapshot::sendable("k"));
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::Success(auth) => assert_eq!(auth.key, "ext-fresh"),
            other => panic!("a successful binary run must return Success, got {other:?}"),
        }
        assert_eq!(
            runner.calls(),
            1,
            "a success must run the binary exactly once"
        );
    }
}
