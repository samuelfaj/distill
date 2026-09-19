// Modified for Distill by Samuel Fajreldines, 2026.
//! DistillEnvironment configuration for the shell crate family.
//!
//! The environment presets (per-environment endpoint URLs, the staging trust check, `EnvVarGuard`) live in the [`distill_env`] leaf crate.
//! Sibling crates (telemetry, tools, workspace) share them without depending on this crate.
//! This module re-exports them and hosts the shell-specific gateway-bridge env vars.
//!
//! # Gateway-bridge mode (env-only)
//! - `GROK_GATEWAY_URL` — when set to a valid URL, `MvpAgent` spawns a
//!   per-session gateway bridge actor and routes prompts through it.
//!   When unset, sessions created in gateway mode fall back to [`DistillEnvironment::gateway_ws_url`] and everything else stays in local mode.
pub use distill_env::{
    DistillEnvironment, PROD_ASSET_SERVER_URL, PROD_CLI_CHAT_PROXY_BASE_URL, PROD_GATEWAY_WS_URL,
    PROD_RELAY_WS_URL, PROD_WS_ORIGIN,
};
/// Public Computer Hub WebSocket URL used by the local-workspace supervisor (`workspace_server --hub-url`) when `agent_config.hub.url` is unset.
pub const PROD_COMPUTER_HUB_WS_URL: &str = "wss://computer-hub.grok.com/v1/tools";
#[cfg(any(test, feature = "test-support"))]
pub use distill_env::EnvVarGuard;
/// Env var that opts a process into gateway-bridge mode.
/// When set to a parseable URL, `session/new` / `session/load` spawns a per-session `gateway_bridge` actor in the shell.
/// When unset the process stays in local mode.
pub const GROK_GATEWAY_URL_ENV: &str = "GROK_GATEWAY_URL";
/// Client kill switch for the gateway-bridge custom-method passthrough. Set to `1` / `true` to force every `custom_method` call back onto agent-local dispatch regardless of the routing table or negotiated capability.
/// That gives an instant revert without a redeploy if the channel misbehaves. Unset, `0`, or `false` keeps normal routing.
pub const GROK_DISABLE_CUSTOM_BRIDGE_ENV: &str = "GROK_DISABLE_CUSTOM_BRIDGE";
/// `true` when the custom-method bridge passthrough is force-disabled via [`GROK_DISABLE_CUSTOM_BRIDGE_ENV`].
/// Accepts `1`/`true` (case-insensitive).
pub fn custom_bridge_disabled() -> bool {
    std::env::var(GROK_DISABLE_CUSTOM_BRIDGE_ENV)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}
/// Parse `GROK_GATEWAY_URL` into a [`url::Url`].
///
/// This build ignores the variable and stays in local mode (warns if set).
pub fn parse_gateway_url() -> Option<url::Url> {
    let raw = std::env::var(GROK_GATEWAY_URL_ENV).ok()?;
    if raw.is_empty() {
        return None;
    }
    tracing::warn!(
        env = GROK_GATEWAY_URL_ENV,
        "GROK_GATEWAY_URL is set but this build does not support gateway-bridge mode; staying in local mode"
    );
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_gateway_url_returns_none_when_unset() {
        let _env = EnvVarGuard::remove(GROK_GATEWAY_URL_ENV);
        assert!(parse_gateway_url().is_none());
    }
    #[test]
    fn parse_gateway_url_returns_none_when_empty() {
        let _env = EnvVarGuard::set(GROK_GATEWAY_URL_ENV, "");
        assert!(parse_gateway_url().is_none());
    }
    #[test]
    fn parse_gateway_url_hard_off_without_gateway_bridge() {
        let _env = EnvVarGuard::set(GROK_GATEWAY_URL_ENV, "wss://gateway.example.com/ws");
        assert!(
            parse_gateway_url().is_none(),
            "valid URL must still be ignored in this build"
        );
    }
    #[test]
    fn custom_bridge_disabled_defaults_false_when_unset() {
        let _env = EnvVarGuard::remove(GROK_DISABLE_CUSTOM_BRIDGE_ENV);
        assert!(!custom_bridge_disabled());
    }
    #[test]
    fn custom_bridge_disabled_true_for_one_and_true() {
        for v in ["1", "true", "TRUE", " true "] {
            let _env = EnvVarGuard::set(GROK_DISABLE_CUSTOM_BRIDGE_ENV, v);
            assert!(
                custom_bridge_disabled(),
                "{v:?} must disable the custom bridge"
            );
        }
    }
    #[test]
    fn custom_bridge_disabled_false_for_zero_and_garbage() {
        for v in ["0", "false", "", "no"] {
            let _env = EnvVarGuard::set(GROK_DISABLE_CUSTOM_BRIDGE_ENV, v);
            assert!(
                !custom_bridge_disabled(),
                "{v:?} must leave the custom bridge enabled"
            );
        }
    }
}
