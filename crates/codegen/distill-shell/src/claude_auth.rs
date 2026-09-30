// Modified for Distill by Samuel Fajreldines, 2026.
//! Isolated Claude (Pro/Max subscription) OAuth account support.
//!
//! Same shape as [`crate::codex_auth`]: its own token store, its own refresh,
//! and a bearer resolver the sampler reads on every request. It never touches
//! Grok's `auth.json`, the Codex store, or Claude Code's own credentials.
//!
//! The OAuth contract (client id, endpoints, scopes, the Claude Code identity
//! the API expects) is Claude Code's, reproduced from memory of its public
//! behavior. None of it is documented by Anthropic for third-party clients, so
//! every value can be overridden through the `DISTILL_CLAUDE_*` variables below.
//! Anthropic restricts subscription tokens to Claude Code; using them from
//! another client may be rejected server-side or breach the terms of service.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;

use crate::codex_auth::{
    Pkce, generate_pkce, generate_state, safe_error_excerpt, serve_callback_until_result,
};

pub const CLAUDE_AUTH_FILE_NAME: &str = "claude-auth.json";
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_INFERENCE_HOST: &str = "api.anthropic.com";
pub const CLAUDE_CLIENT_ID_ENV: &str = "DISTILL_CLAUDE_CLIENT_ID";
pub const CLAUDE_AUTHORIZE_URL_ENV: &str = "DISTILL_CLAUDE_AUTHORIZE_URL";
pub const CLAUDE_TOKEN_URL_ENV: &str = "DISTILL_CLAUDE_TOKEN_URL";

/// Beta flag that makes the Messages API accept a subscription OAuth bearer.
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
const CLAUDE_SCOPE: &str = "org:create_api_key user:profile user:inference";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_WINDOW_SECS: i64 = 5 * 60;
const CLAUDE_AUTH_ANCHOR_HEADER: &str = "x-distill-claude-auth-anchor";
const CLAUDE_ACCOUNT_ANCHOR_HEADER: &str = "x-distill-claude-account-anchor";
const CLAUDE_RESERVED_AUTH_HEADERS: &[&str] =
    &[CLAUDE_AUTH_ANCHOR_HEADER, CLAUDE_ACCOUNT_ANCHOR_HEADER];

pub const CLAUDE_AUTH_REQUIRED_MESSAGE: &str =
    "Claude authentication required; run `distill login --claude` or configure an API key for this model";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

#[derive(Clone, Debug)]
struct ClaudeEndpoints {
    authorize_url: String,
    token_url: String,
    client_id: String,
}

impl Default for ClaudeEndpoints {
    fn default() -> Self {
        Self {
            authorize_url: env_or(CLAUDE_AUTHORIZE_URL_ENV, CLAUDE_AUTHORIZE_URL),
            token_url: env_or(CLAUDE_TOKEN_URL_ENV, CLAUDE_TOKEN_URL),
            client_id: env_or(CLAUDE_CLIENT_ID_ENV, CLAUDE_CLIENT_ID),
        }
    }
}

/// On-disk credential, stored at `~/.opengrok/claude-auth.json` with owner-only
/// permissions. Anthropic's access token is opaque, so the expiry the token
/// endpoint reported is kept next to it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClaudeAuthStore {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCredentials {
    pub access_token: String,
    pub account_uuid: Option<String>,
    pub email: Option<String>,
}

/// The account a session started with. Refresh rotates the bearer, but requests
/// fail closed if a re-login swapped the account underneath a running session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeAuthIdentity {
    account_uuid: Option<String>,
}

