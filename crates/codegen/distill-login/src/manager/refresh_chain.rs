// Modified for Distill by Samuel Fajreldines, 2026.
//! The refresh protocol that runs under the lock: the [`RefreshStep`] machine and its per-step methods.
//! Mutation stays in `manager.rs` (`apply_refresh_outcome`).

use std::sync::Arc;

use crate::error::AuthError;
use crate::model::GrokAuth;
use crate::refresh::{RefreshReason, TokenRefresher};
use crate::storage::AuthFileLock;

use super::lock::{self, LockAcquire};
use super::sleep_gate::InFlightGuard;
use super::{AuthManager, LOCK_TIMEOUT_WAIT, REFRESH_LOCK_TIMEOUT, TokenType};

/// `Held` is the live lock, proven before the irreversible IdP call.
/// `Adopted` is a sibling's freshly rotated token; return it without refreshing.
pub(super) enum LockOutcome {
    Held(AuthFileLock),
    Adopted(Box<GrokAuth>),
}

enum LockFailure {
    TimedOut { holder: Option<lock::LockHolder> },
    Io { error: std::io::Error },
}

/// One refresh attempt; only `Exchange` spends the refresh token.
enum RefreshStep {
    Recheck,
    AdoptBeforeLock,
    AcquireLock,
    DeferForPowerState(ActiveRefresh),
    RevalidateLock(ActiveRefresh),
    Exchange(ActiveRefresh),
    Refreshed(Box<GrokAuth>),
    Failed(AuthError),
}

/// State owned from file-lock acquisition through the exchange.
struct ActiveRefresh {
    file_lock: AuthFileLock,
    refresher: Arc<dyn TokenRefresher>,
    attempted_key: Option<String>,
}

/// Why a not-yet-started refresh must wait for the power state.
enum RefreshDeferral {
    /// The sleep gate is raised; an exchange would straddle the suspend.
    SleepImminent { has_live_token: bool },
    /// A dark wake could re-sleep mid-exchange; this applies only while a wire-valid token makes waiting free.
    DarkWake,
}

