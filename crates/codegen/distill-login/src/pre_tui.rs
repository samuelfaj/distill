// Modified for Distill by Samuel Fajreldines, 2026.
//! Interactive-pager external login that runs on the real TTY before raw mode.
//!
//! `auth_provider_command` already works via `grok login` because stderr is inherited.
//! The TUI path instead pipes that stderr into the welcome copy-link overlay.
//! That overlay is unusable in Docker (no host browser, mouse capture, wrapped URLs).
//! This module is the first-launch equivalent of `grok login`.
//! It prints the provider URL on the real terminal, persists the token, then lets the pager start already authenticated.
//!
//! Deliberately does **not** call [`super::flow::run_auth_flow`]: on provider failure that falls through to browser OIDC.
//! The fallthrough (`Signing in with browser instead...`) would re-enter the TUI splash this path exists to skip.

use std::sync::Arc;

use super::flow::{report_signed_in, run_external_auth_provider};
use super::{AuthManager, GrokAuth, GrokComConfig, try_ensure_fresh_auth};
use distill_shell_base::util::distill_home;

/// Whether the pager should attempt a pre-TUI provider login.
/// Fresh-credential and `--force-login` checks are async and live in [`maybe_run_pre_tui_external_login`].
/// This is the sync half so tests can assert the TTY and provider gate without spawning a binary.
pub fn should_attempt_pre_tui_external_login(has_provider: bool, stdin_is_tty: bool) -> bool {
    has_provider && stdin_is_tty
}

/// Result of [`maybe_run_pre_tui_external_login`].
#[derive(Debug)]
pub enum PreTuiLoginOutcome {
    /// The gate did not pass, or a usable cached credential already exists.
    Skipped,
    /// Provider minted a session credential; `auth.json` is written.
    SignedIn(Box<GrokAuth>),
}

/// Run `auth_provider_command` on the real terminal when the interactive pager needs a sign-in. Call **before** `redirect_native_stderr` and raw mode. On provider failure this returns `Err` (no OIDC/device fallthrough).
/// The pager should exit before taking over the TTY.
pub async fn maybe_run_pre_tui_external_login(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
    force_login: bool,
    stdin_is_tty: bool,
) -> anyhow::Result<PreTuiLoginOutcome> {
    let Some(cmd) = grok_com_config.auth_provider_command.as_deref() else {
        return Ok(PreTuiLoginOutcome::Skipped);
    };
    if !should_attempt_pre_tui_external_login(true, stdin_is_tty) {
        return Ok(PreTuiLoginOutcome::Skipped);
    }
    if !force_login
        && try_ensure_fresh_auth(grok_com_config, proxy_base_url.clone())
            .await
            .is_some()
    {
        return Ok(PreTuiLoginOutcome::Skipped);
    }

    let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &distill_home::distill_home(),
        grok_com_config.clone(),
        proxy_base_url,
    ));
    auth_manager.configure_refresher(Some(cmd.to_owned()), None);
    run_pre_tui_external_login_with(&auth_manager, cmd, force_login).await
}

/// Runs the provider and persists the result.
/// The `AuthManager` is injected so tests can use a temp grok-home instead of the process-cached [`distill_home::distill_home`].
pub async fn run_pre_tui_external_login_with(
    auth_manager: &Arc<AuthManager>,
    command: &str,
    force_login: bool,
) -> anyhow::Result<PreTuiLoginOutcome> {
    let over_stale_credential = force_login || auth_manager.is_expired();
    let (auth, _) =
        run_external_auth_provider(command, auth_manager, over_stale_credential, None).await?;
    report_signed_in(&auth);
    Ok(PreTuiLoginOutcome::SignedIn(Box::new(auth)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GrokAuth;

    fn dead_proxy_url() -> String {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        format!("http://127.0.0.1:{port}")
    }

    fn isolated_manager(cmd: Option<&str>) -> (tempfile::TempDir, Arc<AuthManager>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_provider_command: cmd.map(str::to_owned),
            ..GrokComConfig::default()
        };
        let mgr =
            Arc::new(AuthManager::new(dir.path(), cfg).with_proxy_base_url(&dead_proxy_url()));
        (dir, mgr)
    }

    #[test]
    fn should_attempt_requires_provider_and_tty() {
        assert!(!should_attempt_pre_tui_external_login(false, true));
        assert!(!should_attempt_pre_tui_external_login(true, false));
        assert!(should_attempt_pre_tui_external_login(true, true));
    }

    #[tokio::test]
    async fn maybe_run_skips_without_provider_or_tty() {
        let cfg = GrokComConfig::default();
        let skipped = maybe_run_pre_tui_external_login(&cfg, dead_proxy_url(), false, true).await;
        assert!(matches!(skipped, Ok(PreTuiLoginOutcome::Skipped)));

        let cfg = GrokComConfig {
            auth_provider_command: Some("printf '%s' token".into()),
            ..GrokComConfig::default()
        };
        let skipped = maybe_run_pre_tui_external_login(&cfg, dead_proxy_url(), false, false).await;
        assert!(matches!(skipped, Ok(PreTuiLoginOutcome::Skipped)));
    }

    #[tokio::test]
    async fn force_login_re_runs_provider_over_cached_token() {
        let (_dir, mgr) = isolated_manager(Some("printf '%s' should-not-run"));
        mgr.hot_swap(GrokAuth {
            key: "cached-token".into(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        assert!(mgr.current().is_some());

        let signed_in = run_pre_tui_external_login_with(&mgr, "printf '%s' new-token", true)
            .await
            .expect("force-login must re-run the provider");
        match signed_in {
            PreTuiLoginOutcome::SignedIn(auth) => assert_eq!(auth.key, "new-token"),
            PreTuiLoginOutcome::Skipped => panic!("force-login must not skip"),
        }
    }

    #[tokio::test]
    async fn provider_echo_token_signs_in() {
        let (_dir, mgr) = isolated_manager(Some("printf '%s' xai-ext-token"));
        let outcome = run_pre_tui_external_login_with(&mgr, "printf '%s' xai-ext-token", false)
            .await
            .expect("provider must mint");
        match outcome {
            PreTuiLoginOutcome::SignedIn(auth) => {
                assert_eq!(auth.key, "xai-ext-token");
                assert!(mgr.current().is_some());
            }
            PreTuiLoginOutcome::Skipped => panic!("unsigned start must run the provider"),
        }
    }

    #[tokio::test]
    async fn provider_failure_is_err_without_browser_fallthrough() {
        let (_dir, mgr) = isolated_manager(Some("false"));
        let err = run_pre_tui_external_login_with(&mgr, "false", false)
            .await
            .expect_err("non-zero provider must fail closed");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("Signing in with browser"),
            "must not fall through to OIDC/device: {msg}"
        );
        assert!(mgr.current().is_none(), "failed mint must not persist");
    }

    #[tokio::test]
    async fn large_inherited_stderr_does_not_deadlock() {
        let cmd = r#"sh -c 'i=0; while [ $i -lt 2000 ]; do printf "%s" "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" >&2; i=$((i+1)); done; printf token'"#;
        let (_dir, mgr) = isolated_manager(Some(cmd));
        let outcome = run_pre_tui_external_login_with(&mgr, cmd, false)
            .await
            .expect("inherited stderr must not deadlock the provider");
        match outcome {
            PreTuiLoginOutcome::SignedIn(auth) => assert_eq!(auth.key, "token"),
            PreTuiLoginOutcome::Skipped => panic!("provider must run"),
        }
    }
}
