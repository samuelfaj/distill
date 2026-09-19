// Modified for Distill by Samuel Fajreldines, 2026.
//! The background MCP init pass: parallel handshakes, one committed outcome per server, and the snapshot refreshes
//! they queue. A server-set change cancels it; `Drop` releases what it built.

use super::*;
use crate::session::mcp_servers::{SharedMcpState, Superseded};

/// The pass's snapshot refresher; drop aborts it so a cancelled pass leaves nothing running.
struct RefreshQueue {
    tx: tokio::sync::mpsc::UnboundedSender<()>,
    task: crate::util::AbortOnDrop,
}

impl RefreshQueue {
    fn start(refresher: SnapshotRefresher) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        // One coalescing task, so the drain never waits on a refresh.
        let task = crate::util::AbortOnDrop(tokio::task::spawn_local(async move {
            while rx.recv().await.is_some() {
                while rx.try_recv().is_ok() {}
                refresher.refresh().await;
            }
        }));
        Self { tx, task }
    }

    fn request(&self) {
        let _ = self.tx.send(());
    }

    async fn finish(self) {
        self.request();
        self.stop().await;
    }

    /// Waits rather than aborts: a refresh's descriptor writers run on blocking threads.
    async fn stop(self) {
        let Self { tx, mut task } = self;
        drop(tx);
        let _ = (&mut task.0).await;
    }
}

struct ServerInfo {
    transport: &'static str,
    target: String,
    scope: &'static str,
}

struct HandshakeSuccess {
    server: String,
    registrations: Vec<crate::session::mcp_servers::McpToolRegistration>,
    elapsed: std::time::Duration,
    timeout_sec: u64,
}

struct HandshakeFailure {
    server: String,
    error: crate::session::mcp_servers::McpError,
    needs_auth: bool,
    elapsed: std::time::Duration,
    timeout_sec: u64,
}

type HandshakeOutcome = Result<HandshakeSuccess, HandshakeFailure>;

#[derive(Default)]
pub(super) struct InitTally {
    succeeded: u32,
    failed: u32,
    auth_required: u32,
    tools_registered: u32,
    failed_servers: Vec<String>,
}

impl InitTally {
    fn add_connected(&mut self, tool_count: u32) {
        self.succeeded += 1;
        self.tools_registered += tool_count;
    }

    fn add_failed(&mut self, server: String, needs_auth: bool) {
        self.failed += 1;
        self.auth_required += u32::from(needs_auth);
        self.failed_servers.push(server);
    }
}

/// What a pass starts from, taken under one lock.
pub(super) struct InitClaim {
    pub(super) configs: Vec<acp::McpServer>,
    pub(super) meta_config_map: crate::session::mcp_servers::McpMetaConfigMap,
    pub(super) generation: crate::session::mcp_servers::Generation,
    pub(super) existing_client_names: std::collections::HashSet<String>,
    pub(super) has_servers_to_spawn: bool,
    pub(super) guard: crate::session::mcp_servers::InitClaimGuard,
}

/// Each outcome is recorded in one generation-checked section, so a superseded pass leaves no half-applied server.
pub(super) struct InitPass {
    generation: crate::session::mcp_servers::Generation,
    /// A pass that dies mid-drain reads as abandoned and is re-owned.
    _claim: crate::session::mcp_servers::InitClaimGuard,
    mcp_init_start: std::time::Instant,
    mcp_state: Arc<TokioMutex<crate::session::mcp_servers::McpState>>,
    publication: InitPublication,
    tool_bridge: Arc<crate::tools::bridge::ToolBridge>,
    events: distill_session_events::EventWriter,
    gateway: distill_acp_lib::AcpAgentGatewaySender,
    session_id: String,
    servers: std::collections::HashMap<String, ServerInfo>,
    clients: std::collections::HashMap<String, Arc<crate::session::mcp_servers::McpClient>>,
    /// The completion event's denominator.
    server_count: u32,
    /// The pager's progress denominator.
    handshake_count: u32,
    strategy: McpInitStrategy,
    is_reinit: bool,
    /// `Drop` releases every client not listed here.
    inserted_servers: Vec<String>,
    ui_tools_by_server:
        std::collections::HashMap<String, Vec<crate::extensions::mcp::McpToolEntry>>,
    tally: InitTally,
}

