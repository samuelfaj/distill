// Modified for Distill by Samuel Fajreldines, 2026.
//! WebSocket relay connection management.
//!
//! This module provides a shared `RelayConnection` that handles the WebSocket connection to the grok.com relay server with automatic reconnection.
//! It is used by both `run_headless` and `run_leader` modes.
use super::proxy;
use crate::{teprintln, tprintln};
use distill_login::{GrokAuth, GrokComConfig};
use futures_util::{SinkExt as _, StreamExt as _};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{Message, Utf8Bytes, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
const KEEPALIVE_INTERVAL_SECS: u64 = 15;
/// Read-side liveness deadline. The write half pings every `KEEPALIVE_INTERVAL_SECS`, and a healthy peer answers each ping with a pong. A live connection thus delivers an inbound frame at least that often.
/// If *nothing* arrives for this long the connection is treated as dead and the session is torn down so the reconnect loop can take over.
/// Without it, a half-open TCP connection blocks `ws_inbound.next()` forever and the agent never reconnects. (E.g. the proxy/NAT leg still ACKs our tiny pings while the upstream relay leg is gone.) Sessions stay bricked until the process is killed; the server sees a 1006 close, the client never notices.
const READ_LIVENESS_TIMEOUT_SECS: u64 = 4 * KEEPALIVE_INTERVAL_SECS;
/// Upper bound on a single auth-recovery attempt: a backstop against an indefinitely wedged relay loop, NOT a bound on a healthy refresh.
/// It must stay comfortably above the refresh path's own internal worst case so it only fires when something is truly stuck. `refresh_chain` waits up to 25s for `auth.json.lock` (`REFRESH_LOCK_TIMEOUT`) before IdP IO.
/// Another 25s applies if the suspend-only revalidate re-acquires the lock. The IdP IO has its own timeouts (7s external refresher; 15s per OIDC request with short retries). When this fires the recovery future is dropped (the file lock releases on drop) and the loop falls through to reconnect backoff.
const AUTH_RECOVERY_TIMEOUT_SECS: u64 = 180;
const BASE_DELAY_SECS: u64 = 1;
const MAX_DELAY_SECS: u64 = 60;
const CONNECT_TIMEOUT_SECS: u64 = 30;
/// Bounded wait for the reader after the writer ends a session, so a `-32000` frame that is already in the socket
/// buffer is classified as an auth error instead of being dropped with the reader and reported as a normal close.
const AUTH_DRAIN_TIMEOUT_SECS: u64 = 1;
/// Exponential reconnect backoff for the relay loop.
/// Reset only on evidence the credential was accepted: an authenticated session ended, or recovery produced a new credential. A successful WebSocket handshake is not proof: the relay can accept the socket and reject the bearer on the first JSON-RPC message, and a reset at connect time would retry a standing auth verdict every `2 * BASE_DELAY_SECS` for its whole TTL.
struct ReconnectBackoff {
    attempts: u32,
    delay_secs: u64,
}
impl ReconnectBackoff {
    const fn new() -> Self {
        Self {
            attempts: 0,
            delay_secs: BASE_DELAY_SECS,
        }
    }
    fn reset(&mut self) {
        *self = Self::new();
    }
    /// Record a failed cycle and return the delay to sleep before the next attempt.
    fn next_delay(&mut self) -> Duration {
        self.attempts += 1;
        self.delay_secs = std::cmp::min(self.delay_secs * 2, MAX_DELAY_SECS);
        Duration::from_secs(self.delay_secs)
    }
}
/// JSON-RPC auth error code
const AUTH_ERROR_CODE: i64 = -32000;
use distill_login::AuthManager;
/// Config for the grok.com WebSocket relay.
/// Fields are private so the only constructor is [`RelayConfig::for_session`]: "no relay without a session bearer" is a compile-time guarantee.
#[derive(Clone)]
pub struct RelayConfig {
    ws_url: String,
    ws_origin: String,
    token_header: String,
    auth: GrokAuth,
    auth_manager: Option<Arc<AuthManager>>,
}
impl RelayConfig {
    /// Session gate: builds only for a grok.com first-party session (`is_xai_auth`: x.ai-issuer OIDC or external credential) with a non-empty bearer.
    /// BYOK/ApiKey, non-x.ai issuers (enterprise OIDC, third-party external providers), and deprecated WebLogin get `None`.
    /// With relay off, the leader still serves clients over IPC.
    pub(crate) fn for_session(
        session: &GrokAuth,
        ctx: &GrokComConfig,
        alpha_test_key: Option<String>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Option<Self> {
        if !session.is_xai_auth() || session.key.is_empty() {
            return None;
        }
        let _ = alpha_test_key;
        Some(Self {
            ws_url: ctx.grok_ws_url.clone(),
            ws_origin: ctx.grok_ws_origin.clone(),
            token_header: ctx.token_header.clone(),
            auth: session.clone(),
            auth_manager,
        })
    }
}
/// Callback type for first connection event.
pub(crate) type FirstConnectCallback = Box<dyn FnOnce() + Send + 'static>;
/// Handle to a running relay connection.
/// The relay maintains a persistent WebSocket connection to grok.com with automatic reconnection on disconnection.
pub struct RelayHandle {
    /// Cancel token to stop the relay connection loop
    cancel: CancellationToken,
}
impl RelayHandle {
    /// Stop the relay connection.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
    /// Check if the relay is still running.
    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }
}
impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
/// Spawn a relay connection task that maintains a WebSocket connection. The task runs in the background, automatically reconnecting on disconnection.
/// Messages from the relay are sent to `to_agent_tx`, and messages to send to the relay should be sent via the returned sender. A tuple of (sender for outbound messages, handle to control the relay)
pub fn spawn_relay_connection(
    config: RelayConfig,
    to_agent_tx: mpsc::UnboundedSender<String>,
    parent_cancel: CancellationToken,
) -> (mpsc::UnboundedSender<String>, RelayHandle) {
    spawn_relay_connection_with_callback(config, to_agent_tx, Some(parent_cancel), None)
}
/// Spawn a relay connection with an optional first-connection callback.
///
/// Same as `spawn_relay_connection` but allows providing a callback that will be called once when the first successful connection is established.
pub(crate) fn spawn_relay_connection_with_callback(
    config: RelayConfig,
    to_agent_tx: mpsc::UnboundedSender<String>,
    parent_cancel: Option<CancellationToken>,
    on_first_connect: Option<FirstConnectCallback>,
) -> (mpsc::UnboundedSender<String>, RelayHandle) {
    let cancel = parent_cancel.map_or(CancellationToken::new(), |c| c.child_token());
    let cancel_clone = cancel.clone();
    let (agent_to_ws_tx, agent_to_ws_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        run_relay_loop(
            config,
            to_agent_tx,
            agent_to_ws_rx,
            cancel_clone,
            on_first_connect,
        )
        .await;
    });
    let handle = RelayHandle { cancel };
    (agent_to_ws_tx, handle)
}
/// Check if a connection error is an HTTP 401 from the WebSocket handshake.
fn is_handshake_unauthorized(err: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error as WsError;
    err.downcast_ref::<WsError>()
        .map(|ws_err| {
            matches!(ws_err, WsError::Http(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED)
        })
        .unwrap_or(false)
}
/// Attempt auth recovery after a 401.
/// Returns `true` to reconnect immediately, `false` to exit or fall through to backoff.
async fn attempt_auth_recovery(
    config: &mut RelayConfig,
    cancel: &CancellationToken,
    context: &str,
) -> bool {
    let Some(ref am) = config.auth_manager else {
        teprintln!("Authentication required. Run `grok login` to re-authenticate.");
        cancel.cancel();
        return false;
    };
    info!("auth recovery: relay {context}, attempting refresh");
    let mut recovery = am.unauthorized_recovery(
        Some(config.auth.clone()),
        distill_login::recovery::RecoverySource::Relay,
    );
    let recovered = match tokio::time::timeout(
        Duration::from_secs(AUTH_RECOVERY_TIMEOUT_SECS),
        recovery.next(),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => {
            warn!(
                timeout_secs = AUTH_RECOVERY_TIMEOUT_SECS,
                "auth recovery: relay {context}, refresh timed out"
            );
            distill_telemetry::unified_log::warn(
                "auth recovery: relay refresh timed out",
                None,
                Some(serde_json::json!({
                    "context": context,
                    "timeout_secs": AUTH_RECOVERY_TIMEOUT_SECS,
                })),
            );
            return false;
        }
    };
    match recovered {
        Ok(new_auth) if new_auth.key == config.auth.key => {
            info!("auth recovery: relay {context}, token unchanged, backing off");
            distill_telemetry::unified_log::info(
                "auth recovery: relay token unchanged, backing off",
                None,
                Some(serde_json::json!({
                    "context": context,
                    "key_prefix": distill_auth::bearer_suffix(&new_auth.key),
                })),
            );
            false
        }
        Ok(new_auth) => {
            info!("auth recovery: relay {context}, recovered, reconnecting");
            distill_telemetry::unified_log::info(
                "auth recovery: relay recovered",
                None,
                Some(serde_json::json!({
                    "context": context,
                    "new_key_prefix": distill_auth::bearer_suffix(&new_auth.key),
                })),
            );
            config.auth = new_auth;
            true
        }
        Err(e) if distill_login::recovery::relay_should_cancel(&e) => {
            teprintln!("{e}");
            distill_telemetry::unified_log::warn(
                "auth recovery: relay giving up (terminal)",
                None,
                Some(serde_json::json!({ "context": context, "error": format!("{e}") })),
            );
            cancel.cancel();
            false
        }
        Err(e) => {
            warn!(error = %e, "auth recovery: relay {context}, refresh failed");
            distill_telemetry::unified_log::debug(
                "auth recovery: relay refresh failed",
                None,
                Some(serde_json::json!({ "context": context, "error": format!("{e}") })),
            );
            false
        }
    }
}
/// Internal function that runs the reconnection loop.
async fn run_relay_loop(
    mut config: RelayConfig,
    to_agent_tx: mpsc::UnboundedSender<String>,
    mut agent_to_ws_rx: mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    mut on_first_connect: Option<FirstConnectCallback>,
) {
    let mut backoff = ReconnectBackoff::new();
    let mut first_connection = true;
    let target_host = url::Url::parse(&config.ws_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    let proxy_url = target_host
        .as_deref()
        .and_then(proxy::resolve_proxy_for_host);
    if let Some(ref url) = proxy_url {
        info!(
            proxy = %url,
            target = target_host.as_deref().unwrap_or("unknown"),
            "Using HTTP CONNECT proxy for relay connections"
        );
    }
    loop {
        if cancel.is_cancelled() {
            info!("Relay connection cancelled, stopping");
            break;
        }
        tracing::info!(
            target: crate::instrumentation::TARGET,
            event = "relay_connecting",
            ws_url = %config.ws_url,
            attempt = backoff.attempts,
        );
        match connect_to_relay(&config, proxy_url.as_deref(), &cancel).await {
            Ok(ws) => {
                tracing::info!(
                    target: crate::instrumentation::TARGET,
                    event = "relay_connected",
                    ws_url = %config.ws_url,
                );
                if first_connection {
                    if let Some(callback) = on_first_connect.take() {
                        callback();
                    }
                    first_connection = false;
                }
                let result =
                    run_websocket_session(ws, &to_agent_tx, &mut agent_to_ws_rx, &cancel).await;
                match result {
                    Ok(SessionEndReason::Normal { authenticated }) => {
                        info!(authenticated, "WebSocket session ended normally");
                        if authenticated {
                            backoff.reset();
                        }
                    }
                    Ok(SessionEndReason::AuthError) => {
                        if attempt_auth_recovery(&mut config, &cancel, "Auth error").await {
                            backoff.reset();
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(error = ?e, "WebSocket session ended with error");
                    }
                }
                if cancel.is_cancelled() {
                    break;
                }
                tracing::info!(
                    target: crate::instrumentation::TARGET,
                    event = "relay_disconnected",
                    ws_url = %config.ws_url,
                );
                tprintln!("Disconnected from Grok WebSocket server");
                info!("WebSocket disconnected, will reconnect");
            }
            Err(e) => {
                let handshake_401 = is_handshake_unauthorized(&e);
                tracing::info!(
                    target: crate::instrumentation::TARGET,
                    event = "relay_connection_failed",
                    ws_url = %config.ws_url,
                    error = %e,
                    handshake_401,
                );
                if handshake_401 {
                    if attempt_auth_recovery(&mut config, &cancel, "Handshake 401").await {
                        backoff.reset();
                        continue;
                    }
                } else {
                    warn!(error = %e, "Failed to connect to WebSocket server");
                }
            }
        }
        if cancel.is_cancelled() {
            break;
        }
        let delay = backoff.next_delay();
        info!(
            delay_secs = delay.as_secs(),
            attempt = backoff.attempts,
            "Reconnecting..."
        );
        tprintln!(
            "Attempting to reconnect in {} seconds... (attempt #{})",
            delay.as_secs(),
            backoff.attempts
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}
/// Reason why a WebSocket session ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionEndReason {
    /// Normal disconnection (server closed, network error, etc.).
    /// `authenticated` is set once the relay delivered an ACP message to the agent, which it only does for an
    /// accepted bearer; a session that closed before that proves nothing about the credential.
    Normal { authenticated: bool },
    /// Authentication error that may be recoverable with token refresh
    AuthError,
}
/// Build an HTTP request with the relay authentication headers.
fn build_relay_request(config: &RelayConfig) -> anyhow::Result<axum::http::Request<()>> {
    let mut req = config.ws_url.clone().into_client_request()?;
    req.headers_mut().insert(
        "Origin",
        axum::http::header::HeaderValue::from_str(&config.ws_origin)?,
    );
    req.headers_mut().insert(
        "Authorization",
        axum::http::header::HeaderValue::from_str(&format!("Bearer {}", config.auth.key))?,
    );
    req.headers_mut().insert(
        "X-XAI-Token-Auth",
        axum::http::header::HeaderValue::from_str(&config.token_header)?,
    );
    req.headers_mut().insert(
        "x-userid",
        axum::http::header::HeaderValue::from_str(&config.auth.user_id)?,
    );
    req.headers_mut().insert(
        "x-grok-client-version",
        axum::http::header::HeaderValue::from_static(distill_version::VERSION),
    );
    req.headers_mut().insert(
        crate::http::CLIENT_MODE_HEADER,
        axum::http::header::HeaderValue::from_static(crate::http::process_client_mode()),
    );
    Ok(req)
}
/// Attempt to connect to the relay WebSocket server.
/// If `proxy_url` is `Some`, the connection is established through an HTTP CONNECT tunnel.
/// Otherwise, a direct connection is used.
async fn connect_to_relay(
    config: &RelayConfig,
    proxy_url: Option<&str>,
    cancel: &CancellationToken,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let req = build_relay_request(config)?;
    let connect_timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
    tokio::select! {
        _ = cancel.cancelled() => {
            anyhow::bail!("Connection cancelled");
        }
        result = tokio::time::timeout(connect_timeout, async {
            if let Some(proxy_url) = proxy_url {
                // Proxy path: open TCP to proxy, send CONNECT, then WS handshake.
                let target_host = req.uri().host()
                    .ok_or_else(|| anyhow::anyhow!("WebSocket URL has no host"))?;
                let target_port = req.uri().port_u16().unwrap_or(443);
                let tunneled_stream = proxy::connect_via_proxy(
                    proxy_url,
                    target_host,
                    target_port,
                ).await?;
                // Perform the WebSocket handshake over the tunneled stream.
                let (ws, resp) = tokio_tungstenite::client_async(req, tunneled_stream)
                    .await
                    .map_err(|e| anyhow::Error::from(e).context("WebSocket handshake via proxy failed"))?;
                Ok((ws, resp))
            } else {
                // The default connector never sees the shared trust config.
                let connector =
                    tokio_tungstenite::Connector::Rustls(distill_extra_ca::rustls_client_config());
                connect_async_tls_with_config(req, None, false, Some(connector))
                    .await
                    .map_err(|e| anyhow::Error::from(e).context("WebSocket connection failed"))
            }
        }) => {
            match result {
                Ok(Ok((ws, resp))) => {
                    if let Some(proto) = resp.headers().get("Sec-WebSocket-Protocol") {
                        info!(subprotocol = ?proto, "WS subprotocol negotiated");
                    }
                    Ok(ws)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => anyhow::bail!("WebSocket connection timed out after {} seconds", CONNECT_TIMEOUT_SECS),
            }
        }
    }
}
/// Run a single WebSocket session, handling messages until disconnection.
pub(crate) async fn run_websocket_session<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    to_agent_tx: &mpsc::UnboundedSender<String>,
    from_agent_rx: &mut mpsc::UnboundedReceiver<String>,
    cancel: &CancellationToken,
) -> anyhow::Result<SessionEndReason>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    run_websocket_session_with_liveness(
        ws,
        to_agent_tx,
        from_agent_rx,
        cancel,
        Duration::from_secs(READ_LIVENESS_TIMEOUT_SECS),
    )
    .await
}
/// [`run_websocket_session`] with an explicit read-liveness window (separate entry point so tests can use a short deadline).
pub(crate) async fn run_websocket_session_with_liveness<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    to_agent_tx: &mpsc::UnboundedSender<String>,
    from_agent_rx: &mut mpsc::UnboundedReceiver<String>,
    cancel: &CancellationToken,
    liveness: Duration,
) -> anyhow::Result<SessionEndReason>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    let (mut ws_outbound, mut ws_inbound) = ws.split();
    let (auth_error_tx, mut auth_error_rx) = mpsc::channel::<()>(1);
    let authenticated = std::sync::atomic::AtomicBool::new(false);
    let authenticated_ref = &authenticated;
    let cancel_read = cancel.clone();
    let read_from_ws = async move {
        loop {
            tokio::select! {
                _ = cancel_read.cancelled() => break,
                msg_res = tokio::time::timeout(liveness, ws_inbound.next()) => {
                    let Ok(msg_opt) = msg_res else {
                        // No frame (not even a pong for our keepalive pings) within the liveness window: the connection is dead or half-open
                        // Break so the session ends and the reconnect loop takes over
                        tprintln!("ws_inbound::liveness_timeout");
                        warn!(
                            timeout_secs = liveness.as_secs(),
                            "no WS traffic within liveness window, treating connection as dead"
                        );
                        distill_telemetry::unified_log::warn(
                            "relay: read liveness timeout, reconnecting",
                            None,
                            Some(serde_json::json!({
                                "timeout_secs": liveness.as_secs(),
                            })),
                        );
                        break;
                    };
                    let Some(msg) = msg_opt else { break };
                    match msg {
                        Ok(Message::Text(text)) => {
                            let trimmed_end = text.trim_end_matches(['\r', '\n']);
                            if trimmed_end.is_empty() {
                                debug!("received empty/whitespace WS text frame - skipping");
                                continue;
                            }

                            let json: serde_json::Value = match serde_json::from_str(trimmed_end) {
                                Ok(v) => v,
                                Err(_) => {
                                    debug!("failed to parse WS message as JSON");
                                    continue;
                                }
                            };

                            if let Some(err) = json.get("error") {
                                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                                if code == AUTH_ERROR_CODE {
                                    // Signal auth error to the main loop
                                    let _ = auth_error_tx.send(()).await;
                                    return (false, true); // (normal_end, auth_error)
                                }
                                tracing::warn!(error_code = code, "Server error (skipping)");
                                continue;
                            }

                            match json.get("method").and_then(|m| m.as_str()) {
                                Some(method) => tprintln!("acp_inbound::{}", method),
                                None => tprintln!("ws_inbound::text"),
                            }
                            debug!(bytes = trimmed_end.len(), "received WS text -> agent");

                            let mut json = json;
                            let outbound = if declare_relay_client_capabilities(&mut json) {
                                json.to_string()
                            } else {
                                trimmed_end.to_string()
                            };
                            if to_agent_tx.send(outbound).is_err() {
                                warn!("Failed to forward message to agent - channel closed");
                                break;
                            }
                            authenticated_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        Ok(Message::Binary(bin)) => {
                            tprintln!("ws_inbound::binary");
                            if let Ok(s) = std::str::from_utf8(&bin) {
                                let s = s.trim_end_matches(['\r', '\n']);
                                if s.is_empty() {
                                    debug!("received empty WS binary frame - skipping");
                                    continue;
                                }
                                debug!(bytes = s.len(), "received WS binary(utf8) -> agent");
                                if to_agent_tx.send(s.to_string()).is_err() {
                                    break;
                                }
                                authenticated_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                debug!("received non-utf8 WS binary frame - skipping");
                            }
                        }
                        Ok(Message::Close(frame_opt)) => {
                            tprintln!("ws_inbound::close");
                            if let Some(frame) = frame_opt {
                                info!(code = ?frame.code, reason = %frame.reason, "WS close received");
                            } else {
                                info!("WS close received (no frame)");
                            }
                            break;
                        }
                        Ok(Message::Ping(p)) => {
                            tprintln!("ws_inbound::ping");
                            debug!(len = p.len(), "received WS Ping");
                        }
                        Ok(Message::Pong(p)) => {
                            tprintln!("ws_inbound::pong");
                            debug!(len = p.len(), "received WS Pong");
                        }
                        Ok(Message::Frame(_)) => {
                            tprintln!("ws_inbound::frame");
                        }
                        Err(e) => {
                            tprintln!("ws_inbound::error::{:?}", &e);
                            warn!(error = ?e, "WS read error");
                            break;
                        }
                    }
                }
            }
        }
        (true, false)
    };
    let cancel_write = cancel.clone();
    let write_to_ws = async move {
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
        loop {
            tokio::select! {
                _ = cancel_write.cancelled() => break,
                msg_opt = from_agent_rx.recv() => {
                    match msg_opt {
                        Some(msg) => {
                            // Per-message logging is debug-only: at info level a streaming session mirrors every `session/update` delta here
                            // The full JSON parse and params re-format produced over 100 MB of leader.log churn on dashboard-heavy machines
                            // Skip the parse entirely unless debug logging is enabled
                            if tracing::enabled!(tracing::Level::DEBUG) {
                                if let Ok(json_val) =
                                    serde_json::from_str::<serde_json::Value>(&msg)
                                {
                                    let method = json_val.get("method").and_then(|m| m.as_str());
                                    let line_to_print = match method {
                                        Some("session/update") => {
                                            let params = json_val
                                                .get("params")
                                                .unwrap_or(&serde_json::Value::Null);
                                            format!("acp_outbound::session/update::{params}")
                                        }
                                        Some(m) => format!("acp_outbound::{m}"),
                                        None => "acp_outbound::response".to_string(),
                                    };
                                    debug!("{line_to_print}");
                                } else {
                                    debug!("acp_outbound::response");
                                }
                            }

                            if !msg.is_empty()
                                && let Err(e) = ws_outbound.send(Message::Text(Utf8Bytes::from(msg))).await
                            {
                                warn!(error = ?e, "failed to send to WS");
                                break;
                            }
                        }
                        None => {
                            info!("Agent outbound channel closed");
                            break;
                        }
                    }
                }
                _ = keepalive.tick() => {
                    tprintln!("ws::keep_alive_tick");
                    if let Err(e) = ws_outbound.send(Message::Ping(Vec::new().into())).await {
                        tprintln!("ws::keep_alive::error::{:?}", &e);
                        break;
                    }
                }
            }
        }
        anyhow::Ok(())
    };
    tokio::pin!(read_from_ws);
    tokio::select! {
        (_, auth_error) = &mut read_from_ws => {
            info!("WebSocket read task completed (connection closed)");
            if auth_error {
                return Ok(SessionEndReason::AuthError);
            }
        }
        res = write_to_ws => {
            info!("WebSocket write task completed");
            // The reader may hold an unread auth frame; let it finish classifying before the session is reported as a normal close
            if let Ok((_, true)) = tokio::time::timeout(
                Duration::from_secs(AUTH_DRAIN_TIMEOUT_SECS),
                &mut read_from_ws,
            )
            .await
            {
                return Ok(SessionEndReason::AuthError);
            }
            res?;
        }
    }
    if auth_error_rx.try_recv().is_ok() {
        return Ok(SessionEndReason::AuthError);
    }
    Ok(SessionEndReason::Normal {
        authenticated: authenticated.load(std::sync::atomic::Ordering::Relaxed),
    })
}
/// Capabilities the relay holds as a client regardless of what its `initialize` says.
///
/// The relay is the durable server-side store for every session on this
/// connection and persists `user_message_chunk` solely from the agent's
/// notifications — it never re-derives the prompt from `session/prompt`. Since
/// the live echo became opt-in (`x.ai/userMessageEcho`), a relay whose
/// `initialize` omits the flag silently loses every user prompt from stored
/// history. Declaring it here, where the relay's frames enter the agent, makes
/// persistence independent of the relay build; an explicit value from the relay
/// is left alone. Returns whether the frame was modified.
fn declare_relay_client_capabilities(frame: &mut serde_json::Value) -> bool {
    if frame.get("method").and_then(|m| m.as_str()) != Some("initialize") {
        return false;
    }
    let Some(params) = frame.get_mut("params").and_then(|p| p.as_object_mut()) else {
        return false;
    };
    let caps = params
        .entry("clientCapabilities")
        .or_insert_with(|| serde_json::json!({}));
    let Some(caps) = caps.as_object_mut() else {
        return false;
    };
    let meta = caps.entry("_meta").or_insert_with(|| serde_json::json!({}));
    let Some(meta) = meta.as_object_mut() else {
        return false;
    };
    if meta.contains_key(crate::session::USER_MESSAGE_ECHO_CAPABILITY) {
        return false;
    }
    meta.insert(
        crate::session::USER_MESSAGE_ECHO_CAPABILITY.to_string(),
        serde_json::Value::Bool(true),
    );
    true
}
#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