impl AuthManager {
    /// Runs one refresh attempt; all persistence and verdict recording happen in `apply_refresh_outcome`, the single mutation point.
    /// Callers that can be cancelled mid-exchange must go through `BoundedRefresh`/`SilentRefresh`, which spawn the exchange rather than drop it.
    /// A dropped exchange loses the rotated token.
    #[tracing::instrument(skip(self), fields(?token_type, ?reason))]
    pub async fn refresh_chain(
        self: &Arc<Self>,
        token_type: TokenType,
        reason: RefreshReason,
    ) -> Result<GrokAuth, AuthError> {
        // Checked before the refresh lock so a backed-off chain doesn't block traffic.
        if let Some(err) = self.permanent_failure() {
            if let Some(refreshed) = self.try_adopt_disk_token(
                reason,
                "auth: adopted sibling token during PermanentFailure short-circuit",
            ) {
                return Ok(refreshed);
            }
            // Debug: the verdict transition is already logged once by `record_permanent_failure`.
            distill_telemetry::unified_log::debug(
                "auth: refresh_chain short-circuit on permanent failure",
                /*sid*/ None,
                Some(serde_json::json!({
                    "token_type": format!("{token_type:?}"),
                    "reason": format!("{reason:?}"),
                    "failure": format!("{err}"),
                })),
            );
            return Err(err);
        }

        let pre_lock_key = self.current().map(|a| a.key.clone());

        let _guard = self.refresh_lock.lock().await;

        let mut step = RefreshStep::Recheck;
        loop {
            step = match step {
                RefreshStep::Recheck => {
                    // A ServerRejected token counts only if it changed, since the unchanged one still needs fresh claims
                    if let Some(auth) = self.current()
                        && (reason != RefreshReason::ServerRejected
                            || pre_lock_key.as_deref() != Some(&auth.key))
                    {
                        RefreshStep::Refreshed(Box::new(auth))
                    } else if let Some(err) = self.permanent_failure() {
                        // Re-checked under the mutex so a 401 burst costs one IdP call.
                        RefreshStep::Failed(err)
                    } else {
                        RefreshStep::AdoptBeforeLock
                    }
                }
                // Adopting before the flock keeps waiters from queuing on the lock; this is safe only under the mutex, after the re-checks above
                RefreshStep::AdoptBeforeLock => {
                    match self.try_adopt_disk_token(
                        reason,
                        "auth: refresh adopted sibling token pre-lock",
                    ) {
                        Some(refreshed) => RefreshStep::Refreshed(Box::new(refreshed)),
                        None => RefreshStep::AcquireLock,
                    }
                }
                RefreshStep::AcquireLock => {
                    match self.acquire_refresh_lock_or_adopt(reason).await {
                        Ok(LockOutcome::Adopted(auth)) => RefreshStep::Refreshed(auth),
                        Ok(LockOutcome::Held(file_lock)) => match self.refresher.read().clone() {
                            Some(refresher) => RefreshStep::DeferForPowerState(ActiveRefresh {
                                file_lock,
                                refresher,
                                attempted_key: self.attempted_verdict_key(reason),
                            }),
                            None => {
                                tracing::warn!("auth: no refresher configured");
                                RefreshStep::Failed(AuthError::transient("no refresher configured"))
                            }
                        },
                        Err(err) => RefreshStep::Failed(err),
                    }
                }
                RefreshStep::DeferForPowerState(active) => {
                    match self.defer_refresh_for_power_state(reason) {
                        Ok(()) => RefreshStep::RevalidateLock(active),
                        Err(err) => RefreshStep::Failed(err),
                    }
                }
                RefreshStep::RevalidateLock(ActiveRefresh {
                    file_lock,
                    refresher,
                    attempted_key,
                }) => match self.revalidate_lock_or_reacquire(file_lock, reason).await {
                    Ok(LockOutcome::Held(file_lock)) => RefreshStep::Exchange(ActiveRefresh {
                        file_lock,
                        refresher,
                        attempted_key,
                    }),
                    Ok(LockOutcome::Adopted(auth)) => RefreshStep::Refreshed(auth),
                    Err(err) => RefreshStep::Failed(err),
                },
                RefreshStep::Exchange(active) => {
                    match self.exchange_refresh_token(active, reason).await {
                        Ok(auth) => RefreshStep::Refreshed(Box::new(auth)),
                        Err(err) => RefreshStep::Failed(err),
                    }
                }
                RefreshStep::Refreshed(auth) => return Ok(*auth),
                RefreshStep::Failed(err) => return Err(err),
            };
        }
    }

    /// Sends the refresh token under the in-flight guard and applies the outcome.
    async fn exchange_refresh_token(
        self: &Arc<Self>,
        active: ActiveRefresh,
        reason: RefreshReason,
    ) -> Result<GrokAuth, AuthError> {
        let ActiveRefresh {
            file_lock,
            refresher,
            attempted_key,
        } = active;
        // Never abort an in-flight exchange: the IdP may already have rotated the token.
        let outcome = {
            // Claim the slot before re-checking the gate: a sleep either sees our slot (and waits for us) or we see its gate and back out
            let _in_flight = InFlightGuard::new(self);
            if self.is_sleep_gated() {
                distill_telemetry::unified_log::warn(
                    "auth.sleep.refresh_deferred",
                    /*sid*/ None,
                    Some(serde_json::json!({
                        "reason": format!("{reason:?}"),
                        "has_live_token": self.current().is_some(),
                        "stage": "pre_idp",
                    })),
                );
                return Err(AuthError::transient(
                    "refresh deferred: system sleep imminent",
                ));
            }
            // A dark wake sends no `WillSleep`, so hold the system awake for the exchange.
            let _awake = if self.is_dark_wake() {
                distill_telemetry::unified_log::debug(
                    "auth.refresh.dark_wake_assertion",
                    /*sid*/ None,
                    Some(serde_json::json!({ "reason": format!("{reason:?}") })),
                );
                distill_system_power::hold_awake("grok: OIDC token refresh")
            } else {
                None
            };
            refresher.refresh(reason).await
        };
        self.apply_refresh_outcome(outcome, reason, attempted_key, &file_lock)
            .await
    }

