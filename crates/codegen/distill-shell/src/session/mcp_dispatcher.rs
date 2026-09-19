// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-actor side `StatusDispatcher` for MCP client events.
//!
//! Receives [`distill_mcp::servers::McpClientEvent`]s emitted by:
//! - per-client transport-liveness watchers ([`distill_mcp::liveness`]),
//! - the [`distill_mcp::servers::GrokClientHandler`] (server-pushed `tools/list_changed` and `resources/list_changed`),
//! - the `ensure_initialized` success/failure path,
//! - the session MCP config diff path (`UpdateMcpServers` / toggle).
//!
//! Coalesces events in a **50 ms tumbling window** keyed by `(server_name, McpClientEventKind)`.
//! Two events with the same key collapse into the latest one.
//! An MCP server bursting 100 `tools/list_changed` notifications inside 10 ms produces exactly one ACP push.
//!
//! Each surviving entry is emitted as an ACP [`agent_client_protocol::ExtNotification`] with method `x.ai/mcp/server_status`.
//! The payload schema is defined by [`McpServerStatusPayload`].
//!
//! ## Contract
//!
//! - `ConfigDiff` is fanned out per-server, **not** stored as a single event in the buffer.
//! - After a flush, the dispatcher hands each `TransportClosed` / `HandshakeFailed` key to [`crate::session::mcp_restart::maybe_schedule_restart`].
//!   That gate applies the stdio-only, shutting-down, and configured-and-enabled guard rails.
//!   When they pass it spawns the bounded [`crate::session::mcp_restart::auto_restart_stdio`] task.
//!   The dispatcher itself only coalesces and pushes.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use distill_mcp::servers::{
    McpClientEvent, McpClientEventKind, McpServerName, McpState, mcp_server_name, mcp_transport_str,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::extensions::mcp::{MANAGED_GATEWAY_ENTRY_PREFIX, McpServerSource};

/// Tumbling-window coalescing period. See module doc.
pub(crate) const COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Method name for the ACP push.
pub const SERVER_STATUS_METHOD: &str = "x.ai/mcp/server_status";

/// JSON payload pushed over ACP. Fields written in camelCase per ACP convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusPayload {
    pub session_id: String,
    /// MCP server name (`managed_gateway:linear`, `github`, ...).
    pub name: String,
    /// `managed` for gateway catalog ids (`managed_gateway:*`), else `local`.
    pub source: McpServerSource,
    pub status: McpServerStatus,
    pub reason: McpServerStatusReason,
    /// Optional human-readable detail.
    /// Passes the full handshake / transport error reason to the UI verbatim (no sanitization or truncation) so failures are easy to debug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Reserved for future use; always `null` today.
    /// It may later carry the post-restart tool list so the client can re-render without a follow-up `mcp/list` round-trip.
    pub tools: Option<serde_json::Value>,
}

/// Status enum sent on the wire. Lowercase serialization to match the existing pager `McpSessionStatus` family.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerStatus {
    /// Client is in [`distill_mcp::servers::ClientStateKind::Ready`] and the transport is healthy.
    Ready,
    /// Per-server handshake is in flight, or a restart is being debounced.
    Initializing,
    /// Transport closed, handshake failed, or the server is disabled/unconfigured.
    Unavailable,
    /// OAuth required but not yet acquired.
    NeedsAuth,
}

/// Reason a status delta was emitted. Lowercase, snake_case serialization to keep the wire schema stable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatusReason {
    TransportClosed,
    HandshakeFailed,
    ConfigAdded,
    ConfigRemoved,
    ConfigChanged,
    Disabled,
    AuthExpired,
    /// First-time successful handshake (a new server transitioned from `Initializing` to `Ready`).
    /// Every `McpClientEvent::Ready` maps to this reason.
    Initialized,
    /// A watcher fired `TransportClosed`, the auto-restart path re-handshook, and the new handshake succeeded.
    RestartSucceeded,
    /// The auto-restart path exhausted retries.
    RestartFailed,
    /// Old leaders still emit this after reactive reauth. Not produced anymore.
    ManagedTokenRefreshed,
}

/// Live managed connectors are gateway catalog rows (`managed_gateway:*`); everything else is local.
pub(crate) fn classify_source(name: &str) -> McpServerSource {
    if name.starts_with(MANAGED_GATEWAY_ENTRY_PREFIX) {
        McpServerSource::Managed
    } else {
        McpServerSource::Local
    }
}

/// The auto-restart task consults this set before respawning.
/// A `TransportClosed` that arrives *because* the dispatcher's `kill_on_drop` killed the child must NOT resurrect a server the user just deleted.
/// `flush_window` is the only writer: `ConfigRemoved` marks, `Ready` clears, and nothing expires entries, so a removed server stays marked for good.
#[derive(Default)]
pub(crate) struct ShutdownState {
    shutting_down: HashSet<McpServerName>,
    /// Servers with an `auto_restart_stdio` task currently in flight.
    /// A second `TransportClosed` / `HandshakeFailed` arriving while the first respawn is mid-backoff or mid-handshake is short-circuited.
    /// A duplicate task would race on `start_mcp_server` and `owned_clients.insert`, orphaning an stdio child.
    in_flight_restart: HashSet<McpServerName>,
}

impl ShutdownState {
    pub(crate) fn mark(&mut self, name: McpServerName) {
        self.shutting_down.insert(name);
    }
    /// Used by the auto-restart task: when `true`, skip respawn because the session is tearing the client down on purpose (config diff / toggle-off).
    pub(crate) fn is_shutting_down(&self, name: &str) -> bool {
        self.shutting_down.contains(name)
    }
    pub(crate) fn forget(&mut self, name: &str) {
        self.shutting_down.remove(name);
    }

    /// Atomically claim the in-flight restart slot for `name`.
    pub(crate) fn begin_restart(&mut self, name: McpServerName) -> bool {
        self.in_flight_restart.insert(name)
    }
    /// Release the in-flight restart claim taken by [`Self::begin_restart`].
    pub(crate) fn end_restart(&mut self, name: &str) {
        self.in_flight_restart.remove(name);
    }
}

/// The dispatcher loop writes: `mark` / `forget` from `flush_window`.
/// The auto-restart actions read: `is_shutting_down` from `mcp_restart::auto_restart_stdio`.
/// `std::sync::Mutex` is sufficient because every caller acquires the lock synchronously and holds it only for a `HashSet` insert / lookup.
pub(crate) type SharedShutdownState = Arc<std::sync::Mutex<ShutdownState>>;

pub(crate) fn new_shutdown_state() -> SharedShutdownState {
    Arc::new(std::sync::Mutex::new(ShutdownState::default()))
}

