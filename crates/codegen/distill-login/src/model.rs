// Modified for Distill by Samuel Fajreldines, 2026.
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use distill_auth::bearer_suffix;

use super::is_xai_oauth2_issuer;

pub const TOKEN_TTL: Duration = Duration::days(30);
const DEFAULT_EARLY_INVALIDATION_SECS: u64 = 300; // 5 minutes

/// Legacy auth.json scope key. Fallback for old devbox auth files.
pub(super) const LEGACY_SCOPE: &str = "https://accounts.x.ai/sign-in";

/// auth.json scope key for plain API key auth (desktop login, `grok login --api-key`).
pub(super) const API_KEY_SCOPE: &str = "xai::api_key";

const BLOCKED_REASON_NO_LOGS: &str = "BLOCKED_REASON_NO_LOGS";
const BLOCKED_REASON_NO_LOGS_MODERATED: &str = "BLOCKED_REASON_NO_LOGS_MODERATED";

/// Fresh-credential / missing-field default: opted out until the user or server enrichment opts in.
/// Single source for `GrokAuth`, `AuthMeta`, and every login-path constructor so the sides cannot drift.
pub fn default_coding_data_retention_opt_out() -> bool {
    true
}

/// Token provenance (debugging/auth.json only; no code branches on this).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Deprecated. Kept for deserializing old auth.json files.
    #[serde(alias = "grok")]
    WebLogin,
    /// OIDC or OAuth2 interactive login via customer IdP
    #[serde(alias = "oidc")]
    Oidc,
    /// External auth provider binary
    External,
    /// Plain API key (e.g. from grok-desktop login or `grok login --api-key`)
    ApiKey,
}

/// Wire value of `principal_type` for team OAuth principals (capitalized by the auth service).
/// Single source for every comparison site.
pub const TEAM_PRINCIPAL_TYPE: &str = "Team";

#[derive(Clone, Serialize, Deserialize)]
pub struct GrokAuth {
    pub key: String,
    pub auth_mode: AuthMode,
    pub create_time: DateTime<Utc>,
    pub user_id: String,
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_image_asset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_blocked_reasons: Vec<String>,
    /// Defaults to `true` (opted out) for safer consumer privacy until the user explicitly shares or server enrichment sets the team preference.
    #[serde(default = "default_coding_data_retention_opt_out")]
    pub coding_data_retention_opt_out: bool,

    /// Deprecated. Kept for deserializing existing auth.json files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_grok_code_access: Option<bool>,

    /// Refresh token (OIDC/OAuth2 or external provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Server-provided expiration (from OIDC `expires_in`).
    /// When present, takes precedence over the hardcoded `TOKEN_TTL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,

    /// Issuer URL that issued this token.
    /// For OIDC credentials it drives refresh via discovery; for external-provider credentials it is the provider's `issuer` claim.
    /// In both modes an x.ai issuer marks the credential first-party (`is_xai_auth`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_issuer: Option<String>,

    /// OIDC client_id used to obtain this token (needed for refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_client_id: Option<String>,
}

impl std::fmt::Debug for GrokAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrokAuth")
            .field("key", &bearer_suffix(&self.key))
            .field("auth_mode", &self.auth_mode)
            .field("user_id", &self.user_id)
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_deref().map(bearer_suffix),
            )
            .finish_non_exhaustive()
    }
}

/// Identifies one issuance of a credential.
/// The bearer alone is not enough: an authority may re-issue the same opaque token with a later `expires_at`, and every mint stamps a fresh `create_time`.
/// Two credentials with equal generations are the same issuance; anything else counts as progress (proactive loop) or a new scope for failure budgets (refreshers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialGeneration {
    key: String,
    create_time: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
}

impl GrokAuth {
    pub(crate) fn generation(&self) -> CredentialGeneration {
        CredentialGeneration {
            key: self.key.clone(),
            create_time: self.create_time,
            expires_at: self.expires_at,
        }
    }

    /// Seconds since this credential was minted.
    /// Negative when the clock stepped back past `create_time` (NTP correction, VM restore, or a sibling machine's clock via an adopted auth.json).
    /// `create_time` is always stamped from the minting machine's local clock.
    pub fn mint_age_seconds(&self) -> i64 {
        Utc::now()
            .signed_duration_since(self.create_time)
            .num_seconds()
    }

    /// `true` when the token comes from a first-party Grok account. That is either an OIDC login against https://auth.x.ai (or the local-dev equivalent), or an external auth provider declaring an Grok issuer.
    /// The issuer is a client-side hint, not a trust assertion. Everything it unlocks still authenticates the actual token server-side, and it never influences endpoints.
    pub fn is_xai_auth(&self) -> bool {
        match self.auth_mode {
            AuthMode::Oidc | AuthMode::External => self
                .oidc_issuer
                .as_deref()
                .is_some_and(is_xai_oauth2_issuer),
            AuthMode::ApiKey | AuthMode::WebLogin => false,
        }
    }