    /// On lock timeout, adopts a sibling's fresh token or returns transient; it never proceeds unlocked.
    pub(super) async fn acquire_refresh_lock_or_adopt(
        &self,
        reason: RefreshReason,
    ) -> Result<LockOutcome, AuthError> {
        let lock_started = std::time::Instant::now();
        let acquire = self
            .try_lock_auth_file_async(REFRESH_LOCK_TIMEOUT, lock::Heartbeat::Attach)
            .await;
        self.resolve_refresh_acquire(
            acquire,
            lock_started,
            reason,
            "auth: refresh used disk token",
        )
        .await
    }

    /// Every refresh-path `LockAcquire` resolves here; refresh callers never touch `into_guard`.
    async fn resolve_refresh_acquire(
        &self,
        acquire: LockAcquire,
        lock_started: std::time::Instant,
        reason: RefreshReason,
        adopt_msg: &'static str,
    ) -> Result<LockOutcome, AuthError> {
        let file_lock = match acquire {
            LockAcquire::Acquired(lock) => lock,
            LockAcquire::TimedOut { holder } => {
                return self
                    .adopt_or_bail_without_lock(
                        reason,
                        lock_started,
                        LockFailure::TimedOut { holder },
                    )
                    .await;
            }
            LockAcquire::Failed { error } => {
                return self
                    .adopt_or_bail_without_lock(reason, lock_started, LockFailure::Io { error })
                    .await;
            }
        };
        if let Some(refreshed) = self.try_adopt_disk_token(reason, adopt_msg) {
            return Ok(LockOutcome::Adopted(Box::new(refreshed)));
        }
        Ok(LockOutcome::Held(file_lock))
    }

