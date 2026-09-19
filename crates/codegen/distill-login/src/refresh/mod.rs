mod external_refresher;
mod oidc_refresher;
use crate::backend::AuthBackend;
use crate::manager::AuthManager;
pub use crate::manager::RefreshReason;
use crate::model::GrokAuth;
pub use external_refresher::ExternalBinaryRefresher;
pub use oidc_refresher::OidcRefresher;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
/// Callback for diagnostic log upload on auth refresh failure.
/// Args: `(log_bytes, auth_token_suffix, user_id)`. The upload path is keyed by the user id, never the email.
pub type DiagnosticUploader =
    Arc<dyn Fn(Vec<u8>, String, String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
/// Read-only view of `AuthManager` for refreshers.
/// Refreshers hold `Arc<dyn AuthSnapshot>`, so the type system stops them calling `update()`, `clear()`, `hot_swap()`, or `refresh_chain()`.
pub trait AuthSnapshot: Send + Sync {
    /// Read the current in-memory bearer outside the early-invalidation buffer.
    fn current(&self) -> Option<GrokAuth>;
    /// Read the expired in-memory bearer (for its `refresh_token`).
    fn expired_auth(&self) -> Option<GrokAuth>;
    /// Re-read auth.json from disk for the configured scope.
    /// Credentials are untouched, but the call records what it saw on disk and may emit telemetry about the change.
    fn read_disk_auth(&self) -> Option<GrokAuth>;
    /// Whether the in-memory bearer is expired.
    fn is_expired(&self) -> bool;
    /// Whether the in-memory bearer would still be on the wire after the pre-flight→send gap (`AuthManager::has_sendable_token`).
    fn has_sendable_token(&self) -> bool;
}
impl AuthSnapshot for AuthManager {
    fn current(&self) -> Option<GrokAuth> {
        self.current()
    }
    fn expired_auth(&self) -> Option<GrokAuth> {
        self.expired_auth()
    }
    fn read_disk_auth(&self) -> Option<GrokAuth> {
        self.read_disk_auth()
    }
    fn is_expired(&self) -> bool {
        self.is_expired()
    }
    fn has_sendable_token(&self) -> bool {
        self.has_sendable_token()
    }
}
/// Capability to run the operator's external auth binary.
/// Split out of [`AuthSnapshot`] so read-only OIDC refreshers cannot reach it; only [`ExternalBinaryRefresher`] depends on it.
#[async_trait::async_trait]
pub trait ExternalCommandRunner: Send + Sync {
    /// Run the external auth binary and return the parsed output, or the
    /// failure classification the refresher's verdict depends on.
    async fn run_external_command(
        &self,
        command: &str,
    ) -> Result<GrokAuth, crate::ExternalRefreshError>;
}
#[async_trait::async_trait]
impl ExternalCommandRunner for AuthManager {
    async fn run_external_command(
        &self,
        command: &str,
    ) -> Result<GrokAuth, crate::ExternalRefreshError> {
        self.run_external_refresh_command(command).await
    }
}
/// The credential a refresh sends to the IdP: the disk refresh token first, then the expired in-mem bearer, then current (only on `ServerRejected`).
/// Shared by [`OidcRefresher::refresh`] (the attempt) and `AuthManager::attempted_verdict_key` (the verdict scope), so the two can't drift.
/// The caller supplies the disk read: the verdict path passes a side-effect-free read, the refresher the observing one.
pub fn resolve_refresh_credential(
    snap: &dyn AuthSnapshot,
    disk_auth: Option<GrokAuth>,
    reason: RefreshReason,
) -> Option<GrokAuth> {
    disk_auth
        .filter(|a| a.refresh_token.is_some())
        .or_else(|| snap.expired_auth())
        .or_else(|| {
            (reason == RefreshReason::ServerRejected)
                .then(|| snap.current())
                .flatten()
        })
}
/// Outcome of a refresh attempt. It carries data only: `refresh_chain` handles the mutations.
#[derive(Debug)]
#[must_use = "RefreshOutcome encodes a state transition; route it through refresh_chain"]
pub enum RefreshOutcome {
    /// The authority returned a fresh token; the caller persists it via `update()`.
    Success(Box<GrokAuth>),
    /// Terminal failure (e.g. invalid_grant), or a transient failure escalated to `Other` after repeated occurrences. The caller records a verdict scoped to the rejected credential.
    /// `refresh_chain` discards the access and refresh tokens only for `RefreshTokenRejected`, which holds until the next login. `ClientRejected` and `Other` retain the credentials and age out past the TTL.
    PermanentFailure {
        error: crate::error::RefreshTokenFailedError,
        /// Key of the credential the refresher actually sent to the IdP, so `refresh_chain` scopes the verdict to it.
        /// `None` when the authority has no token key (external binary flow); the caller falls back to its own resolution.
        tried_key: Option<String>,
        /// The refresh token actually spent at the IdP. `refresh_chain` compares it against disk to tell a revoked session apart from a sibling process that rotated the RT out from under us.
        /// A lost rotation must never discard credentials. `tried_key` cannot answer that: it is the access token, and a sibling's rotation changes the RT while the AT the loser holds may be untouched.
        /// `None` when the authority does not expose which RT it sent (external binary flow).
        tried_refresh_token: Option<String>,
    },
    /// Transient or unknown failure; the caller may retry later.
    /// The refresher logs the cause structurally and flattens it to a message here; the retry decision needs recoverability, not the source chain.
    TransientFailure { message: String },
}
impl RefreshOutcome {
    /// A fresh credential from the authority (hides the `Box`).
    pub fn success(auth: GrokAuth) -> Self {
        Self::Success(Box::new(auth))
    }
    /// Terminal failure for an already-classified reason against the credential `tried_key` (the one actually sent to the IdP).
    /// Leaves the tried refresh token unattributed, which disables the sibling-rotation check in `refresh_chain`. Only correct for authorities that genuinely cannot report which RT they spent (the external-binary flow).
    /// Any refresher holding the [`GrokAuth`] it sent must use [`Self::permanent_for`] instead.
    pub fn permanent(
        reason: crate::error::RefreshTokenFailedReason,
        tried_key: Option<String>,
    ) -> Self {
        Self::PermanentFailure {
            error: reason.into(),
            tried_key,
            tried_refresh_token: None,
        }
    }
    /// Terminal failure attributed to the exact credential sent to the IdP.
    /// Prefer this wherever the attempted [`GrokAuth`] is in hand: it captures both the AT key (verdict scope) and the RT (sibling-rotation check).
    /// A lost rotation race then cannot be mistaken for a revoked session.
    pub fn permanent_for(reason: crate::error::RefreshTokenFailedReason, tried: &GrokAuth) -> Self {
        Self::PermanentFailure {
            error: reason.into(),
            tried_key: Some(tried.key.clone()),
            tried_refresh_token: tried.refresh_token.clone(),
        }
    }
    /// A retryable failure carrying a diagnostic message.
    pub fn transient(message: impl Into<String>) -> Self {
        Self::TransientFailure {
            message: message.into(),
        }
    }
}
#[async_trait::async_trait]
pub trait TokenRefresher: Send + Sync {
    /// Attempt to obtain a fresh token from the authority.
    /// Implementations MUST NOT call auth_manager.update(), clear(), hot_swap(), or any other state-mutating method.
    /// Return the result and let refresh_chain handle all mutations.
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome;
}
/// The compiled-in backend chooses the refresh authority, so this build renews only against the one it logs in to.
pub fn build_refresher(
    auth_manager: Arc<AuthManager>,
    auth_provider_command: Option<String>,
    diagnostic_uploader: Option<DiagnosticUploader>,
) -> Arc<dyn TokenRefresher> {
    crate::backend::ActiveAuthBackend::default().refresher(
        auth_manager,
        auth_provider_command,
        diagnostic_uploader,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthMode, GrokAuth, GrokComConfig};
    use chrono::{Duration, Utc};
    /// auth_token_ttl makes is_token_expired use create_time + ttl for External tokens without expires_at, instead of the 30-day fallback.
    #[test]
    fn token_ttl_expires_external_token_by_create_time() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_token_ttl: Some(3600),
            ..GrokComConfig::default()
        };
        let mgr = AuthManager::new(dir.path(), cfg);
        let old_token = GrokAuth {
            key: "old-external-token".into(),
            auth_mode: AuthMode::External,
            create_time: Utc::now() - Duration::hours(2),
            expires_at: None,
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(old_token);
        assert!(
            mgr.current().is_none(),
            "expired external token via auth_token_ttl"
        );
        assert!(mgr.is_expired());
        let new_token = GrokAuth {
            key: "new-external-token".into(),
            auth_mode: AuthMode::External,
            create_time: Utc::now(),
            expires_at: None,
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(new_token);
        assert!(
            mgr.current().is_some(),
            "fresh external token should be valid"
        );
    }
}
