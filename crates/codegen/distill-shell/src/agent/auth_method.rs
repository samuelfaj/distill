// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;

use crate::agent::config::ModelEntry;
use distill_login::PreferredAuthMethod;

/// Shared, live handle to the agent's current ACP auth method id. `Arc` so a clone can cross the per-session-thread boundary at spawn.
/// The `ArcSwapOption` interior lets the agent's `authenticate` handler publish a new method without re-spawning sessions. Every running session's per-turn auth gate observes the new method on its next turn.
/// `None` until the first `authenticate`. Auth is process-global (one user, one `AuthManager`), so all sessions sharing one cell is correct.
pub(crate) type SharedAuthMethodId = std::sync::Arc<arc_swap::ArcSwapOption<acp::AuthMethodId>>;

/// Construct a [`SharedAuthMethodId`]. `None` is the pre-`authenticate` state.
pub(crate) fn new_shared_auth_method_id(initial: Option<acp::AuthMethodId>) -> SharedAuthMethodId {
    std::sync::Arc::new(arc_swap::ArcSwapOption::new(
        initial.map(std::sync::Arc::new),
    ))
}

// The first-party env-key primitives live in the low `distill-login` crate
// (auth needs them without pulling in shell's `ModelEntry`); re-exported here so
// `crate::agent::auth_method::{XAI_API_KEY_ENV_VAR, ..}` call sites keep resolving.
pub use distill_login::auth_method::{
    LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR, has_xai_api_key_env, read_xai_api_key_env,
};

/// Whether `xai.api_key` should be advertised (and pushed FIRST) when building the `auth_methods` list at `initialize()` time.
/// Regression: `xai.api_key` must stay first when only per-model credentials exist (no global `XAI_API_KEY`).
/// Deferring it made BYOK users hit the login screen because the pager uses `auth_methods.first()` for startup metadata. Both inputs can change between calls, so the result is not cached. When true the method is never advertised, regardless of available credentials, so `XAI_API_KEY` can't bypass a deployment's forced IdP login. Presence-only for the first-party env key (treats it as usable).
pub(crate) fn should_advertise_xai_api_key<'a, I>(disable_api_key_auth: bool, models: I) -> bool
where
    I: IntoIterator<Item = &'a ModelEntry>,
{
    should_advertise_xai_api_key_with_env_ok(disable_api_key_auth, models, true)
}

/// Single advertise policy for `xai.api_key`: the kill switch, BYOK, and the first-party env key.
/// The env key is gated by `first_party_env_ok` (probe result, or `true` for presence-only); BYOK still advertises without a probe.
pub(crate) fn should_advertise_xai_api_key_with_env_ok<'a, I>(
    disable_api_key_auth: bool,
    models: I,
    first_party_env_ok: bool,
) -> bool
where
    I: IntoIterator<Item = &'a ModelEntry>,
{
    if disable_api_key_auth {
        return false;
    }
    let has_byok = models.into_iter().any(ModelEntry::has_own_credentials);
    has_byok || (has_xai_api_key_env() && first_party_env_ok)
}

/// Inputs to [`build_auth_methods`]. The caller (`MvpAgent::initialize()`) computes the booleans. They depend on async side effects (token refresh) and shared mutable state (`AuthManager`).
/// The list-construction logic itself is pure so it can be unit-tested without any of that machinery.
pub struct AuthMethodsBuildInputs<'a> {
    /// True if `xai.api_key` should be advertised AT ALL. Login/initialize callers compute it via [`should_advertise_xai_api_key_with_env_ok`] after the validity probe.
    /// Presence-only paths may use [`should_advertise_xai_api_key`]. When `preferred_method` is `Oidc`, this is ignored (API key is never advertised under that pin).
    pub has_external_api_key: bool,
    /// True if a cached session token is available (either present at startup or recovered via silent refresh).
    pub has_cached_token: bool,
    /// True if enterprise OIDC is configured.
    /// Mutually exclusive with the default `grok.com` method.
    pub has_enterprise_oidc: bool,
    /// Required when `has_enterprise_oidc` is true; ignored otherwise.
    pub enterprise_oidc_issuer: Option<&'a str>,
    /// Optional display label for the login method (`grok.com` or `oidc`).
    pub login_label: Option<&'a str>,
    /// True if `grok_com_config.auth_provider_command` is configured (sets `meta.external_provider = true` on the `grok.com` method).
    pub has_auth_provider_command: bool,
    /// Config pin (`[auth] preferred_method`).
    /// `None` keeps multi-method fallthrough; `Some` is fail-closed (only that method family).
    pub preferred_method: Option<PreferredAuthMethod>,
}