/// Several closes in one window could make the last-write-wins collapse discard the id that matches the registered client.
/// `closed` therefore accumulates every close identity per server.
#[derive(Default)]
pub(crate) struct CoalescedWindow {
    /// Last-write-wins per `(server, kind)`, the wire-push dedup.
    pub buf: HashMap<(McpServerName, McpClientEventKind), McpClientEvent>,
    /// All `TransportClosed` client identities per server seen in the window.
    pub closed: HashMap<McpServerName, HashSet<u64>>,
    pub completes: Vec<(McpServerName, String)>,
}

/// Coalesce the buffered events for one window flush.
/// Public so unit tests can exercise the coalescing logic without spinning up the full dispatcher task.
pub(crate) async fn collect_window(
    rx: &mut UnboundedReceiver<McpClientEvent>,
    window: Duration,
) -> Option<CoalescedWindow> {
    let first = rx.recv().await?;
    let mut win = CoalescedWindow::default();
    insert_event(&mut win, first);

    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => insert_event(&mut win, ev),
            // Channel closed: stop collecting; the dispatcher loop will see `None` on its next `rx.recv()` and exit
            Ok(None) => break,
            // Window deadline reached.
            Err(_) => break,
        }
    }

    Some(win)
}

/// Fans `ConfigDiff` out into per-server `McpClientEvent::ConfigAdded` / `ConfigRemoved` entries.
/// The rest of the pipeline then only handles per-server entries.
fn insert_event(win: &mut CoalescedWindow, ev: McpClientEvent) {
    match ev {
        McpClientEvent::ConfigDiff { added, removed } => {
            for name in added {
                win.buf.insert(
                    (name.clone(), McpClientEventKind::ConfigAdded),
                    McpClientEvent::ConfigAdded { server: name },
                );
            }
            for name in removed {
                win.buf.insert(
                    (name.clone(), McpClientEventKind::ConfigRemoved),
                    McpClientEvent::ConfigRemoved { server: name },
                );
            }
        }
        McpClientEvent::ElicitationComplete {
            server,
            elicitation_id,
        } => {
            win.completes.push((server, elicitation_id));
        }
        ev => {
            if let McpClientEvent::TransportClosed { server, client_id } = &ev {
                win.closed
                    .entry(server.clone())
                    .or_default()
                    .insert(*client_id);
            }
            let kind = kind_of(&ev);
            // server_name() returns None only for ConfigDiff, which is handled above
            // The unwrap_or is defensive: a future payload-less variant gets a deterministic key string rather than a panic
            let server = ev.server_name().unwrap_or("").to_string();
            win.buf.insert((server, kind), ev);
        }
    }
}

/// Used as the second half of the coalescing key.
fn kind_of(ev: &McpClientEvent) -> McpClientEventKind {
    match ev {
        McpClientEvent::TransportClosed { .. } => McpClientEventKind::TransportClosed,
        McpClientEvent::HandshakeFailed { .. } => McpClientEventKind::HandshakeFailed,
        McpClientEvent::ToolsChanged { .. } => McpClientEventKind::ToolsChanged,
        McpClientEvent::ResourcesChanged { .. } => McpClientEventKind::ResourcesChanged,
        McpClientEvent::ElicitationComplete { .. } => McpClientEventKind::ElicitationComplete,
        McpClientEvent::Ready { .. } => McpClientEventKind::Ready,
        McpClientEvent::ConfigAdded { .. } => McpClientEventKind::ConfigAdded,
        McpClientEvent::ConfigRemoved { .. } => McpClientEventKind::ConfigRemoved,
        McpClientEvent::ConfigDiff { .. } => {
            unreachable!(
                "ConfigDiff is fanned out into ConfigAdded/ConfigRemoved by insert_event before kind_of is called"
            )
        }
    }
}

pub(crate) fn build_payload(
    session_id: &str,
    key: &(McpServerName, McpClientEventKind),
    event: &McpClientEvent,
) -> McpServerStatusPayload {
    let (server, kind) = key;
    let source = classify_source(server);

    let (status, reason, detail) = match (kind, event) {
        (McpClientEventKind::TransportClosed, _) => (
            McpServerStatus::Unavailable,
            McpServerStatusReason::TransportClosed,
            None,
        ),
        (McpClientEventKind::HandshakeFailed, McpClientEvent::HandshakeFailed { reason, .. }) => {
            let detail = reason.clone();
            (
                McpServerStatus::Unavailable,
                McpServerStatusReason::HandshakeFailed,
                Some(detail),
            )
        }
        (McpClientEventKind::HandshakeFailed, _) => (
            McpServerStatus::Unavailable,
            McpServerStatusReason::HandshakeFailed,
            None,
        ),
        (McpClientEventKind::ToolsChanged, _) => (
            McpServerStatus::Ready,
            McpServerStatusReason::ConfigChanged,
            None,
        ),
        (McpClientEventKind::ElicitationComplete, _) => {
            unreachable!("ElicitationComplete is diverted into win.completes by insert_event")
        }
        (McpClientEventKind::ResourcesChanged, _) => (
            McpServerStatus::Ready,
            McpServerStatusReason::ConfigChanged,
            None,
        ),
        // `Ready` is only emitted from the first-time `ensure_initialized` path
        // Map it to `Initialized`, NOT `RestartSucceeded` (which is reserved for the auto-restart code path)
        (McpClientEventKind::Ready, _) => (
            McpServerStatus::Ready,
            McpServerStatusReason::Initialized,
            None,
        ),
        (McpClientEventKind::ConfigAdded, _) => (
            McpServerStatus::Initializing,
            McpServerStatusReason::ConfigAdded,
            None,
        ),
        (McpClientEventKind::ConfigRemoved, _) => (
            McpServerStatus::Unavailable,
            McpServerStatusReason::ConfigRemoved,
            None,
        ),
    };

    McpServerStatusPayload {
        session_id: session_id.to_string(),
        name: server.clone(),
        source,
        status,
        reason,
        detail,
        tools: None,
    }
}

