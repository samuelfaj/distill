// Modified for Distill by Samuel Fajreldines, 2026.
//! What it takes to get a session back to a usable credential, and the one
//! bounded unattended attempt the startup paths make before asking the user.
use super::{AuthManager, RefreshReason};
use crate::error::AuthError;
use crate::model::GrokAuth;
use crate::token_type::TokenType;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
/// The way back to a usable credential, as of right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRemedy {
    /// A later unattended refresh can still succeed; in the field, almost always a launch seconds after wake, before the network is up.
    SelfHealing,
    /// Only an interactive run of the operator's auth provider can mint one.
    ProviderLogin { label: Option<String> },
    /// Only a user-driven login can.
    ManualLogin,
}
impl AuthRemedy {
    pub fn is_self_healing(&self) -> bool {
        matches!(self, Self::SelfHealing)
    }
    /// `error_type` for a turn that died on this credential.
    pub fn turn_error_type(&self) -> &'static str {
        match self {
            Self::SelfHealing => "auth_transient",
            Self::ProviderLogin { .. } | Self::ManualLogin => "auth",
        }
    }
    /// The same remedy, for a turn that has already spent its automatic retries.
    /// [`Self::SelfHealing`] cannot survive that: its whole message is "retry in a few seconds", exactly what just failed several times over.
    /// What is left is a plain re-authentication, no advice of our own, and classified so the client offers its own way back.
    pub fn after_retries_exhausted(self) -> Self {
        match self {
            Self::SelfHealing => Self::ManualLogin,
            provider_or_manual => provider_or_manual,
        }
    }
    /// What to tell the user beyond the failure itself.
    pub fn advice(&self) -> Option<String> {
        match self {
            Self::SelfHealing => Some(
                "Authentication is temporarily unavailable (often a network blip right \
                 after wake). Your session is still signed in and will recover \
                 automatically — retry in a few seconds; no need to run /login."
                    .to_owned(),
            ),
            Self::ProviderLogin { label } => {
                Some(crate::error::provider_login_message(label.as_deref()).into_owned())
            }
            Self::ManualLogin => None,
        }
    }
}
/// Outcome of a bounded best-effort mint.
/// Callers must distinguish "the deadline elapsed with the exchange still in flight" (spawned, not dropped) from a refresh that resolved.
/// Forcing a second mint after a deadline only queues behind the detached exchange for up to another full budget.
pub enum BoundedRefresh {
    /// The chain finished inside the budget with this result.
    /// Boxed like [`SilentRefresh::Renewed`]: `GrokAuth` is large and the other variant is unit-sized.
    Resolved(Box<Result<GrokAuth, AuthError>>),
    /// The spawned chain outlived the budget and continues in the background (persisting and hot-swapping any minted token when it lands).
    DeadlineElapsed,
}
/// What a [`AuthManager::silent_refresh`] attempt leaves the caller holding.
#[derive(Debug, Clone)]
pub enum SilentRefresh {
    /// The credential [`AuthManager::auth`] vouched for, the one the next request would carry.
    /// Carried, not re-read: `auth()` also succeeds on its grace arm, serving a token still wire-valid but inside the early-invalidation buffer. [`AuthManager::current`] hides exactly that token.
    /// A caller that answered `Renewed` with `current()` would reject the session this outcome just accepted. It would also disagree with the `Failed(SelfHealing)` arm on the very same credential.
    Renewed(Box<GrokAuth>),
    Failed(AuthRemedy),
}
impl AuthManager {
    /// Attempt one unattended refresh, bounded because the caller's response gates the client's first draw. Spawned rather than awaited inline.
    /// Dropping the future at the deadline abandons an IdP exchange whose rotated refresh token the server may already have burned. That is how a suspend mid-refresh revoked whole token families in the field.
    pub async fn silent_refresh(self: &Arc<Self>) -> SilentRefresh {
        self.silent_refresh_within(distill_http::STARTUP_AUTH_REFRESH_TIMEOUT)
            .await
    }
    /// [`Self::silent_refresh`] with the caller's budget.
    pub(crate) async fn silent_refresh_within(self: &Arc<Self>, budget: Duration) -> SilentRefresh {
        let manager = Arc::clone(self);
        let attempt = tokio::spawn(async move { manager.auth().await });
        let outcome = match tokio::time::timeout(budget, attempt).await {
            Ok(Ok(Ok(auth))) => SilentRefresh::Renewed(Box::new(auth)),
            _ => SilentRefresh::Failed(self.auth_remedy()),
        };
        let logged = match &outcome {
            SilentRefresh::Renewed(_) => "Renewed".to_owned(),
            SilentRefresh::Failed(remedy) => format!("Failed({remedy:?})"),
        };
        distill_telemetry::unified_log::info(
            "auth: silent refresh",
            None,
            Some(serde_json::json!({ "outcome": logged })),
        );
        outcome
    }
    /// Never blocks the caller; a no-op when the credential needs no refresh.
    /// `cancel` (the agent's teardown token) stops the task, but an exchange already in flight is abandoned, not dropped, and may still land after teardown ([`AuthManager::silent_refresh`]'s rotated-token safety).
    pub fn prewarm_auth_refresh(self: &Arc<Self>, cancel: CancellationToken) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                // A cancel that lands before the first poll must win, so a
                // failed spawn never starts an exchange at all.
                biased;
                _ = cancel.cancelled() => {}
                _ = manager.silent_refresh() => {}
            }
        });
    }
    /// Bounded best-effort mint for RPC paths that must answer promptly.
    /// Thin wrapper over [`AuthManager::refresh_chain_bounded_outcome`] for callers that treat a deadline like any other retryable failure.
    pub async fn refresh_chain_bounded(
        self: &Arc<Self>,
        token_type: TokenType,
        reason: RefreshReason,
        budget: Duration,
    ) -> Result<GrokAuth, AuthError> {
        match self
            .refresh_chain_bounded_outcome(token_type, reason, budget)
            .await
        {
            BoundedRefresh::Resolved(result) => *result,
            BoundedRefresh::DeadlineElapsed => Err(AuthError::transient(
                "bounded refresh deadline elapsed; refresh continues in background",
            )),
        }
    }
    /// Bounded best-effort mint, deadline distinguished (see [`BoundedRefresh`]). Spawned rather than awaited inline, like [`AuthManager::silent_refresh`].
    /// Dropping the future at the deadline abandons an IdP exchange whose rotated refresh token the server may already have burned. On deadline the spawned chain runs to completion, persisting and hot-swapping any minted token.
    /// The caller gets [`BoundedRefresh::DeadlineElapsed`].
    pub async fn refresh_chain_bounded_outcome(
        self: &Arc<Self>,
        token_type: TokenType,
        reason: RefreshReason,
        budget: Duration,
    ) -> BoundedRefresh {
        let manager = Arc::clone(self);
        let attempt = tokio::spawn(async move { manager.refresh_chain(token_type, reason).await });
        let (result, outcome) = match tokio::time::timeout(budget, attempt).await {
            Ok(Ok(Ok(auth))) => (BoundedRefresh::Resolved(Box::new(Ok(auth))), "ok"),
            Ok(Ok(Err(err))) => (BoundedRefresh::Resolved(Box::new(Err(err))), "err"),
            Ok(Err(join_error)) => {
                tracing::error!(
                    is_panic = join_error.is_panic(),
                    "bounded refresh task failed"
                );
                distill_telemetry::unified_log::error(
                    "auth: bounded refresh task failed",
                    None,
                    Some(serde_json::json!({
                        "reason": format!("{reason:?}"),
                        "is_panic": join_error.is_panic(),
                    })),
                );
                if let Some(key) = self.attempted_verdict_key(reason) {
                    self.record_permanent_failure(
                        key,
                        crate::error::RefreshTokenFailedReason::Other.into(),
                    );
                }
                (
                    BoundedRefresh::Resolved(Box::new(Err(AuthError::permanent(
                        crate::error::RefreshTokenFailedReason::Other,
                    )))),
                    "join_error",
                )
            }
            Err(_) => (BoundedRefresh::DeadlineElapsed, "timeout"),
        };
        distill_telemetry::unified_log::info(
            "auth: bounded refresh",
            None,
            Some(serde_json::json!({
                "reason": format!("{reason:?}"),
                "outcome": outcome,
            })),
        );
        result
    }
    /// Classify the current credential's way back.
    /// The provider arm deliberately ignores the recorded verdict.
    /// Real interactive-only binaries block until something kills them, so their run routinely ends with nothing recorded at all.
    pub fn auth_remedy(&self) -> AuthRemedy {
        let provider_mints_sessions = self.is_external_provider_refresh_authority();
        let user_must_act = self.requires_manual_reauth()
            || (provider_mints_sessions && self.current_wire_valid().is_none());
        match (user_must_act, provider_mints_sessions) {
            (false, _) => AuthRemedy::SelfHealing,
            (true, true) => AuthRemedy::ProviderLogin {
                label: self.grok_com_config().auth_provider_label.clone(),
            },
            (true, false) => AuthRemedy::ManualLogin,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthMode;
    use crate::refresh::{RefreshOutcome, TokenRefresher};
    use crate::{GrokComConfig, error::RefreshTokenFailedReason};
    use chrono::{Duration, Utc};
    fn external_provider_config() -> GrokComConfig {
        GrokComConfig {
            auth_provider_command: Some("acme-auth".to_owned()),
            auth_provider_label: Some("Acme SSO".to_owned()),
            ..GrokComConfig::default()
        }
    }
    fn external_credential(expires_at: chrono::DateTime<Utc>) -> GrokAuth {
        GrokAuth {
            key: "external".into(),
            auth_mode: AuthMode::External,
            expires_at: Some(expires_at),
            ..GrokAuth::test_default()
        }
    }
    /// Wired as production wires it, so nothing here passes on the "no refresh authority" arm.
    fn provider_manager(dir: &std::path::Path, credential: GrokAuth) -> Arc<AuthManager> {
        let config = external_provider_config();
        let command = config.auth_provider_command.clone();
        let manager = Arc::new(AuthManager::new(dir, config));
        manager.hot_swap(credential);
        manager.configure_refresher(command, None);
        manager
    }
    /// The verdict-free arm.
    #[test]
    fn hard_expired_external_credential_needs_the_provider_without_a_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let manager = provider_manager(
            dir.path(),
            external_credential(Utc::now() - Duration::hours(1)),
        );
        assert!(!manager.has_permanent_failure());
        assert_eq!(
            manager.auth_remedy(),
            AuthRemedy::ProviderLogin {
                label: Some("Acme SSO".to_owned())
            }
        );
    }
    /// A bare-token credential the backend rejects: it never expires locally, so only the verdict from the failed run says the user has to act.
    #[test]
    fn wire_valid_external_credential_needs_the_provider_once_its_run_failed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = provider_manager(
            dir.path(),
            external_credential(Utc::now() + Duration::hours(1)),
        );
        assert_eq!(manager.auth_remedy(), AuthRemedy::SelfHealing);
        manager.record_permanent_failure(
            "external".to_owned(),
            RefreshTokenFailedReason::ProviderInteractiveRequired.into(),
        );
        assert_eq!(
            manager.auth_remedy(),
            AuthRemedy::ProviderLogin {
                label: Some("Acme SSO".to_owned())
            }
        );
    }
    /// Inside the early-invalidation buffer the proxy still accepts the token, so a failed refresh must not cost the user a login.
    #[test]
    fn buffer_window_external_credential_is_self_healing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = provider_manager(
            dir.path(),
            external_credential(Utc::now() + Duration::minutes(1)),
        );
        assert!(manager.current().is_none(), "buffer window hides the token");
        assert_eq!(manager.auth_remedy(), AuthRemedy::SelfHealing);
    }
    /// A provider command configured alongside OIDC must not capture OIDC's own refresh path.
    #[test]
    fn expired_oidc_credential_with_a_refresh_token_is_self_healing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = provider_manager(
            dir.path(),
            GrokAuth {
                key: "expired-oidc".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt-live".into()),
                expires_at: Some(Utc::now() - Duration::hours(1)),
                ..GrokAuth::test_default()
            },
        );
        assert_eq!(manager.auth_remedy(), AuthRemedy::SelfHealing);
    }
    /// With no provider command there is no binary to escalate to.
    #[test]
    fn expired_credential_without_a_provider_command_needs_a_manual_login() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        manager.hot_swap(external_credential(Utc::now() - Duration::hours(1)));
        manager.configure_refresher(None, None);
        assert_eq!(manager.auth_remedy(), AuthRemedy::SelfHealing);
        manager.record_permanent_failure(
            "external".to_owned(),
            RefreshTokenFailedReason::ProviderInteractiveRequired.into(),
        );
        assert_eq!(manager.auth_remedy(), AuthRemedy::ManualLogin);
    }
    /// A refresh that fails over a token still inside the early-invalidation buffer is a *success* for [`AuthManager::silent_refresh`].
    /// `auth()` serves the cached bearer the proxy still accepts; `current()` hides that token, so `Renewed` must carry the credential. A caller re-reading `current()` here would reject the session.
    /// `Failed(SelfHealing)` on this very credential would have accepted it via `current_or_expired()`.
    #[tokio::test]
    async fn renewed_carries_the_wire_valid_bearer_the_buffer_hides() {
        struct OfflineRefresher;
        #[async_trait::async_trait]
        impl TokenRefresher for OfflineRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                RefreshOutcome::transient("network unreachable")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        manager.hot_swap(GrokAuth {
            key: "wire-valid".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-live".into()),
            expires_at: Some(Utc::now() + Duration::minutes(1)),
            ..GrokAuth::test_default()
        });
        manager.set_refresher(Arc::new(OfflineRefresher));
        assert!(manager.current().is_none(), "the buffer hides the token");
        assert!(manager.is_expired(), "and reports the session expired");
        let SilentRefresh::Renewed(auth) = manager.silent_refresh().await else {
            panic!("auth()'s grace arm serves the wire-valid bearer");
        };
        assert_eq!(auth.key, "wire-valid");
        assert!(
            manager.current().is_none(),
            "current() still hides it — which is why the outcome carries it",
        );
        assert!(
            manager.auth_remedy().is_self_healing(),
            "and the other arm would have accepted the same credential",
        );
    }
    #[tokio::test]
    async fn prewarm_lands_a_renewed_credential_without_the_caller_awaiting() {
        struct RenewingRefresher;
        #[async_trait::async_trait]
        impl TokenRefresher for RenewingRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                RefreshOutcome::Success(Box::new(GrokAuth {
                    key: "prewarmed".into(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(Utc::now() + Duration::hours(1)),
                    ..GrokAuth::test_default()
                }))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        manager.hot_swap(GrokAuth {
            key: "expired-oidc".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-live".into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        manager.set_refresher(Arc::new(RenewingRefresher));
        assert!(manager.current().is_none(), "the credential starts expired");
        let notifier = manager.refresh_notifier();
        let landed = notifier.notified();
        tokio::pin!(landed);
        landed.as_mut().enable();
        manager.prewarm_auth_refresh(CancellationToken::new());
        tokio::time::timeout(std::time::Duration::from_secs(10), landed)
            .await
            .expect("the prewarmed refresh must land unattended");
        assert_eq!(
            manager.current().map(|a| a.key),
            Some("prewarmed".into()),
            "the renewed credential must be the one startup auth() will serve"
        );
    }
    /// spawn_grok_shell's failure path drops a DropGuard before any agent owner exists; a cancel that precedes the task's first poll must stop the prewarm before it spends a refresh attempt. Iterated so a regression to an unbiased select fails outright instead of flaking.
    #[tokio::test]
    async fn cancel_before_first_poll_stops_the_prewarm_before_it_spends_a_refresh() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct CountingRefresher(Arc<AtomicU32>);
        #[async_trait::async_trait]
        impl TokenRefresher for CountingRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                self.0.fetch_add(1, Ordering::SeqCst);
                RefreshOutcome::transient("unreachable for a cancelled prewarm")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        manager.hot_swap(GrokAuth {
            key: "expired-oidc".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-live".into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        let attempts = Arc::new(AtomicU32::new(0));
        manager.set_refresher(Arc::new(CountingRefresher(attempts.clone())));
        for _ in 0..32 {
            let agent_cancel = CancellationToken::new();
            drop(agent_cancel.clone().drop_guard());
            manager.prewarm_auth_refresh(agent_cancel.child_token());
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "a prewarm cancelled before its first poll must never start an exchange"
        );
        assert!(
            manager.current().is_none(),
            "no credential may land from a cancelled prewarm"
        );
    }
    #[tokio::test]
    async fn panicked_refresh_records_a_permanent_verdict_so_no_retry_respends_the_token() {
        struct PanickingRefresher;
        #[async_trait::async_trait]
        impl TokenRefresher for PanickingRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                panic!("mint died mid-exchange");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        manager.hot_swap(GrokAuth {
            key: "expired-oidc".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-live".into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        manager.set_refresher(Arc::new(PanickingRefresher));
        assert!(!manager.has_permanent_failure());
        let err = manager
            .refresh_chain_bounded(
                TokenType::OidcSession,
                RefreshReason::ServerRejected,
                std::time::Duration::from_secs(5),
            )
            .await
            .expect_err("a panicked mint is a failure");
        assert!(
            matches!(
                err,
                AuthError::Refresh(crate::error::RefreshTokenError::Permanent(_))
            ),
            "non-retryable for the caller, got: {err:?}"
        );
        assert!(
            manager.has_permanent_failure(),
            "and recorded, so a follow-up chain short-circuits at step 1b \
             instead of re-spending the possibly-rotated refresh token"
        );
    }
    #[test]
    fn turn_surface_matches_the_remedy() {
        assert_eq!(AuthRemedy::SelfHealing.turn_error_type(), "auth_transient");
        assert!(
            AuthRemedy::SelfHealing
                .advice()
                .is_some_and(|a| a.contains("no need to run /login"))
        );
        assert_eq!(AuthRemedy::ManualLogin.turn_error_type(), "auth");
        assert_eq!(
            AuthRemedy::ManualLogin.advice(),
            None,
            "the client's own banner already tells the user to run /login"
        );
        let provider = AuthRemedy::ProviderLogin {
            label: Some("Acme SSO".to_owned()),
        };
        assert_eq!(provider.turn_error_type(), "auth");
        let advice = provider.advice().expect("provider advice");
        assert!(advice.contains("Acme SSO") && advice.contains("/login"));
        assert!(!advice.contains("no need to run /login"));
    }
}
