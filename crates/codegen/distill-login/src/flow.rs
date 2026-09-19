// Modified for Distill by Samuel Fajreldines, 2026.
use crate::backend::{ActiveAuthBackend, AuthBackend, LoginRequest};
use crate::config::LEGACY_AUTH_SCOPE;
use crate::{AuthManager, GrokAuth, GrokComConfig, parse_output};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::{mpsc, oneshot};
use distill_http::TransportFailureKind;
use distill_shell_base::util::distill_home;
use distill_telemetry::events::{LoginFailed, LoginFailureKind};
pub type StderrCallback = Box<dyn Fn(&str)>;
/// Reject a cached credential that lacks `oidc_issuer`, has a mismatched issuer, or whose team principal violates the `force_login_team_uuid` pin.
/// Interactive login then starts fresh instead of reusing a stale or wrong-team session.
fn is_cached_credential_compatible(auth: &GrokAuth, grok_com_config: &GrokComConfig) -> bool {
    let expected_issuer = grok_com_config
        .oidc
        .as_ref()
        .map(|c| c.issuer.as_str())
        .or_else(|| grok_com_config.oauth2.as_ref().map(|c| c.issuer.as_str()));
    let issuer_compatible = match (auth.oidc_issuer.as_deref(), expected_issuer) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, Some(_)) => false,
        _ => true,
    };
    if !issuer_compatible {
        return false;
    }
    if let Some(policy) = crate::oidc::login_principal_policy(grok_com_config) {
        let actual = crate::oidc::peek_access_token_principal_id(&auth.key);
        if crate::oidc::enforce_login_principal(Some(&policy), actual.as_deref()).is_err() {
            return false;
        }
    }
    true
}
/// CLI-flag override for the interactive login transport.
/// `--oauth` forces the loopback-callback flow; `--device-auth` forces the device flow.
/// `None` falls through to env / config / default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginTransportOverride {
    /// No CLI override; resolve from env / config / default.
    #[default]
    None,
    /// `--oauth`: force the loopback-callback flow.
    ForceLoopback,
    /// `--device-auth`: force the RFC 8628 device flow.
    ForceDevice,
    /// Transport already resolved (and logged) upstream; `true` means device, `false` means loopback.
    /// The inner flow honors the carried value without re-resolving, so it's never re-logged or mis-attributed to `cli`.
    Preresolved(bool),
}
impl LoginTransportOverride {
    /// Resolve from the `--oauth` / `--device-auth` flags; `--oauth` wins if both are somehow set.
    /// Both the CLI (`run_cli_login`) and ACP (`AuthRequestMeta`) entry points resolve through here.
    pub fn from_flags(force_loopback: bool, force_device: bool) -> Self {
        if force_loopback {
            Self::ForceLoopback
        } else if force_device {
            Self::ForceDevice
        } else {
            Self::None
        }
    }
    /// Map to a `BoolFlag` CLI value: `Some(true)` means device, `Some(false)` means loopback, `None` means no override.
    fn as_cli_bool(self) -> Option<bool> {
        match self {
            Self::None => None,
            Self::ForceLoopback => Some(false),
            Self::ForceDevice => Some(true),
            Self::Preresolved(_) => None,
        }
    }
}
/// Device-flow precedence, highest first: CLI, env, config, remote feature flag, then the loopback default.
/// Returns the deciding tier so the caller can log which one chose the transport.
fn resolve_device_flow(
    login_override: LoginTransportOverride,
    config: Option<bool>,
    remote: Option<bool>,
) -> distill_config_types::Resolved<bool> {
    distill_config_types::BoolFlag::env("GROK_LOGIN_DEVICE_FLOW")
        .cli(login_override.as_cli_bool())
        .config(config)
        .feature_flag(remote)
        .default(false)
        .resolve()
}
/// Whether `run_cli_login` should use the device flow for `config`: only the Grok OAuth2 provider supports it.
/// Enterprise OIDC (`oidc=Some`) always uses the loopback flow, mirroring `run_auth_flow_inner`'s precedence.
async fn cli_should_use_device(
    config: &GrokComConfig,
    config_device_flow: Option<bool>,
    login_override: LoginTransportOverride,
    proxy_base_url: &str,
) -> bool {
    !crate::oidc::is_configured(config)
        && should_use_device_flow(login_override, config_device_flow, proxy_base_url).await
}
/// Whether interactive OAuth2 login uses the RFC 8628 device flow (vs loopback).
/// Precedence: CLI flags, then the device-flow env, then config, then the remote flag; loopback is the default.
/// The caller supplies `proxy_base_url` so this stays off the effective-config load.
async fn should_use_device_flow(
    login_override: LoginTransportOverride,
    config_device_flow: Option<bool>,
    proxy_base_url: &str,
) -> bool {
    if let LoginTransportOverride::Preresolved(use_device) = login_override {
        return use_device;
    }
    let resolved = if login_override.as_cli_bool().is_some() {
        resolve_device_flow(login_override, None, None)
    } else {
        let env = distill_config::env_bool("GROK_LOGIN_DEVICE_FLOW");
        let remote = if env.is_none() && config_device_flow.is_none() {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                fetch_login_device_flow(proxy_base_url),
            )
            .await
            .ok()
            .flatten()
        } else {
            None
        };
        resolve_device_flow(login_override, config_device_flow, remote)
    };
    tracing::info!(
        transport = if resolved.value { "device" } else { "loopback" },
        source = %resolved.source,
        "login: resolved interactive transport",
    );
    resolved.value
}
/// How login presents itself; sent to the TUI via `x.ai/auth/get_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthUrlMode {
    /// Loopback-callback flow: the TUI shows a copyable URL and a paste box.
    Loopback,
    /// External auth provider opened its own browser: the TUI shows a waiting status.
    Command,
    /// RFC 8628 device flow: the TUI shows the device code and a copyable URL, no paste box.
    Device,
}
impl AuthUrlMode {
    /// Wire string for the `x.ai/auth/get_url` ACP response.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Command => "command",
            Self::Device => "device",
        }
    }
    /// Back-compat flag for older clients that only read `external_provider`.
    pub fn is_external_provider(self) -> bool {
        matches!(self, Self::Command)
    }
}
/// Auth URL pushed from the auth flow to the TUI.
pub struct AuthUrlInfo {
    pub url: String,
    pub mode: AuthUrlMode,
}
/// Channels for interactive login between the auth flow and the TUI/extension.
pub struct AuthChannels {
    pub url_tx: Option<oneshot::Sender<AuthUrlInfo>>,
    pub code_rx: mpsc::Receiver<String>,
}
/// Sets no `GROK_AUTH_EXPIRED`: operator binaries, which live outside this repo, read that variable as "headless, don't prompt" and decline the run.
pub async fn run_external_auth_provider(
    command: &str,
    auth_manager: &Arc<AuthManager>,
    over_stale_credential: bool,
    on_stderr: Option<StderrCallback>,
) -> anyhow::Result<(GrokAuth, bool)> {
    let inherit_stderr = on_stderr.is_none();
    tracing::info!(
        cmd = %command,
        over_stale_credential,
        inherit_stderr,
        "auth: running external auth provider (interactive login)"
    );
    let mut cmd = distill_shell_base::util::subprocess::shell_c(command);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    if inherit_stderr {
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        cmd.stderr(std::process::Stdio::piped());
    }
    distill_tools::util::detach_command(&mut cmd);
    cmd.envs(distill_tools::util::pager_env());
    #[allow(clippy::disallowed_methods)]
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to start auth provider `{command}`: {e}"))?;
    let stderr_task = if let Some(cb) = on_stderr {
        let stderr = child.stderr.take().expect("stderr was set to piped");
        Some(tokio::task::spawn_local(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        tracing::debug!(line = trimmed, "auth: provider stderr");
                        cb(trimmed);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "auth: error reading provider stderr");
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("external auth provider `{command}` timed out after 300s"))?
    .map_err(|e| anyhow::anyhow!("external auth provider `{command}` IO error: {e}"))?;
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    let mut auth = parse_output(&output)
        .map_err(|e| anyhow::anyhow!("external auth provider `{command}`: {e}"))?;
    let principal_policy = crate::oidc::login_principal_policy(auth_manager.grok_com_config());
    crate::oidc::enforce_login_principal(
        principal_policy.as_ref(),
        crate::oidc::peek_access_token_principal_id(&auth.key).as_deref(),
    )?;
    match (over_stale_credential, auth_manager.current_or_expired()) {
        (true, Some(prev)) => auth.carry_user_profile_from(&prev),
        _ => auth_manager.enrich_auth_inline(&mut auth).await,
    }
    let auth = auth_manager
        .update(auth)
        .await
        .map_err(|e| anyhow::anyhow!("failed to save external auth credentials: {e}"))?;
    tracing::info!(
        user_id = %auth.user_id,
        email = ?auth.email,
        "auth: external provider login complete"
    );
    Ok((auth, true))
}
/// GUI auth: bridges external provider stderr to `url_tx`, pipes code submission via `code_rx`.
pub async fn run_auth_flow_with_stderr_bridge(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    channels: AuthChannels,
    reauth: bool,
    force_interactive: bool,
    login_override: LoginTransportOverride,
) -> anyhow::Result<(GrokAuth, bool)> {
    let url_tx = Rc::new(RefCell::new(channels.url_tx));
    let stderr_lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let writer = stderr_lines.clone();
    let on_stderr: StderrCallback = Box::new(move |line: &str| {
        writer.borrow_mut().push(line.to_owned());
    });
    let reader = stderr_lines.clone();
    let url_tx_bridge = url_tx.clone();
    let bridge = async move {
        loop {
            tokio::task::yield_now().await;
            let content = {
                let lines = reader.borrow();
                if lines.is_empty() {
                    None
                } else {
                    Some(lines.join("\n"))
                }
            };
            if let Some(joined) = content
                && let Some(tx) = url_tx_bridge.borrow_mut().take()
            {
                let url = joined
                    .split_whitespace()
                    .find(|w| w.starts_with("https://"))
                    .map(|u| u.to_owned())
                    .unwrap_or(joined);
                let _ = tx.send(AuthUrlInfo {
                    url,
                    mode: AuthUrlMode::Command,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    if force_interactive {
        let auth = run_auth_flow_interactive(
            auth_manager,
            grok_com_config,
            config_device_flow,
            Some(on_stderr),
            Some(url_tx),
            Some(channels.code_rx),
            login_override,
        );
        tokio::select! {
            r = auth => r,
            _ = bridge => {
                tracing::error!("auth stderr bridge exited unexpectedly during interactive login");
                Err(anyhow::anyhow!("Login failed. Please try again."))
            },
        }
    } else {
        let auth = run_auth_flow(
            auth_manager,
            grok_com_config,
            config_device_flow,
            reauth,
            Some(on_stderr),
            Some(url_tx),
            Some(channels.code_rx),
            login_override,
        );
        tokio::select! {
            r = auth => r,
            _ = bridge => {
                tracing::error!("auth stderr bridge exited unexpectedly during login");
                Err(anyhow::anyhow!("Login failed. Please try again."))
            },
        }
    }
}
/// Full auth chain: cache, then refresh, then external provider, then interactive (OIDC/OAuth2/legacy).
/// When `url_tx` and `code_rx` are `None`, falls back to stderr/stdin (CLI mode).
pub async fn run_auth_flow(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    reauth: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
    login_override: LoginTransportOverride,
) -> anyhow::Result<(GrokAuth, bool)> {
    run_auth_flow_inner(
        auth_manager,
        grok_com_config,
        config_device_flow,
        reauth,
        false,
        on_stderr,
        url_tx,
        code_rx,
        login_override,
    )
    .await
}
/// Like [`run_auth_flow`] but with `force_interactive`: skip cached credentials without clearing them.
/// Used by `/login` for mid-session re-auth where abandoning the flow must not disrupt the session.
pub async fn run_auth_flow_interactive(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
    login_override: LoginTransportOverride,
) -> anyhow::Result<(GrokAuth, bool)> {
    run_auth_flow_inner(
        auth_manager,
        grok_com_config,
        config_device_flow,
        false,
        true,
        on_stderr,
        url_tx,
        code_rx,
        login_override,
    )
    .await
}
/// Every interactive login returns through here, so reporting the failure here costs one event per attempt.
/// A retried request, or the discovery cache that the background token refresh shares, can't inflate the count.
/// The reporting never changes the result.
async fn run_auth_flow_inner(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    reauth: bool,
    force_interactive: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
    login_override: LoginTransportOverride,
) -> anyhow::Result<(GrokAuth, bool)> {
    let result = ActiveAuthBackend::default()
        .login(LoginRequest {
            auth_manager,
            grok_com_config,
            config_device_flow,
            reauth,
            force_interactive,
            on_stderr,
            url_tx,
            code_rx,
            login_override,
        })
        .await;
    if let Err(err) = &result
        && let Some(event) = login_failure_event(err)
    {
        distill_telemetry::session_ctx::log_event(event);
    }
    result
}
/// `None` when nothing in the chain failed over HTTP (the user backed out, the loopback listener couldn't bind, the id_token didn't validate).
/// Returning `None` beats inventing a transport verdict for those.
fn login_failure_event(err: &anyhow::Error) -> Option<LoginFailed> {
    let source = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())?;
    Some(LoginFailed {
        error_kind: failure_kind(
            distill_http::TransportFailure::classify(source).kind,
            source.is_decode(),
        ),
        os_error: distill_http::find_os_error_code(source),
    })
}
/// A body that won't parse is a decode failure, not a transport one, even though `reqwest` also reports it as a body-phase error.
fn failure_kind(transport: TransportFailureKind, is_decode: bool) -> LoginFailureKind {
    if is_decode {
        return LoginFailureKind::Decode;
    }
    match transport {
        TransportFailureKind::Unreachable => LoginFailureKind::TransportConnect,
        TransportFailureKind::CertificateUntrusted => LoginFailureKind::CertificateUntrusted,
        TransportFailureKind::CertificateInvalid => LoginFailureKind::CertificateInvalid,
        TransportFailureKind::Interrupted => LoginFailureKind::TransportInterrupted,
        TransportFailureKind::Permanent => LoginFailureKind::TransportPermanent,
    }
}
pub(super) async fn run_auth_flow_steps(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    reauth: bool,
    force_interactive: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
    login_override: LoginTransportOverride,
) -> anyhow::Result<(GrokAuth, bool)> {
    tracing::info!(
        has_oidc = grok_com_config.oidc.is_some(),
        has_oauth2 = grok_com_config.oauth2.is_some(),
        has_external_auth = grok_com_config.auth_provider_command.is_some(),
        reauth,
        "auth: starting auth flow"
    );
    if reauth {
        auth_manager.clear()?;
        let _ = auth_manager.remove_scope(LEGACY_AUTH_SCOPE);
    }
    if !force_interactive && let Some(auth) = auth_manager.current() {
        if is_cached_credential_compatible(&auth, grok_com_config) {
            tracing::info!(auth_mode = ?auth.auth_mode, "auth: using cached credentials");
            distill_telemetry::unified_log::info(
                "auth: using cached credentials",
                None,
                Some(serde_json::json!({ "auth_mode": format!("{:?}", auth.auth_mode) })),
            );
            return Ok((auth, false));
        }
        tracing::info!(
            auth_mode = ?auth.auth_mode,
            "auth: cached credential incompatible with requested flow, proceeding to interactive login"
        );
        if auth.auth_mode == super::AuthMode::WebLogin
            && let Err(e) = auth_manager.remove_scope(LEGACY_AUTH_SCOPE)
        {
            tracing::warn!(error = ?e, "auth: failed to remove legacy scope entry (non-fatal)");
        }
    }
    if !force_interactive && !reauth && auth_manager.is_expired() {
        let file_lock = auth_manager
            .try_lock_auth_file_async(
                crate::manager::AUTH_LOCK_TIMEOUT,
                crate::manager::lock::Heartbeat::Skip,
            )
            .await
            .into_guard();
        let disk_auth = auth_manager.read_disk_auth();
        let disk_expired = disk_auth.as_ref().is_some_and(crate::is_expired);
        distill_telemetry::unified_log::info(
            "auth run_auth_flow expired path",
            None,
            Some(serde_json::json!({
                "got_lock": file_lock.is_some(),
                "disk_found": disk_auth.is_some(),
                "disk_expired": disk_expired,
            })),
        );
        if disk_auth.as_ref().is_some_and(|d| {
            !crate::is_expired(d) && is_cached_credential_compatible(d, grok_com_config)
        }) {
            distill_telemetry::unified_log::info(
                "auth run_auth_flow using valid disk token",
                None,
                None,
            );
            let d = disk_auth.unwrap();
            let ret = d.clone();
            auth_manager.hot_swap(d);
            return Ok((ret, false));
        }
        drop(file_lock);
        match auth_manager.auth().await {
            Ok(fresh) => return Ok((fresh, false)),
            Err(e) => {
                if let Some(d) = disk_auth.filter(|d| {
                    matches!(
                        &e,
                        crate::error::AuthError::Refresh(
                            crate::error::RefreshTokenError::Transient(_)
                        )
                    ) && d.refresh_token.is_some()
                }) {
                    distill_telemetry::unified_log::warn(
                        "auth run_auth_flow refresh failed, deferring to consumer refresh",
                        None,
                        Some(serde_json::json!({
                            "error": format!("{e}"),
                        })),
                    );
                    let ret = d.clone();
                    auth_manager.hot_swap(d);
                    return Ok((ret, false));
                }
                distill_telemetry::unified_log::warn(
                    "auth run_auth_flow refresh failed, falling through to interactive",
                    None,
                    Some(serde_json::json!({
                        "error": format!("{e}"),
                    })),
                );
            }
        }
    }
    if let Some(ref cmd) = grok_com_config.auth_provider_command {
        let over_stale_credential = reauth || auth_manager.is_expired();
        match run_external_auth_provider(cmd, auth_manager, over_stale_credential, on_stderr).await
        {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "auth: external auth provider failed, falling through to interactive login"
                );
                eprintln!("Signing in with browser instead...");
            }
        }
    }
    let url_tx = url_tx.and_then(|rc| rc.borrow_mut().take());
    let mut channels = code_rx.map(|code_rx| AuthChannels { url_tx, code_rx });
    if crate::oidc::is_configured(grok_com_config) {
        return crate::oidc::run_login_flow(grok_com_config, auth_manager, channels).await;
    }
    if let Some(ref oauth2_cfg) = grok_com_config.oauth2 {
        if should_use_device_flow(
            login_override,
            config_device_flow,
            auth_manager.proxy_base_url(),
        )
        .await
        {
            match crate::device_code::run_device_code_login_channels(
                &oauth2_cfg.issuer,
                &oauth2_cfg.client_id,
                &oauth2_cfg.scopes,
                auth_manager,
                &mut channels,
            )
            .await
            {
                Err(e)
                    if matches!(
                        e.downcast_ref::<crate::device_code::DeviceCodeError>(),
                        Some(crate::device_code::DeviceCodeError::NotEnabled)
                    ) =>
                {
                    tracing::warn!(
                        "auth: device flow unavailable (404), falling back to loopback login"
                    );
                }
                other => return other,
            }
        }
        return crate::oidc::run_login_flow_with_config(
            &oauth2_cfg.as_oidc(),
            auth_manager,
            channels,
        )
        .await;
    }
    tracing::error!(
        "auth: no OAuth2 configuration available (neither enterprise OIDC nor xAI OAuth2 configured)"
    );
    anyhow::bail!(
        "No OAuth2 configuration available. Run `grok login` to authenticate, or contact your administrator if you use enterprise SSO."
    )
}
/// Non-interactive auth refresh: returns valid credentials if available without ever triggering interactive login (browser, device code, etc.).
/// Tries cached non-expired credentials, then OIDC silent refresh (needs a refresh_token), then the external auth provider command (if configured).
/// Returns `None` when no valid credentials can be obtained non-interactively.
pub async fn try_ensure_fresh_auth(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Option<GrokAuth> {
    try_ensure_fresh_auth_with(&build_startup_auth_manager(grok_com_config, proxy_base_url)).await
}
/// Builds and configures the startup `AuthManager`; the policy helpers below take it injected so tests can substitute their own.
fn build_startup_auth_manager(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Arc<AuthManager> {
    let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &distill_home::distill_home(),
        grok_com_config.clone(),
        proxy_base_url,
    ));
    auth_manager.configure_refresher(grok_com_config.auth_provider_command.clone(), None);
    auth_manager
}
/// Uses cached valid credentials, else a silent refresh; never interactive login.
async fn try_ensure_fresh_auth_with(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    match auth_manager.auth().await {
        Ok(auth) => Some(auth),
        Err(e) => {
            tracing::debug!(error = %e, "try_ensure_fresh_auth: no valid credentials available");
            None
        }
    }
}
/// Readiness-path auth: a bounded refresh plus the expired-but-refreshable cached session, but no cold mint (which can run a provider command up to `STARTUP_AUTH_TIMEOUT`). Minting is deferred to the post-readiness background task, so readiness waits at most `STARTUP_AUTH_REFRESH_TIMEOUT`.
pub async fn try_noninteractive_auth_no_mint(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Option<GrokAuth> {
    try_noninteractive_auth_no_mint_with(&build_startup_auth_manager(
        grok_com_config,
        proxy_base_url,
    ))
    .await
}
/// Policy behind [`try_noninteractive_auth_no_mint`], with the `AuthManager`
/// injected for tests.
async fn try_noninteractive_auth_no_mint_with(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    match tokio::time::timeout(
        distill_http::STARTUP_AUTH_REFRESH_TIMEOUT,
        try_ensure_fresh_auth_with(auth_manager),
    )
    .await
    {
        Ok(Some(auth)) => return Some(auth),
        Ok(None) => {}
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = distill_http::STARTUP_AUTH_REFRESH_TIMEOUT.as_secs(),
                "boot auth refresh timed out; using cached/expired session (mint deferred to background)"
            );
        }
    }
    expired_refreshable_session(auth_manager)
}
/// A cached, refreshable session (not BYOK/ApiKey).
/// Reached only after fresh auth failed, so in practice the token is expired but recoverable on 401.
fn expired_refreshable_session(auth_manager: &AuthManager) -> Option<GrokAuth> {
    auth_manager
        .current_or_expired()
        .filter(|a| a.is_xai_auth() && a.refresh_token.is_some())
}
/// Cold-start mint via non-interactive providers (external command, devbox); `None` when none is available. Persists the result into `auth_manager` (disk and in-memory) so per-request `auth()` self-heals.
/// Carries no timeout of its own: the readiness-path caller imposes `STARTUP_AUTH_TIMEOUT`. The leader's background re-mint runs uncapped (only the provider's ~300s ceiling).
pub async fn mint_session_noninteractive(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    let grok_com_config = auth_manager.grok_com_config();
    if !ActiveAuthBackend::default().is_xai_authority() {
        return None;
    }
    if grok_com_config.blocks_automatic_oidc() {
        tracing::debug!(
            "mint_session_noninteractive: skipped (preferred_method=api_key blocks automatic OIDC)"
        );
        return None;
    }
    if let Some(cmd) = grok_com_config.auth_provider_command.as_deref() {
        match run_external_auth_provider(cmd, auth_manager, false, None).await {
            Ok((auth, _)) => return Some(auth),
            Err(e) => {
                tracing::debug!(error = %e, "mint_session_noninteractive: external provider failed");
            }
        }
    }
    None
}
/// Persist a minted token; on persist failure, return it unpersisted rather than dropping a valid credential.
#[cfg(test)]
async fn persist_or_use_minted(auth_manager: &AuthManager, new_auth: GrokAuth) -> GrokAuth {
    match auth_manager.save_without_enrichment(new_auth.clone()).await {
        Ok(auth) => {
            let _ = auth_manager.remove_scope(LEGACY_AUTH_SCOPE);
            auth
        }
        Err(e) => {
            tracing::warn!(error = %e, "mint persist failed; using unpersisted token");
            new_auth
        }
    }
}
/// Print the CLI "signed in" confirmation, clearing the spinner line first.
pub fn report_signed_in(auth: &GrokAuth) {
    eprint!("\r\x1b[K");
    match auth.email {
        Some(ref email) => eprintln!("✓ Signed in as {email}"),
        None => eprintln!("✓ Signed in"),
    }
}
/// CLI auth entrypoint. For GUI, use `run_auth_flow_with_stderr_bridge`.
pub async fn ensure_authenticated(
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    proxy_base_url: String,
    reauth: bool,
    message_prefix: Option<&str>,
) -> anyhow::Result<GrokAuth> {
    ensure_authenticated_with_override(
        grok_com_config,
        config_device_flow,
        proxy_base_url,
        reauth,
        message_prefix,
        LoginTransportOverride::None,
    )
    .await
}
/// Like [`ensure_authenticated`] but with an explicit login-transport override (from `--oauth` / `--device-auth`).
/// Used by `run_cli_login`.
pub async fn ensure_authenticated_with_override(
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    proxy_base_url: String,
    reauth: bool,
    message_prefix: Option<&str>,
    login_override: LoginTransportOverride,
) -> anyhow::Result<GrokAuth> {
    let distill_home = distill_home::distill_home();
    let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &distill_home,
        grok_com_config.clone(),
        proxy_base_url,
    ));
    if !reauth && let Some(auth) = auth_manager.current() {
        if auth.auth_mode != super::AuthMode::WebLogin {
            return Ok(auth);
        }
        tracing::info!("auth: skipping cached WebLogin credential, will migrate to OIDC");
        auth_manager.clear_in_memory();
        let _ = auth_manager.remove_scope(LEGACY_AUTH_SCOPE);
    }
    if let Some(msg) = message_prefix {
        eprintln!("{msg}");
    }
    let (auth, did_auth) = run_auth_flow(
        &auth_manager,
        grok_com_config,
        config_device_flow,
        reauth,
        None,
        None,
        None,
        login_override,
    )
    .await?;
    if did_auth {
        report_signed_in(&auth);
    }
    Ok(auth)
}
/// Decides *whether to prompt* for an interactive login (the wire credential is chosen separately by `ShellAuthCredentialProvider`).
/// With `has_noninteractive_auth`, only refresh a cached token best-effort (no browser, no cold mint); otherwise require an interactive login.
pub async fn ensure_authenticated_or_noninteractive(
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    proxy_base_url: String,
    has_noninteractive_auth: bool,
    message_prefix: Option<&str>,
) -> anyhow::Result<Option<GrokAuth>> {
    if has_noninteractive_auth {
        Ok(try_ensure_fresh_auth(grok_com_config, proxy_base_url).await)
    } else {
        ensure_authenticated(
            grok_com_config,
            config_device_flow,
            proxy_base_url,
            false,
            message_prefix,
        )
        .await
        .map(Some)
    }
}
/// Unified `grok login` handler for CLI entry points (tui, pager). Precedence: `--oauth` forces loopback, `--device-auth` forces device.
/// Otherwise `GROK_LOGIN_DEVICE_FLOW` env, then `[auth] login_device_flow` config, then the loopback default.
/// Both transports run through `run_auth_flow_inner` so the external auth provider and devbox auto-migration are tried first.
pub async fn run_cli_login(
    grok_com_config: GrokComConfig,
    config_device_flow: Option<bool>,
    proxy_base_url: String,
    oauth: bool,
    device_auth: bool,
    devbox: bool,
    configure_telemetry: impl FnOnce(&AuthManager),
) -> anyhow::Result<GrokAuth> {
    let _ = devbox;
    let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &distill_home::distill_home(),
        grok_com_config.clone(),
        proxy_base_url,
    ));
    configure_telemetry(&auth_manager);
    let result = run_cli_login_steps(
        &grok_com_config,
        config_device_flow,
        &auth_manager,
        oauth,
        device_auth,
    )
    .await;
    distill_telemetry::session_ctx::drain_pending(distill_telemetry::session_ctx::CLI_DRAIN)
        .await;
    result
}
async fn run_cli_login_steps(
    grok_com_config: &GrokComConfig,
    config_device_flow: Option<bool>,
    auth_manager: &Arc<AuthManager>,
    oauth: bool,
    device_auth: bool,
) -> anyhow::Result<GrokAuth> {
    let login_override = LoginTransportOverride::from_flags(oauth, device_auth);
    let authenticated = if cli_should_use_device(
        grok_com_config,
        config_device_flow,
        login_override,
        auth_manager.proxy_base_url(),
    )
    .await
    {
        if grok_com_config.oauth2.is_none() {
            anyhow::bail!("Sign-in is not available for this deployment. Set XAI_API_KEY instead.");
        }
        let (auth, did_auth) = run_auth_flow_interactive(
            auth_manager,
            grok_com_config,
            config_device_flow,
            None,
            None,
            None,
            LoginTransportOverride::Preresolved(true),
        )
        .await?;
        if did_auth {
            report_signed_in(&auth);
        }
        auth
    } else {
        if device_auth && crate::oidc::is_configured(grok_com_config) {
            eprintln!(
                "Device-code login isn't available for your SSO provider; using browser sign-in."
            );
        }
        let (auth, did_auth) = run_auth_flow(
            auth_manager,
            grok_com_config,
            config_device_flow,
            true,
            None,
            None,
            None,
            LoginTransportOverride::Preresolved(false),
        )
        .await?;
        if did_auth {
            report_signed_in(&auth);
        }
        auth
    };
    Ok(authenticated)
}
/// Result of a logout operation.
/// Both the CLI subcommand and the ACP `/logout` slash command use it, so the presentation layer formats the outcome without duplicating auth logic.
pub struct LogoutResult {
    /// `true` if a cached OAuth session was found and cleared.
    pub was_logged_in: bool,
    /// Email of the session that was cleared (if available).
    pub email: Option<String>,
    /// `true` if `XAI_API_KEY` / `GROK_CODE_XAI_API_KEY` env var is set.
    pub api_key_still_set: bool,
}
/// Core logout logic shared by the CLI subcommand and the ACP handler.
/// `None` scope clears the default (same as `/logout`); `Some` removes only that scope entry.
/// `clear_orphan_managed_config` is injected so this crate stays off the shell's managed config.
pub fn perform_logout(
    auth_manager: &AuthManager,
    scope: Option<&str>,
    clear_orphan_managed_config: impl FnOnce(),
) -> std::io::Result<LogoutResult> {
    let auth = auth_manager.current_or_expired();
    let email = auth.as_ref().and_then(|a| a.email.clone());
    let was_logged_in = auth.is_some();
    distill_telemetry::unified_log::info(
        "auth: logout",
        None,
        Some(serde_json::json!({
            "was_logged_in": was_logged_in,
            "scope": scope.unwrap_or("(current)"),
            "user_id": auth.as_ref().map(|a| a.user_id.clone()),
        })),
    );
    if was_logged_in {
        distill_telemetry::external::set_identity(
            distill_telemetry::external::IdentityAttrs::default(),
        );
        distill_telemetry::external::flush();
        if let Some(scope) = scope {
            auth_manager.remove_scope(scope)?;
        } else {
            auth_manager.clear()?;
        }
        clear_orphan_managed_config();
    }
    Ok(LogoutResult {
        was_logged_in,
        email,
        api_key_still_set: crate::auth_method::has_xai_api_key_env(),
    })
}
#[derive(serde::Deserialize)]
struct LoginConfigResponse {
    /// Tri-state: `Some` forces a transport; `None` or an absent flag keeps the client default.
    #[serde(default)]
    device_flow: Option<bool>,
}
/// Fetch the remote device-flow flag from unauthenticated `GET /v1/login-config`.
/// Best-effort: any error or unset flag returns `None` so the caller keeps the loopback default.
/// Caps at 1.5s with no retries since it's on the login path.
async fn fetch_login_device_flow(cli_chat_proxy_base_url: &str) -> Option<bool> {
    let agent_id = tokio::task::spawn_blocking(distill_telemetry::id::agent_id)
        .await
        .ok()?;
    let client = distill_http::shared_client();
    let url = format!("{}/login-config", cli_chat_proxy_base_url);
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_millis(1500))
        .header("x-grok-agent-id", agent_id)
        .header("x-grok-client-version", distill_version::VERSION)
        .header(
            "x-grok-client-identifier",
            distill_http::process_client_identifier(),
        )
        .header(
            distill_http::CLIENT_MODE_HEADER,
            distill_http::process_client_mode(),
        )
        .send()
        .await;
    let resp = match response {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            tracing::debug!(status = resp.status().as_u16(), "login-config fetch failed");
            return None;
        }
        Err(e) => {
            tracing::debug!("login-config fetch error: {e}");
            return None;
        }
    };
    match resp.json::<LoginConfigResponse>().await {
        Ok(cfg) => {
            tracing::debug!(device_flow = ?cfg.device_flow, "Fetched remote login-config");
            cfg.device_flow
        }
        Err(e) => {
            tracing::debug!("Failed to parse login-config response: {e}");
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthMode;
    use crate::config::XAI_OAUTH2_ISSUER;
    use chrono::Utc;
    use std::path::Path;
    use distill_shell_base::env::EnvVarGuard;
    /// `os_error` and the reqwest classification are covered in `distill-http`.
    /// What's local is which `LoginFailureKind` each maps to, and that a decode failure never reads as a transport one.
    #[test]
    fn failure_kinds_map_one_to_one() {
        assert_eq!(
            failure_kind(TransportFailureKind::Unreachable, false),
            LoginFailureKind::TransportConnect
        );
        assert_eq!(
            failure_kind(TransportFailureKind::CertificateUntrusted, false),
            LoginFailureKind::CertificateUntrusted
        );
        assert_eq!(
            failure_kind(TransportFailureKind::CertificateInvalid, false),
            LoginFailureKind::CertificateInvalid
        );
        assert_eq!(
            failure_kind(TransportFailureKind::Interrupted, false),
            LoginFailureKind::TransportInterrupted
        );
        assert_eq!(
            failure_kind(TransportFailureKind::Permanent, false),
            LoginFailureKind::TransportPermanent
        );
        assert_eq!(
            failure_kind(TransportFailureKind::Interrupted, true),
            LoginFailureKind::Decode
        );
    }
    /// A login that never reached the network is not a transport failure.
    /// The positive path needs a real `reqwest::Error`, but building a client here flips `jsonwebtoken` into its "no CryptoProvider" panic.
    /// That breaks unrelated auth tests, so classification is covered in `distill-http`.
    #[test]
    fn non_http_login_failures_are_not_reported() {
        let abandoned = anyhow::anyhow!("Login timed out after 10 minutes. Please try again.");
        assert!(login_failure_event(&abandoned).is_none());
        let nested = abandoned.context("Login failed. Please try again.");
        assert!(login_failure_event(&nested).is_none());
    }
    /// Run `f` with `GROK_LOGIN_DEVICE_FLOW` set to `value` (unset for `None`).
    /// `EnvVarGuard` serializes the process env and restores it on drop, so `resolve_device_flow` reads the env tier from a known state.
    fn with_device_flow_env<T>(value: Option<bool>, f: impl FnOnce() -> T) -> T {
        let _guard = match value {
            Some(true) => EnvVarGuard::set("GROK_LOGIN_DEVICE_FLOW", "true"),
            Some(false) => EnvVarGuard::set("GROK_LOGIN_DEVICE_FLOW", "false"),
            None => EnvVarGuard::remove("GROK_LOGIN_DEVICE_FLOW"),
        };
        f()
    }
    fn oidc_session(key: &str, refresh: Option<&str>) -> GrokAuth {
        GrokAuth {
            key: key.into(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
            refresh_token: refresh.map(str::to_string),
            ..GrokAuth::test_default()
        }
    }
    #[test]
    fn expired_refreshable_session_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = AuthManager::new(dir.path(), GrokComConfig::default());
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-but-refreshable", Some("rt"))
        });
        assert!(
            mgr.current().is_none(),
            "precondition: token must be expired"
        );
        assert_eq!(
            expired_refreshable_session(&mgr).map(|a| a.key),
            Some("expired-but-refreshable".to_string())
        );
        mgr.hot_swap(oidc_session("no-rt", None));
        assert!(expired_refreshable_session(&mgr).is_none());
        mgr.hot_swap(GrokAuth {
            auth_mode: AuthMode::External,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-external", Some("rt"))
        });
        assert_eq!(
            expired_refreshable_session(&mgr).map(|a| a.key),
            Some("expired-external".to_string())
        );
        mgr.hot_swap(GrokAuth {
            oidc_issuer: None,
            auth_mode: AuthMode::External,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-external-3p", Some("rt"))
        });
        assert!(expired_refreshable_session(&mgr).is_none());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn persist_or_use_minted_returns_token_when_save_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        let minted = oidc_session("minted-token", Some("rt"));
        let save = mgr.save_without_enrichment(minted.clone()).await;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(
            save.is_err(),
            "non-root: save into a read-only dir must fail"
        );
        let out = persist_or_use_minted(&mgr, minted).await;
        assert_eq!(
            out.key, "minted-token",
            "must return the unpersisted minted token"
        );
    }
    /// Proxy URL on a closed port: inline enrichment fails fast instead of reaching outside the test.
    fn dead_proxy_url() -> String {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        format!("http://127.0.0.1:{port}")
    }
    #[tokio::test]
    async fn mint_session_noninteractive_uses_external_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_provider_command: Some("printf '%s' xai-ext-token".to_string()),
            ..GrokComConfig::default()
        };
        let mgr = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url(&dead_proxy_url()),
        );
        let auth = mint_session_noninteractive(&mgr).await;
        assert_eq!(auth.map(|a| a.key), Some("xai-ext-token".to_string()));
    }
    #[tokio::test]
    async fn interactive_login_carries_no_expired_flag_even_over_a_stale_credential() {
        let echo_env = "printf '%s' \"e=${GROK_AUTH_EXPIRED:-unset}\"";
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("stale-token", None)
        });
        let (auth, _) = run_external_auth_provider(echo_env, &mgr, true, None)
            .await
            .expect("provider output must parse");
        assert_eq!(
            auth.key, "e=unset",
            "a login over an expired credential must not tell the binary to take its silent path"
        );
    }
    /// The script is the one published in `README.md`, which operators copy.
    #[tokio::test]
    async fn a_provider_written_to_the_published_contract_can_sign_in_after_an_expiry() {
        let conforming =
            r#"if [ "$GROK_AUTH_EXPIRED" = "1" ]; then exit 1; else printf '%s' sso-token; fi"#;
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("stale-token", None)
        });
        let (auth, _) = run_external_auth_provider(conforming, &mgr, true, None)
            .await
            .expect("the sign-in run must reach the binary's interactive branch");
        assert_eq!(auth.key, "sso-token");
    }
    /// External-provider output is team-pinned before persist (parity with OIDC / device-code).
    /// A wrong-team token is rejected and nothing is written.
    #[tokio::test]
    async fn external_provider_rejects_wrong_team_and_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), pinned_cfg("team-good"))
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = format!("printf '%s' {}", team_jwt("team-wrong"));
        assert!(
            run_external_auth_provider(&cmd, &mgr, false, None)
                .await
                .is_err(),
            "wrong-team external token must be rejected"
        );
        assert!(
            mgr.current_or_expired().is_none(),
            "rejected external login must persist nothing"
        );
        assert!(
            !dir.path().join("auth.json").exists(),
            "rejected external login must not write auth.json"
        );
    }
    /// A matching-team external token is accepted and persisted.
    #[tokio::test]
    async fn external_provider_accepts_matching_team() {
        let dir = tempfile::tempdir().unwrap();
        let jwt = team_jwt("team-good");
        let mgr = Arc::new(
            AuthManager::new(dir.path(), pinned_cfg("team-good"))
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = format!("printf '%s' {jwt}");
        let (auth, _) = run_external_auth_provider(&cmd, &mgr, false, None)
            .await
            .expect("matching-team external token must be accepted");
        assert_eq!(auth.key, jwt);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_reauth_without_prev_auth_enriches_inline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().route(
            "/user",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "userId": "u-1",
                    "teamBlockedReasons": ["BLOCKED_REASON_NO_LOGS"],
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&format!("http://127.0.0.1:{port}")),
        );
        assert!(mgr.current_or_expired().is_none(), "precondition: no auth");
        let (auth, _) = run_external_auth_provider("printf '%s' fresh-token", &mgr, true, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "flags must come from /user fetch");
        assert_eq!(auth.user_id, "u-1");
    }
    #[tokio::test]
    async fn external_refresh_carries_profile_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
            organization_id: Some("org-1".into()),
            ..oidc_session("old-token", None)
        });
        let (auth, _) = run_external_auth_provider("printf '%s' fresh-token", &mgr, true, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "flags must carry from previous auth");
        assert_eq!(auth.user_id, "test-user");
        assert_eq!(auth.organization_id.as_deref(), Some("org-1"));
    }
    #[tokio::test]
    async fn device_flow_still_runs_external_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_provider_command: Some("printf '%s' xai-ext-token".to_string()),
            ..GrokComConfig::default()
        };
        assert!(
            cli_should_use_device(
                &cfg,
                None,
                LoginTransportOverride::ForceDevice,
                &dead_proxy_url()
            )
            .await,
            "precondition: --device-auth resolves to the device flow"
        );
        let mgr = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url(&dead_proxy_url()),
        );
        let (auth, did_auth) = run_auth_flow_interactive(
            &mgr,
            &cfg,
            None,
            None,
            None,
            None,
            LoginTransportOverride::ForceDevice,
        )
        .await
        .expect("external provider should satisfy login without device flow");
        assert_eq!(
            auth.key, "xai-ext-token",
            "external provider token must win"
        );
        assert!(did_auth);
    }
    #[test]
    fn login_transport_override_maps_to_cli_bool() {
        assert_eq!(LoginTransportOverride::None.as_cli_bool(), None);
        assert_eq!(
            LoginTransportOverride::ForceLoopback.as_cli_bool(),
            Some(false)
        );
        assert_eq!(
            LoginTransportOverride::ForceDevice.as_cli_bool(),
            Some(true)
        );
        assert_eq!(
            LoginTransportOverride::Preresolved(true).as_cli_bool(),
            None
        );
        assert_eq!(
            LoginTransportOverride::Preresolved(false).as_cli_bool(),
            None
        );
    }
    #[tokio::test]
    async fn preresolved_bypasses_resolver_and_is_never_cli() {
        {
            let _guard = EnvVarGuard::set("GROK_LOGIN_DEVICE_FLOW", "false");
            assert!(
                should_use_device_flow(LoginTransportOverride::Preresolved(true), None, "").await,
                "Preresolved(true) honors device without re-resolving"
            );
        }
        {
            let _guard = EnvVarGuard::set("GROK_LOGIN_DEVICE_FLOW", "true");
            assert!(
                !should_use_device_flow(LoginTransportOverride::Preresolved(false), None, "").await,
                "Preresolved(false) honors loopback without re-resolving"
            );
            assert!(
                should_use_device_flow(LoginTransportOverride::None, None, "").await,
                "the resolver path still honors env (sole resolution)"
            );
        }
        with_device_flow_env(None, || {
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::Preresolved(true), None, Some(true))
                    .source,
                distill_config_types::ConfigSource::Remote,
                "Preresolved must never resolve as the cli tier"
            );
        });
    }
    #[test]
    fn from_flags_prefers_oauth_over_device() {
        assert_eq!(
            LoginTransportOverride::from_flags(true, true),
            LoginTransportOverride::ForceLoopback
        );
        assert_eq!(
            LoginTransportOverride::from_flags(true, false),
            LoginTransportOverride::ForceLoopback
        );
        assert_eq!(
            LoginTransportOverride::from_flags(false, true),
            LoginTransportOverride::ForceDevice
        );
        assert_eq!(
            LoginTransportOverride::from_flags(false, false),
            LoginTransportOverride::None
        );
    }
    #[tokio::test]
    async fn enterprise_oidc_never_uses_device_flow() {
        let cfg = GrokComConfig {
            oidc: Some(crate::OidcAuthConfig {
                issuer: "https://idp.example".into(),
                client_id: "client".into(),
                scopes: vec!["openid".into()],
                audience: None,
            }),
            oauth2: None,
            ..GrokComConfig::default()
        };
        assert!(
            !cli_should_use_device(&cfg, None, LoginTransportOverride::ForceDevice, "").await,
            "enterprise OIDC must stay on loopback"
        );
        let xai = GrokComConfig::default();
        assert!(xai.oauth2.is_some() && xai.oidc.is_none());
        assert!(cli_should_use_device(&xai, None, LoginTransportOverride::ForceDevice, "").await);
    }
    #[test]
    fn device_flow_precedence_cli_beats_env_config_remote() {
        with_device_flow_env(Some(true), || {
            assert!(
                !resolve_device_flow(
                    LoginTransportOverride::ForceLoopback,
                    Some(true),
                    Some(true)
                )
                .value,
                "--oauth must force loopback even when env+config+remote say device"
            );
        });
        with_device_flow_env(Some(false), || {
            assert!(
                resolve_device_flow(
                    LoginTransportOverride::ForceDevice,
                    Some(false),
                    Some(false)
                )
                .value,
                "--device-auth must force device even when env+config+remote say loopback"
            );
        });
    }
    #[test]
    fn device_flow_precedence_env_beats_config() {
        with_device_flow_env(Some(false), || {
            assert!(!resolve_device_flow(LoginTransportOverride::None, Some(true), None).value);
        });
        with_device_flow_env(Some(true), || {
            assert!(resolve_device_flow(LoginTransportOverride::None, Some(false), None).value);
        });
    }
    #[test]
    fn device_flow_env_beats_remote() {
        with_device_flow_env(Some(false), || {
            assert!(
                !resolve_device_flow(LoginTransportOverride::None, None, Some(true)).value,
                "env=loopback must win over remote=device"
            );
        });
        with_device_flow_env(Some(true), || {
            assert!(
                resolve_device_flow(LoginTransportOverride::None, None, Some(false)).value,
                "env=device must win over remote=loopback"
            );
        });
    }
    #[test]
    fn device_flow_config_beats_remote() {
        with_device_flow_env(None, || {
            assert!(
                !resolve_device_flow(LoginTransportOverride::None, Some(false), Some(true)).value,
                "config=loopback must win over remote=device"
            );
            assert!(
                resolve_device_flow(LoginTransportOverride::None, Some(true), Some(false)).value,
                "config=device must win over remote=loopback"
            );
        });
    }
    #[test]
    fn device_flow_precedence_config_then_default() {
        with_device_flow_env(None, || {
            assert!(!resolve_device_flow(LoginTransportOverride::None, Some(false), None).value);
            assert!(resolve_device_flow(LoginTransportOverride::None, Some(true), None).value);
            assert!(
                !resolve_device_flow(LoginTransportOverride::None, None, None).value,
                "default is loopback"
            );
        });
    }
    #[test]
    fn device_flow_remote_then_default() {
        with_device_flow_env(None, || {
            assert!(
                resolve_device_flow(LoginTransportOverride::None, None, Some(true)).value,
                "remote=device rolls device-auth in when nothing local is set"
            );
            assert!(
                !resolve_device_flow(LoginTransportOverride::None, None, Some(false)).value,
                "remote=loopback keeps loopback when nothing local is set"
            );
            assert!(
                !resolve_device_flow(LoginTransportOverride::None, None, None).value,
                "remote settings unavailable falls back to the loopback default"
            );
        });
    }
    #[test]
    fn device_flow_records_deciding_tier() {
        use distill_config_types::ConfigSource;
        with_device_flow_env(Some(false), || {
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::ForceDevice, Some(false), None).source,
                ConfigSource::Cli,
                "an explicit CLI flag is reported as the cli tier"
            );
        });
        with_device_flow_env(Some(true), || {
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::None, None, Some(false)).source,
                ConfigSource::Env
            );
        });
        with_device_flow_env(None, || {
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::None, Some(true), Some(false)).source,
                ConfigSource::Config
            );
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::None, None, Some(true)).source,
                ConfigSource::Remote,
                "the remote feature flag is reported as the remote tier"
            );
            assert_eq!(
                resolve_device_flow(LoginTransportOverride::None, None, None).source,
                ConfigSource::Default
            );
        });
    }
    fn legacy_auth() -> GrokAuth {
        GrokAuth {
            key: "k".into(),
            auth_mode: AuthMode::WebLogin,
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
    fn oidc_auth(issuer: &str) -> GrokAuth {
        GrokAuth {
            oidc_issuer: Some(issuer.into()),
            auth_mode: AuthMode::Oidc,
            ..legacy_auth()
        }
    }
    #[test]
    fn weblogin_cred_is_never_compatible() {
        let cfg = GrokComConfig::default();
        assert!(!is_cached_credential_compatible(&legacy_auth(), &cfg));
    }
    #[test]
    fn oidc_cred_with_matching_issuer_is_compatible() {
        let cfg = GrokComConfig::default();
        assert!(is_cached_credential_compatible(
            &oidc_auth(XAI_OAUTH2_ISSUER),
            &cfg,
        ));
    }
    #[test]
    fn external_cred_compatibility_follows_issuer() {
        let cfg = GrokComConfig::default();
        assert!(is_cached_credential_compatible(
            &GrokAuth {
                auth_mode: AuthMode::External,
                ..oidc_auth(XAI_OAUTH2_ISSUER)
            },
            &cfg,
        ));
        assert!(!is_cached_credential_compatible(
            &GrokAuth {
                auth_mode: AuthMode::External,
                oidc_issuer: None,
                ..legacy_auth()
            },
            &cfg,
        ));
    }
    fn ensure_crypto_provider() {
        let _ = jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.install_default();
    }
    fn team_jwt(principal_id: &str) -> String {
        ensure_crypto_provider();
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({
                "sub": "user-1",
                "principal_type": "Team",
                "principal_id": principal_id,
                "exp": 9999999999u64,
            }),
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }
    fn pinned_cfg(team: &str) -> GrokComConfig {
        GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::Single(team.into())),
            ..GrokComConfig::default()
        }
    }
    /// Under a team pin, a cached session for a different team is not reused by interactive login; it falls through to a fresh, compliant login.
    #[test]
    fn cached_cred_with_wrong_team_is_incompatible() {
        let auth = GrokAuth {
            key: team_jwt("team-wrong"),
            ..oidc_auth(XAI_OAUTH2_ISSUER)
        };
        assert!(!is_cached_credential_compatible(
            &auth,
            &pinned_cfg("team-good")
        ));
    }
    /// A cached session for the pinned team is reused normally.
    #[test]
    fn cached_cred_with_matching_team_is_compatible() {
        let auth = GrokAuth {
            key: team_jwt("team-good"),
            ..oidc_auth(XAI_OAUTH2_ISSUER)
        };
        assert!(is_cached_credential_compatible(
            &auth,
            &pinned_cfg("team-good")
        ));
    }
    /// When the in-memory token is expired but disk has a valid token, run_auth_flow should return the disk token without interactive login.
    #[tokio::test]
    async fn run_auth_flow_uses_valid_disk_token_when_expired() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let valid_disk = GrokAuth {
            key: "fresh-token-from-disk".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            refresh_token: Some("new-rt".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        writer.update(valid_disk).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let expired = GrokAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("old-rt".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(expired);
        assert!(mgr.is_expired());
        let (auth, is_new_login) = run_auth_flow(
            &mgr,
            &cfg,
            None,
            false,
            None,
            None,
            None,
            LoginTransportOverride::None,
        )
        .await
        .unwrap();
        assert_eq!(auth.key, "fresh-token-from-disk");
        assert!(!is_new_login, "should not be a new login");
        assert_eq!(mgr.current().unwrap().key, "fresh-token-from-disk");
    }
    /// When the in-memory token is valid (not expired), run_auth_flow should return it directly without checking disk.
    #[tokio::test]
    async fn run_auth_flow_returns_cached_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let valid = GrokAuth {
            key: "still-valid".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(valid);
        let (auth, is_new_login) = run_auth_flow(
            &mgr,
            &cfg,
            None,
            false,
            None,
            None,
            None,
            LoginTransportOverride::None,
        )
        .await
        .unwrap();
        assert_eq!(auth.key, "still-valid");
        assert!(!is_new_login);
    }
    #[tokio::test]
    async fn run_auth_flow_defers_to_consumer_refresh_on_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let expired_with_rt = GrokAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("valid-refresh-token".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        writer.update(expired_with_rt.clone()).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        mgr.hot_swap(expired_with_rt);
        assert!(mgr.is_expired());
        mgr.set_refresher(std::sync::Arc::new(AlwaysTransientRefresher));
        let (auth, is_new_login) = run_auth_flow(
            &mgr,
            &cfg,
            None,
            false,
            None,
            None,
            None,
            LoginTransportOverride::None,
        )
        .await
        .unwrap();
        assert_eq!(auth.key, "expired-access-token");
        assert!(auth.refresh_token.is_some());
        assert!(!is_new_login);
    }
    #[tokio::test]
    async fn run_auth_flow_falls_through_when_no_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = GrokComConfig::default();
        cfg.oauth2.as_mut().unwrap().issuer = "http://127.0.0.1:1".into();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let expired_no_rt = GrokAuth {
            key: "expired-legacy".into(),
            auth_mode: AuthMode::WebLogin,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: None,
            ..GrokAuth::test_default()
        };
        writer.update(expired_no_rt.clone()).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        mgr.hot_swap(expired_no_rt);
        assert!(mgr.is_expired());
        mgr.set_refresher(std::sync::Arc::new(AlwaysTransientRefresher));
        let result = run_auth_flow(
            &mgr,
            &cfg,
            None,
            false,
            None,
            None,
            None,
            LoginTransportOverride::ForceDevice,
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("/oauth2/device/code"),
            "expected device-code request error (proves flow fell through to interactive login), got: {err}"
        );
    }
    #[test]
    fn extract_url_from_external_provider_stderr() {
        let extract = |input: &str| -> String {
            input
                .split_whitespace()
                .find(|w| w.starts_with("https://"))
                .map(|u| u.to_owned())
                .unwrap_or_else(|| input.to_owned())
        };
        assert_eq!(
            extract(
                "Visit the following link to sign into Grok: https://auth.example.com/login?code=abc"
            ),
            "https://auth.example.com/login?code=abc"
        );
        assert_eq!(
            extract("Please sign in below\nhttps://auth.example.com/sso"),
            "https://auth.example.com/sso"
        );
        assert_eq!(
            extract("https://auth.example.com/login"),
            "https://auth.example.com/login"
        );
        assert_eq!(extract("some opaque output"), "some opaque output");
    }
    /// CLI `grok login` passes `on_stderr=None`; stderr must be inherited so sign-in URLs appear in real time.
    /// Piped stderr with no reader deadlocks once the child writes past the pipe buffer (~64 KiB).
    #[tokio::test]
    async fn external_provider_cli_path_does_not_deadlock_on_large_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = r#"sh -c 'i=0; while [ $i -lt 2000 ]; do printf "%s" "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" >&2; i=$((i+1)); done; printf token'"#;
        let (auth, _) = run_external_auth_provider(cmd, &mgr, false, None)
            .await
            .expect("CLI path must inherit stderr so large stderr does not deadlock");
        assert_eq!(auth.key, "token");
    }
    struct AlwaysTransientRefresher;
    #[async_trait::async_trait]
    impl crate::refresh::TokenRefresher for AlwaysTransientRefresher {
        async fn refresh(
            &self,
            _reason: crate::manager::RefreshReason,
        ) -> crate::refresh::RefreshOutcome {
            crate::refresh::RefreshOutcome::TransientFailure {
                message: "simulated network failure".into(),
            }
        }
    }
    /// Faithful reproduction of the cached-token bypass: the exact repro JWT (wrong team) cached in `auth.json` under a pin.
    /// The token runs through the same `AuthManager::new` and `auth()` engine that `try_ensure_fresh_auth` uses; it must be rejected and cleared.
    #[tokio::test]
    async fn noninteractive_auth_rejects_wrong_team_cached_token() {
        const REPRO_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJwcmluY2lwYWxfaWQiOiJ0ZWFtLXdyb25nIiwic3ViIjoidXNlci0xIn0.Signature";
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::AnyOf(vec![
                "team-good".into(),
            ])),
            ..GrokComConfig::default()
        };
        let mut store = crate::model::AuthStore::new();
        store.insert(
            cfg.auth_scope(),
            GrokAuth {
                key: REPRO_JWT.into(),
                auth_mode: AuthMode::Oidc,
                team_id: Some("team-wrong".into()),
                expires_at: chrono::DateTime::from_timestamp(9_999_999_999, 0),
                ..GrokAuth::test_default()
            },
        );
        let auth_path = dir.path().join("auth.json");
        crate::storage::write_auth_json(&auth_path, &store).unwrap();
        let auth_manager = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        auth_manager.configure_refresher(cfg.auth_provider_command.clone(), None);
        assert!(
            auth_manager.auth().await.is_err(),
            "non-interactive auth must reject the wrong-team cached token"
        );
        assert!(
            auth_manager.current().is_none(),
            "wrong-team token must not be usable via current()"
        );
        assert!(
            !auth_path.exists(),
            "wrong-team auth.json must be cleared, forcing a compliant re-login"
        );
    }
    /// Mock OIDC IdP whose `/token` endpoint never responds, so a refresh attempt hangs until the caller bounds it.
    async fn start_hanging_oidc_idp() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let b = base.clone();
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || {
                    let b = b.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "authorization_endpoint": format!("{b}/authorize"),
                            "token_endpoint": format!("{b}/token"),
                        }))
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    axum::Json(serde_json::json!({}))
                }),
            );
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, handle)
    }
    fn expired_oidc_manager(dir: &Path, issuer: &str) -> Arc<AuthManager> {
        let cfg = GrokComConfig::default();
        let am = Arc::new(AuthManager::new(dir, cfg.clone()));
        am.configure_refresher(cfg.auth_provider_command.clone(), None);
        am.hot_swap(GrokAuth {
            key: "expired".into(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(issuer.into()),
            oidc_client_id: Some("test-client".into()),
            refresh_token: Some("rt".into()),
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        am
    }
    /// The readiness-path `_no_mint` variant bounds the refresh (~5s) and never engages the cold-mint fallback.
    /// Leader readiness therefore can't block on a provider command up to the 60s `STARTUP_AUTH_TIMEOUT` cap.
    #[tokio::test]
    async fn no_mint_readiness_auth_is_bounded() {
        let (idp_base, server) = start_hanging_oidc_idp().await;
        let dir = tempfile::tempdir().unwrap();
        let am = expired_oidc_manager(dir.path(), &idp_base);
        let started = std::time::Instant::now();
        let result = try_noninteractive_auth_no_mint_with(&am).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= distill_http::STARTUP_AUTH_REFRESH_TIMEOUT,
            "expected a bounded refresh attempt (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < distill_http::STARTUP_AUTH_TIMEOUT,
            "no-mint readiness auth must not engage the 60s cold-mint cap (elapsed {elapsed:?}); readiness would block on a provider command"
        );
        assert!(
            result.is_none(),
            "a non-xAI expired session is no first-party fallback and no mint runs on this path, so no auth is produced"
        );
        server.abort();
    }
    const _: () = assert!(
        distill_http::STARTUP_AUTH_REFRESH_TIMEOUT.as_millis()
            < crate::manager::REFRESH_LOCK_TIMEOUT.as_millis(),
        "the startup refresh bound must fire before the lock convoy budget"
    );
    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_auth_stays_bounded_when_auth_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let am = expired_oidc_manager(dir.path(), "http://127.0.0.1:1/");
        let auth_path = dir.path().join("auth.json");
        let lock_path = auth_path.with_file_name(crate::manager::lock::LOCK_FILE_NAME);
        let _held_lock = crate::manager::lock::test_support::hold_backdated_stale_lock(&lock_path);
        let holder_info = std::fs::read_to_string(&lock_path).unwrap();
        let started = std::time::Instant::now();
        let _ = try_noninteractive_auth_no_mint_with(&am).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= distill_http::STARTUP_AUTH_REFRESH_TIMEOUT,
            "refresh must block on the held lock, not fast-return (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < crate::manager::REFRESH_LOCK_TIMEOUT,
            "refresh must not fall through to the lock convoy (elapsed {elapsed:?})"
        );
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            holder_info,
            "the live stale lock must be left untouched, never broken"
        );
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&probe).is_err(),
            "the flock must still be held exclusively after the bounded refresh"
        );
    }
    mod login_config {
        use super::super::{LoginConfigResponse, fetch_login_device_flow};
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use std::sync::{Arc, Mutex};
        #[test]
        fn login_config_response_parses_tristate() {
            let parse = |s: &str| {
                serde_json::from_str::<LoginConfigResponse>(s)
                    .unwrap()
                    .device_flow
            };
            assert_eq!(parse(r#"{"device_flow": true}"#), Some(true));
            assert_eq!(parse(r#"{"device_flow": false}"#), Some(false));
            assert_eq!(parse(r#"{"device_flow": null}"#), None);
            assert_eq!(parse("{}"), None, "absent flag must parse as unset");
        }
        fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        }
        #[derive(Debug, Default, Clone)]
        struct LoginConfigHeaders {
            authorization: Option<String>,
            user_id: Option<String>,
            email: Option<String>,
            agent_id: Option<String>,
            client_identifier: Option<String>,
            client_version: Option<String>,
        }
        #[derive(Clone)]
        struct LoginConfigServerState {
            status_code: StatusCode,
            body: String,
            seen: Arc<Mutex<Vec<LoginConfigHeaders>>>,
        }
        /// Mock cli-chat-proxy serving `GET /v1/login-config` with a fixed status and raw body, recording the request headers it saw.
        async fn start_login_config_server(
            status_code: StatusCode,
            body: String,
        ) -> (
            String,
            Arc<Mutex<Vec<LoginConfigHeaders>>>,
            tokio::task::JoinHandle<()>,
        ) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let state = LoginConfigServerState {
                status_code,
                body,
                seen: seen.clone(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let app = Router::new()
                .route(
                    "/v1/login-config",
                    get(|
                        State(state): State<LoginConfigServerState>,
                        headers: HeaderMap|
                    async move {
                        state
                            .seen
                            .lock()
                            .unwrap()
                            .push(LoginConfigHeaders {
                                authorization: header_str(&headers, "authorization"),
                                user_id: header_str(&headers, "x-userid"),
                                email: header_str(&headers, "x-email"),
                                agent_id: header_str(&headers, "x-grok-agent-id"),
                                client_identifier: header_str(
                                    &headers,
                                    "x-grok-client-identifier",
                                ),
                                client_version: header_str(
                                    &headers,
                                    "x-grok-client-version",
                                ),
                            });
                        (state.status_code, state.body)
                    }),
                )
                .with_state(state);
            let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (format!("{base}/v1"), seen, handle)
        }
        #[tokio::test]
        async fn fetch_login_device_flow_parses_2xx_bodies() {
            for (body, expected) in [
                (r#"{"device_flow": true}"#, Some(true)),
                (r#"{"device_flow": false}"#, Some(false)),
                (r#"{"device_flow": null}"#, None),
                (r#"{}"#, None),
                (r#"{"other": 1}"#, None),
            ] {
                let (base, _seen, server) =
                    start_login_config_server(StatusCode::OK, body.to_string()).await;
                let got = fetch_login_device_flow(&base).await;
                server.abort();
                assert_eq!(got, expected, "body {body:?}");
            }
        }
        #[tokio::test]
        async fn fetch_login_device_flow_errors_return_none() {
            for (status, body) in [
                (StatusCode::NOT_FOUND, r#"{"device_flow": true}"#),
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"device_flow": true}"#,
                ),
                (StatusCode::OK, "not json"),
            ] {
                let (base, _seen, server) =
                    start_login_config_server(status, body.to_string()).await;
                let got = fetch_login_device_flow(&base).await;
                server.abort();
                assert_eq!(got, None, "status {status}, body {body:?}");
            }
        }
        #[tokio::test]
        async fn fetch_login_device_flow_sends_only_unauthenticated_headers() {
            let (base, seen, server) =
                start_login_config_server(StatusCode::OK, r#"{"device_flow": true}"#.to_string())
                    .await;
            let got = fetch_login_device_flow(&base).await;
            server.abort();
            assert_eq!(got, Some(true));
            let seen = seen.lock().unwrap();
            let h = seen
                .last()
                .expect("server should have received one request");
            assert!(
                h.agent_id.as_deref().is_some_and(|v| !v.is_empty()),
                "must send x-grok-agent-id (the bucketing key)"
            );
            assert!(
                h.client_identifier.is_some(),
                "must send x-grok-client-identifier"
            );
            assert!(
                h.client_version.is_some(),
                "must send x-grok-client-version"
            );
            assert_eq!(h.authorization, None, "must not send Authorization");
            assert_eq!(h.user_id, None, "must not send x-userid");
            assert_eq!(h.email, None, "must not send x-email");
        }
    }
}