    /// Wait out the holder, adopt its token, or return transient; never proceed unlocked.
    async fn adopt_or_bail_without_lock(
        &self,
        reason: RefreshReason,
        lock_started: std::time::Instant,
        failure: LockFailure,
    ) -> Result<LockOutcome, AuthError> {
        let elapsed_ms = lock_started.elapsed().as_millis() as u64;
        let mut payload = serde_json::json!({
            "elapsed_ms": elapsed_ms,
            "reason": format!("{reason:?}"),
        });
        match &failure {
            LockFailure::TimedOut { holder } => {
                tracing::warn!("auth: file lock timed out, waiting for sibling to finish");
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("outcome".into(), "timed_out".into());
                    obj.insert("timeout_ms".into(), elapsed_ms.into());
                    obj.insert(
                        "holder_pid".into(),
                        serde_json::json!(holder.and_then(|h| h.pid)),
                    );
                    obj.insert(
                        "holder_state".into(),
                        serde_json::json!(holder.map(|h| h.state.label())),
                    );
                    obj.insert(
                        "holder_age_secs".into(),
                        serde_json::json!(holder.and_then(|h| h.age_secs)),
                    );
                }
            }
            LockFailure::Io { error } => {
                tracing::warn!(error = %error, "auth lock: acquire failed (io)");
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("outcome".into(), "io_failed".into());
                    obj.insert("error".into(), error.to_string().into());
                }
            }
        }
        distill_telemetry::unified_log::warn(
            "auth.refresh.lock_timeout",
            /*sid*/ None,
            Some(payload),
        );
        tokio::time::sleep(LOCK_TIMEOUT_WAIT).await;
        if let Some(refreshed) = self.try_adopt_disk_token(
            reason,
            "auth: refresh adopted sibling token after lock timeout",
        ) {
            return Ok(LockOutcome::Adopted(Box::new(refreshed)));
        }
        tracing::warn!("auth: returning transient to avoid refresh token reuse");
        let message = match failure {
            LockFailure::TimedOut { holder } => {
                let holder_hint = match holder.and_then(|h| h.pid) {
                    Some(pid) => format!(" (holder pid {pid})"),
                    None => String::new(),
                };
                format!(
                    "could not acquire auth.json.lock within timeout{holder_hint}; \
                     sibling may be mid-refresh"
                )
            }
            LockFailure::Io { error } => {
                format!("could not open or lock auth.json.lock: {error}")
            }
        };
        Err(AuthError::transient(message))
    }

    fn power_state_deferral(&self, reason: RefreshReason) -> Option<RefreshDeferral> {
        if self.is_sleep_gated() {
            return Some(RefreshDeferral::SleepImminent {
                has_live_token: self.current().is_some(),
            });
        }
        if reason == RefreshReason::PreRequest
            && self.current_wire_valid().is_some()
            && self.should_defer_for_dark_wake()
        {
            return Some(RefreshDeferral::DarkWake);
        }
        None
    }

    /// Safe to defer: the refresh token has not been sent yet.
    fn defer_refresh_for_power_state(&self, reason: RefreshReason) -> Result<(), AuthError> {
        match self.power_state_deferral(reason) {
            Some(RefreshDeferral::SleepImminent { has_live_token }) => {
                distill_telemetry::unified_log::warn(
                    "auth.sleep.refresh_deferred",
                    /*sid*/ None,
                    Some(serde_json::json!({
                        "reason": format!("{reason:?}"),
                        "has_live_token": has_live_token,
                    })),
                );
                Err(AuthError::transient(
                    "refresh deferred: system sleep imminent",
                ))
            }
            Some(RefreshDeferral::DarkWake) => {
                distill_telemetry::unified_log::warn(
                    "auth.dark_wake.refresh_deferred",
                    /*sid*/ None,
                    Some(serde_json::json!({ "reason": format!("{reason:?}") })),
                );
                Err(AuthError::transient(
                    "refresh deferred: dark wake (display off; system may re-sleep)",
                ))
            }
            None => {
                self.end_dark_wake_defer_run();
                Ok(())
            }
        }
    }

    // TODO: deletable with `AuthFileLock::still_live`.
    /// Re-locks if the lock file was replaced under us; adopts a sibling's fresh token if one landed.
    pub(super) async fn revalidate_lock_or_reacquire(
        &self,
        file_lock: AuthFileLock,
        reason: RefreshReason,
    ) -> Result<LockOutcome, AuthError> {
        if file_lock.still_live(&self.path) {
            return Ok(LockOutcome::Held(file_lock));
        }
        distill_telemetry::unified_log::warn(
            "auth.refresh.lock_lost_before_idp",
            /*sid*/ None,
            Some(serde_json::json!({ "reason": format!("{reason:?}") })),
        );
        let replacer = lock::read_holder_at(&self.path);
        distill_telemetry::session_ctx::log_event(
            distill_telemetry::events::AuthLockReplacedOutFromUnder {
                holder_pid: replacer.and_then(|h| h.pid),
                holder_state: replacer.map(|h| h.state.label()),
                holder_age_secs: replacer.and_then(|h| h.age_secs),
            },
        );
        drop(file_lock);
        let lock_started = std::time::Instant::now();
        let acquire = self
            .try_lock_auth_file_async(REFRESH_LOCK_TIMEOUT, lock::Heartbeat::Attach)
            .await;
        self.resolve_refresh_acquire(
            acquire,
            lock_started,
            reason,
            "auth: adopted sibling token after lock-loss revalidation",
        )
        .await
    }
}