    /// `true` when this auth can access grok.com managed MCP connectors.
    pub fn is_managed_mcp_eligible(&self) -> bool {
        self.is_xai_auth() || self.auth_mode == AuthMode::WebLogin
    }

    /// Whether this credential can access `supported_in_api: false` models. Session logins (WebLogin, OIDC, including enterprise issuers) always qualify.
    /// External-provider credentials qualify only when first-party (`is_xai_auth`). Plain API keys never do.
    pub fn is_session_auth(&self) -> bool {
        match self.auth_mode {
            AuthMode::WebLogin | AuthMode::Oidc => true,
            AuthMode::External => self.is_xai_auth(),
            AuthMode::ApiKey => false,
        }
    }

    pub fn is_team_principal(&self) -> bool {
        self.principal_type.as_deref() == Some(TEAM_PRINCIPAL_TYPE) && self.team_id.is_some()
    }

    /// `true` when the team has Zero Data Retention (ZDR) enabled.
    pub fn is_zdr_team(&self) -> bool {
        self.team_blocked_reasons
            .iter()
            .any(|r| r == BLOCKED_REASON_NO_LOGS || r == BLOCKED_REASON_NO_LOGS_MODERATED)
    }

    /// `true` when the team has ZDR or the user opted out of coding data retention.
    /// Use this for trace-upload and research-data gates.
    /// Product analytics (`telemetry_enabled`) and user-facing sync features should use `is_zdr_team()` directly.
    pub fn is_data_collection_disabled(&self) -> bool {
        self.is_zdr_team() || self.coding_data_retention_opt_out
    }

    /// Carry `/user`-derived fields from a previous auth so refresh rebuilds don't drop them.
    pub fn carry_user_profile_from(&mut self, prev: &GrokAuth) {
        self.user_id = prev.user_id.clone();
        self.email = prev.email.clone();
        self.principal_type = prev.principal_type.clone();
        self.principal_id = prev.principal_id.clone();
        self.team_id = prev.team_id.clone();
        self.team_name = prev.team_name.clone();
        self.team_role = prev.team_role.clone();
        self.organization_id = prev.organization_id.clone();
        self.organization_name = prev.organization_name.clone();
        self.organization_role = prev.organization_role.clone();
        self.user_blocked_reason = prev.user_blocked_reason.clone();
        self.team_blocked_reasons = prev.team_blocked_reasons.clone();
        self.coding_data_retention_opt_out = prev.coding_data_retention_opt_out;
    }
}