impl InitPass {
    pub(super) fn new(
        actor: &SessionActor,
        claim: InitClaim,
        clients: Vec<crate::session::mcp_servers::McpClient>,
        acp_pending_count: usize,
        handshake_count: u32,
        mcp_init_start: std::time::Instant,
    ) -> Self {
        let cwd = std::path::Path::new(actor.session_info.cwd.as_str());
        let servers = claim
            .configs
            .iter()
            .map(|c| {
                let name = mcp_server_name(c);
                (
                    name.to_string(),
                    ServerInfo {
                        transport: mcp_transport_str(c),
                        target: mcp_target_str(c),
                        scope: crate::util::config::mcp_server_scope(name, cwd),
                    },
                )
            })
            .collect();
        Self {
            generation: claim.generation,
            _claim: claim.guard,
            mcp_init_start,
            mcp_state: Arc::clone(&actor.mcp_state),
            publication: actor.init_publication(),
            tool_bridge: actor.agent.borrow().tool_bridge().clone(),
            events: actor.events.writer(),
            gateway: actor.notifications.gateway.clone(),
            session_id: actor.session_info.id.0.to_string(),
            servers,
            clients: clients
                .into_iter()
                .map(|c| (c.server_name().to_string(), Arc::new(c)))
                .collect(),
            server_count: (claim.configs.len() + acp_pending_count) as u32,
            handshake_count,
            strategy: actor.mcp_strategy.get(),
            is_reinit: !claim.existing_client_names.is_empty(),
            inserted_servers: Vec::new(),
            ui_tools_by_server: std::collections::HashMap::new(),
            tally: InitTally::default(),
        }
    }
}

impl Drop for InitPass {
    fn drop(&mut self) {
        for (server, client) in &self.clients {
            if !self.inserted_servers.contains(server) {
                client.discard();
            }
        }
    }
}

impl InitPass {
    /// A server that never spawned counts in the completion event like a failed handshake.
    pub(super) fn tally_spawn_failure(&mut self, server: String, needs_auth: bool) {
        self.tally.add_failed(server, needs_auth);
    }

    pub(super) async fn run(mut self, refresher: SnapshotRefresher) {
        let started = std::time::Instant::now();
        let refresh_queue = RefreshQueue::start(refresher);
        if self.drain(&refresh_queue).await.is_err() {
            self.report_cancelled();
            refresh_queue.stop().await;
            return;
        }
        // The final snapshot lands before waiters are released.
        refresh_queue.finish().await;
        if self.finish(started).await.is_err() {
            self.report_cancelled();
            return;
        }
        let mcp_tool_count = self.mcp_tool_count().await;
        if self.generation.is_cancelled() {
            return;
        }
        self.notify_initialized(started.elapsed(), mcp_tool_count);
    }