/// update `shutting_down` for `ConfigRemoved` / `Ready` keys.
/// emit one ACP `x.ai/mcp/server_status` push per surviving buffer entry, via the provided gateway.
/// Failures are logged and dropped; the dispatcher must not block the session actor.
pub(crate) fn flush_window(
    session_id: &str,
    buf: HashMap<(McpServerName, McpClientEventKind), McpClientEvent>,
    shutdown: &SharedShutdownState,
    gateway: &distill_acp_lib::AcpAgentGatewaySender,
) {
    // Recover from poisoning rather than cascade-panicking: `flush_window` does non-trivial work under this lock
    // A single panic while holding it would otherwise turn every future restart-task check and dispatcher window into a panic
    // The `HashSet` state remains coherent across a panic (no half-updated invariant), so `into_inner()` is safe
    let mut shutdown_guard = shutdown.lock().unwrap_or_else(|e| e.into_inner());
    for (key, event) in buf {
        let (server, kind) = &key;
        // ONLY `ConfigRemoved` marks `shutting_down`: the mark signals user intent (config removed / toggled off), not transport death `run_dispatcher` feeds every `TransportClosed` key into `maybe_schedule_restart`.
        // Marking on `TransportClosed` would therefore classify every stdio crash as an intentional shutdown, and auto-restart would never fire.
        // `Ready` clears the mark: a server that just (re-)handshook is back, so future events on it are processed normally.
        match kind {
            McpClientEventKind::ConfigRemoved => {
                shutdown_guard.mark(server.clone());
            }
            McpClientEventKind::Ready => {
                shutdown_guard.forget(server);
            }
            _ => {}
        }

        let payload = build_payload(session_id, &key, &event);
        let raw = match serde_json::value::to_raw_value(&payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(server = %server, error = %e, "failed to serialize mcp/server_status");
                continue;
            }
        };
        gateway
            .forward_fire_and_forget(acp::ExtNotification::new(SERVER_STATUS_METHOD, raw.into()));
    }
}

fn flush_elicitation_completes(
    session_id: &str,
    completes: Vec<(McpServerName, String)>,
    gateway: &distill_acp_lib::AcpAgentGatewaySender,
) {
    for (server, elicitation_id) in completes {
        let payload = distill_tools::mcp_elicitation::McpElicitCompletePayload {
            session_id: session_id.to_string(),
            elicitation_id,
            server_name: Some(server.clone()),
        };
        match serde_json::value::to_raw_value(&payload) {
            Ok(raw) => {
                gateway.forward_fire_and_forget(acp::ExtNotification::new(
                    distill_mcp::wire::MCP_ELICIT_COMPLETE,
                    raw.into(),
                ));
            }
            Err(e) => {
                tracing::warn!(
                    server = %server,
                    error = %e,
                    "failed to serialize mcp/elicit_complete"
                );
            }
        }
    }
}

/// A server with one or more `TransportClosed` ids in the window.
/// Produced by [`collect_close_candidates`] and consumed by [`drop_dead_clients`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeadClient {
    pub server: McpServerName,
    /// Every `TransportClosed` client id observed for this server in the window.
    /// Eviction fires when the registered client's id is in this set.
    pub closed: HashSet<u64>,
}

/// [`drop_dead_clients`] then evicts only the ones whose registered client id actually matches a close.
/// Only stdio `TransportClosed` participates.
/// An entry still present at flush time can therefore only be a freshly-handshook replacement from a remove and re-add.
pub(crate) fn collect_close_candidates(
    win: &CoalescedWindow,
    http_servers: &HashSet<McpServerName>,
) -> Vec<DeadClient> {
    win.closed
        .iter()
        .filter(|(server, _)| !http_servers.contains(*server))
        .map(|(server, closed)| DeadClient {
            server: server.clone(),
            closed: closed.clone(),
        })
        .collect()
}

/// Names eligible for in-place HTTP recovery: enabled HTTP/SSE config entries.
/// MUST match the recovery gate (`SessionActor::is_http_server_configured`).
/// If the two predicates diverge, a disabled HTTP server still in `configs` is kept here (not evicted) yet rejected by the gate (not recovered).
pub(crate) fn recoverable_http_servers(
    configs: &[acp::McpServer],
    disabled: &HashSet<String>,
) -> HashSet<McpServerName> {
    configs
        .iter()
        .filter(|c| {
            matches!(
                c,
                acp::McpServer::Http(acp::McpServerHttp { .. })
                    | acp::McpServer::Sse(acp::McpServerSse { .. })
            )
        })
        .map(|c| mcp_server_name(c).to_string())
        .filter(|name| !disabled.contains(name))
        .collect()
}

/// Server names with a `TransportClosed` event whose transport is HTTP.
/// These are the candidates for in-place HTTP recovery, kept (not evicted) by [`collect_close_candidates`].
pub(crate) fn collect_http_transport_closed(
    buf: &HashMap<(McpServerName, McpClientEventKind), McpClientEvent>,
    http_servers: &HashSet<McpServerName>,
) -> Vec<McpServerName> {
    buf.keys()
        .filter(|(server, kind)| {
            matches!(kind, McpClientEventKind::TransportClosed) && http_servers.contains(server)
        })
        .map(|(server, _)| server.clone())
        .collect()
}

/// Drop dead clients from `McpState::owned_clients`.
/// A close for id N must not evict a replacement (id M ≠ N) registered under the same name while the event sat in the coalesce window.
/// The caller must strip those buffered entries so a stale event is fully inert: no `unavailable` push, no disconnect span, no spurious restart.
pub(crate) async fn drop_dead_clients(
    mcp_state: &Arc<TokioMutex<McpState>>,
    dead: &[DeadClient],
) -> Vec<McpServerName> {
    let mut stale = Vec::new();
    if dead.is_empty() {
        return stale;
    }
    let mut state = mcp_state.lock().await;
    for d in dead {
        let Some(current) = state.owned_clients.get(&d.server) else {
            // Nothing registered: nothing to evict, and the death status is still accurate, so don't mark stale
            continue;
        };
        if d.closed.contains(&current.client_id()) {
            state.owned_clients.remove(&d.server);
            tracing::info!(
                server = %d.server,
                closed_ids = ?d.closed,
                "mcp status dispatcher dropped dead client from owned_clients",
            );
        } else {
            stale.push(d.server.clone());
            tracing::info!(
                server = %d.server,
                closed_ids = ?d.closed,
                current_client_id = current.client_id(),
                "mcp status dispatcher: stale TransportClosed for a replaced client, keeping current client",
            );
        }
    }
    stale
}

