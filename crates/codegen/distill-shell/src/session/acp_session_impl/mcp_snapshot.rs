// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP snapshot refresh, reminder scheduling, and the bounded handshake gates.

use super::*;
use crate::session::mcp_servers::{SharedMcpState, Superseded};

/// The `McpInitCancelled` reason for a pass whose generation was replaced.
pub(super) fn init_cancelled_reason(
    generation: &crate::session::mcp_servers::Generation,
) -> &'static str {
    match generation.replaced_by() {
        Some(crate::session::mcp_servers::Replacement::Rebuild) => "agent_rebuild",
        Some(crate::session::mcp_servers::Replacement::ServerSetChange) | None => "config_changed",
    }
}
pub(super) const MCP_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const DELIVERY_TOOLS_DEFAULT_PREFIX_WAIT: std::time::Duration =
    std::time::Duration::from_secs(15);
pub(super) const DELIVERY_TOOLS_TEMPLATED_PREFIX_WAIT: std::time::Duration =
    std::time::Duration::from_secs(60);
pub(super) const MCP_INIT_WAIT_BOUND: std::time::Duration = std::time::Duration::from_secs(120);

/// Outcome of [`SessionActor::wait_for_mcp_handshakes_until`]; `GenerationChanged` tells the caller to restart against the current generation's deadline.
#[derive(Debug, Clone, Copy)]
enum McpHandshakeWait {
    Complete,
    DeadlineExpired,
    GenerationChanged,
    /// The pass that owned startup died before finishing; the caller re-owns startup, then waits again.
    Abandoned,
}

/// The single MCP dispatch predicate. A server with a failure record or a handshake still in flight is not
/// dispatched to; as in the reference, a call to a still-connecting server is rejected rather than waited on.
/// Auth-required and "init finished, no ready client" both dispatch, so the bridge surfaces the real error.
pub(super) async fn mcp_server_dispatchable(
    mcp_state: &TokioMutex<McpState>,
    server_name: &str,
) -> bool {
    let (failed, auth_required, client, settled) = {
        let mcp_state = mcp_state.lock().await;
        (
            mcp_state.init_failed.contains_key(server_name),
            mcp_state.auth_required.contains(server_name),
            mcp_state.get_client(server_name).cloned(),
            mcp_state.has_finished_init() && !mcp_state.is_server_handshaking(server_name),
        )
    };
    if failed {
        return false;
    }
    let ready = match client {
        Some(client) => client.is_ready().await,
        None => false,
    };
    ready || auth_required || settled
}

impl McpReminderMode {
    pub(super) fn from_env() -> Self {
        match std::env::var("MCP_REMINDER_MODE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "full" => Self::Full,
            _ => Self::Delta,
        }
    }
}