impl ClaudeCredentials {
    pub(crate) fn identity(&self) -> ClaudeAuthIdentity {
        ClaudeAuthIdentity {
            account_uuid: self.account_uuid.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeAccountSummary {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
}

impl From<&ClaudeCredentials> for ClaudeAccountSummary {
    fn from(value: &ClaudeCredentials) -> Self {
        Self {
            email: value.email.clone(),
            account_uuid: value.account_uuid.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    account: Option<TokenAccount>,
    #[serde(default)]
    organization: Option<TokenOrganization>,
}

#[derive(Debug, Deserialize)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenOrganization {
    uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug)]
struct CallbackState {
    expected_state: String,
    result_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<String, String>>>>,
}

/// A permanent refresh verdict holds only for the exact refresh token that
/// produced it; the token is hashed so the cache keeps no plaintext copy.
type RefreshFailureKey = (PathBuf, [u8; 32]);

static PERMANENT_REFRESH_FAILURES: OnceLock<Mutex<HashMap<RefreshFailureKey, String>>> =
    OnceLock::new();

fn permanent_refresh_failures() -> std::sync::MutexGuard<'static, HashMap<RefreshFailureKey, String>>
{
    PERMANENT_REFRESH_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn refresh_failure_key(path: &Path, refresh_token: &str) -> RefreshFailureKey {
    (
        path.to_path_buf(),
        Sha256::digest(refresh_token.as_bytes()).into(),
    )
}

fn clear_permanent_refresh_failure(path: &Path) {
    permanent_refresh_failures().retain(|(candidate, _), _| candidate != path);
}

pub fn auth_file_path() -> PathBuf {
    crate::util::distill_home::distill_home().join(CLAUDE_AUTH_FILE_NAME)
}

pub fn load_credentials() -> io::Result<Option<ClaudeCredentials>> {
    load_credentials_at(&auth_file_path())
}

pub fn is_logged_in() -> bool {
    load_credentials().ok().flatten().is_some()
}

fn load_store_at(path: &Path) -> io::Result<Option<ClaudeAuthStore>> {
    crate::util::secure_file::ensure_owner_only_permissions(path)?;
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if contents.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn credentials_from_store(store: &ClaudeAuthStore) -> Option<ClaudeCredentials> {
    if store.access_token.trim().is_empty() {
        return None;
    }
    Some(ClaudeCredentials {
        access_token: store.access_token.clone(),
        account_uuid: store.account_uuid.clone(),
        email: store.email.clone(),
    })
}

fn load_credentials_at(path: &Path) -> io::Result<Option<ClaudeCredentials>> {
    Ok(load_store_at(path)?
        .as_ref()
        .and_then(credentials_from_store))
}

fn save_store_at(path: &Path, store: &ClaudeAuthStore) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let file = crate::util::secure_file::open_secure_file(&temp)?;
    let mut writer = io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, store)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()?;
    #[cfg(windows)]
    crate::util::secure_file::set_windows_secure_permissions(&temp)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&temp, path)?;
    crate::util::secure_file::ensure_owner_only_permissions(path)?;
    clear_permanent_refresh_failure(path);
    Ok(())
}

fn acquire_auth_lock(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_file_name(format!("{CLAUDE_AUTH_FILE_NAME}.lock"));
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(lock_path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn build_authorize_url(
    endpoints: &ClaudeEndpoints,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<String> {
    let mut url = url::Url::parse(&endpoints.authorize_url)?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", &endpoints.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CLAUDE_SCOPE)
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.into())
}

async fn callback_handler(
    State(state): State<Arc<CallbackState>>,
    Query(query): Query<CallbackQuery>,
) -> Html<&'static str> {
    const FAILED: &str = "<!doctype html><title>Distill login failed</title><h1>Claude login failed</h1><p>Return to Distill for details.</p>";
    // A stray or forged callback must not complete the one-shot login.
    if query.state.as_deref() != Some(state.expected_state.as_str()) {
        return Html(FAILED);
    }
    let result = if let Some(error) = query.error {
        Err(match query.error_description {
            Some(description) if !description.is_empty() => format!("{error}: {description}"),
            _ => error,
        })
    } else {
        query
            .code
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| "OAuth callback did not include a code".to_owned())
    };
    let success = result.is_ok();
    if let Some(sender) = state.result_tx.lock().await.take() {
        let _ = sender.send(result);
    }
    if success {
        Html(
            "<!doctype html><title>Distill connected</title><h1>Claude connected</h1><p>You can close this window and return to Distill.</p>",
        )
    } else {
        Html(FAILED)
    }
}

async fn post_token_request(
    endpoints: &ClaudeEndpoints,
    body: &serde_json::Value,
    what: &str,
) -> Result<reqwest::Response> {
    reqwest::Client::new()
        .post(&endpoints.token_url)
        .json(body)
        .timeout(AUTH_REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("Claude OAuth {what} request failed"))
}

async fn exchange_code(
    endpoints: &ClaudeEndpoints,
    code: &str,
    state: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let response = post_token_request(
        endpoints,
        &serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "client_id": endpoints.client_id,
            "code_verifier": code_verifier,
        }),
        "token exchange",
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Claude OAuth token exchange returned {status}: {}",
            safe_error_excerpt(&body)
        );
    }
    response
        .json()
        .await
        .context("Claude OAuth token response was invalid")
}