    /// A server-set change ends the drain at its next wait, before any further effect.
    async fn drain(&mut self, refresh_queue: &RefreshQueue) -> Result<(), Superseded> {
        // Taken before the handshakes, or notifications during them are lost.
        let event_tx = self.mcp_state.lock().await.client_event_tx();

        let mut handshakes = tokio::task::JoinSet::new();
        let mut servers_by_task: std::collections::HashMap<tokio::task::Id, String> =
            std::collections::HashMap::new();
        for (server, client) in &self.clients {
            let (transport, target) = self
                .servers
                .get(server)
                .map(|info| (info.transport.to_string(), info.target.clone()))
                .unwrap_or_else(|| ("unknown".to_string(), String::new()));
            let handle = handshakes.spawn_local(run_handshake(
                Arc::clone(client),
                Arc::clone(&self.mcp_state),
                self.events.clone(),
                event_tx.clone(),
                transport,
                target,
            ));
            servers_by_task.insert(handle.id(), server.clone());
        }

        let generation = self.generation.clone();
        let mut completed: u32 = 0;
        loop {
            let joined = generation.or_cancel(handshakes.join_next_with_id()).await?;
            let Some(joined) = joined else {
                return Ok(());
            };
            completed += 1;
            self.notify_progress(completed);

            let outcome = match joined {
                Ok((_task_id, outcome)) => outcome,
                Err(join_err) => {
                    tracing::warn!(error = %join_err, "MCP handshake task failed to join");
                    let Some(server) = servers_by_task.get(&join_err.id()).cloned() else {
                        continue;
                    };
                    Err(HandshakeFailure {
                        server,
                        error: crate::session::mcp_servers::McpError::ClientError(format!(
                            "handshake task failed: {join_err}"
                        )),
                        needs_auth: false,
                        elapsed: std::time::Duration::ZERO,
                        timeout_sec: 0,
                    })
                }
            };
            match outcome {
                Ok(success) => self.record_connected(success).await?,
                Err(failure) => self.record_failed(failure).await?,
            }
            // Per server, or ready servers stay hidden until the slowest handshake lands.
            refresh_queue.request();
        }
    }

    fn report_cancelled(&self) {
        let reason = init_cancelled_reason(&self.generation);
        tracing::info!(session_id = %self.session_id, reason, "MCP init pass superseded; stopping");
        self.events
            .emit(distill_session_events::Event::McpInitCancelled {
                reason: reason.to_string(),
            });
    }

    /// Armed before the outcome is recorded, so the watcher exists once the client is reachable.
    async fn arm_liveness_watcher(
        &self,
        server: &str,
    ) -> Option<Arc<crate::session::mcp_servers::McpClient>> {
        let client = self.clients.get(server).cloned()?;
        let _ = client
            .arm_liveness_watcher(distill_mcp::liveness::DEFAULT_POLL_INTERVAL)
            .await;
        Some(client)
    }

    async fn record_if_current(
        &mut self,
        client: Option<Arc<crate::session::mcp_servers::McpClient>>,
        record: impl FnOnce(
            &mut Self,
            &mut crate::session::mcp_servers::McpState,
            Option<Arc<crate::session::mcp_servers::McpClient>>,
        ),
    ) -> Result<(), Superseded> {
        // Local handles: `record` needs `self` mutably.
        let mcp_state = Arc::clone(&self.mcp_state);
        let generation = self.generation.clone();
        mcp_state
            .write_if_current(&generation, |mcp_state| record(self, mcp_state, client))
            .await
    }

    fn insert_client(
        &mut self,
        mcp_state: &mut crate::session::mcp_servers::McpState,
        server: &str,
        client: Option<Arc<crate::session::mcp_servers::McpClient>>,
    ) {
        if let Some(client) = client {
            mcp_state.owned_clients.insert(server.to_string(), client);
            self.inserted_servers.push(server.to_string());
        }
        mcp_state.mark_server_ready(server);
    }

