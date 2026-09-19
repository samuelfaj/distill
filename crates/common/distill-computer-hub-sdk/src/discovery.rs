// Modified for Distill by Samuel Fajreldines, 2026.
//! One-shot harness-side server discovery: resolve the hub URL and run the
//! hub's `servers.list` with a caller-supplied credential. A single shared
//! helper so env parsing, `ws`/`wss` gating, the throwaway harness session,
//! and pool reuse live in one place for every caller.

use std::sync::Arc;
use std::time::Duration;

use distill_tool_protocol::{ServerInfo, SessionId};

use crate::pool::HubConnectionPool;
use crate::{AuthProvider, ClientError, ToolHarnessBuilder};

/// Why `COMPUTER_HUB_URL` did not yield a usable hub URL.
#[derive(Debug, thiserror::Error)]
pub enum HubUrlError {
    #[error("computer hub is not configured")]
    NotConfigured,
    #[error("invalid COMPUTER_HUB_URL: {0}")]
    Invalid(#[from] url::ParseError),
    #[error("COMPUTER_HUB_URL must be ws:// or wss://")]
    UnsupportedScheme,
}

/// Resolve the computer hub WebSocket URL (`ws://` / `wss://`) from the
/// `COMPUTER_HUB_URL` environment variable.
pub fn resolve_hub_url() -> Result<url::Url, HubUrlError> {
    let raw = std::env::var("COMPUTER_HUB_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or(HubUrlError::NotConfigured)?;
    let url = url::Url::parse(&raw)?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(HubUrlError::UnsupportedScheme);
    }
    Ok(url)
}

/// Why a [`list_servers`] round-trip failed. `Connect` (couldn't reach /
/// handshake the hub) is kept distinct from `List` (the hub answered the
/// connect but the call failed) so callers can map availability vs internal
/// errors differently.
#[derive(Debug, thiserror::Error)]
pub enum ListServersError {
    #[error("invalid session id: {0}")]
    SessionId(#[from] distill_tool_protocol::IdError),
    #[error("failed to connect to computer hub: {0}")]
    Connect(ClientError),
    #[error("servers.list failed: {0}")]
    List(ClientError),
    #[error("servers.list timed out")]
    Timeout,
}

/// Connect to the hub as a harness (process-wide shared pool, deduplicated
/// by `(url, principal)`) and run `servers.list` with `auth`.
///
/// `servers.list` is keyed by the authenticated principal, not a session, so
/// the harness binds a throwaway `{session_id_prefix}-<uuid>` session.
/// `timeout`, when set, bounds the whole connect + round-trip.
pub async fn list_servers(
    url: url::Url,
    auth: Arc<dyn AuthProvider>,
    session_id_prefix: &str,
    timeout: Option<Duration>,
) -> Result<Vec<ServerInfo>, ListServersError> {
    let session_id = SessionId::new(format!("{session_id_prefix}-{}", uuid::Uuid::new_v4()))?;
    let allow_insecure_ws = url.scheme() == "ws";
    let fut = async {
        let harness = ToolHarnessBuilder::default()
            .pool(HubConnectionPool::shared().await)
            .url(url)
            .auth_provider(auth)
            .session(session_id)
            .allow_insecure_ws(allow_insecure_ws)
            .build()
            .await
            .map_err(ListServersError::Connect)?;
        harness.list_servers().await.map_err(ListServersError::List)
    };
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ListServersError::Timeout)?,
        None => fut.await,
    }
}