fn expiry_from(response: &TokenResponse) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::seconds(response.expires_in.max(0))
}

fn scopes_from(response: &TokenResponse) -> Vec<String> {
    response
        .scope
        .as_deref()
        .map(|scope| scope.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn persist_login(path: &Path, response: TokenResponse) -> Result<ClaudeCredentials> {
    let refresh_token = response
        .refresh_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| anyhow!("Claude OAuth token response omitted a refresh token"))?;
    let store = ClaudeAuthStore {
        expires_at: expiry_from(&response),
        scopes: scopes_from(&response),
        account_uuid: response.account.as_ref().and_then(|a| a.uuid.clone()),
        email: response
            .account
            .as_ref()
            .and_then(|a| a.email_address.clone()),
        organization_uuid: response.organization.as_ref().and_then(|o| o.uuid.clone()),
        access_token: response.access_token,
        refresh_token,
    };
    save_store_at(path, &store)?;
    credentials_from_store(&store).ok_or_else(|| anyhow!("Claude OAuth returned an empty token"))
}

pub async fn run_cli_login() -> Result<ClaudeAccountSummary> {
    let credentials =
        run_browser_login_at(&auth_file_path(), &ClaudeEndpoints::default(), true, None).await?;
    Ok(ClaudeAccountSummary::from(&credentials))
}

/// Browser OAuth for the pager. Unlike the CLI entrypoint it never writes to
/// stderr, which would corrupt the alternate screen; when the browser cannot be
/// opened it sends the authorization URL through `browser_fallback` instead.
pub async fn run_tui_login(
    browser_fallback: tokio::sync::oneshot::Sender<String>,
) -> Result<ClaudeAccountSummary> {
    let credentials = run_browser_login_at(
        &auth_file_path(),
        &ClaudeEndpoints::default(),
        false,
        Some(browser_fallback),
    )
    .await?;
    Ok(ClaudeAccountSummary::from(&credentials))
}

async fn run_browser_login_at(
    path: &Path,
    endpoints: &ClaudeEndpoints,
    announce: bool,
    browser_fallback: Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<ClaudeCredentials> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not bind the Claude OAuth callback port")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");
    let pkce = generate_pkce();
    let expected_state = generate_state();
    let auth_url = build_authorize_url(endpoints, &redirect_uri, &pkce, &expected_state)?;
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(CallbackState {
        expected_state: expected_state.clone(),
        result_tx: tokio::sync::Mutex::new(Some(result_tx)),
    });
    let app = Router::new()
        .route("/callback", get(callback_handler))
        .with_state(state);
    if announce {
        eprintln!();
        eprintln!("Signing in to Claude with your Pro/Max subscription...");
        eprintln!("Open this URL if your browser does not open automatically:");
        eprintln!("  {auth_url}");
    }
    let open_url = auth_url.clone();
    let browser_result = tokio::task::spawn_blocking(move || webbrowser::open(&open_url)).await;
    if let Ok(Err(error)) = &browser_result {
        tracing::warn!(%error, "Claude OAuth browser launch failed; waiting for manual sign-in");
        if let Some(browser_fallback) = browser_fallback {
            let _ = browser_fallback.send(auth_url.clone());
        }
    }

    let code = tokio::time::timeout(
        CALLBACK_TIMEOUT,
        serve_callback_until_result(listener, app, result_rx),
    )
    .await
    .context("timed out waiting for the Claude OAuth callback")??;
    let response = exchange_code(
        endpoints,
        &code,
        &expected_state,
        &redirect_uri,
        &pkce.code_verifier,
    )
    .await?;
    let _lock = acquire_auth_lock(path)?;
    persist_login(path, response)
}