/// Output of [`build_auth_methods`].
pub struct BuiltAuthMethods {
    /// Auth methods in advertised order.
    /// ORDER IS THE CONTRACT: the pager's `startup_auth_metadata()` reads `methods.first()` to decide whether interactive login is needed.
    pub methods: Vec<acp::AuthMethod>,
    /// The default `auth_method_id` to install on the agent.
    /// When unpinned, `cached_token` wins over `xai.api_key` when both are present.
    /// When pinned, only the preferred method may appear; `None` means unavailable (fail auth, no cross-method fallthrough).
    pub default_auth_method_id: Option<acp::AuthMethodId>,
}

/// REGRESSION GUARD: when unpinned and `has_external_api_key` is true, the **first** entry MUST be `xai.api_key`.
/// Unpinned ordering (when each method is enabled): `xai.api_key` (if `has_external_api_key`) `cached_token` (if `has_cached_token`) exactly one of: `oidc` (if `has_enterprise_oidc`) `grok.com` (otherwise)
/// Unpinned `default_auth_method_id`: `cached_token` if `has_cached_token` `xai.api_key` else if `has_external_api_key` `None` otherwise Pinned (`preferred_method`): `ApiKey`: only `xai.api_key` if available; else an empty list and `None` (fail). `Oidc`: `cached_token` (if any) then interactive login; never `xai.api_key`.
pub fn build_auth_methods(inputs: AuthMethodsBuildInputs<'_>) -> BuiltAuthMethods {
    let AuthMethodsBuildInputs {
        has_external_api_key,
        has_cached_token,
        has_enterprise_oidc,
        enterprise_oidc_issuer,
        login_label,
        has_auth_provider_command,
        preferred_method,
    } = inputs;

    match preferred_method {
        Some(PreferredAuthMethod::ApiKey) => build_pinned_api_key(has_external_api_key),
        Some(PreferredAuthMethod::Oidc) => build_pinned_oidc(
            has_cached_token,
            has_enterprise_oidc,
            enterprise_oidc_issuer,
            login_label,
            has_auth_provider_command,
        ),
        None => build_unpinned(
            has_external_api_key,
            has_cached_token,
            has_enterprise_oidc,
            enterprise_oidc_issuer,
            login_label,
            has_auth_provider_command,
        ),
    }
}

fn build_pinned_api_key(has_external_api_key: bool) -> BuiltAuthMethods {
    if !has_external_api_key {
        distill_telemetry::unified_log::warn(
            "auth: preferred_method=api_key but no API key credentials available",
            None,
            None,
        );
        return BuiltAuthMethods {
            methods: Vec::new(),
            default_auth_method_id: None,
        };
    }
    BuiltAuthMethods {
        methods: vec![xai_api_key_auth_method()],
        default_auth_method_id: Some(acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID)),
    }
}

fn build_pinned_oidc(
    has_cached_token: bool,
    has_enterprise_oidc: bool,
    enterprise_oidc_issuer: Option<&str>,
    login_label: Option<&str>,
    has_auth_provider_command: bool,
) -> BuiltAuthMethods {
    let mut methods: Vec<acp::AuthMethod> = Vec::new();
    let mut default_auth_method_id: Option<acp::AuthMethodId> = None;

    if has_cached_token {
        methods.push(cached_token_auth_method());
        default_auth_method_id = Some(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
    }

    push_interactive_login(
        &mut methods,
        has_enterprise_oidc,
        enterprise_oidc_issuer,
        login_label,
        has_auth_provider_command,
    );

    BuiltAuthMethods {
        methods,
        default_auth_method_id,
    }
}

fn build_unpinned(
    has_external_api_key: bool,
    has_cached_token: bool,
    has_enterprise_oidc: bool,
    enterprise_oidc_issuer: Option<&str>,
    login_label: Option<&str>,
    has_auth_provider_command: bool,
) -> BuiltAuthMethods {
    let mut methods: Vec<acp::AuthMethod> = Vec::new();
    let mut default_auth_method_id: Option<acp::AuthMethodId> = None;

    if has_external_api_key {
        methods.push(xai_api_key_auth_method());
        default_auth_method_id = Some(acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID));
    }

    if has_cached_token {
        methods.push(cached_token_auth_method());
        // cached_token wins over xai.api_key for default_auth_method_id so is_session_based_auth() returns true and OIDC refresh stays alive
        let overrode_api_key = default_auth_method_id.is_some();
        default_auth_method_id = Some(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
        if overrode_api_key {
            distill_telemetry::unified_log::info(
                "auth method priority: cached_token overrides xai.api_key for default_auth_method_id",
                None,
                Some(serde_json::json!({
                    "has_external_api_key": has_external_api_key,
                    "has_cached_token": has_cached_token,
                })),
            );
        }
    }

    push_interactive_login(
        &mut methods,
        has_enterprise_oidc,
        enterprise_oidc_issuer,
        login_label,
        has_auth_provider_command,
    );

    BuiltAuthMethods {
        methods,
        default_auth_method_id,
    }
}

fn push_interactive_login(
    methods: &mut Vec<acp::AuthMethod>,
    has_enterprise_oidc: bool,
    enterprise_oidc_issuer: Option<&str>,
    login_label: Option<&str>,
    has_auth_provider_command: bool,
) {
    if has_enterprise_oidc {
        // Caller invariant: `enterprise_oidc_issuer` MUST be `Some(...)` when `has_enterprise_oidc` is true
        // Production callers derive both from the same `cfg.grok_com_config.oidc` Option
        // The inconsistent `(true, None)` combination is a programmer error, so panic loudly
        let issuer = enterprise_oidc_issuer
            .expect("enterprise_oidc_issuer is required when has_enterprise_oidc is true");
        methods.push(oidc_auth_method(issuer, login_label));
    } else {
        methods.push(grok_com_auth_method(login_label, has_auth_provider_command));
    }
}

/// ACP session auth method. Use `is_session_based_method` for classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethodKind {
    XaiApiKey,
    CachedToken,
    GrokCom,
    Oidc,
    Unknown,
}

