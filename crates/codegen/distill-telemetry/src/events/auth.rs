//! Auth, manual-auth, and auth-lock product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct Login {
    pub auth_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// The login-method picker was shown.
/// `trigger` is "startup", "logout", or "mid_session".
#[derive(Serialize)]
pub struct LoginPickerShown {
    pub trigger: String,
}

/// A login method was chosen from the picker.
/// `method` is "xai" or "api_key"; `mode` is "device", "loopback", or "api_key".
#[derive(Serialize)]
pub struct LoginMethodChosen {
    pub method: String,
    pub mode: String,
}

/// A login flow completed successfully.
/// `method` is "xai" or "api_key"; `mode` is the resolved auth mode.
/// `mid_session` is true for `/login`/401 re-auth (as opposed to the startup/logout flow).
#[derive(Serialize)]
pub struct LoginCompleted {
    pub method: String,
    pub mode: String,
    pub duration_ms: u64,
    pub mid_session: bool,
}

/// How a login attempt's HTTP request failed.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginFailureKind {
    /// `is_connect`: a dead TCP connect *or* a TLS handshake killed mid-flight.
    /// `os_error` tells them apart.
    TransportConnect,
    /// TLS certificate rejected for an untrusted issuer (e.g. an uninstalled proxy root).
    CertificateUntrusted,
    /// TLS certificate otherwise invalid (expired, wrong hostname).
    CertificateInvalid,
    /// In-flight request cut short: reset, close, timeout, body phase.
    TransportInterrupted,
    /// Client-side request construction / redirect policy defect.
    TransportPermanent,
    Decode,
}

/// One per failed login attempt, emitted by the login funnel so a retried request can't inflate the count.
/// Failures that never reached HTTP (user backed out, loopback bind, id_token validation) are not reported.
#[derive(Serialize)]
pub struct LoginFailed {
    pub error_kind: LoginFailureKind,
    /// OS code from the failure's cause chain (54/104 ECONNRESET, 10054 on Windows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_error: Option<i32>,
}

/// The user backed out of the login funnel.
/// `stage` is "picker", "api_key_entry", "loopback_paste", "device_wait", "api_key_wait", or "command_wait"; `via` is "esc" or "quit".
#[derive(Serialize)]
pub struct LoginAbandoned {
    pub stage: String,
    pub via: String,
}

#[derive(Serialize)]
pub struct ApiKeySaveResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A contended `auth.json.lock` acquisition; instant acquisitions stay silent.
#[derive(Serialize)]
pub struct AuthLockWait {
    pub wait_ms: u64,
    pub budget_ms: u64,
}

/// An `auth.json.lock` wait that exhausted its budget.
#[derive(Serialize)]
pub struct AuthLockTimeout {
    pub budget_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_state: Option<&'static str>,
}

/// A held lock's file was replaced out from under it: an unlink-recovery binary is still active in the fleet.
/// The holder fields describe the replacer.
#[derive(Serialize)]
pub struct AuthLockReplacedOutFromUnder {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_age_secs: Option<u64>,
}

/// Why auth recovery could not refresh the credential, forcing the user to manually re-authenticate.
/// Mapped from shell's `AuthError`; only terminal failures map, transient ones don't emit (recovery retries).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ManualAuthReason {
    /// IdP rejected the refresh token (`invalid_grant`); a re-login is required.
    RefreshTokenRejected,
    /// Token type has no refresh authority (API key / legacy / OIDC without a refresh token).
    NoRefreshAuthority,
    /// The operator's auth-provider command could not mint a credential unattended, so only an interactive run of it can restore the session.
    ProviderInteractiveRequired,
    RecoveryExhausted,
    TokenExpiredNoRefresh,
    /// Recovered session violated the `force_login_team_uuid` pin.
    WrongTeam,
}

/// User-facing surface where the manual re-auth was triggered.
/// Background recoveries (storage/telemetry uploads) do not emit this event.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ManualAuthSurface {
    /// A chat/inference turn (the yellow `ReAuthRequired` banner).
    Turn,
    /// The relay / leader connection handshake.
    Relay,
}

/// The kind of bearer that was rejected.
/// Mirrors shell's `TokenType` as a stable wire enum (don't serialize the shell `Debug` repr).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AuthTokenKind {
    OidcSession,
    ExternalBinary,
    LegacySession,
    ApiKey,
    None,
}

/// Product-events only (no external export). Count `distinct(principal)`, never raw events. `trigger` is whichever
/// surface fired first, not a reliable per-surface split. API-key sessions are excluded (a 401 there means rotate the
/// key, not `/login`). A downstream crate's `cfg(test)` can't turn on `cfg_attr(test,...)` here
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ManualAuth {
    pub reason: ManualAuthReason,
    pub trigger: ManualAuthSurface,
    pub token_kind: AuthTokenKind,
    /// `user_id` of the locked-out account, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}