fn access_token_is_fresh(store: &ClaudeAuthStore) -> bool {
    store.expires_at.timestamp() > Utc::now().timestamp() + REFRESH_WINDOW_SECS
}

/// The current credential, refreshed first when it is close to expiry.
pub async fn fresh_credentials() -> Result<Option<ClaudeCredentials>> {
    refresh_at(&auth_file_path(), &ClaudeEndpoints::default()).await
}

async fn refresh_at(path: &Path, endpoints: &ClaudeEndpoints) -> Result<Option<ClaudeCredentials>> {
    let Some(initial) = load_store_at(path)? else {
        return Ok(None);
    };
    if access_token_is_fresh(&initial) {
        return Ok(credentials_from_store(&initial));
    }
    let path_owned = path.to_path_buf();
    let _lock = tokio::task::spawn_blocking(move || acquire_auth_lock(&path_owned)).await??;
    // Another process may have refreshed while this one waited for the lock.
    let Some(mut store) = load_store_at(path)? else {
        return Ok(None);
    };
    if access_token_is_fresh(&store) {
        return Ok(credentials_from_store(&store));
    }
    if store.refresh_token.trim().is_empty() {
        bail!("Claude OAuth refresh token is missing; run `distill login --claude`");
    }
    let failure_key = refresh_failure_key(path, &store.refresh_token);
    if let Some(message) = permanent_refresh_failures().get(&failure_key).cloned() {
        return Err(anyhow!(message));
    }
    let response = post_token_request(
        endpoints,
        &serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": store.refresh_token,
            "client_id": endpoints.client_id,
        }),
        "refresh",
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // A rejected refresh token stays rejected; only 429/5xx can recover.
        let permanent = matches!(status.as_u16(), 400 | 401);
        let action = if permanent {
            " Run `distill login --claude` to reconnect."
        } else {
            ""
        };
        let message = format!(
            "Claude OAuth refresh returned {status}: {}.{action}",
            safe_error_excerpt(&body)
        );
        if permanent {
            permanent_refresh_failures().insert(failure_key, message.clone());
        }
        return Err(anyhow!(message));
    }
    let refreshed: TokenResponse = response
        .json()
        .await
        .context("Claude OAuth refresh response was invalid")?;
    store.expires_at = expiry_from(&refreshed);
    if !scopes_from(&refreshed).is_empty() {
        store.scopes = scopes_from(&refreshed);
    }
    // The refresh token rotates: the old one is spent once a new one is issued.
    if let Some(refresh_token) = refreshed.refresh_token.filter(|t| !t.trim().is_empty()) {
        store.refresh_token = refresh_token;
    }
    store.access_token = refreshed.access_token;
    save_store_at(path, &store)?;
    Ok(credentials_from_store(&store))
}

/// Anthropic documents no revocation endpoint for these tokens, so signing out
/// removes the local credential only.
pub async fn run_cli_logout() -> Result<bool> {
    logout_at(&auth_file_path())
}

fn logout_at(path: &Path) -> Result<bool> {
    let _lock = acquire_auth_lock(path)?;
    let removed = match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    clear_permanent_refresh_failure(path);
    Ok(removed)
}

fn encode_anchor(value: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
}

fn decode_anchor(value: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn header_value_case_insensitive<'a>(
    headers: &'a IndexMap<String, String>,
    expected: &str,
) -> Option<&'a str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(expected))
        .map(|(_, value)| value.as_str())
}

/// Replace every model-supplied auth-anchor header with one derived from the
/// stored credential. The sampler strips these before the request is sent.
fn set_identity_anchor(
    headers: &mut IndexMap<String, String>,
    credentials: Option<&ClaudeCredentials>,
) {
    headers.retain(|name, _| {
        !CLAUDE_RESERVED_AUTH_HEADERS
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    });
    let Some(credentials) = credentials else {
        return;
    };
    headers.insert(CLAUDE_AUTH_ANCHOR_HEADER.to_owned(), "1".to_owned());
    if let Some(account_uuid) = credentials.account_uuid.as_deref() {
        headers.insert(
            CLAUDE_ACCOUNT_ANCHOR_HEADER.to_owned(),
            encode_anchor(account_uuid),
        );
    }
}