pub(super) fn gateway_tool_is_disabled(
    tool: &crate::session::managed_mcp::GatewayTool,
    disabled_gateway_tools: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> bool {
    let qualified_name = tool.qualified_name();
    disabled_gateway_tools
        .get(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY)
        .is_some_and(|set| set.contains(&tool.connector_id))
        || disabled_gateway_tools
            .get(&tool.connector_id)
            .is_some_and(|set| set.contains(&qualified_name))
}

/// What a turn reads about init outside `McpState`: the searchable snapshot's completion flag and the reminder's
/// re-check bit. Reset with the server set; set by the pass's final step, under the state lock.
#[derive(Clone)]
pub(super) struct InitPublication {
    tool_metadata_snapshot: Arc<std::sync::Mutex<crate::session::tool_index::ToolMetadataSnapshot>>,
    mcp_reminder_dirty: Arc<std::sync::atomic::AtomicBool>,
}

impl InitPublication {
    pub(super) fn complete(&self, mcp_state: &mut McpState) {
        self.tool_metadata_snapshot.lock().unwrap().mcp_initialized = true;
        mcp_state.complete_init();
        // The reminder re-checks the failed servers against the complete state.
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn reset(&self) {
        self.tool_metadata_snapshot.lock().unwrap().mcp_initialized = false;
    }
}

impl SessionActor {
    pub(super) fn init_publication(&self) -> InitPublication {
        InitPublication {
            tool_metadata_snapshot: Arc::clone(&self.tool_metadata_snapshot),
            mcp_reminder_dirty: Arc::clone(&self.mcp_reminder_dirty),
        }
    }
}

pub(super) struct SnapshotRefresher {
    /// A pass-owned refresh never publishes for a replaced server set. It is not interrupted mid-way: its descriptor
    /// writers run on blocking threads.
    pub(super) generation: Option<crate::session::mcp_servers::Generation>,
    pub(super) tool_bridge: Arc<crate::tools::bridge::ToolBridge>,
    pub(super) mcp_state: Arc<TokioMutex<McpState>>,
    pub(super) refresh_gate: Arc<TokioMutex<()>>,
    pub(super) managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    pub(super) tool_metadata_snapshot:
        Arc<std::sync::Mutex<crate::session::tool_index::ToolMetadataSnapshot>>,
    pub(super) mcp_reminder_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(super) disabled_gateway_tools:
        std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// External harness only: `Some` also updates the on-disk `mcps/` descriptor mirror.
    pub(super) mcps_root: Option<std::path::PathBuf>,
}

impl SnapshotRefresher {
    fn is_superseded(&self) -> bool {
        self.generation
            .as_ref()
            .is_some_and(crate::session::mcp_servers::Generation::is_cancelled)
    }

    pub(super) async fn refresh(&self) {
        let SnapshotRefresher {
            generation: _,
            tool_bridge,
            mcp_state,
            refresh_gate,
            managed_mcp_handle,
            tool_metadata_snapshot,
            mcp_reminder_dirty,
            disabled_gateway_tools,
            mcps_root,
        } = self;
        if self.is_superseded() {
            return;
        }
        // Refreshes serialize on this gate so the last write is the latest read.
        let _refresh_serialized = refresh_gate.lock().await;
        if self.is_superseded() {
            return;
        }

        let (gateway_catalog, mut gateway_connectors) = {
            let state = managed_mcp_handle.lock().await;
            let catalog = if state.gateway_tools_active {
                match &state.gateway_tool_cache {
                    crate::session::managed_mcp::GatewayToolCatalogCache::Ready(catalog) => {
                        Some(catalog.clone())
                    }
                    _ => None,
                }
            } else {
                None
            };
            let connectors: Vec<String> =
                state.gateway_tool_connectors_seen.iter().cloned().collect();
            (catalog, connectors)
        };

        if let Some(catalog) = gateway_catalog.as_ref() {
            gateway_connectors.extend(catalog.tools.iter().map(|tool| tool.connector_id.clone()));
        }
        gateway_connectors.sort_unstable();
        gateway_connectors.dedup();
        // Mirror before dirty: a reminder never announces a server whose descriptor files do not exist yet.
        if let Some(mcps_root) = mcps_root.as_ref() {
            let clients: Vec<(String, Arc<crate::session::mcp_servers::McpClient>)> = {
                let state = mcp_state.lock().await;
                state
                    .all_clients()
                    .map(|(n, c)| (n.clone(), Arc::clone(c)))
                    .collect()
            };
            let protected_connectors = clients.iter().map(|(name, _)| name.clone()).collect();
            let mut gateway_descriptors = Vec::new();
            if let Some(catalog) = gateway_catalog.as_ref() {
                for tool in &catalog.tools {
                    if gateway_tool_is_disabled(tool, disabled_gateway_tools) {
                        continue;
                    }
                    gateway_descriptors.push(
                        crate::session::mcp_descriptors::GatewayToolDescriptor {
                            connector_id: tool.connector_id.clone(),
                            tool_id: tool.tool_id.clone(),
                            description: tool.description.clone(),
                            json_schema: tool.json_schema.clone(),
                        },
                    );
                }
            }
            crate::session::mcp_descriptors::materialize_descriptors_for_clients(
                mcps_root, clients,
            )
            .await;
            crate::session::mcp_descriptors::materialize_descriptors_for_gateway_tools(
                mcps_root,
                gateway_descriptors,
                gateway_connectors,
                protected_connectors,
            )
            .await;
        }

        // A config change mid-build can unregister tools the catalog still lists.
        const MAX_REBUILDS: usize = 3;
        for _ in 0..MAX_REBUILDS {
            if self.is_superseded() {
                return;
            }
            let generation = match &self.generation {
                Some(generation) => generation.clone(),
                None => mcp_state.lock().await.current_generation(),
            };
            let catalog = build_mcp_catalog(
                tool_bridge,
                mcp_state,
                gateway_catalog.as_ref(),
                disabled_gateway_tools,
            )
            .await;
            // `search_tool` and `use_tool` read under the bridge's resource lock, so both publications land in one
            // critical section.
            let mut published = Err(Superseded);
            tool_bridge
                .update_resources_with(|resources| {
                    if generation.is_cancelled() {
                        return;
                    }
                    resources.insert(distill_tools::types::resources::ManagedGatewayToolCatalog(
                        catalog.gateway_resource_entries.into_iter().collect(),
                    ));
                    let mut snapshot = tool_metadata_snapshot.lock().unwrap();
                    snapshot.tools = catalog.tools;
                    snapshot.servers = catalog.servers;
                    mcp_reminder_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    published = Ok(());
                })
                .await;
            if published.is_err() {
                continue;
            }
            tracing::debug!("MCP snapshot updated, reminder marked dirty");
            return;
        }
        tracing::warn!("MCP snapshot refresh skipped: configs kept changing while it was built");
    }
}

struct McpCatalog {
    tools: Vec<crate::session::tool_index::ToolMetadata>,
    servers: Vec<crate::session::tool_index::ServerMetadata>,
    gateway_resource_entries: Vec<(
        String,
        distill_tools::types::resources::ManagedGatewayToolSource,
    )>,
}

/// Reads the bridge and the connected clients; the caller publishes only if the generation held throughout.
async fn build_mcp_catalog(
    tool_bridge: &crate::tools::bridge::ToolBridge,
    mcp_state: &TokioMutex<McpState>,
    gateway_catalog: Option<&crate::session::managed_mcp::GatewayToolCatalog>,
    disabled_gateway_tools: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> McpCatalog {
    use crate::session::tool_index::{
        ServerMetadata, ToolMetadata, extract_parameter_names, split_qualified_name,
    };

    let all_defs = tool_bridge.tool_definitions().await;
    let mut seen_tools = std::collections::HashSet::new();
    let mut tools: Vec<ToolMetadata> = all_defs
        .iter()
        .filter(|d| d.function.name.contains("__"))
        .filter(|d| seen_tools.insert(d.function.name.clone()))
        .map(|d| {
            let (server, tool) = split_qualified_name(&d.function.name);
            ToolMetadata {
                qualified_name: d.function.name.clone(),
                server_name: server.to_string(),
                tool_name: tool.to_string(),
                description: d.function.description.clone().unwrap_or_default(),
                parameters: extract_parameter_names(&d.function.parameters),
                input_schema: d.function.parameters.clone(),
            }
        })
        .collect();

    let mut gateway_resource_entries = Vec::new();
    if let Some(catalog) = gateway_catalog {
        for tool in &catalog.tools {
            let qualified_name = tool.qualified_name();
            if gateway_tool_is_disabled(tool, disabled_gateway_tools) {
                continue;
            }
            if !seen_tools.insert(qualified_name.clone()) {
                continue;
            }
            gateway_resource_entries.push((
                qualified_name.clone(),
                distill_tools::types::resources::ManagedGatewayToolSource {
                    connector_id: tool.connector_id.clone(),
                    connector_name: tool.connector_name.clone(),
                    tool_id: tool.tool_id.clone(),
                    tool_name: tool.tool_name.clone(),
                    call_id: tool.call_id.clone(),
                },
            ));
            tools.push(ToolMetadata {
                qualified_name,
                server_name: tool.connector_id.clone(),
                tool_name: tool.tool_id.clone(),
                description: tool.description.clone(),
                parameters: extract_parameter_names(&tool.json_schema),
                input_schema: tool.json_schema.clone(),
            });
        }
    }

    let servers_with_tools: std::collections::HashSet<&str> =
        tools.iter().map(|t| t.server_name.as_str()).collect();
    let clients_with_tools: Vec<(String, Arc<crate::session::mcp_servers::McpClient>)> = {
        let mcp_state = mcp_state.lock().await;
        mcp_state
            .all_clients()
            .filter(|(name, _)| servers_with_tools.contains(name.as_str()))
            .map(|(n, c)| (n.clone(), Arc::clone(c)))
            .collect()
    };
    let mut servers = Vec::new();
    for (name, client) in clients_with_tools {
        servers.push(ServerMetadata {
            description: client.server_instructions().await,
            name,
        });
    }

    McpCatalog {
        tools,
        servers,
        gateway_resource_entries,
    }
}

#[cfg(test)]
pub(crate) async fn refresh_mcp_snapshot_for_test(
    tool_bridge: Arc<crate::tools::bridge::ToolBridge>,
    mcp_state: Arc<TokioMutex<McpState>>,
    managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    tool_metadata_snapshot: Arc<std::sync::Mutex<crate::session::tool_index::ToolMetadataSnapshot>>,
    disabled_gateway_tools: std::collections::HashMap<String, std::collections::HashSet<String>>,
) {
    SnapshotRefresher {
        generation: None,
        tool_bridge,
        mcp_state,
        refresh_gate: Arc::new(TokioMutex::new(())),
        managed_mcp_handle,
        tool_metadata_snapshot,
        mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        disabled_gateway_tools,
        mcps_root: None,
    }
    .refresh()
    .await;
}

/// The prompt gates' per-generation waits: the startup grace is armed once, and a bounded full wait times out at most once.
/// Keyed by the generation token, so a config change starts both over.
#[derive(Default)]
pub(crate) struct McpStartupWaits {
    grace: std::cell::RefCell<
        Option<(
            crate::session::mcp_servers::Generation,
            tokio::time::Instant,
        )>,
    >,
    full_wait_timed_out: std::cell::RefCell<Option<crate::session::mcp_servers::Generation>>,
}

impl McpStartupWaits {
    fn grace_deadline(
        &self,
        generation: &crate::session::mcp_servers::Generation,
        grace: std::time::Duration,
    ) -> tokio::time::Instant {
        let mut armed = self.grace.borrow_mut();
        match &*armed {
            Some((armed_for, deadline)) if !armed_for.is_cancelled() => *deadline,
            _ => {
                let deadline = tokio::time::Instant::now() + grace;
                *armed = Some((generation.clone(), deadline));
                deadline
            }
        }
    }

    pub(crate) fn full_wait_timed_out(&self) -> bool {
        self.full_wait_timed_out
            .borrow()
            .as_ref()
            .is_some_and(|timed_out_for| !timed_out_for.is_cancelled())
    }

    fn mark_full_wait_timed_out(&self, generation: crate::session::mcp_servers::Generation) {
        *self.full_wait_timed_out.borrow_mut() = Some(generation);
    }
}

impl SessionActor {
    #[cfg(test)]
    pub(crate) fn full_mcp_wait_timed_out(&self) -> bool {
        self.mcp_startup_waits.full_wait_timed_out()
    }

    pub(super) async fn wait_for_mcp_initialized_bounded_once(&self) {
        if self.mcp_startup_waits.full_wait_timed_out() {
            return;
        }
        let generation = self.mcp_state.lock().await.current_generation();
        let timed_out = tokio::time::timeout(MCP_INIT_WAIT_BOUND, self.wait_for_mcp_initialized())
            .await
            .is_err();
        if timed_out {
            self.mcp_startup_waits.mark_full_wait_timed_out(generation);
            tracing::warn!(
                "MCP initialization did not settle within {MCP_INIT_WAIT_BOUND:?}; proceeding"
            );
        }
    }

    /// One deadline armed at first use, so a session pays the grace at most once; `deliveryTools` sessions keep full waits.
    /// A config change mid-grace restarts the wait against the new generation's freshly armed deadline.
    pub(super) async fn wait_for_mcp_startup_grace(&self) {
        self.wait_for_mcp_handshakes(|generation| {
            self.mcp_startup_waits
                .grace_deadline(generation, MCP_STARTUP_GRACE)
        })
        .await;
    }

    /// A server-set change re-reads the deadline for the new set; an abandoned startup is re-owned once, outside any
    /// deadline.
    pub(super) async fn wait_for_mcp_handshakes(
        &self,
        deadline: impl Fn(&crate::session::mcp_servers::Generation) -> tokio::time::Instant,
    ) {
        let mut re_owned = false;
        loop {
            let generation = self.mcp_state.lock().await.current_generation();
            match self
                .wait_for_mcp_handshakes_until(deadline(&generation), &generation)
                .await
            {
                McpHandshakeWait::GenerationChanged => {}
                McpHandshakeWait::Abandoned if !re_owned => {
                    re_owned = true;
                    self.ensure_mcp_tools_initialized().await;
                }
                McpHandshakeWait::Abandoned
                | McpHandshakeWait::Complete
                | McpHandshakeWait::DeadlineExpired => return,
            }
        }
    }
    #[tracing::instrument(skip_all)]
    async fn wait_for_mcp_handshakes_until(
        &self,
        deadline: tokio::time::Instant,
        generation: &crate::session::mcp_servers::Generation,
    ) -> McpHandshakeWait {
        let start = std::time::Instant::now();
        let signal = self.mcp_state.lock().await.init_wait_signal();
        let mut first_pass = true;
        let outcome = loop {
            // `notify_waiters` stores no permit: enable before reading state or a notify in between is lost.
            let notified = signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if generation.is_cancelled() {
                return McpHandshakeWait::GenerationChanged;
            }
            let settled = {
                let s = self.mcp_state.lock().await;
                // An abandoned pass's empty set would read as complete.
                if s.is_init_abandoned() {
                    return McpHandshakeWait::Abandoned;
                }
                !s.has_mcp_servers() || s.is_initialized()
            };
            if settled {
                if first_pass {
                    return McpHandshakeWait::Complete;
                }
                break McpHandshakeWait::Complete;
            }
            first_pass = false;
            let woke = generation
                .or_cancel(async {
                    tokio::select! {
                        () = &mut notified => true,
                        () = tokio::time::sleep_until(deadline) => false,
                    }
                })
                .await;
            match woke {
                Ok(true) => {}
                Ok(false) => break McpHandshakeWait::DeadlineExpired,
                Err(Superseded) => return McpHandshakeWait::GenerationChanged,
            }
        };

        // Only acquire the post-wait snapshot when INFO tracing is active; in production the extra lock and string cloning is unnecessary
        if tracing::enabled!(tracing::Level::INFO) {
            let s = self.mcp_state.lock().await;
            tracing::info!(
                session_id = %self.session_info.id.0,
                outcome = ?outcome,
                elapsed_ms = start.elapsed().as_millis() as u64,
                final_initializing_names = ?s.handshaking_servers_iter().cloned().collect::<Vec<_>>(),
                final_client_names = ?s.all_clients().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                "wait_for_mcp_handshakes_until: done"
            );
        }
        outcome
    }

    /// Re-register MCP tools from the existing clients onto a freshly built `ToolBridge` after a zero-turn harness rebuild.
    /// A client that cannot list its tools is evicted so the rebuild's own pass respawns it; once a server-set change
    /// owns the set, the eviction is left to its pass.
    pub(super) async fn re_register_mcp_tools_on_rebuilt_bridge(&self) {
        let (generation, servers) = {
            let st = self.mcp_state.lock().await;
            let servers: Vec<String> = st.all_clients().map(|(name, _)| name.clone()).collect();
            (st.current_generation(), servers)
        };

        if servers.is_empty() {
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            return;
        }

        tracing::info!(
            session_id = %self.session_info.id.0,
            server_count = servers.len(),
            "re_register_mcp_tools_on_rebuilt_bridge: re-registering MCP tools from existing clients"
        );

        let mut all_ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for server in &servers {
            self.relist_server_on_rebuilt_bridge(server, &generation, &mut all_ui_tools)
                .await;
        }

        // Refresh the snapshot so search_tool returns accurate results against the newly-registered tools
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
        self.emit_mcp_tools_changed_notifications(all_ui_tools);
    }

    /// Registers the slot's current client; a client installed under `server` during the list (a respawn landing) is
    /// listed in turn, so the registration always lands on the client the slot holds.
    async fn relist_server_on_rebuilt_bridge(
        &self,
        server: &str,
        generation: &crate::session::mcp_servers::Generation,
        ui_tools: &mut std::collections::HashMap<String, Vec<crate::extensions::mcp::McpToolEntry>>,
    ) {
        let mut listed: Option<std::sync::Arc<crate::session::mcp_servers::McpClient>> = None;
        loop {
            let client = {
                let st = self.mcp_state.lock().await;
                match st.get_client(server) {
                    Some(now)
                        if listed
                            .as_ref()
                            .is_none_or(|prev| !std::sync::Arc::ptr_eq(prev, now)) =>
                    {
                        std::sync::Arc::clone(now)
                    }
                    _ => return,
                }
            };
            match client
                .get_tool_registrations(std::sync::Arc::clone(&self.mcp_state))
                .await
            {
                Ok(registrations) => {
                    let tool_count = registrations.len();
                    let registered = self
                        .mcp_state
                        .write_if_installed(server, &client, |mcp_state| {
                            for reg in registrations {
                                self.register_mcp_tool(server, reg, mcp_state, ui_tools);
                            }
                        })
                        .await;
                    if registered.is_ok() {
                        tracing::info!(
                            session_id = %self.session_info.id.0,
                            server,
                            tool_count,
                            "re_register_mcp_tools_on_rebuilt_bridge: re-registered tools"
                        );
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %self.session_info.id.0,
                        server,
                        error = %error,
                        "re_register_mcp_tools_on_rebuilt_bridge: tools/list failed; evicting so the next init respawns it"
                    );
                    let _ = self
                        .mcp_state
                        .write_if_current(generation, |mcp_state| {
                            mcp_state.owned_clients.remove_if_same(server, &client)
                        })
                        .await;
                }
            }
            listed = Some(client);
        }
    }
}