    fn transport(&self, server: &str) -> &'static str {
        self.servers
            .get(server)
            .map_or("unknown", |info| info.transport)
    }

    fn scope(&self, server: &str) -> &'static str {
        self.servers
            .get(server)
            .map_or("unknown", |info| info.scope)
    }

    async fn record_connected(&mut self, success: HandshakeSuccess) -> Result<(), Superseded> {
        let HandshakeSuccess {
            server,
            registrations,
            elapsed,
            timeout_sec,
        } = success;
        tracing::info!(
            server = %server,
            elapsed_ms = elapsed.as_millis() as u64,
            timeout_sec,
            tool_count = registrations.len(),
            "MCP handshake succeeded",
        );
        let client = self.arm_liveness_watcher(&server).await;
        let tool_count = registrations.len() as u32;
        let tool_prefix = format!(
            "{}{}",
            server,
            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
        );
        let tool_names: Vec<String> = registrations
            .iter()
            .map(|r| {
                r.name
                    .strip_prefix(&tool_prefix)
                    .unwrap_or(&r.name)
                    .to_string()
            })
            .collect();

        self.record_if_current(client, |pass, mcp_state, client| {
            pass.tool_bridge.unregister_tools_by_prefix(&tool_prefix);
            for reg in registrations {
                register_mcp_tool(
                    &pass.tool_bridge,
                    &pass.events,
                    &server,
                    reg,
                    mcp_state,
                    &mut pass.ui_tools_by_server,
                );
            }
            pass.insert_client(mcp_state, &server, client);
        })
        .await?;
        self.notify_tools_changed();

        let transport = self.transport(&server);
        let transport_kind = match transport {
            "stdio" => distill_telemetry::events::McpTransport::Stdio,
            "sse" => distill_telemetry::events::McpTransport::Sse,
            _ => distill_telemetry::events::McpTransport::Http,
        };
        debug_assert!(
            distill_telemetry::activity::gauge_value(
                distill_telemetry::activity::MCP_SERVERS_CONNECTED_KEY
            ) >= 1,
            "McpServerConnected must stamp a self-inclusive count"
        );
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::McpServerConnected {
            server_name: server.clone(),
            tool_count,
            transport: transport_kind,
            duration_ms: elapsed.as_millis() as u64,
        });
        self.events
            .emit(distill_session_events::Event::McpServerConnected {
                server_name: server.clone(),
                transport: transport.to_string(),
                tool_count,
                duration_ms: elapsed.as_millis() as u64,
                tools: tool_names,
            });
        crate::session::telemetry::emit_mcp_connection_span(
            "connected",
            &server,
            transport,
            self.scope(&server),
            Some(elapsed.as_millis() as i64),
            Some(tool_count as i64),
            None,
        );
        self.tally.add_connected(tool_count);
        Ok(())
    }

    async fn record_failed(&mut self, failure: HandshakeFailure) -> Result<(), Superseded> {
        let HandshakeFailure {
            server,
            error,
            needs_auth,
            elapsed,
            timeout_sec,
        } = failure;
        let client = self.arm_liveness_watcher(&server).await;
        let detail = (!needs_auth).then(|| {
            distill_tools::util::truncate_str_with_marker(&error.to_string(), 200).into_owned()
        });

        let unreachable = !needs_auth && error.is_connect_failure();
        self.record_if_current(client, |pass, mcp_state, client| {
            if unreachable {
                mcp_state.record_unreachable_failure(&server, detail.unwrap_or_default());
            } else {
                mcp_state.record_init_failure(&server, needs_auth, detail);
            }
            pass.insert_client(mcp_state, &server, client);
        })
        .await?;

        let error_category = if needs_auth {
            distill_session_events::McpErrorCategory::AuthRequired
        } else {
            error.error_category()
        };
        let error_type = match error_category {
            distill_session_events::McpErrorCategory::AuthRequired => {
                distill_telemetry::events::McpErrorType::Auth
            }
            distill_session_events::McpErrorCategory::Timeout => {
                distill_telemetry::events::McpErrorType::Timeout
            }
            _ => distill_telemetry::events::McpErrorType::HandshakeFailed,
        };
        let transport = self.transport(&server);
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::McpServerFailed {
            server_name: server.clone(),
            error_type,
            duration_ms: elapsed.as_millis() as u64,
            timeout_sec,
            error_message: Some(error.to_string()),
        });
        crate::session::telemetry::emit_mcp_connection_span(
            "failed",
            &server,
            transport,
            self.scope(&server),
            Some(elapsed.as_millis() as i64),
            None,
            Some(error_type.as_ref()),
        );
        self.events
            .emit(distill_session_events::Event::McpServerFailed {
                server_name: server.clone(),
                transport: Some(transport.to_string()),
                target: self.servers.get(&server).map(|info| info.target.clone()),
                error_type: error_category,
                error_message: error.to_string(),
                duration_ms: Some(elapsed.as_millis() as u64),
                timeout_sec: Some(timeout_sec),
            });
        self.tally.add_failed(server, needs_auth);
        Ok(())
    }

    async fn finish(&mut self, started: std::time::Instant) -> Result<(), Superseded> {
        self.mcp_state
            .write_if_current(&self.generation, |mcp_state| {
                self.publication.complete(mcp_state);
                tracing::info!(
                    session_id = %self.session_id,
                    inserted = ?self.inserted_servers,
                    total_clients = mcp_state.owned_clients.len() + mcp_state.shared_clients.len(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "mcp_bg_handshake: clients inserted, waking init waiters"
                );
            })
            .await?;

        distill_telemetry::session_ctx::log_event(distill_telemetry::events::McpInitCompleted {
            total_duration_ms: started.elapsed().as_millis() as u64,
            spawn_duration_ms: started.duration_since(self.mcp_init_start).as_millis() as u64,
            server_count: self.server_count,
            servers_succeeded: self.tally.succeeded,
            servers_failed: self.tally.failed,
            servers_auth_required: self.tally.auth_required,
            total_tools_registered: self.tally.tools_registered,
            strategy: self.strategy,
            is_reinit: self.is_reinit,
        });
        self.events
            .emit(distill_session_events::Event::McpInitCompleted {
                total_servers: self.server_count,
                succeeded: self.tally.succeeded,
                failed: self.tally.failed,
                auth_required: self.tally.auth_required,
                total_tools: self.tally.tools_registered,
                duration_ms: started.elapsed().as_millis() as u64,
                is_reinit: self.is_reinit,
                failed_servers: std::mem::take(&mut self.tally.failed_servers),
            });
        Ok(())
    }

    fn notify_progress(&self, connected: u32) {
        if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
            "total": self.handshake_count,
            "connected": connected,
            "sessionId": self.session_id,
        })) {
            self.gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    crate::extensions::mcp::mcp_methods::INIT_PROGRESS,
                    params.into(),
                ));
        }
    }

    /// Sent as each server is recorded, while the pass is known to be current.
    fn notify_tools_changed(&mut self) {
        for (server_name, tools) in std::mem::take(&mut self.ui_tools_by_server) {
            let payload = crate::extensions::mcp::McpToolsChanged {
                session_id: self.session_id.clone(),
                server_name,
                tools,
            };
            if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                self.gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        crate::extensions::mcp::mcp_methods::TOOLS_CHANGED,
                        params.into(),
                    ));
            }
        }
    }

    async fn mcp_tool_count(&self) -> usize {
        self.tool_bridge
            .tool_definitions()
            .await
            .iter()
            .filter(|t| t.function.name.contains("__"))
            .count()
    }

    fn notify_initialized(&self, elapsed: std::time::Duration, mcp_tool_count: usize) {
        tracing::info!(
            target: crate::instrumentation::TARGET,
            event = "timing",
            name = "session.mcp_handshakes_bg",
            elapsed_us = elapsed.as_micros() as u64,
        );
        tracing::info!("MCP background handshakes completed in {:?}", elapsed);
        if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": self.session_id,
            "mcpToolCount": mcp_tool_count,
            "elapsedMs": elapsed.as_millis() as u64,
        })) {
            self.gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/mcp_initialized",
                    params.into(),
                ));
        }
    }
}