fn identity_anchor(headers: &IndexMap<String, String>) -> Option<ClaudeAuthIdentity> {
    (header_value_case_insensitive(headers, CLAUDE_AUTH_ANCHOR_HEADER) == Some("1")).then(|| {
        ClaudeAuthIdentity {
            account_uuid: header_value_case_insensitive(headers, CLAUDE_ACCOUNT_ANCHOR_HEADER)
                .and_then(decode_anchor),
        }
    })
}

/// Per-request resolver used by the sampler. It renews the bearer right before
/// a send and reads whatever this or another Distill process last stored.
#[derive(Debug, Default)]
pub struct ClaudeBearerResolver {
    expected_identity: Option<ClaudeAuthIdentity>,
}

impl ClaudeBearerResolver {
    pub(crate) fn from_credentials(credentials: Option<&ClaudeCredentials>) -> Self {
        Self {
            expected_identity: credentials.map(ClaudeCredentials::identity),
        }
    }

    /// Restores the resolver from a persisted session, which keeps the anchor
    /// headers but cannot keep the resolver itself.
    pub(crate) fn from_headers(headers: &IndexMap<String, String>) -> Self {
        Self {
            expected_identity: identity_anchor(headers),
        }
    }

    fn resolve_credentials(
        &self,
        credentials: ClaudeCredentials,
    ) -> Option<distill_sampler::config::ResolvedBearerAuth> {
        if &credentials.identity() != self.expected_identity.as_ref()? {
            return None;
        }
        Some(distill_sampler::config::ResolvedBearerAuth::bearer_only(
            credentials.access_token,
        ))
    }
}

impl distill_sampler::BearerResolver for ClaudeBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        self.current_auth().map(|auth| auth.bearer)
    }

    fn current_auth(&self) -> Option<distill_sampler::config::ResolvedBearerAuth> {
        self.resolve_credentials(load_credentials().ok().flatten()?)
    }

    fn reserved_headers(&self) -> &'static [&'static str] {
        CLAUDE_RESERVED_AUTH_HEADERS
    }

    fn fail_closed_on_missing(&self) -> bool {
        true
    }

    fn prepare_for_send(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            if let Err(error) = fresh_credentials().await {
                tracing::warn!(%error, "Could not refresh Claude credentials before inference");
            }
        })
    }
}

/// Whether a base URL points at the Anthropic API.
pub fn is_claude_backend(base_url: &str) -> bool {
    url::Url::parse(base_url.trim()).is_ok_and(|url| {
        url.scheme() == "https" && url.host_str() == Some(CLAUDE_INFERENCE_HOST)
    })
}

/// `existing` with the flags the subscription bearer needs added, keeping any
/// beta the model entry already asked for.
fn merge_beta_header(existing: Option<&str>) -> String {
    let mut flags: Vec<&str> = existing
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|flag| !flag.is_empty())
        .collect();
    for required in [CLAUDE_CODE_BETA, CLAUDE_OAUTH_BETA] {
        if !flags.contains(&required) {
            flags.push(required);
        }
    }
    flags.join(",")
}

/// The `anthropic-beta` value a request needs for the subscription bearer.
pub(crate) fn subscription_beta_header() -> String {
    merge_beta_header(None)
}

/// Applies the subscription requirements to a resolved sampler config for a
/// model pointed at the Anthropic API. An explicit `api_key` or resolver wins,
/// so a BYOK entry is left exactly as configured.
pub fn apply_claude_backend(cfg: &mut distill_sampler::SamplerConfig) {
    if !is_claude_backend(&cfg.base_url) || cfg.bearer_resolver.is_some() || cfg.api_key.is_some() {
        return;
    }
    apply_claude_credentials(cfg, load_credentials().ok().flatten());
}

