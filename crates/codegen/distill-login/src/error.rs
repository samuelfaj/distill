use std::borrow::Cow;

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("Not logged in. Run `grok login`.")]
    NotLoggedIn,

    /// The token expired and no refresh authority is available.
    #[error("Token expired. Run `grok login` to re-authenticate.")]
    TokenExpiredNoRefresh,

    /// Server rejected the token (401) with no recovery path.
    #[error("Authentication rejected by server. Run `grok login` to re-authenticate.")]
    ServerRejectedNoRecovery,

    /// All recovery strategies are exhausted.
    #[error("Auth recovery exhausted; re-authentication required.")]
    RecoveryExhausted,

    /// A session's team principal violates the `force_login_team_uuid` pin.
    /// `message` states which team is required and which was returned.
    #[error("{message} Run `grok login` to sign in with the required team.")]
    PinnedTeamMismatch { message: String },

    /// The cached API-key session was rejected because API-key auth is disabled.
    #[error("API-key auth is disabled by your administrator. Run `grok login` to authenticate.")]
    ApiKeyAuthDisabled,

    /// Outcome of a refresh-authority attempt.
    /// Recoverability (and, for permanent failures, the reason) lives in [`RefreshTokenError`].
    #[error(transparent)]
    Refresh(#[from] RefreshTokenError),
}

/// Whether a token-refresh failure is permanent or transient.
/// Deliberately not `#[non_exhaustive]`: "permanent or transient" is a decision every caller must make.
/// A third state should break consumers loudly.
#[derive(Debug, Error)]
pub enum RefreshTokenError {
    /// The credential is dead; the user must re-authenticate.
    #[error(transparent)]
    Permanent(#[from] RefreshTokenFailedError),
    /// A network error, a 5xx, or an unknown blip; safe to retry later, with the cause attached.
    #[error(transparent)]
    Transient(RefreshTransientError),
}

/// A retryable refresh failure, wrapping its cause. No public `From`: construct only via [`AuthError::transient`] or [`AuthError::transient_source`].
/// That way a stray `?` on some error cannot silently classify a permanent failure as retryable.
/// Display wraps the cause in "auth refresh failed" so internal messages (lock timeout, sleep defer) are not shown to the user bare.
#[derive(Debug, Error)]
#[error("auth refresh failed: {0}")]
pub struct RefreshTransientError(#[source] Box<dyn std::error::Error + Send + Sync>);

/// A terminal refresh failure.
/// `reason` is machine-readable; the user-facing copy is derived from it via [`RefreshTokenFailedReason::user_message`], so the two can never drift.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", .reason.user_message())]
#[non_exhaustive]
pub struct RefreshTokenFailedError {
    pub reason: RefreshTokenFailedReason,
}

impl From<RefreshTokenFailedReason> for RefreshTokenFailedError {
    fn from(reason: RefreshTokenFailedReason) -> Self {
        Self { reason }
    }
}

/// Why a token refresh terminally failed, grounded in the OAuth2 error codes our IdP actually emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshTokenFailedReason {
    /// `invalid_grant`: the refresh token is no longer valid (expired, reused, or revoked; the IdP does not distinguish these).
    RefreshTokenRejected,
    /// `invalid_client`: the client/app credential was rejected.
    ClientRejected,
    /// The operator's `auth_provider_command` could not produce a credential in a headless run (`GROK_AUTH_EXPIRED=1`).
    ProviderInteractiveRequired,
    /// Escalation from repeated transient failures (OIDC).
    /// Never a raw IdP code: an unrecognized terminal code is classified transient, not `Other` (see `classify_terminal`).
    Other,
}

impl RefreshTokenFailedReason {
    /// Sticky until the credential changes (never ages out): a revoked refresh token never self-heals.
    /// Client rotation and transient escalation recover, so those age out past the TTL.
    pub fn is_sticky(self) -> bool {
        match self {
            Self::RefreshTokenRejected => true,
            Self::ClientRejected | Self::ProviderInteractiveRequired | Self::Other => false,
        }
    }

    /// Whether the verdict rules out an unattended retry for as long as it stands.
    /// Independent of [`Self::is_sticky`], which is about whether the verdict ever ages out.
    pub fn blocks_unattended_retry(self) -> bool {
        match self {
            Self::RefreshTokenRejected | Self::ProviderInteractiveRequired => true,
            Self::ClientRejected | Self::Other => false,
        }
    }

    /// User-facing copy for a terminal refresh failure; the raw IdP code stays in logs.
    pub fn user_message(self) -> Cow<'static, str> {
        match self {
            Self::RefreshTokenRejected => {
                "Your session has expired. Run `grok login` to sign in again.".into()
            }
            Self::ClientRejected => {
                "Authentication is temporarily unavailable. Run `grok login` if this persists."
                    .into()
            }
            Self::ProviderInteractiveRequired => provider_login_message(None),
            Self::Other => {
                "Authentication could not be refreshed. Run `grok login` to sign in again.".into()
            }
        }
    }
}

/// `label` is the operator's `auth_provider_label`, when the caller has one.
pub fn provider_login_message(label: Option<&str>) -> Cow<'static, str> {
    match label {
        Some(label) => format!(
            "Your session expired and {label} could not renew it in the background. \
             Run /login to sign in again."
        )
        .into(),
        None => "Your session expired and your sign-in helper could not renew it in the \
                 background. Run /login to sign in again."
            .into(),
    }
}

impl AuthError {
    /// A retryable refresh failure for the sites that have only a message (lock timeout, sleep/dark-wake defer, no refresher).
    /// Use [`Self::transient_source`] when a real error is in hand.
    pub fn transient(message: impl Into<String>) -> Self {
        Self::transient_source(message.into())
    }

    /// A retryable refresh failure that preserves `source` in the error chain (`Transient` carries the cause).
    /// Callers with a real error should use this rather than flattening it to a string.
    pub fn transient_source(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        AuthError::Refresh(RefreshTokenError::Transient(RefreshTransientError(
            source.into(),
        )))
    }

    /// A terminal refresh failure for an already-classified `reason`.
    pub fn permanent(reason: RefreshTokenFailedReason) -> Self {
        AuthError::Refresh(RefreshTokenError::Permanent(reason.into()))
    }

    /// True for a retryable refresh failure (network, 5xx, sleep/dark-wake defer, etc.).
    /// Permanent failures, NotLoggedIn, and policy rejects are not transient.
    pub fn is_transient(&self) -> bool {
        matches!(self, AuthError::Refresh(RefreshTokenError::Transient(_)))
    }
}