impl AuthMethodKind {
    pub fn from_id(id: &acp::AuthMethodId) -> Self {
        match id.0.as_ref() {
            XAI_API_KEY_METHOD_ID => Self::XaiApiKey,
            CACHED_TOKEN_AUTH_METHOD_ID => Self::CachedToken,
            GROK_COM_METHOD_ID => Self::GrokCom,
            OIDC_METHOD_ID => Self::Oidc,
            _ => Self::Unknown,
        }
    }

    /// API key auth: no auth.json, no refresh, no user interaction.
    pub fn is_api_key(self) -> bool {
        matches!(self, Self::XaiApiKey)
    }

    /// `true` for session-based methods (cached_token, grok.com, oidc).
    pub(crate) fn is_session_based(self) -> bool {
        matches!(self, Self::CachedToken | Self::GrokCom | Self::Oidc)
    }

    /// Requires user interaction (browser, OIDC redirect, or external auth command).
    pub fn needs_interactive_login(self) -> bool {
        matches!(self, Self::GrokCom | Self::Oidc)
    }
}

/// `true` for session-based ACP methods (cached_token, grok.com, oidc).
pub(crate) fn is_session_based_method(method_id: &acp::AuthMethodId) -> bool {
    AuthMethodKind::from_id(method_id).is_session_based()
}

/// Per-model BYOK status: whether the selected model carries its own `[model.*]` `api_key`/`env_key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ModelByok {
    /// Model has its own per-model key (not refreshable).
    Byok,
    /// Model has no per-model key (session auth governs).
    NotByok,
    /// Config couldn't be loaded/parsed; BYOK status indeterminate.
    Unknown,
}
/// Whether this session and model combination uses a refreshable session token. Gates on stable inputs, not `Credentials.auth_type`. `model_byok` still excludes genuine per-model BYOK, whose keys are not refreshable.
/// It must **not** demote a live session to non-refreshable api-key mode. Instead, `Unknown` refreshes only when `endpoint_is_first_party`.
/// On a first-party host (cli-chat-proxy / first-party API) the session token cannot leak to a third-party BYOK endpoint. A definite `NotByok` always refreshes (it only ever routes to the session endpoint); a definite `Byok` never does.
pub(crate) fn session_token_auth_gate(
    is_session_based_method: bool,
    model_byok: ModelByok,
    endpoint_is_first_party: bool,
) -> bool {
    is_session_based_method
        && match model_byok {
            ModelByok::NotByok => true,
            ModelByok::Byok => false,
            ModelByok::Unknown => endpoint_is_first_party,
        }
}

pub const AUTH_ERROR_SESSION_EXPIRED: &str =
    "Session expired. Run `grok login` to re-authenticate.";

pub const AUTH_ERROR_API_KEY: &str = "Authentication failed. Run `grok login`, set XAI_API_KEY, or add api_key to ~/.grok/config.toml.";

/// Next ACP method id when `cached_token` cannot proceed (missing / expired / legacy WebLogin), or `None` when fallthrough is forbidden.
/// Unpinned: prefer non-interactive `xai.api_key` when advertiseable, else interactive `grok.com`. Pinned `oidc`: **no** fallthrough to api_key; return `None` so the caller fails auth.
/// Pinned `api_key` should not reach this path (cached_token is not advertised).
pub(crate) fn method_id_after_cached_token_unavailable(
    has_external_api_key: bool,
    preferred_method: Option<PreferredAuthMethod>,
) -> Option<&'static str> {
    match preferred_method {
        Some(PreferredAuthMethod::Oidc) | Some(PreferredAuthMethod::ApiKey) => None,
        None => Some(if has_external_api_key {
            XAI_API_KEY_METHOD_ID
        } else {
            GROK_COM_METHOD_ID
        }),
    }
}