fn apply_claude_credentials(
    cfg: &mut distill_sampler::SamplerConfig,
    credentials: Option<ClaudeCredentials>,
) {
    let Some(credentials) = credentials else {
        return;
    };
    cfg.auth_scheme = distill_sampler::config::AuthScheme::Bearer;
    cfg.bearer_resolver = Some(Arc::new(ClaudeBearerResolver::from_credentials(Some(
        &credentials,
    ))));
    cfg.extra_headers
        .entry("anthropic-version".to_owned())
        .or_insert_with(|| ANTHROPIC_VERSION.to_owned());
    let beta_key = cfg
        .extra_headers
        .keys()
        .find(|name| name.eq_ignore_ascii_case("anthropic-beta"))
        .cloned()
        .unwrap_or_else(|| "anthropic-beta".to_owned());
    let merged = merge_beta_header(cfg.extra_headers.get(&beta_key).map(String::as_str));
    cfg.extra_headers.insert(beta_key, merged);
    set_identity_anchor(&mut cfg.extra_headers, Some(&credentials));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store(expires_in_secs: i64) -> ClaudeAuthStore {
        ClaudeAuthStore {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: Utc::now() + chrono::Duration::seconds(expires_in_secs),
            scopes: vec![],
            account_uuid: Some("account-1".to_owned()),
            email: Some("a@example.com".to_owned()),
            organization_uuid: None,
        }
    }

    fn endpoints(token_url: &str) -> ClaudeEndpoints {
        ClaudeEndpoints {
            authorize_url: CLAUDE_AUTHORIZE_URL.to_owned(),
            token_url: token_url.to_owned(),
            client_id: CLAUDE_CLIENT_ID.to_owned(),
        }
    }

    async fn spawn_token_mock(
        calls: Arc<AtomicUsize>,
        status: axum::http::StatusCode,
    ) -> (String, tokio::task::JoinHandle<()>) {
        async fn handler(
            State((calls, status)): State<(Arc<AtomicUsize>, axum::http::StatusCode)>,
            axum::Json(request): axum::Json<serde_json::Value>,
        ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
            calls.fetch_add(1, Ordering::SeqCst);
            if !status.is_success() {
                return (
                    status,
                    axum::Json(serde_json::json!({"error": "invalid_grant"})),
                );
            }
            assert_eq!(request["grant_type"], "refresh_token");
            assert_eq!(request["refresh_token"], "refresh");
            (
                status,
                axum::Json(serde_json::json!({
                    "access_token": "access-2",
                    "refresh_token": "refresh-2",
                    "expires_in": 3600,
                })),
            )
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/token", axum::routing::post(handler))
            .with_state((calls, status));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/token"), server)
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_subscription_scopes() {
        let pkce = Pkce {
            code_verifier: "verifier".to_owned(),
            code_challenge: "challenge".to_owned(),
        };
        let url = build_authorize_url(
            &endpoints("http://unused"),
            "http://localhost:4000/callback",
            &pkce,
            "state",
        )
        .unwrap();
        let url = url::Url::parse(&url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(query["client_id"], CLAUDE_CLIENT_ID);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"], "challenge");
        assert_eq!(query["state"], "state");
        assert_eq!(query["redirect_uri"], "http://localhost:4000/callback");
        // Inference is what the subscription bearer is for; without it the
        // token could not call the Messages API at all.
        assert!(query["scope"].split(' ').any(|s| s == "user:inference"));
    }

    #[test]
    fn only_the_anthropic_api_host_is_treated_as_claude_backend() {
        assert!(is_claude_backend("https://api.anthropic.com/v1"));
        assert!(is_claude_backend("https://api.anthropic.com"));
        // A look-alike host or a plain-http URL must never receive the bearer.
        assert!(!is_claude_backend("https://api.anthropic.com.evil.test/v1"));
        assert!(!is_claude_backend("http://api.anthropic.com/v1"));
        assert!(!is_claude_backend("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn beta_header_keeps_user_flags_and_adds_required_ones_once() {
        assert_eq!(
            merge_beta_header(None),
            "claude-code-20250219,oauth-2025-04-20"
        );
        assert_eq!(
            merge_beta_header(Some("interleaved-thinking-2025-05-14, oauth-2025-04-20")),
            "interleaved-thinking-2025-05-14,oauth-2025-04-20,claude-code-20250219"
        );
    }

    #[test]
    fn freshness_uses_the_reported_expiry_with_a_refresh_window() {
        assert!(access_token_is_fresh(&store(3600)));
        // Inside the 5 minute window the token is renewed before it can expire mid-request.
        assert!(!access_token_is_fresh(&store(60)));
        assert!(!access_token_is_fresh(&store(-10)));
    }

    #[test]
    fn storage_is_owner_only_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_AUTH_FILE_NAME);
        let saved = store(3600);
        save_store_at(&path, &saved).unwrap();
        assert_eq!(load_store_at(&path).unwrap().unwrap(), saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "tokens must not be group/world readable");
        }
    }

    #[test]
    fn resolver_fails_closed_when_the_account_changes() {
        let session_account = ClaudeCredentials {
            access_token: "token-a".to_owned(),
            account_uuid: Some("account-a".to_owned()),
            email: None,
        };
        let resolver = ClaudeBearerResolver::from_credentials(Some(&session_account));
        assert_eq!(
            resolver.resolve_credentials(session_account.clone()).map(|a| a.bearer),
            Some("token-a".to_owned())
        );
        // A re-login as someone else must not silently bill another account.
        let other = ClaudeCredentials {
            access_token: "token-b".to_owned(),
            account_uuid: Some("account-b".to_owned()),
            email: None,
        };
        assert_eq!(resolver.resolve_credentials(other), None);
        // No anchor means the session never authenticated through Claude OAuth.
        assert_eq!(
            ClaudeBearerResolver::default().resolve_credentials(session_account),
            None
        );
    }

    #[test]
    fn identity_anchor_survives_a_session_round_trip_and_ignores_forged_headers() {
        let credentials = ClaudeCredentials {
            access_token: "token".to_owned(),
            account_uuid: Some("account-a".to_owned()),
            email: None,
        };
        let mut headers = IndexMap::new();
        headers.insert("X-Distill-Claude-Account-Anchor".to_owned(), "forged".to_owned());
        set_identity_anchor(&mut headers, Some(&credentials));
        assert_eq!(
            ClaudeBearerResolver::from_headers(&headers).expected_identity,
            Some(credentials.identity())
        );
        assert_eq!(headers.len(), 2, "the forged casing variant must be replaced");
    }

    #[tokio::test]
    async fn fresh_token_never_calls_the_token_endpoint() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_token_mock(calls.clone(), axum::http::StatusCode::OK).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_AUTH_FILE_NAME);
        save_store_at(&path, &store(3600)).unwrap();

        let credentials = refresh_at(&path, &endpoints(&url)).await.unwrap().unwrap();

        assert_eq!(credentials.access_token, "access");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn expired_token_refreshes_and_persists_the_rotated_refresh_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_token_mock(calls.clone(), axum::http::StatusCode::OK).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_AUTH_FILE_NAME);
        save_store_at(&path, &store(-10)).unwrap();

        let credentials = refresh_at(&path, &endpoints(&url)).await.unwrap().unwrap();

        assert_eq!(credentials.access_token, "access-2");
        // The old refresh token is spent; losing the new one would force a re-login.
        let stored = load_store_at(&path).unwrap().unwrap();
        assert_eq!(stored.refresh_token, "refresh-2");
        assert_eq!(stored.account_uuid.as_deref(), Some("account-1"));
        assert!(access_token_is_fresh(&stored));
        server.abort();
    }

    #[tokio::test]
    async fn rejected_refresh_token_is_not_retried_until_the_file_changes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (url, server) =
            spawn_token_mock(calls.clone(), axum::http::StatusCode::BAD_REQUEST).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_AUTH_FILE_NAME);
        save_store_at(&path, &store(-10)).unwrap();

        let first = refresh_at(&path, &endpoints(&url)).await.unwrap_err();
        let second = refresh_at(&path, &endpoints(&url)).await.unwrap_err();

        assert!(first.to_string().contains("distill login --claude"));
        assert_eq!(first.to_string(), second.to_string());
        // Every request would otherwise hit the auth server with a dead token.
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        save_store_at(&path, &store(-10)).unwrap();
        let _ = refresh_at(&path, &endpoints(&url)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a fresh login clears the verdict");
        server.abort();
    }

    #[tokio::test]
    async fn callback_state_mismatch_does_not_consume_the_login_attempt() {
        let (result_tx, mut result_rx) = tokio::sync::oneshot::channel();
        let state = Arc::new(CallbackState {
            expected_state: "expected".to_owned(),
            result_tx: tokio::sync::Mutex::new(Some(result_tx)),
        });
        let _ = callback_handler(
            State(state.clone()),
            Query(CallbackQuery {
                code: Some("stolen".to_owned()),
                state: Some("other".to_owned()),
                error: None,
                error_description: None,
            }),
        )
        .await;
        assert!(result_rx.try_recv().is_err());

        let _ = callback_handler(
            State(state),
            Query(CallbackQuery {
                code: Some("real".to_owned()),
                state: Some("expected".to_owned()),
                error: None,
                error_description: None,
            }),
        )
        .await;
        assert_eq!(result_rx.try_recv().unwrap(), Ok("real".to_owned()));
    }

    fn anthropic_config() -> distill_sampler::SamplerConfig {
        distill_sampler::SamplerConfig {
            base_url: "https://api.anthropic.com/v1".to_owned(),
            api_backend: distill_sampler::ApiBackend::Messages,
            ..Default::default()
        }
    }

    fn signed_in() -> ClaudeCredentials {
        credentials_from_store(&store(3600)).unwrap()
    }

    #[test]
    fn backend_uses_the_subscription_only_when_signed_in() {
        // Signed out: the model entry is left exactly as configured.
        let mut signed_out = anthropic_config();
        apply_claude_credentials(&mut signed_out, None);
        assert!(signed_out.bearer_resolver.is_none());
        assert!(signed_out.extra_headers.is_empty());

        let mut cfg = anthropic_config();
        cfg.extra_headers.insert(
            "Anthropic-Beta".to_owned(),
            "interleaved-thinking-2025-05-14".to_owned(),
        );
        apply_claude_credentials(&mut cfg, Some(signed_in()));
        assert!(cfg.bearer_resolver.is_some());
        assert_eq!(cfg.auth_scheme, distill_sampler::config::AuthScheme::Bearer);
        assert_eq!(cfg.extra_headers["anthropic-version"], ANTHROPIC_VERSION);
        // The user's own beta survives under its original casing, with ours appended.
        assert_eq!(
            cfg.extra_headers["Anthropic-Beta"],
            "interleaved-thinking-2025-05-14,claude-code-20250219,oauth-2025-04-20"
        );
        assert!(cfg.extra_headers.contains_key(CLAUDE_AUTH_ANCHOR_HEADER));
    }

    #[test]
    fn backend_leaves_byok_and_other_providers_alone() {
        // `apply_claude_backend` decides from the config before it reads any
        // stored credential, so these hold whatever is on disk.
        let mut byok = anthropic_config();
        byok.api_key = Some("sk-ant-own".to_owned());
        apply_claude_backend(&mut byok);
        assert!(byok.bearer_resolver.is_none());
        assert!(byok.extra_headers.is_empty());

        // The subscription bearer must never be attached to another provider.
        let mut other = distill_sampler::SamplerConfig {
            base_url: "https://api.openai.com/v1".to_owned(),
            ..Default::default()
        };
        apply_claude_backend(&mut other);
        assert!(other.bearer_resolver.is_none());
        assert!(other.extra_headers.is_empty());
    }

    #[test]
    fn logout_removes_the_store_and_reports_when_there_was_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_AUTH_FILE_NAME);
        assert!(!logout_at(&path).unwrap());
        save_store_at(&path, &store(3600)).unwrap();
        assert!(logout_at(&path).unwrap());
        assert!(load_credentials_at(&path).unwrap().is_none());
    }
}
