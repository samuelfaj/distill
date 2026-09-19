// Modified for Distill by Samuel Fajreldines, 2026.
//! Server integration for the workspace, over a single [`ToolServer`] connection.
//!
//! **Provider direction:** The workspace exposes its session tools to the server via [`ToolServer`] and [`WorkspaceToolHandler`].
//! When the server receives a `tool_call_request` it routes to the workspace's handler.
//! The handler dispatches to the workspace session matching the `session_id`.
//! Sessions are created on demand via `session.bind`; there is no privileged "main" session.
//!
//! **Session multiplexing:** Multiple sessions can be bound to the same workspace server concurrently.
//! Each gets its own workspace session (isolated CWD, shell state, toolset).
//! Sessions are created when the server sends a `session.bind` notification, and cleaned up on disconnect or explicit unbind.
//!
//! **Notifications:** The same `ToolServer` connection is used for subscribing to notifications (tool changes).
//! It also sends workspace events / tool notifications back to the server.
//!
//! The [`HubConnectionPool`] and auth credential are shared, so everything multiplexes over one WebSocket per `(url, principal)`.
//!
//! # Security considerations
//!
//! - **Provider direction** returns full `result.prompt_text` to the remote server.
//!   This may contain sensitive workspace data (file contents, env vars).
//!   Callers must ensure the server endpoint is trusted.
//! - **Consumer direction** remote tools are merged with `kind: None` and are only visible under `CapabilityMode::All`.
//!   They are dropped in subagent sessions with restricted capability modes.
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::handle::WorkspaceHandle;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::task::JoinHandle;
use url::Url;
use distill_computer_hub_sdk::{
    AuthProvider, CLOSE_CODE_SANDBOX_TERMINATED, ClientError, HubConnectionPool,
    InitialConnectPolicy, RefusalCode, ToolServer, ToolServerBuilder, ToolServerHandler,
};
use distill_diag_server::DiagHandle;
use distill_tools::registry::types::ToolConfig;
use distill_tool_protocol::ToolId;
use distill_tool_runtime::{
    ToolCallContext, ToolError, ToolErrorKind, ToolStream, ToolStreamItem, TypedToolOutput,
    terminal_only,
};
use distill_tool_types::ToolDescription;
/// Configuration for connecting to a server instance, via [`WorkspaceConfig::hub_config`](crate::config::WorkspaceConfig::hub_config).
/// When `Some`, connect after construction via [`WorkspaceHandle::connect_hub`](crate::handle::WorkspaceHandle::connect_hub).
#[derive(Clone)]
pub struct HubConfig {
    /// Server WebSocket URL (`ws://` or `wss://`).
    pub url: Url,
    pub auth: Arc<dyn AuthProvider>,
    /// Activity tracker to poke on reconnect so the status publisher sends an immediate heartbeat (prevents status reverting to null).
    pub activity_tracker: Option<Arc<crate::activity::ActivityTracker>>,
    /// Stable server ID for `register_server` / `servers.list` / `server.bind`.
    /// When `None`, the SDK default (`"workspace-server"`) is used.
    /// Set to the sandbox `session_id` in production so each workspace server has a unique, predictable identity.
    pub server_id: Option<String>,
    /// Optional extra access key attached on the server connection when the non-production feature set is enabled.
    /// `None` on prod / local-dev.
    pub alpha_test_key: Option<String>,
    /// Permit a plaintext `ws://` server on a non-loopback host (mesh-secured).
    pub allow_insecure_ws: bool,
    /// Diagnostics-server state handle that drives the `/ready` state from connection events.
    /// `None` means no diagnostics server (embedded/local use).
    pub diag: Option<DiagHandle>,
    /// Told when a reconnect's upgrade is refused `401`/`403` (with the policy code a `403` names);
    /// the connection is over, and an owner that supervises it acts at once rather than at its
    /// next liveness sweep.
    pub on_handshake_refused: Option<HandshakeRefused>,
}
/// See [`HubConfig::on_handshake_refused`].
pub type HandshakeRefused = Arc<dyn Fn(u16, Option<RefusalCode>) + Send + Sync>;
impl std::fmt::Debug for HubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubConfig")
            .field("url", &self.url.as_str())
            .field("auth", &"<redacted>")
            .field("server_id", &self.server_id)
            .finish()
    }
}
/// Live handle to a server connection, tool server, and notification listener.
/// Stored on [`WorkspaceShared`](crate::session::WorkspaceShared); created by [`WorkspaceHandle::connect_hub`](crate::handle::WorkspaceHandle::connect_hub).
pub(crate) struct HubHandle {
    /// The tool server exposing workspace tools to the server (provider direction).
    /// Also used for subscribing to and sending notifications.
    pub(crate) server: ToolServer,
    /// Kept alive so the underlying WebSocket connection is not dropped.
    /// Dropping this last reference tears down the connection.
    #[allow(dead_code)]
    pub(crate) pool: Arc<HubConnectionPool>,
    /// Background tool server run loop task handle.
    server_task: Option<JoinHandle<()>>,
    /// Background notification listener task handle.
    notification_task: Option<JoinHandle<()>>,
    /// Background task that forwards `WorkspaceEvent`s as `tool.notify` custom frames through the server.
    event_publisher_task: Option<JoinHandle<()>>,
    /// Background drain feeding the `ActivityTracker` from the session tool-notification stream (see `run_activity_feed`).
    activity_feed_task: Option<JoinHandle<()>>,
    /// Background task that publishes `tool_server.status` to the server.
    status_publisher_task: Option<JoinHandle<()>>,
    /// Background task that listens for `session.bind` and creates workspace sessions.
    session_bind_task: Option<JoinHandle<()>>,
    /// Background codebase-index event forwarder.
    /// Tracked so shutdown aborts it (it holds the `events` sender and cannot self-terminate).
    codebase_index_forwarder_task: Option<JoinHandle<()>>,
    /// Background client ext-notification forwarder.
    client_ext_forwarder_task: Option<JoinHandle<()>>,
    /// Background tool-definitions event forwarder.
    /// Tracked so shutdown aborts it; otherwise a reconnect would stack a second subscriber processing every workspace event.
    tool_defs_forwarder_task: Option<JoinHandle<()>>,
    /// Background `FsChanged` producer for the exposed root. It holds the only strong reference to
    /// the shared OS watcher, so the handle aborts it on drop: a `HubHandle` that goes away without
    /// `shutdown` still releases the watch.
    fs_change_producer_task: Option<tokio_util::task::AbortOnDropHandle<()>>,
}
impl std::fmt::Debug for HubHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubHandle")
            .field("server", &self.server)
            .field(
                "server_task",
                if self.server_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "notification_task",
                if self.notification_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "event_publisher_task",
                if self.event_publisher_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "activity_feed_task",
                if self.activity_feed_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "status_publisher_task",
                if self.status_publisher_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "session_bind_task",
                if self.session_bind_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "codebase_index_forwarder_task",
                if self.codebase_index_forwarder_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .field(
                "client_ext_forwarder_task",
                if self.client_ext_forwarder_task.is_some() {
                    &"Some(<running>)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}
/// WS timing knobs forwarded from [`crate::StatusConfig`] into the SDK builder.
#[derive(Clone)]
pub(crate) struct HubWsTiming {
    pub ping: std::time::Duration,
    pub reconnect_backoff: Option<Vec<std::time::Duration>>,
    pub liveness_deadline: Option<std::time::Duration>,
    pub initial_connect: InitialConnectPolicy,
}
impl HubWsTiming {
    pub(crate) fn from_status(cfg: &crate::StatusConfig) -> Self {
        Self {
            ping: cfg.ws_ping,
            reconnect_backoff: cfg.ws_reconnect_backoff.clone(),
            liveness_deadline: cfg.ws_liveness_deadline,
            initial_connect: InitialConnectPolicy {
                attempt_timeout: None,
                hedge_after: cfg.hub_connect_hedge_after,
                deadline: cfg.hub_connect_deadline,
            },
        }
    }
}
impl HubHandle {
    /// Build the connection pool and tool server and return a handle. Sessions are bound later via `session.bind`.
    /// The run loop and notification listener are not started here; set those tasks after spawning.
    pub(crate) async fn connect(
        config: &HubConfig,
        ws: HubWsTiming,
        tool_handlers: Vec<std::sync::Arc<dyn ToolServerHandler>>,
        server_metadata: Option<serde_json::Value>,
        session_handler_resolver: Option<distill_computer_hub_sdk::SessionHandlerResolver>,
        on_session_unbound: Option<std::sync::Arc<distill_computer_hub_sdk::SessionUnboundCallback>>,
    ) -> Result<Self, ClientError> {
        let pool = HubConnectionPool::new();
        let server_url = config.url.clone();
        let mut server_builder = ToolServerBuilder::default()
            .pool(pool.clone())
            .url(server_url)
            .auth_provider(config.auth.clone())
            .allow_insecure_ws(config.allow_insecure_ws)
            .binary_version(distill_version::VERSION)
            .image_capabilities(
                crate::image_capabilities::image_capabilities()
                    .wire()
                    .to_vec(),
            )
            .with_ws_ping_interval(ws.ping)
            .with_initial_connect(ws.initial_connect);
        if let Some(schedule) = ws.reconnect_backoff {
            server_builder = server_builder.with_reconnect_backoff(schedule);
        }
        if let Some(deadline) = ws.liveness_deadline {
            server_builder = server_builder.with_ws_liveness_deadline(deadline);
        }
        let activity_tracker = config.activity_tracker.clone();
        if activity_tracker.is_some() {
            server_builder = server_builder.on_reconnect(move |_| {
                if let Some(tracker) = &activity_tracker {
                    tracker.poke();
                }
            });
        }
        if let Some(diag) = config.diag.clone() {
            let on_connect = diag.clone();
            let on_disconnect = diag.clone();
            let on_terminal_close = diag.clone();
            server_builder = server_builder
                .reconnect_after_terminal_close_codes([CLOSE_CODE_SANDBOX_TERMINATED])
                .on_connect(move || on_connect.set_connected())
                .on_disconnect(move || on_disconnect.set_disconnected())
                .on_terminal_close(move |code| on_terminal_close.set_terminal_close(code))
                .on_reconnect_settled(move || {
                    diag.revive_connected(&[CLOSE_CODE_SANDBOX_TERMINATED]);
                });
        }
        if let Some(refused) = config.on_handshake_refused.clone() {
            server_builder =
                server_builder.on_handshake_refused(move |status, code| refused(status, code));
        }
        if let Some(ref id) = config.server_id {
            server_builder = server_builder.server_id(parse_server_id(id)?);
        }
        for handler in tool_handlers {
            server_builder = server_builder.tool_dyn(handler);
        }
        if let Some(meta) = server_metadata {
            server_builder = server_builder.metadata(meta);
        }
        if let Some(resolver) = session_handler_resolver {
            server_builder = server_builder.session_handler_resolver(resolver);
        }
        if let Some(cb) = on_session_unbound {
            server_builder = server_builder.on_session_unbound(move |sid| cb(sid));
        }
        let server = server_builder.build().await?;
        Ok(Self {
            server,
            pool,
            server_task: None,
            notification_task: None,
            event_publisher_task: None,
            activity_feed_task: None,
            status_publisher_task: None,
            session_bind_task: None,
            codebase_index_forwarder_task: None,
            client_ext_forwarder_task: None,
            tool_defs_forwarder_task: None,
            fs_change_producer_task: None,
        })
    }
    /// Attach the background tool server run loop task.
    pub(crate) fn set_server_task(&mut self, task: JoinHandle<()>) {
        self.server_task = Some(task);
    }
    /// Attach the background notification listener task.
    pub(crate) fn set_notification_task(&mut self, task: JoinHandle<()>) {
        self.notification_task = Some(task);
    }
    /// Attach the background workspace event publisher task.
    pub(crate) fn set_event_publisher_task(&mut self, task: JoinHandle<()>) {
        self.event_publisher_task = Some(task);
    }
    /// Attach the background activity-feed drain task.
    pub(crate) fn set_activity_feed_task(&mut self, task: JoinHandle<()>) {
        self.activity_feed_task = Some(task);
    }
    /// Attach the background status publisher task.
    pub(crate) fn set_status_publisher_task(&mut self, task: JoinHandle<()>) {
        self.status_publisher_task = Some(task);
    }
    /// Attach the background codebase-index event forwarder task.
    pub(crate) fn set_codebase_index_forwarder_task(&mut self, task: JoinHandle<()>) {
        self.codebase_index_forwarder_task = Some(task);
    }
    /// Attach the background client ext-notification forwarder task.
    pub(crate) fn set_client_ext_forwarder_task(&mut self, task: JoinHandle<()>) {
        self.client_ext_forwarder_task = Some(task);
    }
    /// Attach the background tool-definitions event forwarder task.
    pub(crate) fn set_tool_defs_forwarder_task(&mut self, task: JoinHandle<()>) {
        self.tool_defs_forwarder_task = Some(task);
    }
    /// Attach the background `FsChanged` producer task.
    pub(crate) fn set_fs_change_producer_task(
        &mut self,
        task: tokio_util::task::AbortOnDropHandle<()>,
    ) {
        self.fs_change_producer_task = Some(task);
    }
    /// Cooperative shutdown: unregister tools and sessions, then abort background tasks.
    /// Guarded by a 5-second timeout so an unreachable server cannot block indefinitely.
    pub(crate) async fn shutdown(self) {
        const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.server.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "tool server shutdown error"),
            Err(_) => tracing::warn!("tool server shutdown timed out"),
        }
        if let Some(task) = self.server_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.notification_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.event_publisher_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.activity_feed_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.status_publisher_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.session_bind_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.codebase_index_forwarder_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.client_ext_forwarder_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.tool_defs_forwarder_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.fs_change_producer_task {
            task.abort();
            let _ = task.await;
        }
    }
}
/// Per-tool handler dispatched to the session matching `session_id`. The server routes by `tool_id` with no meta-wrapper.
/// Sessions must be bound via `session.bind` first; there is no implicit default session.
pub(crate) struct SessionRoutedToolHandler {
    tool_id: ToolId,
    desc: ToolDescription,
    schema: Option<Value>,
    workspace: WorkspaceHandle,
}
impl SessionRoutedToolHandler {
    pub(crate) fn new(
        name: String,
        desc: ToolDescription,
        schema: Option<Value>,
        workspace: WorkspaceHandle,
    ) -> Result<Self, distill_tool_protocol::IdError> {
        Ok(Self {
            tool_id: ToolId::new(name)?,
            desc,
            schema,
            workspace,
        })
    }
    fn name(&self) -> &str {
        self.tool_id.as_str()
    }
}
/// RAII guard for a tool call's activity accounting. Start fires at stream construction; [`Drop`] completes it.
/// Completion runs whether the stream finishes or the consumer drops it early.
struct CallCompletedGuard {
    tracker: Arc<crate::activity::ActivityTracker>,
    call_id: String,
    session_id: Option<String>,
    outcome: distill_session_events::ToolOutcome,
}
impl CallCompletedGuard {
    fn new(
        tracker: Arc<crate::activity::ActivityTracker>,
        call_id: String,
        session_id: Option<String>,
    ) -> Self {
        Self {
            tracker,
            call_id,
            session_id,
            outcome: distill_session_events::ToolOutcome::Cancelled,
        }
    }
    fn set_outcome(&mut self, outcome: distill_session_events::ToolOutcome) {
        self.outcome = outcome;
    }
}
impl Drop for CallCompletedGuard {
    fn drop(&mut self) {
        self.tracker
            .tool_call_completed(&self.call_id, self.session_id.as_deref(), self.outcome);
    }
}
#[async_trait]
impl ToolServerHandler for SessionRoutedToolHandler {
    fn tool_id(&self) -> ToolId {
        self.tool_id.clone()
    }
    fn description(&self) -> ToolDescription {
        self.desc.clone()
    }
    fn input_schema(&self) -> Option<Value> {
        self.schema.clone()
    }
    async fn handle_call(&self, ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        let tool_id = self.tool_id();
        let hub_session = ctx
            .extensions
            .get::<distill_tool_runtime::SessionContext>()
            .map(|s| s.0.clone());
        let tracker = &self.workspace.shared.activity_tracker;
        if tracker.is_draining() {
            return terminal_only(Err(ToolError::new(
                ToolErrorKind::TerminalError,
                "workspace is draining — no new tool calls accepted",
            )));
        }
        let session_id = match &hub_session {
            Some(sid) => sid.as_str(),
            None => {
                return terminal_only(Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    "tool_call_request missing session_id",
                )));
            }
        };
        let session = match self.workspace.session(session_id) {
            Some(s) => s,
            None => {
                return terminal_only(Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    format!("session not bound: {session_id}"),
                )));
            }
        };
        let call_id = ctx.call_id.to_string();
        if self.workspace.shared.tool_approval == crate::permission::ToolApprovalGate::Enforced
            && let Err(denied) = crate::permission::approve_hub_call(
                &self.workspace,
                &session,
                self.name(),
                &call_id,
                &args,
            )
            .await
        {
            return terminal_only(Err(denied));
        }
        let toolset = session.toolset();
        tracing::debug!(
            tool = %self.name(),
            call_id = %call_id,
            session = %session_id,
            "dispatching tool call"
        );
        tracker.tool_call_started(&call_id, self.name(), hub_session.as_deref());
        let virt = session.path_virtualization().cloned();
        let args = match &virt {
            Some(v) => v.rewrite_json_inbound(args),
            None => args,
        };
        let inner = toolset.call_streaming(self.name(), args, &call_id, None);
        let tracker = self.workspace.shared.activity_tracker.clone();
        let name = self.name().to_owned();
        let session_label = session_id.to_owned();
        let guard = CallCompletedGuard::new(tracker, call_id, Some(session_label.clone()));
        Box::pin(async_stream::stream! {
            use futures::StreamExt;
            let mut _guard = guard;
            let mut inner = inner;
            while let Some(item) = inner.next().await {
                match item {
                    ToolStreamItem::Progress(p) => {
                        let p = match &virt {
                            Some(v) => v.rewrite_progress(p),
                            None => p,
                        };
                        yield ToolStreamItem::Progress(p);
                    }
                    ToolStreamItem::Terminal(Ok(run_result)) => {
                        _guard.set_outcome(distill_session_events::ToolOutcome::Success);
                        let output = run_result.into_typed_tool_output(tool_id);
                        let output = match &virt {
                            Some(v) => v.rewrite_typed_output(output),
                            None => output,
                        };
                        yield ToolStreamItem::Terminal(Ok(output));
                        return;
                    }
                    ToolStreamItem::Terminal(Err(e)) => {
                        tracing::error!(
                            tool = %name,
                            session = %session_label,
                            error = %e,
                            kind = %e.variant_name(),
                            "tool call failed"
                        );
                        _guard.set_outcome(distill_session_events::ToolOutcome::Error);
                        let e = match &virt {
                            Some(v) => v.rewrite_error(e),
                            None => e,
                        };
                        yield ToolStreamItem::Terminal(Err(e));
                        return;
                    }
                }
            }
            yield ToolStreamItem::Terminal(Err(ToolError::new(
                ToolErrorKind::TerminalError,
                "tool stream ended without a terminal",
            )));
        })
    }
}
/// Each remote tool gets a `hub:`-prefixed id (no collision with baseline/MCP), `kind: None`, and a bare `name_override`.
/// Remote-origin `kind: None` tools are dropped under non-`All` modes, matching MCP; only the main session's `All` keeps them.
pub(crate) fn hub_tool_ids_to_tool_configs(tool_ids: &[ToolId]) -> Vec<ToolConfig> {
    if !tool_ids.is_empty() {
        tracing::info!(
            count = tool_ids.len(),
            tools = ?tool_ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            "Registering remote tools"
        );
    }
    tool_ids
        .iter()
        .map(|id| {
            let name = id.as_str().to_owned();
            let mut tc = ToolConfig::from_id(format!("hub:{name}"));
            tc.name_override = Some(name);
            tc
        })
        .collect()
}
/// Apply a `ToolsChanged` notification to the current remote tools snapshot.
///
/// Returns the new snapshot.
pub(crate) fn apply_tools_changed(
    current: &[ToolConfig],
    added: &[ToolId],
    removed: &[ToolId],
    updated: &[ToolId],
) -> Vec<ToolConfig> {
    let evicted: std::collections::HashSet<String> = removed
        .iter()
        .chain(updated.iter())
        .map(|id| format!("hub:{}", id.as_str()))
        .collect();
    let mut new_tools: Vec<ToolConfig> = current
        .iter()
        .filter(|t| !evicted.contains(&t.id))
        .cloned()
        .collect();
    let mut to_add = Vec::with_capacity(added.len() + updated.len());
    to_add.extend_from_slice(added);
    to_add.extend_from_slice(updated);
    let added_configs = hub_tool_ids_to_tool_configs(&to_add);
    let existing_ids: std::collections::HashSet<String> =
        new_tools.iter().map(|t| t.id.clone()).collect();
    for tc in added_configs {
        if !existing_ids.contains(&tc.id) {
            new_tools.push(tc);
        }
    }
    new_tools
}
fn parse_server_id(id: &str) -> Result<distill_tool_protocol::ServerId, ClientError> {
    distill_tool_protocol::ServerId::new(id)
        .map_err(|e| ClientError::InvalidConfig(format!("invalid server_id {id:?}: {e}")))
}
/// Map a [`ClientError`] into a [`WorkspaceError::HubError`].
pub(crate) fn client_error_to_workspace(err: ClientError) -> WorkspaceError {
    match err {
        ClientError::HandshakeAuthFailed { status, refusal } => {
            WorkspaceError::HubRefused { status, refusal }
        }
        other => WorkspaceError::HubError(other.to_string()),
    }
}
/// Map a server connection failure into a [`WorkspaceResult`].
pub(crate) fn hub_result<T>(result: Result<T, ClientError>) -> WorkspaceResult<T> {
    result.map_err(client_error_to_workspace)
}
#[cfg(test)]
mod tests {
    use super::*;
    use distill_tools::types::tool::ToolKind;
    #[test]
    fn hub_tool_ids_to_tool_configs_basic() {
        let ids = vec![
            ToolId::new("read_file").unwrap(),
            ToolId::new("web_search").unwrap(),
        ];
        let configs = hub_tool_ids_to_tool_configs(&ids);
        assert_eq!(configs.len(), 2);
        let [cfg0, cfg1] = configs.as_slice() else {
            panic!("expected two configs: {configs:?}");
        };
        assert_eq!(cfg0.id, "hub:read_file");
        assert_eq!(cfg0.name_override.as_deref(), Some("read_file"));
        assert_eq!(cfg0.kind, None::<ToolKind>);
        assert_eq!(cfg1.id, "hub:web_search");
        assert_eq!(cfg1.name_override.as_deref(), Some("web_search"));
    }
    #[test]
    fn hub_tool_ids_to_tool_configs_empty() {
        let configs = hub_tool_ids_to_tool_configs(&[]);
        assert!(configs.is_empty());
    }
    #[test]
    fn apply_tools_changed_adds_and_removes() {
        let initial = hub_tool_ids_to_tool_configs(&[
            ToolId::new("tool_a").unwrap(),
            ToolId::new("tool_b").unwrap(),
        ]);
        let result = apply_tools_changed(
            &initial,
            &[ToolId::new("tool_c").unwrap()],
            &[ToolId::new("tool_a").unwrap()],
            &[],
        );
        let ids: Vec<&str> = result.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"hub:tool_b"));
        assert!(ids.contains(&"hub:tool_c"));
        assert!(!ids.contains(&"hub:tool_a"));
    }
    #[test]
    fn apply_tools_changed_updates_replace() {
        let initial = hub_tool_ids_to_tool_configs(&[ToolId::new("tool_a").unwrap()]);
        let result = apply_tools_changed(&initial, &[], &[], &[ToolId::new("tool_a").unwrap()]);
        assert_eq!(result.len(), 1);
        let Some(first) = result.first() else {
            panic!("expected one config: {result:?}");
        };
        assert_eq!(first.id, "hub:tool_a");
    }
    use futures::StreamExt;
    use distill_tool_runtime::{SessionContext, ToolCallId};
    fn make_handler(workspace: &WorkspaceHandle, tool_name: &str) -> SessionRoutedToolHandler {
        SessionRoutedToolHandler::new(
            tool_name.to_owned(),
            ToolDescription::new(tool_name.to_owned(), String::new()),
            None,
            workspace.clone(),
        )
        .expect("test tool name is a valid ToolId")
    }
    #[tokio::test]
    async fn handler_construction_rejects_invalid_tool_name_without_panic() {
        let handle = crate::handle::tests::make_handle();
        let err = SessionRoutedToolHandler::new(
            "not a tool id!".to_owned(),
            ToolDescription::new("not a tool id!".to_owned(), String::new()),
            None,
            handle.clone(),
        );
        assert!(
            err.is_err(),
            "invalid name must be rejected at construction"
        );
    }
    #[tokio::test]
    async fn handler_tool_id_round_trips_the_validated_name() {
        let handle = crate::handle::tests::make_handle();
        let handler = make_handler(&handle, "read_file");
        assert_eq!(handler.tool_id().as_str(), "read_file");
    }
    #[test]
    fn parse_server_id_maps_invalid_id_to_invalid_config_error() {
        for bad in ["auto:tool:x", ""] {
            assert!(
                matches!(parse_server_id(bad), Err(ClientError::InvalidConfig(_))),
                "server_id {bad:?} must map to InvalidConfig"
            );
        }
        assert!(parse_server_id("sess-abc123").is_ok());
    }
    fn make_ctx(session_id: &str) -> (ToolCallContext, String) {
        let mut ctx = ToolCallContext::new(ToolCallId::new_v7());
        ctx.insert(SessionContext(session_id.to_owned()));
        let call_id = ctx.call_id.to_string();
        (ctx, call_id)
    }
    #[tokio::test]
    async fn handle_call_is_passthrough_zero_progress_one_terminal() {
        let handle = crate::handle::tests::make_handle();
        let handler = make_handler(&handle, "read_file");
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler
            .handle_call(
                ctx,
                serde_json::json!({ "target_file": "does-not-exist.txt" }),
            )
            .await;
        let items: Vec<_> = stream.collect().await;
        let progress = items
            .iter()
            .filter(|i| matches!(i, ToolStreamItem::Progress(_)))
            .count();
        let terminal = items
            .iter()
            .filter(|i| matches!(i, ToolStreamItem::Terminal(_)))
            .count();
        assert_eq!(progress, 0, "gate-off pass-through must emit zero Progress");
        assert_eq!(terminal, 1, "must emit exactly one Terminal");
        assert!(matches!(items.last(), Some(ToolStreamItem::Terminal(_))));
    }
    /// The gate is consulted from `handle_call` when the handle says `Enforced`: a guarded tool
    /// with no hub transport is denied before dispatch, a read still runs. Pins the check itself,
    /// not the gate's internals (those are `permission::hub_gate::tests`).
    #[tokio::test]
    async fn handle_call_on_an_enforced_handle_denies_a_guarded_tool_without_a_transport() {
        let handle = crate::handle::tests::make_enforced_handle();
        handle
            .create_session_with_config(
                "gate",
                None,
                Some(distill_agent::workspace_distill_toolset()),
                crate::capability::CapabilityMode::All,
                None,
                false,
            )
            .expect("daemon toolset session");
        let (ctx, _) = make_ctx("gate");
        let items: Vec<_> = make_handler(&handle, "run_terminal_command")
            .handle_call(
                ctx,
                serde_json::json!({ "command": "cargo build", "description": "build" }),
            )
            .await
            .collect()
            .await;
        match items.as_slice() {
            [ToolStreamItem::Terminal(Err(denied))] => {
                assert_eq!(
                    distill_tool_runtime::ToolErrorKind::PermissionDenied,
                    denied.kind
                );
            }
            other => panic!("expected one PermissionDenied terminal, got {other:?}"),
        }
        let (ctx, _) = make_ctx("gate");
        let items: Vec<_> = make_handler(&handle, "read_file")
            .handle_call(ctx, serde_json::json!({ "target_file": "missing.txt" }))
            .await
            .collect()
            .await;
        assert!(
            matches!(items.as_slice(), [ToolStreamItem::Terminal(Ok(_))]),
            "a read runs unasked (a missing file is the tool's own answer): {items:?}"
        );
    }
    #[tokio::test]
    async fn handle_call_terminal_matches_non_streaming_call() {
        let handle = crate::handle::tests::make_handle();
        let session = handle.session("main").expect("main session present");
        let toolset = session.toolset();
        let args = serde_json::json!({ "target_file": "missing-file.txt" });
        let reference = toolset
            .call("read_file", args.clone(), "ref-call", None)
            .await;
        let handler = make_handler(&handle, "read_file");
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler.handle_call(ctx, args).await;
        let items: Vec<_> = stream.collect().await;
        let terminal = items
            .into_iter()
            .find_map(|i| match i {
                ToolStreamItem::Terminal(t) => Some(t),
                ToolStreamItem::Progress(_) => None,
            })
            .expect("a terminal item");
        match reference {
            Ok(run_result) => {
                let reference_value = serde_json::to_value(&run_result).unwrap();
                let typed = terminal.expect("streaming terminal must be Ok when call() is Ok");
                assert_eq!(
                    typed.value, reference_value,
                    "streaming terminal must serialize identically to the non-streaming path"
                );
                assert_eq!(
                    typed.model_output,
                    vec![distill_tool_runtime::ContentBlock::Text {
                        text: run_result.prompt_text.clone(),
                    }],
                    "model_output must be the prompt_text, not a JSON dump"
                );
            }
            Err(_) => {
                assert!(
                    terminal.is_err(),
                    "streaming terminal must be Err when call() is Err"
                );
            }
        }
    }
    #[tokio::test]
    async fn handle_call_draining_returns_single_terminal_error() {
        let handle = crate::handle::tests::make_handle();
        let tracker = handle.activity_tracker().clone();
        tracker.set_draining();
        let handler = make_handler(&handle, "read_file");
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler
            .handle_call(ctx, serde_json::json!({ "target_file": "x.txt" }))
            .await;
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1, "draining yields exactly one item");
        let Some(item) = items.first() else {
            panic!("expected one stream item: {items:?}");
        };
        match item {
            ToolStreamItem::Terminal(Err(e)) => {
                assert!(e.to_string().contains("draining"), "got: {e}");
            }
            ToolStreamItem::Terminal(Ok(_)) => {
                panic!("expected Terminal(Err), got Terminal(Ok)")
            }
            ToolStreamItem::Progress(_) => panic!("expected Terminal(Err), got Progress"),
        }
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            0,
            "draining must not start a tool call"
        );
    }
    #[tokio::test]
    async fn handle_call_guard_completes_on_early_drop() {
        let handle = crate::handle::tests::make_handle();
        let tracker = handle.activity_tracker().clone();
        let handler = make_handler(&handle, "read_file");
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler
            .handle_call(ctx, serde_json::json!({ "target_file": "x.txt" }))
            .await;
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            1,
            "tool_call_started fires at stream construction"
        );
        drop(stream);
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            0,
            "dropping the stream must run the RAII completion guard"
        );
    }
    use distill_tools::types::tool_metadata::ToolMetadata as XaiToolMetadata;
    #[derive(Debug)]
    struct GateStreamingStub;
    impl XaiToolMetadata for GateStreamingStub {
        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }
        fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
            distill_tools::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "gate streaming stub"
        }
    }
    impl distill_tool_runtime::Tool for GateStreamingStub {
        type Args = serde_json::Value;
        type Output = String;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("gate_streaming_stub").expect("valid tool id")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("gate_streaming_stub", "gate streaming stub")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> Result<String, distill_tool_runtime::ToolError> {
            Ok("terminal-value".into())
        }
        async fn execute(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> distill_tool_runtime::ToolStream<String> {
            use distill_tool_runtime::{ToolProgress, ToolStreamItem};
            Box::pin(futures::stream::iter(vec![
                ToolStreamItem::Progress(ToolProgress::Text {
                    text: "stub-progress-1".into(),
                }),
                ToolStreamItem::Progress(ToolProgress::Text {
                    text: "stub-progress-2".into(),
                }),
                ToolStreamItem::Terminal(Ok("stub-terminal".into())),
            ]))
        }
    }
    fn register_gate_stub(handle: &WorkspaceHandle, tool_name: &str) {
        let session = handle.session("main").expect("main session present");
        let toolset = session.toolset();
        toolset
            .register_tool(
                tool_name.to_owned(),
                GateStreamingStub,
                Some(serde_json::json!({"type": "object", "properties": {}})),
            )
            .expect("register_tool must succeed");
    }
    async fn drain_counts<T>(mut stream: distill_tool_runtime::ToolStream<T>) -> (usize, usize, bool) {
        let mut progress = 0;
        let mut terminal = 0;
        let mut last_is_terminal = false;
        while let Some(item) = stream.next().await {
            match item {
                ToolStreamItem::Progress(_) => {
                    progress += 1;
                    last_is_terminal = false;
                }
                ToolStreamItem::Terminal(_) => {
                    terminal += 1;
                    last_is_terminal = true;
                }
            }
        }
        (progress, terminal, last_is_terminal)
    }
    #[tokio::test]
    async fn handle_call_forwards_inner_streaming_tool_progress() {
        let handle = crate::handle::tests::make_handle();
        register_gate_stub(&handle, "gate_streamer_forward");
        let handler = make_handler(&handle, "gate_streamer_forward");
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler.handle_call(ctx, serde_json::json!({})).await;
        let (progress, terminal, last_is_terminal) = drain_counts(stream).await;
        assert_eq!(
            progress, 2,
            "workspace must forward both stub Progress items end-to-end"
        );
        assert_eq!(terminal, 1, "exactly one Terminal");
        assert!(last_is_terminal, "Terminal must be the final item");
    }
    #[tokio::test]
    async fn handle_call_preserves_bash_chat_completion_output() {
        let handle = crate::handle::tests::make_handle();
        crate::handle::tests::register_bash_cco_stub(&handle);
        let handler = make_handler(&handle, crate::handle::tests::BASH_CCO_STUB_NAME);
        let (ctx, _call_id) = make_ctx("main");
        let stream = handler.handle_call(ctx, serde_json::json!({})).await;
        let typed = crate::handle::tests::drain_terminal_ok(stream).await;
        crate::handle::tests::assert_bash_cco_terminal(&typed);
    }
    use crate::capability::CapabilityMode;
    use crate::session::tool_config::test_support::tc;
    use std::time::Duration;
    use distill_tools::notification::types::{ToolNotification, ToolNotificationHandle};
    use distill_tools::registry::types::ToolServerConfig;
    fn bg_config() -> ToolServerConfig {
        ToolServerConfig {
            tools: vec![
                tc("Distill:run_terminal_cmd", Some(ToolKind::Execute)),
                tc(
                    "Distill:get_task_output",
                    Some(ToolKind::BackgroundTaskAction),
                ),
                tc("Distill:kill_task", Some(ToolKind::KillTaskAction)),
                tc("Distill:monitor", Some(ToolKind::Monitor)),
            ],
            behavior_preset: None,
        }
    }
    fn install_activity_feed(handle: &WorkspaceHandle) {
        let (sink, rx) = ToolNotificationHandle::channel();
        handle
            .shared()
            .activity_notify_handle
            .store(Arc::new(Some(sink)));
        tokio::spawn(crate::handle::run_activity_feed(
            handle.activity_tracker().clone(),
            rx,
        ));
    }
    fn make_bg_handle_with_config(cfg: ToolServerConfig) -> WorkspaceHandle {
        let handle = WorkspaceHandle::for_test();
        install_activity_feed(&handle);
        handle
            .create_session_with_config("main", None, Some(cfg), CapabilityMode::All, None, false)
            .expect("create main session with background tools");
        handle
    }
    fn make_bg_tracking_handle() -> WorkspaceHandle {
        make_bg_handle_with_config(bg_config())
    }
    async fn wait_until(
        tracker: &crate::activity::ActivityTracker,
        pred: impl Fn(&distill_tool_protocol::ToolServerStatusPayload) -> bool,
        timeout: Duration,
    ) -> distill_tool_protocol::ToolServerStatusPayload {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snap = tracker.snapshot();
            if pred(&snap) || tokio::time::Instant::now() >= deadline {
                return snap;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    async fn run_tool_in_session(handle: &WorkspaceHandle, session: &str, tool: &str, args: Value) {
        let handler = make_handler(handle, tool);
        let (ctx, _call_id) = make_ctx(session);
        let mut stream = handler.handle_call(ctx, args).await;
        while let Some(item) = stream.next().await {
            if matches!(item, ToolStreamItem::Terminal(_)) {
                break;
            }
        }
    }
    fn bg_started_notif(task_id: &str) -> ToolNotification {
        use distill_tools::notification::types::{
            BashExecutionBackgrounded, BashNotificationBase,
        };
        ToolNotification::BashExecutionBackgrounded(BashExecutionBackgrounded {
            base: BashNotificationBase {
                tool_call_id: task_id.to_owned(),
                command: "sleep 1".to_owned(),
                output: Vec::new(),
                total_bytes: 0,
                truncated: false,
                cwd: std::path::PathBuf::from("/tmp"),
            },
            output_file: std::path::PathBuf::from("/tmp/x.log"),
            task_id: task_id.to_owned(),
            monitor_description: None,
            description: None,
        })
    }
    fn task_completed_notif(task_id: &str) -> ToolNotification {
        use distill_tools::computer::types::{TaskKind, TaskSnapshot};
        ToolNotification::TaskCompleted(TaskSnapshot {
            task_id: task_id.to_owned(),
            command: "sleep 1".to_owned(),
            display_command: None,
            cwd: "/tmp".to_owned(),
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: Some(std::time::SystemTime::UNIX_EPOCH),
            output: String::new(),
            output_file: std::path::PathBuf::from("/tmp/x.log"),
            truncated: false,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind: TaskKind::Bash,
            block_waited: false,
            explicitly_killed: false,
            kill_result_delivered: false,
            owner_session_id: None,
            description: None,
            is_backgrounded: false,
            output_total_bytes: 0,
        })
    }
    fn started_id(n: &ToolNotification) -> &str {
        match n {
            ToolNotification::BashExecutionBackgrounded(b) => &b.task_id,
            _ => "",
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backgrounded_bash_increments_then_decrements_through_real_wiring() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        run_tool_in_session(
                &handle,
                "main",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 2", "description": "test", "is_background": true }),
            )
            .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1 && s.idle_since_ms.is_none(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(busy.background_tasks, 1, "running bg bash must increment");
        assert!(busy.idle_since_ms.is_none(), "idle withheld while bg runs");
        let idle = wait_until(
            &tracker,
            |s| s.background_tasks == 0 && s.idle_since_ms.is_some(),
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(
            idle.background_tasks, 0,
            "TaskCompleted must decrement to zero"
        );
        assert!(
            idle.idle_since_ms.is_some(),
            "idle restored when nothing runs"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_background_on_timeout_increments_then_decrements_through_real_wiring() {
        let mut cfg = bg_config();
        let Some(tool) = cfg.tools.first_mut() else {
            panic!("expected at least one tool in bg_config");
        };
        tool.params = serde_json::json!({
            "enabled_background": true,
            "auto_background_on_timeout": true,
        })
        .as_object()
        .cloned();
        let handle = make_bg_handle_with_config(cfg);
        let tracker = handle.activity_tracker().clone();
        run_tool_in_session(
            &handle,
            "main",
            "run_terminal_cmd",
            serde_json::json!({ "command": "sleep 2", "description": "test", "timeout": 300 }),
        )
        .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1 && s.idle_since_ms.is_none(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            busy.background_tasks, 1,
            "auto-backgrounded task must increment"
        );
        assert!(busy.idle_since_ms.is_none());
        let idle = wait_until(
            &tracker,
            |s| s.background_tasks == 0 && s.idle_since_ms.is_some(),
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(
            idle.background_tasks, 0,
            "auto-bg completion must decrement (matching task_id)"
        );
        assert!(idle.idle_since_ms.is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_increments_then_decrements_through_real_wiring() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        run_tool_in_session(
            &handle,
            "main",
            "monitor",
            serde_json::json!({ "command": "sleep 2", "description": "test monitor" }),
        )
        .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(busy.background_tasks, 1, "a started monitor must increment");
        let idle = wait_until(
            &tracker,
            |s| s.background_tasks == 0 && s.idle_since_ms.is_some(),
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(
            idle.background_tasks, 0,
            "monitor completion must decrement (the previously-lost decrement)"
        );
        assert!(idle.idle_since_ms.is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_background_tasks_track_independently() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        run_tool_in_session(
                &handle,
                "main",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 2", "description": "test", "is_background": true }),
            )
            .await;
        run_tool_in_session(
                &handle,
                "main",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 5", "description": "test", "is_background": true }),
            )
            .await;
        let two = wait_until(
            &tracker,
            |s| s.background_tasks == 2,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            two.background_tasks, 2,
            "both concurrent bg tasks must count"
        );
        assert!(two.idle_since_ms.is_none(), "not idle with two bg tasks");
        let one = wait_until(
            &tracker,
            |s| s.background_tasks == 1 && s.idle_since_ms.is_none(),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(
            one.background_tasks, 1,
            "one finishing must not zero the counter"
        );
        assert!(
            one.idle_since_ms.is_none(),
            "still not idle while one remains"
        );
        let zero = wait_until(
            &tracker,
            |s| s.background_tasks == 0 && s.idle_since_ms.is_some(),
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(zero.background_tasks, 0);
        assert!(
            zero.idle_since_ms.is_some(),
            "idle restored only after the last ends"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forked_child_background_task_feeds_tracker() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        let mut cfg = crate::config::AgentSessionConfig::new("child");
        cfg.parent_session_id = Some("main".to_owned());
        cfg.capability_mode = CapabilityMode::All;
        cfg.tool_config = Some(bg_config());
        handle.fork_session(cfg).await.expect("fork child session");
        run_tool_in_session(
                &handle,
                "child",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 2", "description": "test", "is_background": true }),
            )
            .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            busy.background_tasks, 1,
            "a forked subagent's bg task must feed the connection-level tracker"
        );
        let zero = wait_until(
            &tracker,
            |s| s.background_tasks == 0,
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(
            zero.background_tasks, 0,
            "the fork's bg task must decrement on completion"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compose_session_notification_handle_covers_all_branches() {
        let handle = WorkspaceHandle::for_test();
        let shared = handle.shared();
        assert!(shared.compose_session_notification_handle(None).is_none());
        let (sys, mut sys_rx) = ToolNotificationHandle::channel();
        shared
            .compose_session_notification_handle(Some(sys))
            .expect("system-only sink")
            .send(bg_started_notif("sys-only"));
        assert!(matches!(sys_rx.try_recv(), Ok(n) if started_id(&n) == "sys-only"));
        let (activity, mut activity_rx) = ToolNotificationHandle::channel();
        shared
            .activity_notify_handle
            .store(Arc::new(Some(activity)));
        shared
            .compose_session_notification_handle(None)
            .expect("activity-only sink")
            .send(bg_started_notif("act-only"));
        assert!(matches!(activity_rx.try_recv(), Ok(n) if started_id(&n) == "act-only"));
        let (sys2, mut sys2_rx) = ToolNotificationHandle::channel();
        shared
            .compose_session_notification_handle(Some(sys2))
            .expect("tee sink")
            .send(bg_started_notif("both"));
        assert!(
            matches!(activity_rx.try_recv(), Ok(n) if started_id(&n) == "both"),
            "tee must deliver to the activity (tracker) leg"
        );
        assert!(
            matches!(sys2_rx.try_recv(), Ok(n) if started_id(&n) == "both"),
            "tee must deliver to the system.notify leg"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn activity_feed_drains_and_dedups_by_task_id() {
        let tracker = Arc::new(crate::activity::ActivityTracker::new());
        let (sink, rx) = ToolNotificationHandle::channel();
        let feed = tokio::spawn(crate::handle::run_activity_feed(tracker.clone(), rx));
        sink.send(bg_started_notif("dup"));
        sink.send(bg_started_notif("dup"));
        let one = wait_until(
            &tracker,
            |s| s.background_tasks == 1,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            one.background_tasks, 1,
            "duplicate started must not double-count"
        );
        sink.send(bg_started_notif("other"));
        let two = wait_until(
            &tracker,
            |s| s.background_tasks == 2,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(two.background_tasks, 2);
        sink.send(task_completed_notif("dup"));
        sink.send(task_completed_notif("other"));
        let zero = wait_until(
            &tracker,
            |s| s.background_tasks == 0,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            zero.background_tasks, 0,
            "completions drain via run_activity_feed"
        );
        drop(sink);
        feed.await
            .expect("activity feed must exit cleanly once senders drop");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_tool_config_preserves_tracker_feed() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        handle
            .update_tool_config("main", "main", bg_config())
            .await
            .expect("update_tool_config rebuilds the toolset");
        run_tool_in_session(
                &handle,
                "main",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 2", "description": "test", "is_background": true }),
            )
            .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            busy.background_tasks, 1,
            "a bg task after update_tool_config must still feed the tracker"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_resolve_all_sessions_preserves_tracker_feed() {
        let handle = make_bg_tracking_handle();
        let tracker = handle.activity_tracker().clone();
        let rebuilt = handle
            .shared()
            .re_resolve_all_sessions("test_preserves_feed", true)
            .await;
        assert!(rebuilt >= 1, "the main session must be re-resolved");
        run_tool_in_session(
                &handle,
                "main",
                "run_terminal_cmd",
                serde_json::json!({ "command": "sleep 2", "description": "test", "is_background": true }),
            )
            .await;
        let busy = wait_until(
            &tracker,
            |s| s.background_tasks == 1,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            busy.background_tasks, 1,
            "a bg task after re_resolve_all_sessions must still feed the tracker"
        );
    }
    #[derive(Clone, Debug)]
    struct PathEchoStub {
        received: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }
    impl XaiToolMetadata for PathEchoStub {
        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }
        fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
            distill_tools::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "path echo stub"
        }
    }
    impl distill_tool_runtime::Tool for PathEchoStub {
        type Args = serde_json::Value;
        type Output = serde_json::Value;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("path_echo").expect("valid tool id")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("path_echo", "path echo stub")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            input: serde_json::Value,
        ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
            *self.received.lock().expect("path echo lock") = Some(input.clone());
            Ok(serde_json::json!({
                "received": input.get("path").cloned().unwrap_or(serde_json::Value::Null),
                "guest": "/workspace/conv-abc/out.txt",
            }))
        }
        async fn execute(
            &self,
            ctx: distill_tool_runtime::ToolCallContext,
            input: serde_json::Value,
        ) -> distill_tool_runtime::ToolStream<serde_json::Value> {
            use distill_tool_runtime::{ToolProgress, ToolStreamItem};
            let value = self.run(ctx, input).await;
            Box::pin(futures::stream::iter(vec![
                ToolStreamItem::Progress(ToolProgress::Text {
                    text: "wrote /workspace/conv-abc/out.txt".into(),
                }),
                ToolStreamItem::Terminal(value),
            ]))
        }
    }
    fn register_path_echo(
        handle: &WorkspaceHandle,
        session_id: &str,
    ) -> Arc<std::sync::Mutex<Option<serde_json::Value>>> {
        let received = Arc::new(std::sync::Mutex::new(None));
        let session = handle.session(session_id).expect("session present");
        session
            .toolset()
            .register_tool(
                "path_echo".to_owned(),
                PathEchoStub {
                    received: received.clone(),
                },
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                })),
            )
            .expect("register path_echo");
        received
    }
    async fn drain_terminal(
        stream: distill_tool_runtime::ToolStream<distill_tool_runtime::TypedToolOutput>,
    ) -> (
        Vec<distill_tool_runtime::ToolProgress>,
        distill_tool_runtime::TypedToolOutput,
    ) {
        use futures::StreamExt;
        let mut progress = Vec::new();
        let mut inner = stream;
        while let Some(item) = inner.next().await {
            match item {
                ToolStreamItem::Progress(p) => progress.push(p),
                ToolStreamItem::Terminal(Ok(out)) => return (progress, out),
                ToolStreamItem::Terminal(Err(e)) => {
                    panic!("expected Ok terminal, got {e}")
                }
            }
        }
        panic!("tool stream ended without a terminal")
    }
    #[tokio::test]
    async fn handle_call_virtualizes_inbound_outbound_and_progress() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt", None)
            .expect("create virt session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        let received = register_path_echo(&handle, "virt");
        let handler = make_handler(&handle, "path_echo");
        let (ctx, _call_id) = make_ctx("virt");
        let (progress, out) = drain_terminal(
            handler
                .handle_call(ctx, serde_json::json!({ "path": "/workspace/foo.txt" }))
                .await,
        )
        .await;
        let inbound = received.lock().expect("lock");
        assert_eq!(
            inbound.as_ref().and_then(|v| v.get("path")),
            Some(&serde_json::json!("/workspace/conv-abc/foo.txt")),
            "inbound /workspace must resolve to the session root"
        );
        let dumped = out.value.to_string();
        assert!(
            dumped.contains("/workspace/out.txt"),
            "outbound must rewrite the guest path: {dumped}"
        );
        assert!(
            !dumped.contains("/workspace/conv-abc/"),
            "outbound must not leak the real root: {dumped}"
        );
        match progress.first() {
            Some(distill_tool_runtime::ToolProgress::Text { text }) => {
                assert_eq!(text, "wrote /workspace/out.txt");
            }
            other => panic!("expected rewritten progress, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn handle_call_virtualizes_inbound_artifacts_alias() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-art", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        let received = register_path_echo(&handle, "virt-art");
        let handler = make_handler(&handle, "path_echo");
        let (ctx, _call_id) = make_ctx("virt-art");
        let (_progress, _out) = drain_terminal(
            handler
                .handle_call(
                    ctx,
                    serde_json::json!({ "path": "/workspace/artifacts/legacy.rs" }),
                )
                .await,
        )
        .await;
        assert_eq!(
            received
                .lock()
                .expect("lock")
                .as_ref()
                .and_then(|v| v.get("path")),
            Some(&serde_json::json!("/workspace/conv-abc/legacy.rs")),
            "inbound /workspace/artifacts must alias the session root"
        );
    }
    #[tokio::test]
    async fn handle_call_leaves_outside_paths_unchanged() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-out", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        let received = register_path_echo(&handle, "virt-out");
        let handler = make_handler(&handle, "path_echo");
        let (ctx, _call_id) = make_ctx("virt-out");
        let (_progress, out) = drain_terminal(
            handler
                .handle_call(ctx, serde_json::json!({ "path": "/tmp/secret" }))
                .await,
        )
        .await;
        assert_eq!(
            received
                .lock()
                .expect("lock")
                .as_ref()
                .and_then(|v| v.get("path")),
            Some(&serde_json::json!("/tmp/secret")),
            "paths outside the session root must not be rewritten inbound"
        );
        let dumped = out.value.to_string();
        assert!(
            dumped.contains("/workspace/out.txt"),
            "outbound still rewrites the in-root guest path: {dumped}"
        );
        assert!(
            !dumped.contains("/workspace/conv-abc/"),
            "outbound must not leak the real root: {dumped}"
        );
    }
    #[tokio::test]
    async fn handle_call_without_session_root_is_identity() {
        let handle = crate::handle::tests::make_handle();
        let received = register_path_echo(&handle, "main");
        let handler = make_handler(&handle, "path_echo");
        let (ctx, _call_id) = make_ctx("main");
        let (_progress, out) = drain_terminal(
            handler
                .handle_call(ctx, serde_json::json!({ "path": "/workspace/foo.txt" }))
                .await,
        )
        .await;
        assert_eq!(
            received
                .lock()
                .expect("lock")
                .as_ref()
                .and_then(|v| v.get("path")),
            Some(&serde_json::json!("/workspace/foo.txt")),
            "no session_root must not rewrite inbound"
        );
        let dumped = out.value.to_string();
        assert!(
            dumped.contains("/workspace/conv-abc/out.txt"),
            "no session_root must not rewrite outbound: {dumped}"
        );
    }
    #[tokio::test]
    async fn handle_call_clips_inbound_walk_outs_and_leaves_tmp() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-walk", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        let received = register_path_echo(&handle, "virt-walk");
        let handler = make_handler(&handle, "path_echo");
        for (inbound, expected, why) in [
            (
                "/workspace/artifacts/../other-conv/secret",
                "/workspace/conv-abc",
                "artifacts .. walk-out must clip to real_root",
            ),
            (
                "/workspace/conv-abc/../other",
                "/workspace/conv-abc",
                "already-guest .. walk-out must clip to real_root",
            ),
            (
                "/workspace/../other-conv/secret",
                "/workspace/conv-abc",
                "visible-root .. walk-out must clip to real_root",
            ),
            (
                "/tmp/../etc/passwd",
                "/tmp/../etc/passwd",
                "/tmp identity must stay unchanged",
            ),
            (
                "/home/user/../other",
                "/home/user/../other",
                "/home identity must stay unchanged",
            ),
        ] {
            let (ctx, _call_id) = make_ctx("virt-walk");
            let (_progress, _out) = drain_terminal(
                handler
                    .handle_call(ctx, serde_json::json!({ "path": inbound }))
                    .await,
            )
            .await;
            assert_eq!(
                received
                    .lock()
                    .expect("lock")
                    .as_ref()
                    .and_then(|v| v.get("path")),
                Some(&serde_json::json!(expected)),
                "{why}"
            );
        }
    }
    #[tokio::test]
    async fn handle_call_virtualizes_inbound_other_conv_under_visible_root() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-other", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        let received = register_path_echo(&handle, "virt-other");
        let handler = make_handler(&handle, "path_echo");
        let (ctx, _call_id) = make_ctx("virt-other");
        let (_progress, _out) = drain_terminal(
            handler
                .handle_call(
                    ctx,
                    serde_json::json!({ "path": "/workspace/other-conv/file" }),
                )
                .await,
        )
        .await;
        assert_eq!(
            received
                .lock()
                .expect("lock")
                .as_ref()
                .and_then(|v| v.get("path")),
            Some(&serde_json::json!("/workspace/conv-abc/other-conv/file")),
            "inbound /workspace/other-conv must map into this session root"
        );
    }
    #[derive(Debug)]
    struct PathErrorStub;
    impl XaiToolMetadata for PathErrorStub {
        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }
        fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
            distill_tools::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "path error stub"
        }
    }
    impl distill_tool_runtime::Tool for PathErrorStub {
        type Args = serde_json::Value;
        type Output = serde_json::Value;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("path_error").expect("valid tool id")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("path_error", "path error stub")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
            Err(distill_tool_runtime::ToolError::new(
                distill_tool_runtime::ToolErrorKind::Execution,
                "missing /workspace/conv-abc/gone.txt",
            )
            .with_details(serde_json::json!({
                "path": "/workspace/conv-abc/gone.txt"
            })))
        }
    }
    #[tokio::test]
    async fn handle_call_rewrites_tool_error_paths() {
        use futures::StreamExt;
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-err", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        session
            .toolset()
            .register_tool(
                "path_error".to_owned(),
                PathErrorStub,
                Some(serde_json::json!({"type": "object", "properties": {}})),
            )
            .expect("register path_error");
        let handler = make_handler(&handle, "path_error");
        let (ctx, _call_id) = make_ctx("virt-err");
        let stream = handler.handle_call(ctx, serde_json::json!({})).await;
        let mut inner = stream;
        let err = loop {
            match inner.next().await {
                Some(ToolStreamItem::Terminal(Err(e))) => break e,
                Some(ToolStreamItem::Progress(_)) => {}
                Some(ToolStreamItem::Terminal(Ok(_))) => {
                    panic!("expected error terminal")
                }
                None => panic!("stream ended"),
            }
        };
        assert_eq!(err.detail, "missing /workspace/gone.txt");
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("path")),
            Some(&serde_json::json!("/workspace/gone.txt"))
        );
    }
    #[derive(Debug)]
    struct BashCcoPathStub;
    impl XaiToolMetadata for BashCcoPathStub {
        fn kind(&self) -> ToolKind {
            ToolKind::Execute
        }
        fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
            distill_tools::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "bash cco path stub"
        }
    }
    impl distill_tool_runtime::Tool for BashCcoPathStub {
        type Args = serde_json::Value;
        type Output = distill_tools::types::output::ToolOutput;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("bash_cco_path").expect("valid tool id")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("bash_cco_path", "bash cco path stub")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> Result<distill_tools::types::output::ToolOutput, distill_tool_runtime::ToolError>
        {
            let stdout = "/workspace/conv-abc/out.txt";
            Ok(distill_tools::types::output::ToolOutput::Bash(
                distill_tools::types::output::BashOutput {
                    output: stdout.as_bytes().to_vec(),
                    output_for_prompt:
                        distill_tools::types::output::BashOutput::make_output_for_prompt(stdout),
                    exit_code: 0,
                    command: "cat /workspace/conv-abc/out.txt".into(),
                    truncated: false,
                    signal: None,
                    timed_out: false,
                    description: None,
                    current_dir: "/workspace/conv-abc".into(),
                    output_file: String::new(),
                    total_bytes: stdout.len(),
                    output_delta: None,
                    was_bare_echo: false,
                },
            ))
        }
    }
    #[tokio::test]
    async fn handle_call_rewrites_bash_cco_paths() {
        let handle = crate::handle::tests::make_handle();
        let session = handle
            .create_session_with_cwd("virt-cco", None)
            .expect("create session");
        session.set_path_virtualization(
            crate::path_virtualization::PathVirtualization::try_from_session_root(
                "/workspace/conv-abc",
            )
            .expect("valid session root"),
        );
        session
            .toolset()
            .register_tool(
                "bash_cco_path".to_owned(),
                BashCcoPathStub,
                Some(serde_json::json!({"type": "object", "properties": {}})),
            )
            .expect("register bash_cco_path");
        let handler = make_handler(&handle, "bash_cco_path");
        let (ctx, _call_id) = make_ctx("virt-cco");
        let stream = handler.handle_call(ctx, serde_json::json!({})).await;
        let typed = crate::handle::tests::drain_terminal_ok(stream).await;
        let resp = typed
            .chat_completion_output
            .as_ref()
            .expect("bash CCO must be present");
        let cer = resp
            .result
            .as_ref()
            .and_then(|r| r.code_execution_result.as_ref())
            .expect("code_execution_result");
        assert_eq!(cer.stdout, "/workspace/out.txt");
        assert!(
            !cer.stdout.contains("/workspace/conv-abc"),
            "CCO stdout must not leak the real root"
        );
        let dumped = typed.value.to_string();
        assert!(
            !dumped.contains("/workspace/conv-abc"),
            "CCO value JSON must not leak the real root: {dumped}"
        );
    }
}