/// Error when `preferred_method=api_key` but no key/BYOK credentials exist.
pub const PREFERRED_API_KEY_UNAVAILABLE: &str = "preferred_method=api_key but no API key is configured (set XAI_API_KEY or model api_key/env_key in config.toml).";

/// Error when `preferred_method=oidc` but the session path cannot proceed.
pub const PREFERRED_OIDC_UNAVAILABLE: &str =
    "preferred_method=oidc but no session is available. Run `grok login` to authenticate.";

pub const XAI_API_KEY_METHOD_ID: &str = "xai.api_key";
pub(crate) fn xai_api_key_auth_method() -> acp::AuthMethod {
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID),
            "xai.api_key".to_string(),
        )
        .description(Some(format!(
            "{XAI_API_KEY_ENV_VAR} or api_key/env_key in config.toml"
        ))),
    )
}

pub const CACHED_TOKEN_AUTH_METHOD_ID: &str = "cached_token";
pub(crate) fn cached_token_auth_method() -> acp::AuthMethod {
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID),
            "cached_token".to_string(),
        )
        .description(Some("Cached token from ~/.grok/auth.json".to_string())),
    )
}

pub const GROK_COM_METHOD_ID: &str = "grok.com";

/// Grok OAuth2/OIDC auth. Method id `"grok.com"` kept for ACP wire compatibility.
pub(crate) fn grok_com_auth_method(
    label: Option<&str>,
    has_auth_provider_command: bool,
) -> acp::AuthMethod {
    let name = label.unwrap_or("Grok");
    let meta = if has_auth_provider_command {
        let mut m = acp::Meta::new();
        m.insert("external_provider".to_owned(), serde_json::json!(true));
        Some(m)
    } else {
        None
    };
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(acp::AuthMethodId::new(GROK_COM_METHOD_ID), name.to_string())
            .description(Some(format!("Sign in with {name}")))
            .meta(meta),
    )
}