/// Spawned via `tokio::task::spawn_local` from the session actor's LocalSet (the gateway is `!Send`, so this MUST run on a LocalSet).
/// `drop_dead_clients`: remove `TransportClosed` entries from [`McpState::owned_clients`] BEFORE pushing status notifications.
/// Skipped entirely when `restart_actions` is `None`.
pub(crate) async fn run_dispatcher(
    session_id: String,
    mut rx: UnboundedReceiver<McpClientEvent>,
    gateway: distill_acp_lib::AcpAgentGatewaySender,
    mcp_state: Arc<TokioMutex<McpState>>,
    shutdown: SharedShutdownState,
    restart_actions: Option<Rc<dyn crate::session::mcp_restart::RestartActions>>,
    cwd: std::path::PathBuf,
) {
    // Cancellation source for spawned `auto_restart_stdio` tasks.
    // The dispatcher exiting (channel closed) means the session is shutting down, so we cancel.
    // An in-flight backoff sleep then aborts promptly instead of running for up to 21s and pushing status through a gateway that is tearing down.
    let restart_cancel = tokio_util::sync::CancellationToken::new();
    loop {
        let Some(mut win) = collect_window(&mut rx, COALESCE_WINDOW).await else {
            tracing::debug!(
                session_id = %session_id,
                "mcp status dispatcher channel closed; exiting",
            );
            break;
        };
        let completes = std::mem::take(&mut win.completes);
        // Completes are independent fire-and-forget notifications, so they flush here regardless of whether any status entries survive below
        flush_elicitation_completes(&session_id, completes, &gateway);
        if win.buf.is_empty() {
            continue;
        }
        let has_transport_closed = win
            .buf
            .keys()
            .any(|(_, k)| matches!(k, McpClientEventKind::TransportClosed));
        let has_config_removed = win
            .buf
            .keys()
            .any(|(_, k)| matches!(k, McpClientEventKind::ConfigRemoved));

        // Classify each configured server's transport (single lock).
        // `http_servers` decides recover-in-place vs evict; `transport_map` feeds the disconnect telemetry below `recoverable_http_servers` excludes disabled names so it stays in lockstep with the recovery gate.
        // Otherwise a disabled HTTP server would be neither evicted nor recovered.
        let (http_servers, transport_map): (
            HashSet<McpServerName>,
            HashMap<McpServerName, &'static str>,
        ) = if has_transport_closed || has_config_removed {
            let disabled = crate::util::config::disabled_mcp_server_names(&cwd);
            let state = mcp_state.lock().await;
            let http = recoverable_http_servers(&state.configs, &disabled);
            let tmap = state
                .configs
                .iter()
                .map(|c| (mcp_server_name(c).to_string(), mcp_transport_str(c)))
                .collect();
            (http, tmap)
        } else {
            (HashSet::new(), HashMap::new())
        };

        // Evict dead stdio clients, gated on client identity (see [`collect_close_candidates`])
        // HTTP `TransportClosed` clients are KEPT for in-place recovery
        // `ConfigRemoved` is not evicted here (the config diff already dropped the old client)
        let dead = collect_close_candidates(&win, &http_servers);
        let stale = drop_dead_clients(&mcp_state, &dead).await;
        // Strip stale closes BEFORE the restart-key capture, the disconnect telemetry, and the status flush below
        // The healthy replacement is then neither reported unavailable nor respawned
        for server in &stale {
            win.buf
                .remove(&(server.clone(), McpClientEventKind::TransportClosed));
        }
        let buf = win.buf;
        if buf.is_empty() {
            continue;
        }
        // Capture restart/recovery candidates BEFORE flush_window consumes `buf` so we don't need to clone events
        //
        // stdio respawn: `TransportClosed` (non-HTTP) and `HandshakeFailed`
        let restart_keys: Vec<(McpServerName, McpClientEventKind)> = if restart_actions.is_some() {
            buf.keys()
                .filter(|(server, k)| match k {
                    McpClientEventKind::TransportClosed => !http_servers.contains(server),
                    McpClientEventKind::HandshakeFailed => true,
                    _ => false,
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        // in-place HTTP recovery: `TransportClosed` for HTTP servers.
        let http_recover: Vec<McpServerName> = if restart_actions.is_some() {
            collect_http_transport_closed(&buf, &http_servers)
        } else {
            Vec::new()
        };

        // Runtime disconnect spans (status=disconnected), enriched with transport and scope from the live config, data flush_window can't reach
        // Emitted before flush_window consumes `buf`
        if has_transport_closed {
            for (server, kind) in buf.keys() {
                if matches!(kind, McpClientEventKind::TransportClosed) {
                    crate::session::telemetry::emit_mcp_connection_span(
                        "disconnected",
                        server.as_str(),
                        transport_map
                            .get(server.as_str())
                            .copied()
                            .unwrap_or("unknown"),
                        crate::util::config::mcp_server_scope(server, &cwd),
                        None,
                        None,
                        None,
                    );
                }
            }
        }
        flush_window(&session_id, buf, &shutdown, &gateway);
        if let Some(actions) = restart_actions.as_ref() {
            for (server, kind) in restart_keys {
                let _ = crate::session::mcp_restart::maybe_schedule_restart(
                    Rc::clone(actions),
                    session_id.clone(),
                    server,
                    kind,
                    restart_cancel.clone(),
                )
                .await;
            }
            for server in http_recover {
                let _ = crate::session::mcp_restart::maybe_schedule_http_recovery(
                    Rc::clone(actions),
                    server,
                    restart_cancel.clone(),
                )
                .await;
            }
        }
    }
    // Channel closed means the session is shutting down. Cancel any in-flight auto-restart backoff sleeps so they don't outlive the dispatcher.
    restart_cancel.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Contract: 100 ToolsChanged events for the same server within 10 ms coalesce into exactly one buffer entry.
    #[tokio::test(start_paused = true)]
    async fn coalesce_within_50ms_window() {
        let (tx, mut rx) = unbounded_channel::<McpClientEvent>();
        for _ in 0..100 {
            tx.send(McpClientEvent::ToolsChanged {
                server: "github".to_string(),
            })
            .unwrap();
        }
        // Drop the sender so collect_window terminates promptly once the buffered events are drained; the window deadline is the backstop
        // Under `start_paused = true` time only advances on `tokio::time::advance` or when a task awaits a timer
        drop(tx);

        let win = collect_window(&mut rx, COALESCE_WINDOW)
            .await
            .expect("at least one event");
        assert_eq!(win.buf.len(), 1, "100 ToolsChanged collapse to one");
        let key = ("github".to_string(), McpClientEventKind::ToolsChanged);
        assert!(win.buf.contains_key(&key));
    }

    #[tokio::test(start_paused = true)]
    async fn elicitation_completes_accumulate_in_window() {
        let (tx, mut rx) = unbounded_channel::<McpClientEvent>();
        tx.send(McpClientEvent::ElicitationComplete {
            server: "github".to_string(),
            elicitation_id: "a".to_string(),
        })
        .unwrap();
        tx.send(McpClientEvent::ElicitationComplete {
            server: "github".to_string(),
            elicitation_id: "b".to_string(),
        })
        .unwrap();
        drop(tx);

        let win = collect_window(&mut rx, COALESCE_WINDOW)
            .await
            .expect("events arrived");
        assert!(win.buf.is_empty());
        assert_eq!(
            win.completes,
            vec![
                ("github".to_string(), "a".to_string()),
                ("github".to_string(), "b".to_string()),
            ]
        );
    }

    /// End-to-end: an `ElicitationComplete`-only window has an empty status buffer (`win.buf`).
    /// The complete notification must still be forwarded to the client.
    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn run_dispatcher_forwards_completes_when_buf_is_empty() {
        use distill_mcp::servers::McpState;

        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let shutdown = new_shutdown_state();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (gw_tx, mut gw_rx) = tokio::sync::mpsc::unbounded_channel();
        let gateway = distill_acp_lib::AcpAgentGatewaySender::new(gw_tx);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(
                    "sess-1".to_string(),
                    rx,
                    gateway,
                    mcp_state,
                    shutdown,
                    None,
                    std::path::PathBuf::from("."),
                ));

                tx.send(McpClientEvent::ElicitationComplete {
                    server: "github".to_string(),
                    elicitation_id: "e-1".to_string(),
                })
                .unwrap();

                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_millis(60)).await;
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }

                let msg = gw_rx
                    .try_recv()
                    .expect("complete must be forwarded even with an empty status buffer");
                let distill_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                    panic!("expected ExtNotification");
                };
                assert_eq!(
                    args.request.method.as_ref(),
                    distill_mcp::wire::MCP_ELICIT_COMPLETE
                );
                let v: serde_json::Value = serde_json::from_str(args.request.params.get()).unwrap();
                assert_eq!(v.get("sessionId"), Some(&serde_json::json!("sess-1")));
                assert_eq!(v.get("elicitationId"), Some(&serde_json::json!("e-1")));
                assert_eq!(v.get("serverName"), Some(&serde_json::json!("github")));

                dispatcher.abort();
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn coalesce_keys_distinguish_server_and_kind() {
        let (tx, mut rx) = unbounded_channel::<McpClientEvent>();
        tx.send(McpClientEvent::ToolsChanged {
            server: "github".to_string(),
        })
        .unwrap();
        tx.send(McpClientEvent::ToolsChanged {
            server: "linear".to_string(),
        })
        .unwrap();
        tx.send(McpClientEvent::Ready {
            server: "github".to_string(),
        })
        .unwrap();
        drop(tx);

        let win = collect_window(&mut rx, COALESCE_WINDOW)
            .await
            .expect("events arrived");
        assert_eq!(win.buf.len(), 3);
    }

    /// The post-fan-out values are the dedicated `McpClientEvent::ConfigAdded` / `ConfigRemoved` variants.
    #[tokio::test(start_paused = true)]
    async fn config_diff_fans_out_per_server() {
        let (tx, mut rx) = unbounded_channel::<McpClientEvent>();
        tx.send(McpClientEvent::ConfigDiff {
            added: vec!["new1".to_string(), "new2".to_string()],
            removed: vec!["old".to_string()],
        })
        .unwrap();
        drop(tx);

        let win = collect_window(&mut rx, COALESCE_WINDOW)
            .await
            .expect("events arrived");
        assert_eq!(win.buf.len(), 3);
        let added_key = ("new1".to_string(), McpClientEventKind::ConfigAdded);
        assert!(win.buf.contains_key(&added_key));
        match win.buf.get(&added_key).unwrap() {
            McpClientEvent::ConfigAdded { server } => assert_eq!(server, "new1"),
            other => panic!("expected ConfigAdded payload, got {other:?}"),
        }
        let removed_key = ("old".to_string(), McpClientEventKind::ConfigRemoved);
        assert!(win.buf.contains_key(&removed_key));
        match win.buf.get(&removed_key).unwrap() {
            McpClientEvent::ConfigRemoved { server } => assert_eq!(server, "old"),
            other => panic!("expected ConfigRemoved payload, got {other:?}"),
        }
    }

    /// End-to-end-ish: the live-config-reload path runs `update_configs_diff` and emits `McpClientEvent::ConfigDiff`.
    /// The dispatcher must then emit one `mcp/server_status` payload per added server with `reason: config_added`.
    /// Builds the per-key payload directly with the same `build_payload` the live `flush_window` calls.
    #[test]
    fn project_config_change_emits_per_server_status_for_added_and_removed() {
        let added_key = ("server_x".to_string(), McpClientEventKind::ConfigAdded);
        let added_ev = McpClientEvent::ConfigAdded {
            server: "server_x".to_string(),
        };
        let added_payload = build_payload("sess-pr6", &added_key, &added_ev);
        assert_eq!(added_payload.name, "server_x");
        assert_eq!(added_payload.status, McpServerStatus::Initializing);
        assert_eq!(added_payload.reason, McpServerStatusReason::ConfigAdded);
        let added_json = serde_json::to_value(&added_payload).unwrap();
        assert_eq!(
            added_json.get("reason"),
            Some(&serde_json::json!("config_added"))
        );

        let removed_key = ("server_y".to_string(), McpClientEventKind::ConfigRemoved);
        let removed_ev = McpClientEvent::ConfigRemoved {
            server: "server_y".to_string(),
        };
        let removed_payload = build_payload("sess-pr6", &removed_key, &removed_ev);
        assert_eq!(removed_payload.name, "server_y");
        assert_eq!(removed_payload.status, McpServerStatus::Unavailable);
        assert_eq!(removed_payload.reason, McpServerStatusReason::ConfigRemoved);
        let removed_json = serde_json::to_value(&removed_payload).unwrap();
        assert_eq!(
            removed_json.get("reason"),
            Some(&serde_json::json!("config_removed"))
        );
    }

    #[test]
    fn payload_maps_transport_closed_to_unavailable() {
        let key = ("linear".to_string(), McpClientEventKind::TransportClosed);
        let ev = McpClientEvent::TransportClosed {
            server: "linear".to_string(),
            client_id: 1,
        };
        let payload = build_payload("sess1", &key, &ev);
        assert_eq!(payload.name, "linear");
        assert_eq!(payload.status, McpServerStatus::Unavailable);
        assert_eq!(payload.reason, McpServerStatusReason::TransportClosed);
        assert_eq!(payload.detail, None);
        assert!(payload.tools.is_none());
    }

    /// Contract: HandshakeFailed `reason` is passed through verbatim (full error, no sanitization/truncation) so debugging is unobstructed.
    #[test]
    fn payload_passes_full_handshake_reason() {
        let key = ("linear".to_string(), McpClientEventKind::HandshakeFailed);
        let ev = McpClientEvent::HandshakeFailed {
            server: "linear".to_string(),
            // Internal service names and full length must pass through untouched: the UI shows the raw error
            reason: "cli-chat-proxy returned 502".to_string(),
        };
        let payload = build_payload("sess1", &key, &ev);
        assert_eq!(payload.status, McpServerStatus::Unavailable);
        assert_eq!(payload.reason, McpServerStatusReason::HandshakeFailed);
        let detail = payload.detail.expect("detail set on handshake failure");
        assert_eq!(
            detail, "cli-chat-proxy returned 502",
            "reason must be passed through verbatim, got: {detail}",
        );
    }

    /// Handshake auth wording on a spawned local client stays `Unavailable` (local recovery is the OAuth path, not `server_status` NeedsAuth).
    #[test]
    fn local_handshake_auth_rejection_stays_unavailable() {
        let key = ("github".to_string(), McpClientEventKind::HandshakeFailed);
        let ev = McpClientEvent::HandshakeFailed {
            server: "github".to_string(),
            reason: "401 Unauthorized".to_string(),
        };
        let payload = build_payload("sess1", &key, &ev);
        assert_eq!(payload.source, McpServerSource::Local);
        assert_eq!(payload.status, McpServerStatus::Unavailable);
        assert_eq!(payload.reason, McpServerStatusReason::HandshakeFailed);
    }

    /// Gateway catalog ids are `Managed`; `grok_com_*` spawned names are not.
    #[test]
    fn classify_source_gateway_is_managed() {
        assert_eq!(
            classify_source("managed_gateway:linear"),
            McpServerSource::Managed
        );
        assert_eq!(classify_source("grok_com_linear"), McpServerSource::Local);
        assert_eq!(classify_source("github"), McpServerSource::Local);
    }

    /// Snapshot of the wire shape for one TransportClosed status push.
    /// Locks the camelCase field naming, lowercase enum values, and the `tools: null` placeholder.
    /// Update the expected JSON alongside any schema bumps so the wire contract and code stay in sync.
    #[test]
    fn server_status_payload_snapshot() {
        let key = ("github".to_string(), McpClientEventKind::TransportClosed);
        let ev = McpClientEvent::TransportClosed {
            server: "github".to_string(),
            client_id: 1,
        };
        let payload = build_payload("sess-1", &key, &ev);
        let json = serde_json::to_value(&payload).unwrap();
        let expected = serde_json::json!({
            "sessionId": "sess-1",
            "name": "github",
            "source": "local",
            "status": "unavailable",
            "reason": "transport_closed",
            "tools": serde_json::Value::Null,
        });
        assert_eq!(json, expected);
    }

    /// Regression guard: a first-time successful `ensure_initialized` fires `McpClientEvent::Ready`.
    /// The dispatcher must emit it as `reason=initialized`, NOT `restart_succeeded` (which is reserved for the auto-restart path).
    #[test]
    fn ready_event_maps_to_initialized_not_restart_succeeded() {
        let key = ("github".to_string(), McpClientEventKind::Ready);
        let ev = McpClientEvent::Ready {
            server: "github".to_string(),
        };
        let payload = build_payload("sess-1", &key, &ev);
        assert_eq!(payload.status, McpServerStatus::Ready);
        assert_eq!(payload.reason, McpServerStatusReason::Initialized);
        assert_ne!(payload.reason, McpServerStatusReason::RestartSucceeded);
        // Wire-level check: serializes to `"initialized"`.
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("reason"), Some(&serde_json::json!("initialized")));
    }

    #[test]
    fn managed_token_refreshed_still_deserializes() {
        let reason: McpServerStatusReason =
            serde_json::from_str("\"managed_token_refreshed\"").unwrap();
        assert_eq!(reason, McpServerStatusReason::ManagedTokenRefreshed);
    }

    /// A `TransportClosed` carrying the registered client's identity must remove it from `owned_clients`.
    #[tokio::test]
    async fn dispatcher_drops_dead_clients_on_transport_closed() {
        use distill_mcp::servers::{McpClient, McpState};
        use std::sync::Arc as StdArc;

        let mcp_state = StdArc::new(TokioMutex::new(McpState::new(vec![])));
        // Pre-populate with a stub client so we have something to remove.
        let github = StdArc::new(McpClient::stub("github"));
        let github_id = github.client_id();
        {
            let mut guard = mcp_state.lock().await;
            guard.owned_clients.insert("github".to_string(), github);
            guard
                .owned_clients
                .insert("linear".to_string(), StdArc::new(McpClient::stub("linear")));
            assert_eq!(guard.owned_clients.len(), 2);
        }

        let mut win = CoalescedWindow::default();
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "github".to_string(),
                client_id: github_id,
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::ToolsChanged {
                server: "linear".to_string(),
            },
        );

        // No HTTP servers configured, so "github" (stdio) is evicted
        let dead = collect_close_candidates(&win, &HashSet::new());
        assert_eq!(
            dead,
            vec![DeadClient {
                server: "github".to_string(),
                closed: HashSet::from([github_id]),
            }]
        );
        let stale = drop_dead_clients(&mcp_state, &dead).await;
        assert!(stale.is_empty(), "a matching id is not a stale event");

        let guard = mcp_state.lock().await;
        assert!(!guard.owned_clients.contains_key("github"));
        assert!(
            guard.owned_clients.contains_key("linear"),
            "ToolsChanged must not remove the client; only TransportClosed does",
        );
    }

    #[test]
    fn collect_close_candidates_picks_transport_closed_only() {
        let mut win = CoalescedWindow::default();
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "a".to_string(),
                client_id: 7,
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::ConfigDiff {
                added: vec![],
                removed: vec!["b".to_string()],
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::ToolsChanged {
                server: "c".to_string(),
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::Ready {
                server: "d".to_string(),
            },
        );

        let dead = collect_close_candidates(&win, &HashSet::new());
        assert_eq!(
            dead,
            vec![DeadClient {
                server: "a".to_string(),
                closed: HashSet::from([7]),
            }],
            "only TransportClosed collects; ConfigRemoved/ToolsChanged/Ready must not",
        );
    }

    /// Closed ids accumulate across the window.
    #[tokio::test]
    async fn window_accumulates_all_closed_ids_and_evicts_current_client() {
        use distill_mcp::servers::{McpClient, McpState};
        use std::sync::Arc as StdArc;

        let old_client = StdArc::new(McpClient::stub("demo-mcp"));
        let current = StdArc::new(McpClient::stub("demo-mcp"));

        let mcp_state = StdArc::new(TokioMutex::new(McpState::new(vec![])));
        mcp_state
            .lock()
            .await
            .owned_clients
            .insert("demo-mcp".to_string(), StdArc::clone(&current));

        // The CURRENT client's close arrives first; the STALE one arrives last and wins the buffer's last-write-wins slot
        let mut win = CoalescedWindow::default();
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "demo-mcp".to_string(),
                client_id: current.client_id(),
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "demo-mcp".to_string(),
                client_id: old_client.client_id(),
            },
        );
        assert_eq!(
            win.buf.len(),
            1,
            "wire dedup unchanged: one buffered entry per (server, kind)",
        );

        let dead = collect_close_candidates(&win, &HashSet::new());
        let stale = drop_dead_clients(&mcp_state, &dead).await;

        assert!(
            !mcp_state
                .lock()
                .await
                .owned_clients
                .contains_key("demo-mcp"),
            "the current client's close must evict it even when the stale \
             event wins the buffer's last-write-wins slot",
        );
        assert!(
            stale.is_empty(),
            "an eviction window is not stale — status/restart must proceed",
        );
    }

    /// Remove-and-re-add race: a stale `TransportClosed` carries the id of an already-replaced client.
    /// It must NOT evict the replacement registered under the same name, and must be reported stale.
    #[tokio::test]
    async fn stale_transport_closed_does_not_evict_replacement_client() {
        use distill_mcp::servers::{McpClient, McpState};
        use std::sync::Arc as StdArc;

        let old_client = StdArc::new(McpClient::stub("demo-mcp"));
        let old_id = old_client.client_id();
        let replacement = StdArc::new(McpClient::stub("demo-mcp"));
        assert_ne!(old_id, replacement.client_id(), "ids must be unique");

        let mcp_state = StdArc::new(TokioMutex::new(McpState::new(vec![])));
        // The config diff already removed `old_client` and the background handshake inserted the replacement under the same name
        // That is the state the dispatcher observes at flush time
        mcp_state
            .lock()
            .await
            .owned_clients
            .insert("demo-mcp".to_string(), StdArc::clone(&replacement));

        let dead = vec![DeadClient {
            server: "demo-mcp".to_string(),
            closed: HashSet::from([old_id]),
        }];
        let stale = drop_dead_clients(&mcp_state, &dead).await;
        assert_eq!(
            stale,
            vec!["demo-mcp".to_string()],
            "a skipped eviction must be reported stale so the caller \
             suppresses the status push / restart for it",
        );

        let guard = mcp_state.lock().await;
        let current = guard
            .owned_clients
            .get("demo-mcp")
            .expect("replacement client must survive a stale TransportClosed");
        assert!(
            StdArc::ptr_eq(current, &replacement),
            "the surviving entry must be the replacement instance",
        );
    }

    #[test]
    fn collect_close_candidates_keeps_http_transport_closed() {
        let mut win = CoalescedWindow::default();
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "http-mcp-server".to_string(),
                client_id: 1,
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "local_stdio".to_string(),
                client_id: 2,
            },
        );
        insert_event(
            &mut win,
            McpClientEvent::ConfigDiff {
                added: vec![],
                removed: vec!["http-mcp-server".to_string()],
            },
        );

        let http: HashSet<McpServerName> = ["http-mcp-server".to_string()].into_iter().collect();

        // stdio TransportClosed drops; http-mcp-server's TransportClosed is kept (HTTP is recovered in place); ConfigRemoved never evicts
        let dead = collect_close_candidates(&win, &http);
        assert_eq!(
            dead,
            vec![DeadClient {
                server: "local_stdio".to_string(),
                closed: HashSet::from([2]),
            }],
        );

        // Recovery candidates: only http-mcp-server's TransportClosed.
        let recover = collect_http_transport_closed(&win.buf, &http);
        assert_eq!(recover, vec!["http-mcp-server".to_string()]);
    }

    fn http_cfg(name: &str) -> agent_client_protocol::McpServer {
        agent_client_protocol::McpServer::Http(
            agent_client_protocol::McpServerHttp::new(
                name.to_string(),
                format!("https://example.test/{name}"),
            )
            .headers(vec![]),
        )
    }

    fn stdio_cfg(name: &str) -> agent_client_protocol::McpServer {
        agent_client_protocol::McpServer::Stdio(
            agent_client_protocol::McpServerStdio::new(
                name.to_string(),
                std::path::PathBuf::from("x"),
            )
            .args(vec![])
            .env(vec![]),
        )
    }

    /// `recoverable_http_servers` keeps only non-disabled HTTP/SSE entries, the same predicate as the recovery gate.
    #[test]
    fn recoverable_http_servers_excludes_stdio_and_disabled() {
        let configs = vec![
            http_cfg("http-mcp-server"),
            http_cfg("grok_com_slack"),
            http_cfg("admin_off"),    // disabled
            stdio_cfg("local_stdio"), // stdio
        ];
        let disabled: HashSet<String> = ["admin_off".to_string()].into_iter().collect();
        let got = recoverable_http_servers(&configs, &disabled);
        let want: HashSet<String> = ["http-mcp-server".to_string(), "grok_com_slack".to_string()]
            .into_iter()
            .collect();
        assert_eq!(got, want);
    }

    /// Scope guard: a disabled HTTP server still present in `configs` must be EVICTED, not orphaned.
    #[test]
    fn disabled_http_server_is_evicted_not_recovered() {
        let configs = vec![http_cfg("admin_off")];
        let disabled: HashSet<String> = ["admin_off".to_string()].into_iter().collect();
        let http_servers = recoverable_http_servers(&configs, &disabled);
        assert!(
            http_servers.is_empty(),
            "disabled HTTP server is not recoverable"
        );

        let mut win = CoalescedWindow::default();
        insert_event(
            &mut win,
            McpClientEvent::TransportClosed {
                server: "admin_off".to_string(),
                client_id: 3,
            },
        );
        assert_eq!(
            collect_close_candidates(&win, &http_servers),
            vec![DeadClient {
                server: "admin_off".to_string(),
                closed: HashSet::from([3]),
            }],
            "disabled HTTP server must be evicted",
        );
        assert!(
            collect_http_transport_closed(&win.buf, &http_servers).is_empty(),
            "disabled HTTP server must not be a recovery candidate",
        );
    }

    // ── Integration test: end-to-end run_dispatcher with restart_actions wired

    /// Construct an `AcpAgentGatewaySender` whose receiver half is dropped immediately.
    /// All `forward_fire_and_forget` calls silently no-op.
    /// Suitable only for tests that don't assert on wire payloads.
    fn discard_gateway() -> distill_acp_lib::AcpAgentGatewaySender {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        distill_acp_lib::AcpAgentGatewaySender::new(tx)
    }

    /// `RestartActions` test double for the integration test.
    /// Uses `RefCell` for internal state, matching the `MockActions` convention in `mcp_restart.rs`.
    /// `is_in_shutting_down` consults a shared `SharedShutdownState`.
    struct CountingActions {
        configured: std::cell::RefCell<HashSet<String>>,
        respawn_calls: std::cell::RefCell<Vec<String>>,
        shutdown: SharedShutdownState,
    }

    impl CountingActions {
        fn new(shutdown: SharedShutdownState) -> Self {
            Self {
                configured: std::cell::RefCell::new(HashSet::new()),
                respawn_calls: std::cell::RefCell::new(Vec::new()),
                shutdown,
            }
        }
        fn configure(&self, name: &str) {
            self.configured.borrow_mut().insert(name.to_string());
        }
        fn respawn_calls(&self) -> Vec<String> {
            self.respawn_calls.borrow().clone()
        }
    }

    #[async_trait::async_trait(?Send)]
    impl crate::session::mcp_restart::RestartActions for CountingActions {
        async fn is_stdio_server_configured(&self, server: &str) -> bool {
            self.configured.borrow().contains(server)
        }
        fn is_in_shutting_down(&self, server: &str) -> bool {
            self.shutdown
                .lock()
                .expect("ShutdownState mutex poisoned")
                .is_shutting_down(server)
        }
        async fn respawn_stdio(
            &self,
            server: &str,
        ) -> Result<crate::session::mcp_restart::Respawn, String> {
            self.respawn_calls.borrow_mut().push(server.to_string());
            Ok(crate::session::mcp_restart::Respawn::Installed)
        }
        fn push_status(&self, _payload: &crate::session::mcp_dispatcher::McpServerStatusPayload) {}
    }

    /// Pre-fix, the dispatcher's `flush_window` would mark `"svr"` on `TransportClosed` and the mock would observe `true`.
    /// `maybe_schedule_restart` would then short-circuit with `reason=shutting_down`, `respawn_calls` would stay empty, and this test would FAIL.
    /// With the fix, only `ConfigRemoved` marks the set, the mock observes `false` for `"svr"`, and the restart task fires.
    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn run_dispatcher_schedules_restart_on_transport_closed() {
        use distill_mcp::servers::McpState;

        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let shutdown = new_shutdown_state();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let gateway = discard_gateway();

        let actions = Rc::new(CountingActions::new(Arc::clone(&shutdown)));
        actions.configure("svr");
        let actions_for_assert = Rc::clone(&actions);
        let restart_actions: Rc<dyn crate::session::mcp_restart::RestartActions> = actions;

        // run_dispatcher is `!Send` (gateway and actions both LocalSet bound). Run on a LocalSet.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(
                    "sess-1".to_string(),
                    rx,
                    gateway,
                    mcp_state,
                    shutdown,
                    Some(restart_actions),
                    std::path::PathBuf::from("."),
                ));

                tx.send(McpClientEvent::TransportClosed {
                    server: "svr".to_string(),
                    client_id: 1,
                })
                .unwrap();

                // Let dispatcher poll: collect_window receives the first event and starts the 50 ms timeout_at
                tokio::task::yield_now().await;
                // Advance past the 50 ms collect_window deadline so the timeout fires
                // flush_window then runs and maybe_schedule_restart spawns auto_restart_stdio
                tokio::time::advance(Duration::from_millis(60)).await;
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }
                // Advance past BACKOFF[0] = 1 s so the spawned auto_restart_stdio's first sleep elapses and respawn_stdio is invoked
                tokio::time::advance(Duration::from_secs(1)).await;
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }

                let calls = actions_for_assert.respawn_calls();
                assert_eq!(
                    calls,
                    vec!["svr".to_string()],
                    "TransportClosed must schedule exactly one respawn_stdio call \
                     (regression guard)",
                );

                dispatcher.abort();
            })
            .await;
    }

    /// End-to-end: a stale `TransportClosed` must be fully inert in `run_dispatcher`.
    /// The replacement stays registered, no status is pushed, and no restart is scheduled (even though the server is stdio-configured).
    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn run_dispatcher_stale_transport_closed_is_fully_inert() {
        use distill_mcp::servers::{McpClient, McpState};
        use std::sync::Arc as StdArc;

        let old_client = StdArc::new(McpClient::stub("svr"));
        let replacement = StdArc::new(McpClient::stub("svr"));
        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        mcp_state
            .lock()
            .await
            .owned_clients
            .insert("svr".to_string(), StdArc::clone(&replacement));

        let shutdown = new_shutdown_state();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Capturing gateway: keep the receiver so the test can assert that NOTHING was pushed for the stale event
        let (gw_tx, mut gw_rx) = tokio::sync::mpsc::unbounded_channel();
        let gateway = distill_acp_lib::AcpAgentGatewaySender::new(gw_tx);

        let actions = Rc::new(CountingActions::new(Arc::clone(&shutdown)));
        // Configured-as-stdio on purpose: proves the no-restart outcome comes from the stale-event suppression, not from the stdio guard rail
        actions.configure("svr");
        let actions_for_assert = Rc::clone(&actions);
        let restart_actions: Rc<dyn crate::session::mcp_restart::RestartActions> = actions;

        let state_for_dispatcher = Arc::clone(&mcp_state);
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(
                    "sess-1".to_string(),
                    rx,
                    gateway,
                    state_for_dispatcher,
                    Arc::clone(&shutdown),
                    Some(restart_actions),
                    std::path::PathBuf::from("."),
                ));

                tx.send(McpClientEvent::TransportClosed {
                    server: "svr".to_string(),
                    client_id: old_client.client_id(),
                })
                .unwrap();

                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_millis(60)).await;
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }
                // Past BACKOFF[0]: a (wrongly) scheduled restart would have respawned by now
                tokio::time::advance(Duration::from_secs(1)).await;
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }

                let guard = mcp_state.lock().await;
                let current = guard
                    .owned_clients
                    .get("svr")
                    .expect("replacement must survive the stale event");
                assert!(
                    StdArc::ptr_eq(current, &replacement),
                    "registered client must still be the replacement",
                );
                drop(guard);
                assert!(
                    actions_for_assert.respawn_calls().is_empty(),
                    "a stale TransportClosed must not schedule a restart",
                );
                assert!(
                    gw_rx.try_recv().is_err(),
                    "a stale TransportClosed must not push any server_status",
                );

                dispatcher.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn flush_window_marks_shutting_down_only_on_config_removed() {
        let shutdown = new_shutdown_state();
        let gateway = discard_gateway();

        // TransportClosed alone: must NOT mark.
        let mut buf: HashMap<(McpServerName, McpClientEventKind), McpClientEvent> = HashMap::new();
        buf.insert(
            ("crashed".to_string(), McpClientEventKind::TransportClosed),
            McpClientEvent::TransportClosed {
                server: "crashed".to_string(),
                client_id: 1,
            },
        );
        flush_window("s", buf, &shutdown, &gateway);
        assert!(
            !shutdown.lock().unwrap().is_shutting_down("crashed"),
            "TransportClosed alone must not mark shutting_down (C1 fix)",
        );

        // ConfigRemoved: must mark.
        let mut buf: HashMap<(McpServerName, McpClientEventKind), McpClientEvent> = HashMap::new();
        buf.insert(
            ("removed".to_string(), McpClientEventKind::ConfigRemoved),
            McpClientEvent::ConfigRemoved {
                server: "removed".to_string(),
            },
        );
        flush_window("s", buf, &shutdown, &gateway);
        assert!(
            shutdown.lock().unwrap().is_shutting_down("removed"),
            "ConfigRemoved must mark shutting_down (kill_on_drop guard rail)",
        );

        // Ready clears.
        let mut buf: HashMap<(McpServerName, McpClientEventKind), McpClientEvent> = HashMap::new();
        buf.insert(
            ("removed".to_string(), McpClientEventKind::Ready),
            McpClientEvent::Ready {
                server: "removed".to_string(),
            },
        );
        flush_window("s", buf, &shutdown, &gateway);
        assert!(
            !shutdown.lock().unwrap().is_shutting_down("removed"),
            "Ready must clear shutting_down",
        );
    }
}
