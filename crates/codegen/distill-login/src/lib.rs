// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Authentication subsystem for the grok shell crate family.
//!
//! Extracted from `distill-shell::auth`; the shell re-exports this crate as
//! `distill_shell::auth` so existing `crate::*` paths keep resolving.
#![deny(clippy::indexing_slicing)]
pub use distill_telemetry::unified_log;
pub mod api_key_probe;
pub mod attribution;
pub mod auth_method;
pub mod auth_provider;
pub mod backend;
pub mod config;
pub mod credential_provider;
pub mod device_code;
pub mod error;
pub mod external_auth;
pub mod flow;
pub mod grok_auth_credentials;
pub mod jwt;
pub mod manager;
pub mod model;
pub mod oidc;
pub mod pre_tui;
pub mod recovery;
pub mod refresh;
pub mod single_flight;
pub mod storage;
pub mod token_output;
pub mod token_type;
pub use api_key_probe::{
    DEFAULT_PROBE_TIMEOUT, first_party_env_key_allows_advertise, should_probe_first_party_env_key,
};
pub use auth_provider::AuthProviderRef;
pub use auth_provider::{
    PROVIDER_TIMEOUT_CEILING_SECS, PROVIDER_TOKEN_EXPIRY_SKEW_SECS, ProviderRefreshOutcome,
};
#[cfg(any(test, feature = "test-support"))]
pub use auth_provider::{test_backdate_provider_mint, test_counting_provider};
pub use config::LEGACY_AUTH_SCOPE;
pub use config::{
    ForceLoginTeam, GrokComConfig, OAuth2ProviderConfig, OidcAuthConfig, PreferredAuthMethod,
    XAI_OAUTH2_ISSUER, is_xai_oauth2_issuer, xai_oauth2_issuer,
};
pub use config::{
    force_login_team_from_env, force_login_team_from_requirements_value, resolve_force_login_team,
};
pub use external_auth::{ExternalRefreshError, parse_output, refresh_with_command};
pub use flow::{
    AuthChannels, mint_session_noninteractive, run_auth_flow, run_auth_flow_with_stderr_bridge,
    try_noninteractive_auth_no_mint,
};
pub use flow::{
    AuthUrlInfo, AuthUrlMode, LoginTransportOverride, LogoutResult, ensure_authenticated,
    ensure_authenticated_or_noninteractive, ensure_authenticated_with_override, perform_logout,
    run_cli_login, try_ensure_fresh_auth,
};
pub use jwt::{is_jwt_expired_or_near, parse_jwt_expiration};
pub use pre_tui::{PreTuiLoginOutcome, maybe_run_pre_tui_external_login};
pub use distill_config_types::AuthProviderConfig;
pub mod meta;
pub use error::{AuthError, RefreshTokenError, RefreshTokenFailedReason};
pub use manager::{AuthManager, shared_api_key_provider};
pub use manager::{AuthRemedy, CachedTokenState, SilentRefresh};
pub use meta::{AuthMeta, GateInfo};
pub use model::{AuthMode, GrokAuth, lookup_auth};
pub use model::{TOKEN_TTL, UserInfo, default_coding_data_retention_opt_out, is_expired};
pub use refresh::DiagnosticUploader;
pub use storage::auth_json_path;
pub use storage::{clear_api_key, read_api_key, read_auth_json, store_api_key};