pub const OIDC_METHOD_ID: &str = "oidc";
pub(crate) fn oidc_auth_method(issuer: &str, label: Option<&str>) -> acp::AuthMethod {
    let name = label
        .map(|l| l.to_string())
        .unwrap_or_else(|| format!("Single sign-on ({})", issuer));
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(acp::AuthMethodId::new(OIDC_METHOD_ID), name.clone())
            .description(Some(format!("Sign in with {name}"))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{Config, resolve_model_list};
    use agent_client_protocol as acp;
    use serial_test::serial;

    /// When API-key credentials are advertiseable, fall through from a dead `cached_token` to non-interactive `xai.api_key` (not browser OAuth).
    /// Covers the both-advertised case: `has_cached_token` was true at initialize but the session later went missing/expired/legacy. Advertise order still puts `xai.api_key` first while `default_auth_method_id` prefers session.
    /// After the session fails, this helper must still pick `xai.api_key`.
    #[test]
    fn after_cached_token_unavailable_prefers_api_key_when_advertiseable() {
        assert_eq!(
            method_id_after_cached_token_unavailable(true, None),
            Some(XAI_API_KEY_METHOD_ID),
        );
    }

    /// With no advertiseable API-key credentials, fall to interactive `grok.com`.
    #[test]
    fn after_cached_token_unavailable_falls_to_grok_com_without_api_key() {
        assert_eq!(
            method_id_after_cached_token_unavailable(false, None),
            Some(GROK_COM_METHOD_ID),
        );
    }

    /// Pinned methods never fall through between api_key and oidc.
    #[test]
    fn after_cached_token_unavailable_fails_closed_when_pinned() {
        assert_eq!(
            method_id_after_cached_token_unavailable(true, Some(PreferredAuthMethod::Oidc)),
            None,
        );
        assert_eq!(
            method_id_after_cached_token_unavailable(true, Some(PreferredAuthMethod::ApiKey)),
            None,
        );
    }

    /// Classifier matrix for all auth method variants.
    #[test]
    fn auth_method_kind_classifier_matrix() {
        let session_methods = [
            CACHED_TOKEN_AUTH_METHOD_ID,
            GROK_COM_METHOD_ID,
            OIDC_METHOD_ID,
        ];
        for method_id in session_methods {
            let id = acp::AuthMethodId::new(method_id);
            let kind = AuthMethodKind::from_id(&id);
            assert!(
                kind.is_session_based(),
                "{method_id}: kind must be session-based"
            );
            assert!(
                is_session_based_method(&id),
                "{method_id}: wrapper must agree"
            );
        }
        let api_id = acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID);
        let api_kind = AuthMethodKind::from_id(&api_id);
        assert!(!api_kind.is_session_based());
        assert!(api_kind.is_api_key());
        assert!(!is_session_based_method(&api_id));
        assert!(!is_session_based_method(&acp::AuthMethodId::new(
            "unknown-method"
        )));
    }

    use distill_test_support::EnvGuard;

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Default inputs to `build_auth_methods` representing a session-only user with no API key anywhere.
    /// Tests override only the fields they care about.
    fn default_inputs() -> AuthMethodsBuildInputs<'static> {
        AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: false,
            has_enterprise_oidc: false,
            enterprise_oidc_issuer: None,
            login_label: None,
            has_auth_provider_command: false,
            preferred_method: None,
        }
    }

    fn method_ids(built: &BuiltAuthMethods) -> Vec<&str> {
        built.methods.iter().map(|m| m.id().0.as_ref()).collect()
    }

    fn default_id(built: &BuiltAuthMethods) -> Option<&str> {
        built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.as_ref())
    }

    fn first_kind(methods: &[acp::AuthMethod]) -> Option<AuthMethodKind> {
        methods.first().map(|m| AuthMethodKind::from_id(m.id()))
    }

    // build_auth_methods regression: pin production call-site ordering.
    // Reordering so `xai.api_key` is after login methods must fail the tests below.

    /// BYOK with only per-model `env_key` must list `xai.api_key` first.
    #[test]
    fn enterprise_byok_first_method_is_xai_api_key() {
        let inputs = AuthMethodsBuildInputs {
            has_external_api_key: true, // enterprise user with resolved per-model env_key
            has_cached_token: false,
            ..default_inputs()
        };
        let built = build_auth_methods(inputs);

        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::XaiApiKey),
            "BYOK enterprise-style: auth_methods.first() MUST be xai.api_key \
             (deferred-to-last ordering sends users to the login screen)",
        );
        assert_eq!(
            built
                .default_auth_method_id
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some(XAI_API_KEY_METHOD_ID),
        );
        // Cross-check with the pager-side predicate: the first method must not require interactive login
        // That is the exact condition the pager's `startup_auth_metadata()` uses
        assert!(
            built
                .methods
                .first()
                .is_some_and(|m| !AuthMethodKind::from_id(m.id()).needs_interactive_login()),
            "first method MUST NOT need interactive login when xai.api_key is available",
        );
    }

    /// BYOK plus a cached session token: xai.api_key stays first in the methods list, skipping the login screen.
    /// `default_auth_method_id` is still `cached_token`, which keeps OIDC refresh alive.
    #[test]
    fn byok_with_cached_token_keeps_xai_api_key_first() {
        let inputs = AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: true,
            ..default_inputs()
        };
        let built = build_auth_methods(inputs);

        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::XaiApiKey),
            "xai.api_key MUST precede cached_token in advertised order",
        );
        // Sanity: cached_token still appears, just second.
        assert!(
            built
                .methods
                .iter()
                .any(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::CachedToken),
            "cached_token must still be advertised when present",
        );
        // cached_token wins for default_auth_method_id (keeps OIDC refresh alive).
        assert_eq!(
            built
                .default_auth_method_id
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some(CACHED_TOKEN_AUTH_METHOD_ID),
        );
    }

    /// Session-only user (no API key anywhere): cached_token first, then `grok.com`.
    /// `auth_methods.first()` does NOT need interactive login, so this user also skips the login screen at startup.
    #[test]
    fn session_only_user_first_method_is_cached_token() {
        let inputs = AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: true,
            ..default_inputs()
        };
        let built = build_auth_methods(inputs);

        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::CachedToken)
        );
        assert_eq!(
            built
                .default_auth_method_id
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some(CACHED_TOKEN_AUTH_METHOD_ID),
        );
    }

    /// Brand-new user (no API key, no cached token): only `grok.com` is advertised, and the pager will (correctly) show the login screen.
    /// `default_auth_method_id` is None so the pager falls back to the advertised login method.
    #[test]
    fn fresh_user_only_advertises_grok_com_and_requires_login() {
        let built = build_auth_methods(default_inputs());

        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::GrokCom));
        assert!(built.default_auth_method_id.is_none());
        assert_eq!(built.methods.len(), 1);
    }

    /// Enterprise OIDC replaces `grok.com` (mutually exclusive).
    /// xai.api_key, when present, still leads.
    #[test]
    fn enterprise_oidc_replaces_grok_com_but_xai_api_key_still_first() {
        let inputs = AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: false,
            has_enterprise_oidc: true,
            enterprise_oidc_issuer: Some("https://sso.example.com"),
            ..default_inputs()
        };
        let built = build_auth_methods(inputs);

        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::XaiApiKey));
        assert!(
            built
                .methods
                .iter()
                .any(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::Oidc),
            "oidc must be advertised when has_enterprise_oidc",
        );
        assert!(
            !built
                .methods
                .iter()
                .any(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::GrokCom),
            "grok.com and oidc are mutually exclusive",
        );
    }

    /// `has_auth_provider_command` reaches the `grok.com` method as `meta.external_provider = true`.
    /// Pinned here so the pager's `AuthStartMode::Command` path keeps working.
    #[test]
    fn auth_provider_command_sets_external_provider_meta() {
        let inputs = AuthMethodsBuildInputs {
            has_auth_provider_command: true,
            login_label: Some("Acme Corp"),
            ..default_inputs()
        };
        let built = build_auth_methods(inputs);

        let grok = built
            .methods
            .iter()
            .find(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::GrokCom)
            .expect("grok.com must be advertised");
        assert_eq!(grok.name(), "Acme Corp");
        let meta = grok.meta().expect("meta should be set");
        assert_eq!(
            meta.get("external_provider").and_then(|v| v.as_bool()),
            Some(true),
        );
    }

    // ── End-to-end: enterprise TOML to resolved models to build_auth_methods ─

    /// END-TO-END REGRESSION TEST: parses the literal enterprise-style `~/.grok/config.toml` skeleton from the bug report, walks it through the same predicate (`should_advertise_xai_api_key`) and the same list-builder (`build_auth_methods`) that `MvpAgent::initialize()` uses in production, and asserts that `auth_methods.first()` is `xai.api_key` (which causes the pager to skip the login screen).
    /// This is the test that *would have caught* that regression.
    /// If the bug returns (xai.api_key pushed LAST when only per-model credentials exist), `first_kind` stops being `XaiApiKey` and this test fails.
    #[test]
    #[serial]
    fn enterprise_byok_config_does_not_require_login() {
        const TEST_ENV_VAR: &str = "TEST_ENTERPRISE_REGRESSION_AUTH_TOKEN";

        // Make sure no global key is masking the per-model path we're trying to exercise
        // Held until end-of-scope so we restore on panic too
        let _global = EnvGuard::unset(XAI_API_KEY_ENV_VAR);

        let dm = distill_models::default_model();
        let toml: toml::Value = toml::from_str(&format!(
            r#"
            [model."{dm}"]
            model = "{dm}"
            base_url = "https://inference.example.com/v1"
            context_window = 200000
            env_key = "{TEST_ENV_VAR}"
            "#,
        ))
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml).expect("config should parse");
        let models = resolve_model_list(&cfg, None);
        let model = models.get(dm).expect("enterprise-style model should exist");
        assert_eq!(
            model.env_key.as_ref().map(|k| k.names()),
            Some(vec![TEST_ENV_VAR])
        );

        // Without the env var present, has_own_credentials() and the predicate return false, and the builder advertises only the login method
        // Confirms the predicate isn't trivially true
        {
            let _unset = EnvGuard::unset(TEST_ENV_VAR);
            let has_external_api_key = should_advertise_xai_api_key(false, models.values());
            assert!(!has_external_api_key);
            let built = build_auth_methods(AuthMethodsBuildInputs {
                has_external_api_key,
                ..default_inputs()
            });
            assert_ne!(
                first_kind(&built.methods),
                Some(AuthMethodKind::XaiApiKey),
                "without env_key resolved, xai.api_key must NOT be advertised first",
            );
        }

        // With the env var present (the actual enterprise scenario), the predicate returns true
        // The builder MUST put `xai.api_key` first so the pager's `startup_auth_metadata()` returns `needs_login = false`
        {
            let _set = EnvGuard::set(TEST_ENV_VAR, "enterprise-secret-token");
            let has_external_api_key = should_advertise_xai_api_key(false, models.values());
            assert!(has_external_api_key);
            let built = build_auth_methods(AuthMethodsBuildInputs {
                has_external_api_key,
                // Realistic enterprise user: no cached session token, default grok.com login (no enterprise OIDC)
                has_cached_token: false,
                ..default_inputs()
            });
            assert_eq!(
                first_kind(&built.methods),
                Some(AuthMethodKind::XaiApiKey),
                "BYOK: xai.api_key must be auth_methods.first(); deferred-to-last \
                 ordering sends enterprise users to the login screen",
            );
            assert!(
                built
                    .methods
                    .first()
                    .is_some_and(|m| !AuthMethodKind::from_id(m.id()).needs_interactive_login()),
                "auth_methods.first() MUST NOT need interactive login -- this \
                 is the exact predicate the pager's startup_auth_metadata() \
                 uses to decide whether to show the login screen",
            );
        }
    }

    /// `XAI_API_KEY` alone (no per-model creds) also triggers advertising `xai.api_key` as the first method.
    /// Historical "external key" path; covered here so the predicate keeps treating env-var-only users the same as per-model users.
    #[test]
    #[serial]
    fn global_external_api_key_advertises_xai_api_key_first() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-external-key");
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);
        let has_external_api_key = should_advertise_xai_api_key(false, models.values());
        assert!(has_external_api_key);
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key,
            ..default_inputs()
        });
        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::XaiApiKey));
    }

    /// Admin kill switch (`disable_api_key_auth`): the predicate must return false even when credentials are available everywhere.
    /// That includes both the global env var and a per-model env_key.
    /// The builder then never advertises `xai.api_key`, and the pager sends the user to the deployment's login method instead.
    #[test]
    #[serial]
    fn disable_api_key_auth_suppresses_xai_api_key_method() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-external-key");
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);

        // Flag off: today's behavior (advertised first).
        assert!(should_advertise_xai_api_key(false, models.values()));

        // Flag on: never advertised, regardless of credentials.
        let has_external_api_key = should_advertise_xai_api_key(true, models.values());
        assert!(!has_external_api_key);
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key,
            ..default_inputs()
        });
        assert!(
            !built
                .methods
                .iter()
                .any(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::XaiApiKey),
            "xai.api_key must not be advertised when disable_api_key_auth is set",
        );
        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::GrokCom),
            "with api-key auth disabled and no cached token, the login method \
             must lead so the pager requires interactive login",
        );
        assert!(built.default_auth_method_id.is_none());
    }

    #[test]
    #[serial]
    fn env_key_probe_unusable_suppresses_advertise_without_byok() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-dead-key");
        let _legacy = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);
        assert!(
            should_advertise_xai_api_key(false, models.values()),
            "presence-only helper still sees the env key"
        );
        assert!(
            !should_advertise_xai_api_key_with_env_ok(false, models.values(), false),
            "probe-unusable env key alone must not advertise"
        );
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            ..default_inputs()
        });
        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::GrokCom));
    }

    #[test]
    #[serial]
    fn env_key_probe_ok_still_advertises() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-live-key");
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);
        assert!(should_advertise_xai_api_key_with_env_ok(
            false,
            models.values(),
            true
        ));
    }

    #[test]
    #[serial]
    fn byok_advertises_even_when_env_probe_unusable() {
        const TEST_ENV_VAR: &str = "TEST_BYOK_PROBE_INDEPENDENT_TOKEN";
        let _unset = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
        let _legacy = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
        let _byok = EnvGuard::set(TEST_ENV_VAR, "enterprise-secret-token");

        let dm = distill_models::default_model();
        let toml: toml::Value = toml::from_str(&format!(
            r#"
            [model."{dm}"]
            model = "{dm}"
            base_url = "https://inference.example.com/v1"
            context_window = 200000
            env_key = "{TEST_ENV_VAR}"
            "#,
        ))
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml).expect("config should parse");
        let models = resolve_model_list(&cfg, None);
        assert!(
            should_advertise_xai_api_key_with_env_ok(false, models.values(), false),
            "BYOK must not depend on the first-party env probe"
        );
    }

    /// Legacy `GROK_CODE_XAI_API_KEY` env var is accepted as a fallback when `XAI_API_KEY` is not set, so existing deployments keep working.
    #[test]
    #[serial]
    fn legacy_env_var_fallback_advertises_xai_api_key() {
        let _unset_new = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
        let _set_legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "xai-legacy-key");
        assert!(has_xai_api_key_env());
        assert_eq!(read_xai_api_key_env().unwrap(), "xai-legacy-key");

        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);
        let has_external_api_key = should_advertise_xai_api_key(false, models.values());
        assert!(has_external_api_key);
    }

    /// When both `XAI_API_KEY` and `GROK_CODE_XAI_API_KEY` are set, the new name takes precedence.
    #[test]
    #[serial]
    fn new_env_var_takes_precedence_over_legacy() {
        let _new = EnvGuard::set(XAI_API_KEY_ENV_VAR, "new-key");
        let _legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "old-key");
        assert_eq!(read_xai_api_key_env().unwrap(), "new-key");
    }

    // -- grok login --legacy regression coverage ------------------------ `grok login --legacy` produces a GrokAuth with `auth_mode: WebLogin`, `oidc_issuer: None`, and no `expires_at` (30-day hardcoded TTL)
    // When this token is in the `GROK_AUTH` env var (or the legacy scope fallback in auth.json), `AuthManager::new` returns it from `current()`
    // That feeds `has_cached_token = true` into `build_auth_methods`, which puts `cached_token` first `startup_auth_metadata()` then returns `needs_login = false`: legacy users get frictionless auth, no login screen This test pins the env-var path (highest priority in AuthManager) end-to-end

    /// END-TO-END REGRESSION TEST for a legacy auth token (WebLogin, no expires_at) in the `GROK_AUTH` env var with no other auth available.
    /// `AuthManager` MUST load it and `build_auth_methods` must advertise `cached_token` first.
    /// The pager therefore skips the login screen (frictionless legacy auth).
    #[test]
    #[serial]
    fn grok_login_legacy_token_does_not_require_login() {
        use distill_login::{AuthManager, AuthMode, GrokAuth, GrokComConfig};

        // Ensure clean slate for "no other auth available".
        let _g1 = EnvGuard::unset("GROK_AUTH_PATH");
        let _g2 = EnvGuard::unset(XAI_API_KEY_ENV_VAR);

        // Construct a legacy-style token exactly as `grok login --legacy` produces it
        // That means WebLogin mode, no OIDC fields, no refresh_token, no expires_at (is_expired falls back to the 30-day age check)
        let legacy_token = GrokAuth {
            key: "legacy-relay-token".into(),
            auth_mode: AuthMode::WebLogin,
            create_time: chrono::Utc::now(),
            user_id: "legacy-user".into(),
            email: Some("legacy@example.com".into()),
            oidc_issuer: None,
            oidc_client_id: None,
            refresh_token: None,
            expires_at: None,
            ..GrokAuth::test_default()
        };

        // Provide it via the GROK_AUTH env var (highest priority code path in AuthManager::new)
        // This is the "legacy auth token exists in the env" case with no other auth
        let legacy_json = serde_json::to_string(&legacy_token).expect("serialize legacy token");
        let _g = EnvGuard::set("GROK_AUTH", &legacy_json);

        // AuthManager picks it up from the env var directly (no file needed).
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let mgr = AuthManager::new(dir.path(), cfg);
        let current = mgr.current();
        assert!(
            current.is_some(),
            "legacy token in GROK_AUTH env MUST be loaded directly -- if this fails, \
             users with legacy auth in env would be sent to the login screen",
        );
        assert_eq!(
            current.as_ref().unwrap().key,
            "legacy-relay-token",
            "loaded token must match the one injected via env",
        );

        // Derive has_cached_token exactly as initialize() does
        let has_cached_token = mgr.current().is_some();
        assert!(has_cached_token);

        // With only this legacy token (no xai api key), the first method must be cached_token so the pager skips the login screen
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token,
            ..default_inputs()
        });

        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::CachedToken),
            "legacy token in env: cached_token MUST be auth_methods.first() \
             (pager startup_auth_metadata returns needs_login=false)",
        );
        assert!(
            built
                .methods
                .first()
                .is_some_and(|m| !AuthMethodKind::from_id(m.id()).needs_interactive_login()),
            "auth_methods.first() MUST NOT need interactive login when legacy token \
             is in env -- prevents login screen regression",
        );
        assert_eq!(
            built
                .default_auth_method_id
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some(CACHED_TOKEN_AUTH_METHOD_ID),
        );
    }

    /// Negative case for the legacy flow: when auth.json does NOT contain a legacy-scope entry, AuthManager::current() is None.
    /// has_cached_token is then false and build_auth_methods advertises only the login method.
    /// This pins the predicate's "no" answer so the test above isn't trivially passing.
    #[test]
    #[serial]
    fn no_legacy_token_means_no_cached_token_advertised() {
        use distill_login::{AuthManager, GrokComConfig};

        let _g1 = EnvGuard::unset("GROK_AUTH");
        let _g2 = EnvGuard::unset("GROK_AUTH_PATH");

        let dir = tempfile::tempdir().unwrap();
        // No auth.json in the tempdir.
        let cfg = GrokComConfig::default();
        let mgr = AuthManager::new(dir.path(), cfg);
        assert!(mgr.current().is_none());

        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: mgr.current().is_some(),
            ..default_inputs()
        });
        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::GrokCom),
            "no cached token AND no api key: pager must show login (grok.com first)",
        );
    }

    // ── preferred_method pin (fail-closed) ──────────────────────────────

    #[test]
    fn pin_api_key_with_key_only_advertises_api_key() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: true,
            preferred_method: Some(PreferredAuthMethod::ApiKey),
            ..default_inputs()
        });
        assert_eq!(method_ids(&built), vec![XAI_API_KEY_METHOD_ID]);
        assert_eq!(default_id(&built), Some(XAI_API_KEY_METHOD_ID));
    }

    #[test]
    fn pin_api_key_without_key_fails_closed_even_with_session() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: true,
            preferred_method: Some(PreferredAuthMethod::ApiKey),
            ..default_inputs()
        });
        assert!(built.methods.is_empty());
        assert!(built.default_auth_method_id.is_none());
    }

    #[test]
    fn pin_oidc_with_session_hides_api_key() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: true,
            preferred_method: Some(PreferredAuthMethod::Oidc),
            ..default_inputs()
        });
        assert_eq!(
            method_ids(&built),
            vec![CACHED_TOKEN_AUTH_METHOD_ID, GROK_COM_METHOD_ID]
        );
        assert_eq!(default_id(&built), Some(CACHED_TOKEN_AUTH_METHOD_ID));
    }

    #[test]
    fn pin_oidc_without_session_is_interactive_only() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: false,
            preferred_method: Some(PreferredAuthMethod::Oidc),
            ..default_inputs()
        });
        assert_eq!(method_ids(&built), vec![GROK_COM_METHOD_ID]);
        assert!(built.default_auth_method_id.is_none());
    }
}