impl Default for GrokAuth {
    fn default() -> Self {
        Self {
            key: String::new(),
            auth_mode: AuthMode::Oidc,
            create_time: Utc::now(),
            user_id: String::new(),
            email: None,
            first_name: None,
            last_name: None,
            profile_image_asset_id: None,
            principal_type: None,
            principal_id: None,
            team_id: None,
            team_name: None,
            team_role: None,
            organization_id: None,
            organization_name: None,
            organization_role: None,
            user_blocked_reason: None,
            team_blocked_reasons: vec![],
            coding_data_retention_opt_out: default_coding_data_retention_opt_out(),
            has_grok_code_access: None,
            refresh_token: None,
            expires_at: None,
            oidc_issuer: None,
            oidc_client_id: None,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl GrokAuth {
    /// Returns a `GrokAuth` with sensible defaults for tests.
    /// Override fields with struct update syntax: `GrokAuth { key: "...".into(), ..GrokAuth::test_default() }`.
    pub fn test_default() -> Self {
        Self {
            key: "test-key".into(),
            user_id: "test-user".into(),
            // Tests that exercise collection gates need sharing enabled by default; opt out explicitly when asserting the privacy path
            coding_data_retention_opt_out: false,
            ..Default::default()
        }
    }
}

pub type AuthStore = BTreeMap<String, GrokAuth>;

/// User information from the cli-chat-proxy `GET /v1/user` endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub user_id: String,
    #[serde(default)]
    pub(super) email: Option<String>,
    #[serde(default)]
    pub(super) first_name: Option<String>,
    #[serde(default)]
    pub(super) last_name: Option<String>,
    #[serde(default)]
    pub(super) profile_image_asset_id: Option<String>,
    #[serde(default)]
    pub(super) principal_type: Option<String>,
    #[serde(default)]
    pub(super) principal_id: Option<String>,
    #[serde(default)]
    pub(super) team_id: Option<String>,
    #[serde(default)]
    pub(super) team_name: Option<String>,
    #[serde(default)]
    pub(super) team_role: Option<String>,
    #[serde(default)]
    pub(super) organization_id: Option<String>,
    #[serde(default)]
    pub(super) organization_name: Option<String>,
    #[serde(default)]
    pub(super) organization_role: Option<String>,
    #[serde(default)]
    pub(super) user_blocked_reason: Option<String>,
    #[serde(default)]
    pub(super) team_blocked_reasons: Option<Vec<String>>,
    #[serde(default)]
    pub(super) coding_data_retention_opt_out: Option<bool>,
    /// Live subscription tier from the backend (only present when `?include=subscription` is passed to `/user`).
    #[serde(default)]
    pub subscription_tier: Option<String>,
}

/// Look up auth from the store by scope key. Legacy `WebLogin` tokens (from the pre-OIDC `grok login --legacy` flow) are skipped. They are validated via a per-request DB lookup server-side, which fails at high volume.
/// Skipping them here forces affected users to re-authenticate via OIDC on next launch.
pub fn lookup_auth(map: &AuthStore, scope: &str) -> Option<GrokAuth> {
    let auth = map
        .get(scope)
        .cloned()
        .or_else(|| inherited_lookup(map, scope))?;
    if auth.auth_mode == AuthMode::WebLogin {
        tracing::info!("auth: ignoring legacy WebLogin token — re-authentication required");
        return None;
    }
    Some(auth)
}

/// Falls back to a scope the active backend inherits, skipping the one already tried.
fn inherited_lookup(map: &AuthStore, scope: &str) -> Option<GrokAuth> {
    use crate::backend::{ActiveAuthBackend, AuthBackend};

    ActiveAuthBackend::default()
        .inherited_scopes()
        .iter()
        .filter(|inherited| **inherited != scope)
        .find_map(|inherited| map.get(*inherited).cloned())
}

/// Early-invalidation buffer.
/// Override with `GROK_AUTH_EARLY_INVALIDATION_SECS` for testing (e.g. `=5` to shrink the buffer to 5 seconds).
pub(super) fn early_invalidation() -> Duration {
    std::env::var("GROK_AUTH_EARLY_INVALIDATION_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| Duration::seconds(s as i64))
        .unwrap_or_else(|| Duration::seconds(DEFAULT_EARLY_INVALIDATION_SECS as i64))
}

pub fn is_expired(auth: &GrokAuth) -> bool {
    is_expired_with_buffer(auth, early_invalidation())
}

/// Like [`is_expired`] but with an explicit pre-expiry buffer.
/// Pass `Duration::zero()` for actual (hard) expiry: the instant the token would really be rejected on the wire, with no early-invalidation margin.
pub fn is_expired_with_buffer(auth: &GrokAuth, buffer: Duration) -> bool {
    if let Some(expires_at) = auth.expires_at {
        Utc::now() >= (expires_at - buffer)
    } else {
        let age = Utc::now().signed_duration_since(auth.create_time);
        age >= (TOKEN_TTL - buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_auth(mode: AuthMode) -> GrokAuth {
        GrokAuth {
            key: "k".into(),
            auth_mode: mode,
            create_time: Utc::now(),
            user_id: "u".into(),
            email: None,
            first_name: None,
            last_name: None,
            profile_image_asset_id: None,
            principal_type: None,
            principal_id: None,
            team_id: None,
            team_name: None,
            team_role: None,
            organization_id: None,
            organization_name: None,
            organization_role: None,
            user_blocked_reason: None,
            team_blocked_reasons: vec![],
            coding_data_retention_opt_out: false,
            has_grok_code_access: None,
            refresh_token: None,
            expires_at: None,
            oidc_issuer: None,
            oidc_client_id: None,
        }
    }

    #[test]
    fn is_xai_auth_matrix() {
        use crate::XAI_OAUTH2_ISSUER;
        let with_issuer = |mode: AuthMode, issuer: Option<&str>| GrokAuth {
            oidc_issuer: issuer.map(str::to_owned),
            ..make_auth(mode)
        };

        // Only Oidc/External qualify, and only with an x.ai issuer.
        assert!(with_issuer(AuthMode::Oidc, Some(XAI_OAUTH2_ISSUER)).is_xai_auth());
        assert!(with_issuer(AuthMode::External, Some(XAI_OAUTH2_ISSUER)).is_xai_auth());
        assert!(!with_issuer(AuthMode::Oidc, None).is_xai_auth());
        assert!(!with_issuer(AuthMode::External, None).is_xai_auth());
        assert!(!with_issuer(AuthMode::Oidc, Some("https://idp.acme.example")).is_xai_auth());
        assert!(!with_issuer(AuthMode::External, Some("https://idp.acme.example")).is_xai_auth());

        // ApiKey / WebLogin stay false even with an x.ai issuer set.
        assert!(!with_issuer(AuthMode::ApiKey, Some(XAI_OAUTH2_ISSUER)).is_xai_auth());
        assert!(!with_issuer(AuthMode::WebLogin, Some(XAI_OAUTH2_ISSUER)).is_xai_auth());
    }

    #[test]
    fn is_session_auth_requires_first_party_for_external() {
        use crate::XAI_OAUTH2_ISSUER;
        let with_issuer = |mode: AuthMode, issuer: Option<&str>| GrokAuth {
            oidc_issuer: issuer.map(str::to_owned),
            ..make_auth(mode)
        };

        // Session logins qualify regardless of issuer (incl. enterprise OIDC).
        assert!(with_issuer(AuthMode::WebLogin, None).is_session_auth());
        assert!(with_issuer(AuthMode::Oidc, None).is_session_auth());
        assert!(with_issuer(AuthMode::Oidc, Some("https://idp.acme.example")).is_session_auth());

        // External qualifies only when first-party.
        assert!(with_issuer(AuthMode::External, Some(XAI_OAUTH2_ISSUER)).is_session_auth());
        assert!(!with_issuer(AuthMode::External, None).is_session_auth());
        assert!(
            !with_issuer(AuthMode::External, Some("https://idp.acme.example")).is_session_auth()
        );

        // Plain API keys never do.
        assert!(!with_issuer(AuthMode::ApiKey, Some(XAI_OAUTH2_ISSUER)).is_session_auth());
    }

    #[test]
    fn lookup_auth_skips_weblogin_on_primary_scope() {
        let mut map = AuthStore::new();
        map.insert("scope".into(), make_auth(AuthMode::WebLogin));
        assert!(lookup_auth(&map, "scope").is_none());
    }

    #[test]
    fn lookup_auth_skips_weblogin_on_legacy_fallback() {
        let mut map = AuthStore::new();
        map.insert(LEGACY_SCOPE.into(), make_auth(AuthMode::WebLogin));
        assert!(lookup_auth(&map, "other-scope").is_none());
    }

    #[test]
    fn lookup_auth_returns_oidc_token() {
        let mut map = AuthStore::new();
        map.insert("scope".into(), make_auth(AuthMode::Oidc));
        assert!(lookup_auth(&map, "scope").is_some());
    }

    #[test]
    fn lookup_auth_returns_api_key_token() {
        let mut map = AuthStore::new();
        map.insert("scope".into(), make_auth(AuthMode::ApiKey));
        assert!(lookup_auth(&map, "scope").is_some());
    }

    /// subscriptionTier present deserializes to Some.
    #[test]
    fn user_info_subscription_tier_present() {
        let json = r#"{
            "userId": "u1",
            "subscriptionTier": "SuperGrokPro"
        }"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.subscription_tier.as_deref(), Some("SuperGrokPro"));
    }

    /// subscriptionTier absent deserializes to None (backwards compat).
    #[test]
    fn user_info_subscription_tier_absent() {
        let json = r#"{"userId": "u1"}"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert!(info.subscription_tier.is_none());
    }

    /// subscriptionTier null deserializes to None.
    #[test]
    fn user_info_subscription_tier_null() {
        let json = r#"{"userId": "u1", "subscriptionTier": null}"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert!(info.subscription_tier.is_none());
    }

    /// subscriptionTier empty string deserializes to Some("").
    /// The paywall poller treats this as "no subscription" (its guard is `Some(tier) if !tier.is_empty()`) and keeps polling.
    #[test]
    fn user_info_subscription_tier_empty_string() {
        let json = r#"{"userId": "u1", "subscriptionTier": ""}"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.subscription_tier.as_deref(), Some(""));
    }

    /// A pre-default auth.json (no coding_data_retention_opt_out key) must deserialize as opted-out, not the old fail-open false.
    #[test]
    fn missing_coding_data_retention_opt_out_deserializes_opted_out() {
        let json = r#"{
            "key": "k",
            "auth_mode": "oidc",
            "create_time": "2020-01-01T00:00:00Z",
            "user_id": "u"
        }"#;
        let auth: GrokAuth = serde_json::from_str(json).unwrap();
        assert!(
            auth.coding_data_retention_opt_out,
            "missing field must default to opted-out"
        );
        assert!(default_coding_data_retention_opt_out());
        assert!(GrokAuth::default().coding_data_retention_opt_out);
    }
}