async fn run_handshake(
    client: Arc<crate::session::mcp_servers::McpClient>,
    mcp_state: Arc<TokioMutex<crate::session::mcp_servers::McpState>>,
    events: distill_session_events::EventWriter,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<distill_mcp::servers::McpClientEvent>>,
    transport: String,
    target: String,
) -> HandshakeOutcome {
    let server = client.server_name().to_string();
    let start = std::time::Instant::now();
    let timeout_sec = client.startup_timeout_sec();
    events.emit(distill_session_events::Event::McpServerStarting {
        server_name: server.clone(),
        transport,
        target,
        timeout_sec,
    });
    if let Some(tx) = event_tx {
        client.set_event_tx(Some(tx));
    }
    super::mcp::attach_elicitation_tx(&*mcp_state.lock().await, &client);
    match client.get_tool_registrations(mcp_state).await {
        Ok(registrations) => Ok(HandshakeSuccess {
            server,
            registrations,
            elapsed: start.elapsed(),
            timeout_sec,
        }),
        Err(error) => {
            // Login can only rebuild HTTP clients; other transports keep init_failed.
            let needs_auth = client.has_auth()
                || (client.is_http()
                    && !client.has_configured_auth_header()
                    && error.is_auth_rejection());
            tracing::warn!(
                server = server.as_str(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                timeout_sec,
                error = %error,
                needs_auth,
                "MCP server failed to initialize"
            );
            Err(HandshakeFailure {
                server,
                error,
                needs_auth,
                elapsed: start.elapsed(),
                timeout_sec,
            })
        }
    }
}

pub(super) fn register_mcp_tool(
    tool_bridge: &crate::tools::bridge::ToolBridge,
    events: &distill_session_events::EventWriter,
    server_name: &str,
    reg: crate::session::mcp_servers::McpToolRegistration,
    mcp_state: &mut McpState,
    ui_tools_by_server: &mut std::collections::HashMap<
        String,
        Vec<crate::extensions::mcp::McpToolEntry>,
    >,
) {
    let qualified_name = reg.name.clone();
    let prefix = format!(
        "{}{}",
        server_name,
        crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
    );
    let unqualified = qualified_name
        .strip_prefix(&prefix)
        .unwrap_or(&qualified_name)
        .to_string();

    mcp_state.record_tool_icons(qualified_name.clone(), reg.icons.clone());
    if let Some(meta) = reg.meta.as_ref() {
        mcp_state
            .mcp_tool_meta
            .insert(qualified_name.clone(), meta.clone());

        if meta
            .get("ui")
            .and_then(|ui| ui.get("resourceUri"))
            .is_some()
        {
            ui_tools_by_server
                .entry(server_name.to_string())
                .or_default()
                .push(crate::extensions::mcp::McpToolEntry {
                    name: unqualified.clone(),
                    display_name: None,
                    description: Some(reg.description.clone()),
                    meta: Some(meta.clone()),
                    icons: reg.icons.clone(),
                    enabled: !mcp_state.is_tool_disabled(server_name, &unqualified),
                });
        }
    }

    if mcp_state.is_tool_disabled(server_name, &unqualified) {
        tracing::info!(
            "Stashing disabled MCP tool '{}' from '{}'",
            qualified_name,
            server_name
        );
        mcp_state
            .disabled_tool_registrations
            .insert(qualified_name, reg);
        return;
    }

    if !reg.model_visible {
        tracing::debug!(
            "Skipping app-only MCP tool '{}' from '{}'",
            qualified_name,
            server_name
        );
        return;
    }
    if let Err(e) = tool_bridge.register_mcp_tools(reg.name, reg.tool, Some(reg.input_schema)) {
        tracing::warn!(
            "Failed to register tool '{}' from MCP server '{}': {}",
            qualified_name,
            server_name,
            e
        );
        events.emit(distill_session_events::Event::McpToolRegistrationFailed {
            server_name: server_name.to_string(),
            tool_name: qualified_name,
            error: e.to_string(),
        });
    } else {
        tracing::debug!(
            "Registered MCP tool '{}' from server '{}'",
            qualified_name,
            server_name
        );
    }
}
