// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP on the session actor: starting an init pass, the full init wait, config changes, the auth and retry paths.
//! The pass itself is `mcp_init.rs`; snapshot refreshes and the prompt-side gates are `mcp_snapshot.rs`.
use super::mcp_failed_reminder::{classify_failed_servers, render_failed_section};
use super::*;
use crate::session::mcp_servers::{McpOauthDiscovery, SharedMcpState, Superseded};
use distill_telemetry::instrument_task;
use distill_telemetry::region::Parent;
/// Wire the session's elicitation inbox into a freshly built client so its `elicitation/create` requests reach the coordinator.
/// Takes the already-locked `McpState` so each caller keeps its own lock scope.
pub(super) fn attach_elicitation_tx(
    state: &crate::session::mcp_servers::McpState,
    client: &crate::session::mcp_servers::McpClient,
) {
    if let Some(tx) = state.elicitation_tx() {
        client.set_elicitation_tx(Some(tx));
    }
}
impl SessionActor {
    /// Waits until init is complete, starting it if nothing owns it. An abandoned pass is re-owned once.
    pub(super) async fn wait_for_mcp_initialized(&self) {
        let signal = self.mcp_state.lock().await.init_wait_signal();
        let mut re_owned = false;
        loop {
            let notified = signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (initialized, initializing, abandoned) = {
                let mcp_state = self.mcp_state.lock().await;
                (
                    mcp_state.is_initialized(),
                    mcp_state.is_initializing(),
                    mcp_state.is_init_abandoned(),
                )
            };
            if initialized {
                return;
            }
            if !initializing {
                if abandoned {
                    if re_owned {
                        return;
                    }
                    re_owned = true;
                }
                if !self.ensure_mcp_tools_initialized().await {
                    return;
                }
                continue;
            }
            notified.await;
        }
    }
    /// Register tools from shared (inherited) MCP clients on this session's ToolBridge.
    /// Shared clients are already connected (Arc-shared from parent).
    /// `get_tool_registrations` reuses the existing transport with no new handshake.
    async fn register_shared_client_tools(
        &self,
        generation: &crate::session::mcp_servers::Generation,
    ) -> Result<(), Superseded> {
        let shared_clients: Vec<(
            String,
            std::sync::Arc<crate::session::mcp_servers::McpClient>,
        )> = {
            let st = self.mcp_state.lock().await;
            if st.shared_clients.is_empty() {
                return Ok(());
            }
            st.shared_clients
                .iter()
                .map(|(n, c)| (n.clone(), std::sync::Arc::clone(c)))
                .collect()
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            count = shared_clients.len(),
            "Registering tools from shared MCP clients"
        );
        let mcp_state_arc = std::sync::Arc::clone(&self.mcp_state);
        let mut ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for (server_name, client) in &shared_clients {
            let regs = match client
                .get_tool_registrations(std::sync::Arc::clone(&mcp_state_arc))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "Failed to list tools from shared MCP client, skipping"
                    );
                    continue;
                }
            };
            self.mcp_state
                .write_if_current(generation, |mcp_state| {
                    for reg in regs {
                        self.register_mcp_tool(server_name, reg, mcp_state, &mut ui_tools);
                    }
                })
                .await?;
        }
        self.refresh_mcp_snapshot_for(generation).await;
        if generation.is_cancelled() {
            return Err(Superseded);
        }
        if !ui_tools.is_empty() {
            self.emit_mcp_tools_changed_notifications(ui_tools);
        }
        Ok(())
    }
    pub(super) fn register_mcp_tool(
        &self,
        server_name: &str,
        reg: crate::session::mcp_servers::McpToolRegistration,
        mcp_state: &mut crate::session::mcp_servers::McpState,
        ui_tools_by_server: &mut std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        >,
    ) {
        register_mcp_tool(
            self.agent.borrow().tool_bridge(),
            &self.events.writer(),
            server_name,
            reg,
            mcp_state,
            ui_tools_by_server,
        );
    }
    /// Emit per-server `x.ai/mcp/tools_changed` notifications.
    /// Each emission carries the owning `sessionId` so the pager can route via `find_session_match` instead of falling back to `app.active_view`.
    /// Without that field, a background agent's push would silently land on the foregrounded agent's modal.
    pub(super) fn emit_mcp_tools_changed_notifications(
        &self,
        ui_tools_by_server: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        >,
    ) {
        let session_id = self.session_id_string();
        for (server_name, tools) in ui_tools_by_server {
            let payload = crate::extensions::mcp::McpToolsChanged {
                session_id: session_id.clone(),
                server_name,
                tools,
            };
            if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        crate::extensions::mcp::mcp_methods::TOOLS_CHANGED,
                        params.into(),
                    ));
            }
        }
    }
    /// Handle explicit auth trigger from the client (x.ai/mcp/auth_trigger).
    ///
    /// Runs force_reauth (browser flow), then re-initializes the server and registers its tools.
    pub(super) async fn handle_mcp_auth_trigger(&self, server_name: &str) -> Result<(), String> {
        self.wait_for_server_settled(server_name).await;
        let existing_client = {
            let state = self.mcp_state.lock().await;
            state.get_client(server_name).cloned()
        };
        let client = match &existing_client {
            Some(c) if c.has_auth() => std::sync::Arc::clone(c),
            _ => {
                self.rebuild_http_client_with_oauth(
                    server_name,
                    McpOauthDiscovery::Network,
                    existing_client.as_ref(),
                )
                .await?
            }
        };
        if !client.force_reauth(true).await {
            return Err(format!(
                "Authentication failed for MCP server '{}'",
                server_name
            ));
        }
        let registrations = client
            .get_tool_registrations(self.mcp_state.clone())
            .await
            .map_err(|e| format!("Failed to get tools after auth: {}", e))?;
        let mut ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        self.mcp_state
            .write_if_installed(server_name, &client, |mcp_state| {
                mcp_state.auth_required.remove(server_name);
                mcp_state.clear_init_failed(server_name);
                for reg in registrations {
                    self.register_mcp_tool(server_name, reg, mcp_state, &mut ui_tools);
                }
            })
            .await
            .map_err(|Superseded| {
                format!("server '{server_name}' was removed or reconfigured during authentication")
            })?;
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
        self.emit_mcp_tools_changed_notifications(ui_tools);
        self.refresh_goal_harness_enabled().await;
        tracing::info!(
            server = server_name,
            "MCP server authenticated and tools registered via auth_trigger"
        );
        Ok(())
    }
    /// `replacing` is the slot as the caller saw it; the install lands only if the slot still holds exactly that.
    async fn rebuild_http_client_with_oauth(
        &self,
        server_name: &str,
        discovery: McpOauthDiscovery,
        replacing: Option<&std::sync::Arc<crate::session::mcp_servers::McpClient>>,
    ) -> Result<std::sync::Arc<crate::session::mcp_servers::McpClient>, String> {
        let (server_config, meta_config, event_tx, generation) = {
            let mcp_state = self.mcp_state.lock().await;
            let server_config = mcp_state
                .configs
                .iter()
                .find(|c| crate::session::mcp_servers::mcp_server_name(c) == server_name)
                .cloned()
                .ok_or_else(|| format!("MCP server '{}' not found in config", server_name))?;
            match &server_config {
                acp::McpServer::Http(_) | acp::McpServer::Sse(_) => {}
                _ => {
                    return Err(format!("MCP server '{}' does not use OAuth", server_name));
                }
            }
            let meta_config = mcp_state.meta_config_map.get(server_name).cloned();
            let event_tx = mcp_state.client_event_tx();
            let generation = mcp_state.current_generation();
            (server_config, meta_config, event_tx, generation)
        };
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let session_id = self.session_info.id.0.as_ref();
        let (_, oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let byo_config = oauth_config_map.get(server_name).cloned();
        let event_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &event_writer,
            crate::session::mcp_servers::OauthInteractivity::Interactive,
            self.tool_context.process_scope.as_ref(),
        )
        .with_oauth_discovery(discovery);
        let new_client = crate::session::mcp_servers::start_mcp_server(
            server_config.clone(),
            Some(cwd),
            meta_config.as_ref(),
            byo_config.as_ref(),
            &ctx,
        )
        .await
        .map_err(|e| format!("Failed to prepare OAuth for '{}': {}", server_name, e))?;
        if !new_client.has_auth() {
            return Err(match discovery {
                McpOauthDiscovery::Network => {
                    format!(
                        "MCP server '{}' does not support OAuth (discovery found no authorization support)",
                        server_name
                    )
                }
                McpOauthDiscovery::Disk => {
                    format!(
                        "MCP server '{}' has no stored OAuth credentials",
                        server_name
                    )
                }
            });
        }
        if let Some(tx) = event_tx {
            new_client.set_event_tx(Some(tx));
        }
        attach_elicitation_tx(&*self.mcp_state.lock().await, &new_client);
        let arc = std::sync::Arc::new(new_client);
        let install = |mcp_state: &mut McpState| {
            mcp_state
                .owned_clients
                .insert(server_name.to_string(), arc.clone());
            mcp_state.auth_required.insert(server_name.to_string());
            mcp_state.clear_init_failed(server_name);
        };
        let installed = self
            .mcp_state
            .write_if_slot_is(server_name, replacing, &generation, install)
            .await;
        if installed.is_err() {
            arc.discard();
            return Err(format!(
                "config for server '{server_name}' changed while its client was being rebuilt"
            ));
        }
        tracing::info!(
            server = server_name,
            discovery = ?discovery,
            "Rebuilt MCP HTTP client with OAuth manager"
        );
        Ok(arc)
    }
    pub(super) async fn retry_auth_required_servers(&self) {
        let servers_to_retry: Vec<String> = {
            let state = self.mcp_state.lock().await;
            state.auth_required.iter().cloned().collect()
        };
        if servers_to_retry.is_empty() {
            return;
        }
        let mut recovered = false;
        let mut all_ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for server_name in &servers_to_retry {
            self.wait_for_server_settled(server_name).await;
            let existing = {
                let state = self.mcp_state.lock().await;
                state.get_client(server_name).cloned()
            };
            let client = match &existing {
                Some(c) if c.has_auth() => {
                    if !c.try_reauth_from_disk().await {
                        continue;
                    }
                    std::sync::Arc::clone(c)
                }
                _ => {
                    match self
                        .rebuild_http_client_with_oauth(
                            server_name,
                            McpOauthDiscovery::Disk,
                            existing.as_ref(),
                        )
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::debug!(
                                server = server_name.as_str(),
                                %e,
                                "retry_auth_required: no stored-credential rebuild"
                            );
                            continue;
                        }
                    }
                }
            };
            let registrations = match client.get_tool_registrations(self.mcp_state.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(
                        server = server_name.as_str(),
                        %e,
                        "retry_auth_required: handshake still failing"
                    );
                    continue;
                }
            };
            let mut ui_tools: std::collections::HashMap<
                String,
                Vec<crate::extensions::mcp::McpToolEntry>,
            > = std::collections::HashMap::new();
            let registered = self
                .mcp_state
                .write_if_installed(server_name, &client, |mcp_state| {
                    mcp_state.auth_required.remove(server_name);
                    for reg in registrations {
                        self.register_mcp_tool(server_name, reg, mcp_state, &mut ui_tools);
                    }
                })
                .await;
            if registered.is_err() {
                continue;
            }
            all_ui_tools.extend(ui_tools);
            tracing::info!(
                server = server_name.as_str(),
                "MCP server recovered via retry_auth_required (tokens found on disk)"
            );
            recovered = true;
        }
        if recovered {
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            self.emit_mcp_tools_changed_notifications(all_ui_tools);
        }
    }
    /// The OAuth config map every spawn path must use.
    /// It merges the config file's own OAuth settings with plugin-provided OAuth (client IDs, callbacks) for plugin MCP servers.
    /// Both initial init and the unreachable respawn share it, so a recovered plugin server keeps its OAuth identity.
    pub(super) fn spawn_oauth_config_map(
        &self,
        cwd: &std::path::Path,
    ) -> crate::util::config::McpOAuthConfigMap {
        let (_, mut oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let plugin_registry_snapshot = self.plugin_registry.borrow().clone();
        let plugin_oauth = crate::session::managed_mcp::collect_plugin_oauth_configs(
            plugin_registry_snapshot.as_deref(),
        );
        let toml_mcp_names = crate::util::config::all_toml_mcp_server_names(cwd);
        crate::session::managed_mcp::merge_plugin_oauth_into(
            &mut oauth_config_map,
            plugin_oauth,
            &toml_mcp_names,
        );
        oauth_config_map
    }
    /// Attempt to respawn MCP servers whose last spawn failed as unreachable ([`distill_mcp::servers::McpError::Unreachable`]).
    /// A transient connectivity loss during one startup probe must not strip the session of the server's tools for its remaining lifetime.
    /// Parallel triggers therefore cannot double-spawn, and a stale attempt cannot overwrite a newer client or re-pollute cleaned records.
    pub(super) async fn retry_unreachable_servers(&self) {
        let (attempts, meta_config_map, configs) = {
            let mut state = self.mcp_state.lock().await;
            let attempts = state.take_unreachable_retry_candidates();
            let configs: Vec<acp::McpServer> = state
                .configs
                .iter()
                .filter(|c| attempts.iter().any(|(n, _)| n == mcp_server_name(c)))
                .cloned()
                .collect();
            (attempts, state.meta_config_map.clone(), configs)
        };
        if attempts.is_empty() || configs.is_empty() {
            return;
        }
        let attempt_tokens: std::collections::HashMap<String, u64> = attempts.into_iter().collect();
        let mut unsettled: std::collections::HashMap<String, u64> = attempt_tokens.clone();
        tracing::info!(
            servers = ?attempt_tokens.keys().collect::<Vec<_>>(),
            "Retrying spawn of unreachable MCP servers"
        );
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let oauth_config_map = self.spawn_oauth_config_map(cwd);
        let spawn_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            self.session_info.id.0.as_ref(),
            &spawn_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let results = crate::session::mcp_servers::start_mcp_servers(
            configs,
            Some(cwd),
            &meta_config_map,
            &oauth_config_map,
            &ctx,
        )
        .await;
        let mut recovered = false;
        let mut all_ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for result in results {
            match result {
                Ok(client) => {
                    let server_name = client.server_name().to_string();
                    let Some(&token) = attempt_tokens.get(&server_name) else {
                        continue;
                    };
                    unsettled.remove(&server_name);
                    {
                        let state = self.mcp_state.lock().await;
                        if let Some(tx) = state.client_event_tx() {
                            client.set_event_tx(Some(tx));
                        }
                        attach_elicitation_tx(&state, &client);
                    }
                    let arc = std::sync::Arc::new(client);
                    let registrations =
                        match arc.get_tool_registrations(self.mcp_state.clone()).await {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::debug!(
                                    server = server_name.as_str(),
                                    %e,
                                    "retry_unreachable: handshake still failing"
                                );
                                self.settle_failed_unreachable_attempt(
                                    &server_name,
                                    token,
                                    &e,
                                    Some(arc.clone()),
                                )
                                .await;
                                continue;
                            }
                        };
                    let _ = arc
                        .arm_liveness_watcher(distill_mcp::liveness::DEFAULT_POLL_INTERVAL)
                        .await;
                    let mut mcp_state = self.mcp_state.lock().await;
                    if !mcp_state.finish_unreachable_attempt(&server_name, token) {
                        tracing::info!(
                            server = server_name.as_str(),
                            "retry_unreachable: attempt token stale; discarding client"
                        );
                        arc.discard();
                        continue;
                    }
                    mcp_state
                        .owned_clients
                        .insert(server_name.clone(), arc.clone());
                    let mut ui_tools: std::collections::HashMap<
                        String,
                        Vec<crate::extensions::mcp::McpToolEntry>,
                    > = std::collections::HashMap::new();
                    for reg in registrations {
                        self.register_mcp_tool(&server_name, reg, &mut mcp_state, &mut ui_tools);
                    }
                    drop(mcp_state);
                    all_ui_tools.extend(ui_tools);
                    tracing::info!(
                        server = server_name.as_str(),
                        "MCP server recovered via retry_unreachable (respawn succeeded)"
                    );
                    recovered = true;
                }
                Err(e) => {
                    let Some(sname) = e.server_name().map(str::to_string) else {
                        continue;
                    };
                    let Some(&token) = attempt_tokens.get(&sname) else {
                        continue;
                    };
                    unsettled.remove(&sname);
                    tracing::debug!(
                        server = sname.as_str(),
                        %e,
                        "retry_unreachable: spawn still failing"
                    );
                    self.settle_failed_unreachable_attempt(&sname, token, &e, None)
                        .await;
                }
            }
        }
        for (name, token) in unsettled {
            tracing::debug!(
                server = name.as_str(),
                "retry_unreachable: spawn produced no attributable result"
            );
            self.mcp_state
                .lock()
                .await
                .settle_unreachable_attempt_failed(
                    &name,
                    token,
                    "respawn attempt produced no attributable result".to_string(),
                );
        }
        if recovered {
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            self.emit_mcp_tools_changed_notifications(all_ui_tools);
        }
    }
    /// Settle a failed respawn attempt by error class.
    /// An auth rejection hands off to the auth-required flow, keeping the fresh client (when one exists) for its recovery paths.
    /// Anything else (protocol rejection, malformed `tools/list`, redirect loops) is a terminal init failure.
    async fn settle_failed_unreachable_attempt(
        &self,
        server_name: &str,
        token: u64,
        error: &crate::session::mcp_servers::McpError,
        client_for_auth: Option<std::sync::Arc<crate::session::mcp_servers::McpClient>>,
    ) {
        let detail =
            || distill_tools::util::truncate_str_with_marker(&error.to_string(), 200).into_owned();
        let mut state = self.mcp_state.lock().await;
        if error.is_auth_rejection() {
            if !state.settle_unreachable_attempt_unretryable(server_name, token) {
                return;
            }
            if let Some(client) = client_for_auth {
                state.owned_clients.insert(server_name.to_string(), client);
            }
            state.clear_init_failed(server_name);
            state.record_init_failure(server_name, true, None);
        } else if error.is_transient_connectivity() {
            state.settle_unreachable_attempt_failed(server_name, token, detail());
        } else {
            if !state.settle_unreachable_attempt_unretryable(server_name, token) {
                return;
            }
            state.record_init_failure(server_name, false, Some(detail()));
        }
    }
    /// Refresh the MCP tool/search snapshot from current tool bridge state.
    /// `maybe_inject_mcp_reminder` can then inject the next `<system-reminder>` at a turn boundary.
    /// The `search_tool` description itself stays static (cacheable).
    pub(super) async fn refresh_mcp_snapshot_and_schedule_reminder(&self) {
        let disabled_gateway_tools = crate::util::config::get_all_mcp_disabled_tools(
            std::path::Path::new(&self.session_info.cwd),
        );
        self.refresh_mcp_snapshot_and_schedule_reminder_with_disabled(&disabled_gateway_tools)
            .await;
    }
    /// A refresh that never publishes once `generation` is replaced.
    async fn refresh_mcp_snapshot_for(&self, generation: &crate::session::mcp_servers::Generation) {
        let disabled_gateway_tools = crate::util::config::get_all_mcp_disabled_tools(
            std::path::Path::new(&self.session_info.cwd),
        );
        self.snapshot_refresher(Some(generation.clone()), disabled_gateway_tools)
            .refresh()
            .await;
    }
    pub(super) async fn refresh_mcp_snapshot_and_schedule_reminder_with_disabled(
        &self,
        disabled_gateway_tools: &std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        >,
    ) {
        self.snapshot_refresher(None, disabled_gateway_tools.clone())
            .refresh()
            .await;
    }
    fn snapshot_refresher(
        &self,
        generation: Option<crate::session::mcp_servers::Generation>,
        disabled_gateway_tools: std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        >,
    ) -> SnapshotRefresher {
        SnapshotRefresher {
            generation,
            tool_bridge: self.agent.borrow().tool_bridge().clone(),
            mcp_state: Arc::clone(&self.mcp_state),
            refresh_gate: Arc::clone(&self.mcp_refresh_gate),
            managed_mcp_handle: self.managed_mcp_handle.clone(),
            tool_metadata_snapshot: self.tool_metadata_snapshot.clone(),
            mcp_reminder_dirty: Arc::clone(&self.mcp_reminder_dirty),
            disabled_gateway_tools,
            mcps_root: self.cursor_mcps_root(),
        }
    }
    /// This build never writes descriptor files, so this is always `None`.
    fn cursor_mcps_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Snapshot both MCP and skill announcement tracking state and send it to the persistence channel for atomic write to `announcement_state.json`.
    ///
    /// Called after MCP fingerprint changes, skill update effects, and compaction so that resumed sessions start with accurate tracking state.
    pub(super) async fn persist_announcement_state(&self) {
        let skill_names = self.tool_bridge_handle().get_announced_skill_names().await;
        let (mcp_server_fingerprints, announced_failed) = {
            let announced = self.mcp_announcements.lock();
            (
                crate::session::announcement_state::to_persisted_fingerprints(
                    &announced.fingerprints,
                ),
                announced.persisted_failed(),
            )
        };
        let state = crate::session::announcement_state::AnnouncementState {
            mcp_server_fingerprints,
            announced_skill_names: skill_names,
            announced_failed_servers: announced_failed,
        };
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::AnnouncementState(state));
    }
    /// Inject an MCP server system-reminder if the set changed since the last announcement.
    /// Skips if not dirty.
    /// The dirty flag is cleared up front so a cancelled run degrades to a missed (re-triggerable) injection, never an in-session duplicate.
    pub(super) async fn maybe_inject_mcp_reminder(&self) {
        if !self
            .mcp_reminder_dirty
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let has_restored_fingerprints = !self.mcp_announcements.lock().fingerprints.is_empty();
        let has_live_servers = !self.connected_server_summaries().is_empty();
        {
            let mcp_state = self.mcp_state.lock().await;
            let has_pending_servers =
                !mcp_state.configs.is_empty() || !mcp_state.pending_acp_server_names().is_empty();
            let has_settled_failure =
                !mcp_state.init_failed.is_empty() || !mcp_state.auth_required.is_empty();
            let is_unresolved_restore =
                !has_settled_failure && !has_live_servers && has_restored_fingerprints;
            if has_pending_servers
                && !mcp_state.is_initialized()
                && (mcp_state.is_initializing()
                    || mcp_state.is_init_abandoned()
                    || is_unresolved_restore)
            {
                return;
            }
        }
        use distill_tools::implementations::search_tool::fingerprint_servers;
        self.mcp_reminder_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        struct RearmOnDrop<'a>(Option<&'a std::sync::atomic::AtomicBool>);
        impl Drop for RearmOnDrop<'_> {
            fn drop(&mut self) {
                if let Some(dirty) = self.0.take() {
                    dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        let mut rearm_on_drop = RearmOnDrop(Some(&self.mcp_reminder_dirty));
        let server_summaries = self.connected_server_summaries();
        let new_fingerprints = fingerprint_servers(&server_summaries);
        let clients: Vec<(
            String,
            std::sync::Arc<crate::session::mcp_servers::McpClient>,
        )> = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state
                .all_clients()
                .map(|(n, c)| (n.clone(), std::sync::Arc::clone(c)))
                .collect()
        };
        let mut connected_names: std::collections::HashSet<String> =
            server_summaries.iter().map(|s| s.name.clone()).collect();
        for (name, client) in clients {
            if client.is_ready().await {
                connected_names.insert(name);
            }
        }
        let (currently_failed, unconnected_configured) = {
            let mcp_state = self.mcp_state.lock().await;
            connected_names.retain(|name| !mcp_state.has_failure_record(name));
            classify_failed_servers(&mcp_state, &connected_names)
        };
        let hint = self.rendered_mcp_hint().await;
        let announcements_changed = self.latch_and_push_mcp_reminder(
            &server_summaries,
            new_fingerprints,
            currently_failed,
            &unconnected_configured,
            hint.as_deref(),
        );
        rearm_on_drop.0 = None;
        if announcements_changed {
            self.persist_announcement_state().await;
        }
    }
    /// Latch fingerprints and failure episodes under one lock and push the resulting reminder, if any.
    /// The single lock scope means a concurrent persist cannot snapshot half an update.
    /// A future dropped there by a turn cancel would swallow the announcement for good.
    fn latch_and_push_mcp_reminder(
        &self,
        server_summaries: &[distill_tools::types::tool_index::ServerSummary],
        new_fingerprints: std::collections::HashMap<
            String,
            distill_tools::implementations::search_tool::ServerFingerprint,
        >,
        currently_failed: Vec<crate::session::announcement_state::FailedServer>,
        unconnected_configured: &std::collections::HashSet<String>,
        hint: Option<&str>,
    ) -> bool {
        use distill_tools::implementations::search_tool::{
            build_delta_reminder, build_server_reminder,
        };
        let (mut reminder_text, announcements_changed, to_announce) = {
            let mut announced = self.mcp_announcements.lock();
            let text = match self.mcp_reminder_mode {
                McpReminderMode::Delta => {
                    build_delta_reminder(&announced.fingerprints, server_summaries)
                }
                McpReminderMode::Full => {
                    if announced.fingerprints == new_fingerprints {
                        None
                    } else if server_summaries.is_empty() {
                        Some("All MCP servers have disconnected.".to_string())
                    } else {
                        build_server_reminder(server_summaries)
                    }
                }
            };
            let fingerprints_changed = announced.fingerprints != new_fingerprints;
            if fingerprints_changed {
                announced.fingerprints = new_fingerprints;
            }
            let (to_announce, failed_changed) =
                announced.note_failures(currently_failed, unconnected_configured);
            (text, fingerprints_changed || failed_changed, to_announce)
        };
        let has_failed = !to_announce.is_empty();
        if has_failed {
            reminder_text
                .get_or_insert_with(String::new)
                .push_str(&render_failed_section(&to_announce));
        }
        if let (Some(text), Some(hint)) = (reminder_text.as_mut(), hint) {
            text.push_str(hint);
        }
        if let Some(text) = reminder_text {
            self.push_system_reminder_with_tag(&text, self.reminder_wrapper_tag());
            tracing::info!(
                servers = server_summaries.len(),
                has_failed,
                mode = ?self.mcp_reminder_mode,
                "Injected MCP server system-reminder"
            );
        } else {
            tracing::debug!(
                servers = server_summaries.len(),
                "MCP servers unchanged, skipping reminder injection"
            );
        }
        announcements_changed
    }
    /// Clears the announced episodes and marks the reminder dirty so the next injection re-announces servers that are still down.
    /// Persists the cleared tracking so a resume starts from it.
    /// A rewind's kept prefix usually retains the initial listing, so clearing would inject a duplicate.
    pub(crate) async fn rearm_failed_server_announcements(&self) {
        self.mcp_announcements.lock().rearm_failed();
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.persist_announcement_state().await;
    }
    /// Returns `true` iff `server` has a `Stdio` entry in [`McpState::configs`] and is not on the per-cwd disabled list.
    /// HTTP / HttpAuth entries always return `false` here, which is what the auto-restart task wants.
    /// Performs one synchronous read of the per-cwd disabled-MCP list (`crate::util::config::disabled_mcp_server_names`, which parses `~/.grok/config.toml` + the project `.grok/config.toml`) on every call.
    pub(crate) async fn is_stdio_server_configured(&self, server: &str) -> bool {
        let mcp_state = self.mcp_state.lock().await;
        let is_stdio_in_configs = mcp_state
            .configs
            .iter()
            .any(|c| {
                matches!(c, acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == server)
            });
        if !is_stdio_in_configs {
            return false;
        }
        drop(mcp_state);
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let disabled = crate::util::config::disabled_mcp_server_names(cwd);
        !disabled.contains(server)
    }
    /// HTTP analog of [`Self::is_stdio_server_configured`]: `true` iff `server` has an enabled `Http` / `Sse` config entry.
    /// Gates [`crate::session::mcp_restart::maybe_schedule_http_recovery`].
    pub(crate) async fn is_http_server_configured(&self, server: &str) -> bool {
        let mcp_state = self.mcp_state.lock().await;
        let is_http_in_configs = mcp_state
            .configs
            .iter()
            .any(|c| {
                matches!(
                c,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) | acp::McpServer::Sse(acp::McpServerSse { name, .. }) if name == server
            )
            });
        if !is_http_in_configs {
            return false;
        }
        drop(mcp_state);
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let disabled = crate::util::config::disabled_mcp_server_names(cwd);
        !disabled.contains(server)
    }
    /// Recover a dead HTTP client in place via [`McpClient::recover`] (reset, re-handshake, restart the liveness watcher).
    /// Unlike [`Self::respawn_stdio`] the existing `Arc<McpClient>` is kept, so its tools stay valid.
    /// Report the race instead of a false success on a detached client.
    pub(crate) async fn reset_http_client(&self, server: &str) -> Result<(), String> {
        let client = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state.get_client(server).cloned()
        };
        let Some(client) = client else {
            return Err(format!("no client for server '{server}'"));
        };
        if !client.is_http() {
            return Err(format!("server '{server}' is not an HTTP client"));
        }
        client.recover().await.map_err(|e| e.to_string())?;
        let still_current = self.mcp_state.lock().await.has_client(server, &client);
        if !still_current || !self.is_http_server_configured(server).await {
            client.set_liveness_handle(None);
            return Err(format!(
                "server '{server}' was removed or disabled during HTTP recovery"
            ));
        }
        Ok(())
    }
    pub(super) fn apply_mcp_config_diff(
        &self,
        diff: &crate::session::mcp_servers::McpConfigDiff,
        dispatch_event_tx: Option<
            tokio::sync::mpsc::UnboundedSender<distill_mcp::servers::McpClientEvent>,
        >,
    ) {
        if (!diff.added.is_empty() || !diff.removed.is_empty())
            && let Some(tx) = dispatch_event_tx
        {
            let _ = tx.send(distill_mcp::servers::McpClientEvent::ConfigDiff {
                added: diff.added.clone(),
                removed: diff.removed.clone(),
            });
        }
        for name in &diff.removed {
            self.unregister_server_tools(name);
        }
    }
    pub(super) async fn start_mcp_servers_after_config_change(
        &self,
        change: crate::session::mcp_servers::McpConfigChange,
    ) {
        self.run_startup(move |actor| {
            Box::pin(async move {
                if !change.diff.removed.is_empty() {
                    actor.refresh_mcp_snapshot_and_schedule_reminder().await;
                }
                actor.run_mcp_init_with_claim(change.claim).await;
            })
        })
        .await;
    }
    /// Sync, so the bridge and the searchable snapshot lose the server before the caller can yield.
    pub(crate) fn unregister_server_tools(&self, server: &str) {
        let prefix = format!(
            "{}{}",
            server,
            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
        );
        let removed = self
            .agent
            .borrow()
            .tool_bridge()
            .unregister_tools_by_prefix(&prefix);
        {
            let mut snapshot = self.tool_metadata_snapshot.lock().unwrap();
            snapshot.tools.retain(|t| t.server_name != server);
            snapshot.servers.retain(|s| s.name != server);
        }
        if removed > 0 {
            tracing::info!(
                server = %server,
                tools_removed = removed,
                "unregistered MCP server tools",
            );
        }
    }
    /// Stdio-only restart: handshake, start the liveness watcher, then atomically install the new `Arc<McpClient>`.
    /// Wire `set_event_tx` after `ensure_initialized` so a restart emits only `RestartSucceeded`, not a second `Initialized` from `Ready`.
    /// Re-check `is_stdio_server_configured` before insert; a disable during the long start must drop the new client (`kill_on_drop`) instead of installing it.
    pub(crate) async fn respawn_stdio(
        &self,
        server: &str,
    ) -> Result<crate::session::mcp_restart::Respawn, String> {
        use crate::session::mcp_restart::Respawn;
        self.wait_for_server_settled(server).await;
        let (server_config, meta_config, event_tx, generation, replacing) = {
            let mcp_state = self.mcp_state.lock().await;
            let Some(server_config) = mcp_state
                .configs
                .iter()
                .find(|c| {
                    matches!(c, acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == server)
                })
                .cloned() else {
                return Ok(Respawn::Superseded);
            };
            let meta_config = mcp_state.meta_config_map.get(server).cloned();
            let event_tx = mcp_state.client_event_tx();
            let generation = mcp_state.current_generation();
            let replacing = mcp_state.get_client(server).cloned();
            (server_config, meta_config, event_tx, generation, replacing)
        };
        if let Some(client) = &replacing
            && client.is_healthy().await
        {
            return Ok(Respawn::Superseded);
        }
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let session_id = self.session_info.id.0.as_ref();
        let (_, oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let byo_config = oauth_config_map.get(server).cloned();
        let event_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &event_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let new_client = crate::session::mcp_servers::start_mcp_server(
            server_config.clone(),
            Some(cwd),
            meta_config.as_ref(),
            byo_config.as_ref(),
            &ctx,
        )
        .await
        .map_err(|e| e.to_string())?;
        attach_elicitation_tx(&*self.mcp_state.lock().await, &new_client);
        new_client
            .ensure_initialized()
            .await
            .map_err(|e| e.to_string())?;
        if !self.is_stdio_server_configured(server).await {
            drop(new_client);
            return Err(format!(
                "server '{server}' was disabled or removed during respawn"
            ));
        }
        if let Some(tx) = &event_tx {
            new_client.set_event_tx(Some(tx.clone()));
        }
        let arc_client = std::sync::Arc::new(new_client);
        let _ = arc_client
            .arm_liveness_watcher(distill_mcp::liveness::DEFAULT_POLL_INTERVAL)
            .await;
        let installed = self
            .mcp_state
            .write_if_slot_is(server, replacing.as_ref(), &generation, |mcp_state| {
                mcp_state
                    .owned_clients
                    .insert(server.to_string(), std::sync::Arc::clone(&arc_client));
                mcp_state.clear_init_failed(server);
            })
            .await;
        if installed.is_err() {
            arc_client.discard();
            return Ok(Respawn::Superseded);
        }
        if let Some(tx) = event_tx {
            let _ = tx.send(distill_mcp::servers::McpClientEvent::ToolsChanged {
                server: server.to_string(),
            });
        }
        Ok(Respawn::Installed)
    }
    pub(super) async fn maybe_inject_mcp_connecting_reminder(&self) {
        if self.mcp_connecting_reminder_injected.get() {
            return;
        }
        let connecting: Vec<String> = {
            let mcp_state = self.mcp_state.lock().await;
            let mut names: Vec<String> = mcp_state.handshaking_servers_iter().cloned().collect();
            names.sort_unstable();
            names
        };
        if connecting.is_empty() {
            return;
        }
        self.mcp_connecting_reminder_injected.set(true);
        let delivery_tools = self.delivery_tools.borrow().clone();
        let text = format_mcp_connecting_reminder(&connecting, &delivery_tools);
        self.push_system_reminder(&text);
        tracing::info!(
            servers = ?connecting,
            ?delivery_tools,
            "Injected MCP connecting system-reminder"
        );
    }
    /// `deliveryTools` sessions emit user-visible output only through MCP tools, so both gates hold full waits for them, under either strategy.
    pub(super) fn requires_full_mcp_wait(&self) -> bool {
        !self.delivery_tools.borrow().is_empty()
    }
    /// Re-apply the attaching client's per-attachment policy.
    /// The resident `session/load` rail sends this when the request carries explicit `startupHints`.
    /// Only policy fields are touched; structural spawn-time hints (subagent identity, inherited prefix, preserved system head) stay frozen.
    pub(super) fn apply_attach_policy(&self, hints: &crate::session::StartupHints) {
        let strategy = hints.resolve_mcp_strategy();
        let changed = self.mcp_strategy.get() != strategy
            || self.attach_non_interactive.get() != hints.non_interactive
            || *self.delivery_tools.borrow() != hints.delivery_tools;
        self.mcp_strategy.set(strategy);
        self.attach_non_interactive.set(hints.non_interactive);
        *self.delivery_tools.borrow_mut() = hints.delivery_tools.clone();
        if changed {
            self.mcp_connecting_reminder_injected.set(false);
        }
        tracing::info!(
            ?strategy,
            non_interactive = hints.non_interactive,
            delivery_tools = ?hints.delivery_tools,
            changed,
            "apply_attach_policy: updated per-attachment policy from session request startupHints"
        );
    }
    fn cancel_superseded_init(
        &self,
        generation: &crate::session::mcp_servers::Generation,
        init_claim: crate::session::mcp_servers::InitClaimGuard,
    ) {
        drop(init_claim);
        let reason = init_cancelled_reason(generation);
        tracing::info!(session_id = %self.session_info.id.0, reason, "MCP init superseded; stopping");
        self.events
            .emit(distill_session_events::Event::McpInitCancelled {
                reason: reason.to_string(),
            });
    }
    /// Concludes a pass with nothing to handshake.
    async fn finish_init_without_handshakes(
        &self,
        generation: &crate::session::mcp_servers::Generation,
        init_claim: crate::session::mcp_servers::InitClaimGuard,
    ) {
        if self
            .mcp_state
            .write_if_current(generation, |mcp_state| mcp_state.finish_init())
            .await
            .is_err()
        {
            self.cancel_superseded_init(generation, init_claim);
            return;
        }
        self.refresh_mcp_snapshot_for(generation).await;
        let publication = self.init_publication();
        let completed = self
            .mcp_state
            .write_if_current(generation, |mcp_state| publication.complete(mcp_state))
            .await;
        if completed.is_err() {
            self.cancel_superseded_init(generation, init_claim);
            return;
        }
        if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": self.session_info.id.0.as_ref(),
            "mcpToolCount": 0_u32,
            "elapsedMs": 0_u64,
        })) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/mcp_initialized",
                    params.into(),
                ));
        }
    }
    /// An install from outside the pass waits until no live pass can still hand `server` a client.
    pub(super) async fn wait_for_server_settled(&self, server: &str) {
        let signal = self.mcp_state.lock().await.init_wait_signal();
        loop {
            let notified = signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.mcp_state.lock().await.is_server_pending(server) {
                return;
            }
            notified.await;
        }
    }
    /// A server-set change hands its successor pass the claim and resets what turns read about init, in one step.
    pub(super) fn update_mcp_configs(
        &self,
        mcp_state: &mut McpState,
        configs: Vec<acp::McpServer>,
    ) -> Option<crate::session::mcp_servers::McpConfigChange> {
        let change = mcp_state.change_configs(configs)?;
        self.init_publication().reset();
        Some(change)
    }
    pub(super) fn restart_mcp_init(
        &self,
        mcp_state: &mut McpState,
    ) -> crate::session::mcp_servers::InitClaimGuard {
        self.init_publication().reset();
        mcp_state.restart_init()
    }
    /// Starts init if nothing owns it and waits for the pass to reach `finish_init`. Returns `false` once the run
    /// loop has ended and nothing can start init.
    pub(super) async fn ensure_mcp_tools_initialized(&self) -> bool {
        self.run_startup(|actor| Box::pin(actor.start_mcp_init()))
            .await
    }
    /// Runs `startup` as a run-loop-owned task and waits for it, so a dropped caller abandons only its wait.
    /// Returns `false` once the run loop has ended and nothing can run it.
    async fn run_startup<F>(&self, startup: F) -> bool
    where
        F: for<'a> FnOnce(&'a SessionActor) -> futures::future::LocalBoxFuture<'a, ()> + 'static,
    {
        let Some(actor) = self.weak_self.upgrade() else {
            startup(self).await;
            return true;
        };
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = async move {
            startup(&actor).await;
            let _ = done_tx.send(());
        };
        match self.startup_tasks.spawn_local(task) {
            StartupHandoff::Spawned => {
                let _ = done_rx.await;
            }
            StartupHandoff::NoRunLoopYet(task) => task.await,
            StartupHandoff::RunLoopEnded => return false,
        }
        true
    }
    async fn start_mcp_init(&self) {
        use tracing::Instrument;
        match self.mcp_startup_reroot_span() {
            Some(span) => self.run_mcp_init(true).instrument(span).await,
            None => self.run_mcp_init(false).await,
        }
    }
    fn mcp_startup_reroot_span(&self) -> Option<tracing::Span> {
        let tp = self.startup_hints.take_mcp_reroot_traceparent()?;
        let span = tracing::info_span!("session.mcp_startup", session_id = %self.session_info.id.0);
        distill_otel::link_span_to_meta(&span, &serde_json::json!({ "traceparent": tp }))
            .then_some(span)
    }
    async fn claim_init(&self) -> Option<InitClaim> {
        let mut mcp_state = self.mcp_state.lock().await;
        let guard = mcp_state.try_start_init()?;
        Some(self.claim_with(&mut mcp_state, guard))
    }
    fn claim_with(
        &self,
        mcp_state: &mut McpState,
        guard: crate::session::mcp_servers::InitClaimGuard,
    ) -> InitClaim {
        tracing::info!(
            session_id = %self.session_info.id.0,
            config_count = mcp_state.configs.len(),
            config_names = ?mcp_state.configs.iter().map(crate::session::mcp_servers::mcp_server_name).collect::<Vec<_>>(),
            existing_client_count = mcp_state.owned_clients.len() + mcp_state.shared_clients.len(),
            "ensure_mcp_tools_initialized: starting MCP init"
        );
        mcp_state.set_event_writer(self.events.writer());
        if mcp_state.disabled_tools.is_empty() {
            let cwd = std::path::Path::new(&self.session_info.cwd);
            let dt = crate::util::config::get_all_mcp_disabled_tools(cwd);
            if !dt.is_empty() {
                tracing::info!(servers = dt.len(), "Loaded disabled_tools from config");
                mcp_state.disabled_tools = dt;
            }
        }
        InitClaim {
            configs: mcp_state.configs.clone(),
            meta_config_map: mcp_state.meta_config_map.clone(),
            generation: mcp_state.current_generation(),
            existing_client_names: mcp_state.owned_clients.keys().cloned().collect(),
            has_servers_to_spawn: !mcp_state.configs.is_empty() || mcp_state.has_acp_servers(),
            guard,
        }
    }
    async fn run_mcp_init(&self, reroot_active: bool) {
        let Some(claim) = self.claim_init().await else {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "ensure_mcp_tools_initialized: skipped (already initialized or in progress)"
            );
            return;
        };
        self.run_init_pass(claim, reroot_active).await;
    }
    /// The caller's claim (a harness rebuild) becomes the pass's; a claim a config change released starts nothing.
    pub(super) async fn run_mcp_init_with_claim(
        &self,
        guard: crate::session::mcp_servers::InitClaimGuard,
    ) {
        let Ok(claim) = self
            .mcp_state
            .write_if_owner(guard, |mcp_state, guard| self.claim_with(mcp_state, guard))
            .await
        else {
            return;
        };
        self.run_init_pass(claim, false).await;
    }
    async fn run_init_pass(&self, claim: InitClaim, reroot_active: bool) {
        let generation = claim.generation.clone();
        if self
            .register_shared_client_tools(&generation)
            .await
            .is_err()
        {
            self.cancel_superseded_init(&generation, claim.guard);
            return;
        }
        if !claim.has_servers_to_spawn {
            self.finish_init_without_handshakes(&generation, claim.guard)
                .await;
            return;
        }
        let configs_to_start: Vec<_> = claim
            .configs
            .iter()
            .filter(|c| !claim.existing_client_names.contains(mcp_server_name(c)))
            .cloned()
            .collect();
        let marked = self
            .mcp_state
            .write_if_current(&generation, |mcp_state| {
                let acp_pending_names = mcp_state.pending_acp_server_names();
                let names: Vec<String> = configs_to_start
                    .iter()
                    .map(|c| mcp_server_name(c).to_string())
                    .chain(acp_pending_names.iter().cloned())
                    .collect();
                for name in &names {
                    tracing::info!(server = %name, "Added server to handshaking set");
                }
                mcp_state.mark_servers_initializing(names);
                acp_pending_names
            })
            .await;
        let Ok(acp_pending_names) = marked else {
            self.cancel_superseded_init(&generation, claim.guard);
            return;
        };
        self.events
            .emit(crate::session::mcp_servers::build_config_resolved_event(
                &claim.configs,
                std::path::Path::new(&self.session_info.cwd),
            ));
        self.mcp_connecting_reminder_injected.set(false);
        let init_total = (configs_to_start.len() + acp_pending_names.len()) as u32;
        if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
            "total": init_total,
            "connected": 0,
            "sessionId": self.session_info.id.0.as_ref(),
        })) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    crate::extensions::mcp::mcp_methods::INIT_PROGRESS,
                    params.into(),
                ));
        }
        if configs_to_start.is_empty() && acp_pending_names.is_empty() {
            self.finish_init_without_handshakes(&generation, claim.guard)
                .await;
            return;
        }
        let mcp_init_start = std::time::Instant::now();
        let mut timer = crate::instrumentation_timer!("session.mcp_init");
        timer.with_field("session_id", self.session_info.id.0.as_ref());
        timer.with_field("server_count", configs_to_start.len() as u64);
        tracing::info!(
            "Starting MCP initialization ({} new servers, {} already initialized, strategy: {:?})",
            configs_to_start.len(),
            claim.existing_client_names.len(),
            self.mcp_strategy.get()
        );
        let session_id = self.session_info.id.0.as_ref();
        tokio::task::yield_now().await;
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let oauth_config_map = self.spawn_oauth_config_map(cwd);
        let spawn_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &spawn_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let Ok(mcp_results) = generation
            .or_cancel(build_pending_clients(
                &self.mcp_state,
                configs_to_start,
                Some(cwd),
                &claim.meta_config_map,
                &oauth_config_map,
                &ctx,
            ))
            .await
        else {
            self.cancel_superseded_init(&generation, claim.guard);
            return;
        };
        tokio::task::yield_now().await;
        let mut spawn_auth_failures: Vec<String> = Vec::new();
        let mut spawn_unreachable_failures: Vec<(String, String)> = Vec::new();
        let mcp_clients: Vec<_> = mcp_results
            .into_iter()
            .filter_map(|result| match result {
                Ok(client) => {
                    tracing::debug!("MCP server '{}' spawned", client.server_name());
                    Some(client)
                }
                Err(e) => {
                    tracing::warn!("Failed to spawn MCP server: {}", e);
                    let sname = e.server_name().unwrap_or("unknown").to_string();
                    if e.is_auth_rejection() && sname != "unknown" {
                        spawn_auth_failures.push(sname.clone());
                    } else if e.is_unreachable() && sname != "unknown" {
                        spawn_unreachable_failures.push((sname.clone(), e.to_string()));
                    }
                    let cfg = claim
                        .configs
                        .iter()
                        .find(|c| mcp_server_name(c) == sname.as_str());
                    self.events
                        .emit(distill_session_events::Event::McpServerFailed {
                            server_name: sname,
                            transport: cfg.map(|c| mcp_transport_str(c).to_string()),
                            target: cfg.map(mcp_target_str),
                            error_type: e.error_category(),
                            error_message: e.to_string(),
                            duration_ms: None,
                            timeout_sec: None,
                        });
                    None
                }
            })
            .collect();
        let spawned_names: std::collections::HashSet<String> = mcp_clients
            .iter()
            .map(|c| c.server_name().to_string())
            .collect();
        let finished = self
            .mcp_state
            .write_if_current(&generation, |mcp_state| {
                let failed_spawns: Vec<String> = mcp_state
                    .handshaking_servers_iter()
                    .filter(|name| !spawned_names.contains(name.as_str()))
                    .cloned()
                    .collect();
                for name in &failed_spawns {
                    tracing::warn!(
                        server = name.as_str(),
                        "MCP server spawn failed, removing from initializing set"
                    );
                    if spawn_auth_failures.iter().any(|n| n == name) {
                        mcp_state.record_init_failure(name, true, None);
                    } else if let Some((_, detail)) =
                        spawn_unreachable_failures.iter().find(|(n, _)| n == name)
                    {
                        mcp_state.record_unreachable_failure(name, detail.clone());
                    }
                    mcp_state.mark_server_ready(name);
                }
                mcp_state.finish_init();
                failed_spawns
            })
            .await;
        let Ok(failed_spawns) = finished else {
            self.cancel_superseded_init(&generation, claim.guard);
            return;
        };
        let refresher = self.snapshot_refresher(
            Some(generation.clone()),
            crate::util::config::get_all_mcp_disabled_tools(std::path::Path::new(
                &self.session_info.cwd,
            )),
        );
        let mut pass = InitPass::new(
            self,
            claim,
            mcp_clients,
            acp_pending_names.len(),
            init_total,
            mcp_init_start,
        );
        for name in failed_spawns {
            let needs_auth = spawn_auth_failures.contains(&name);
            pass.tally_spawn_failure(name, needs_auth);
        }
        let mcp_init_task_parent = if reroot_active {
            Parent::Inherit
        } else {
            Parent::Root
        };
        spawn_after_reaping(
            &mut self.mcp_init_tasks.borrow_mut(),
            instrument_task!(
                "session.mcp_init_task",
                mcp_init_task_parent,
                pass.run(refresher)
            ),
        );
    }
    /// Summaries of the currently connected MCP servers, from the live tool-metadata snapshot.
    /// The single source for every consumer of the server list.
    pub(crate) fn connected_server_summaries(
        &self,
    ) -> Vec<distill_tools::types::tool_index::ServerSummary> {
        use distill_tools::types::tool_index::ToolSearchIndex;
        crate::session::tool_index::Bm25ToolSearchIndex::new(self.tool_metadata_snapshot.clone())
            .list_server_summaries()
    }
    /// Render the tool usage hint appended to every injected MCP reminder body, with the session's tool names substituted.
    /// Shared by the injector and the `/context` estimate.
    /// `None` when the template fails to render.
    async fn rendered_mcp_hint(&self) -> Option<String> {
        let hint_template = "\nTo use MCP tools, you MUST call `${{ tools.by_kind.search_tool }}` first to retrieve the tool's input schema before calling `${{ tools.by_kind.use_tool }}`. NEVER guess parameter names — always use the exact schema returned by `${{ tools.by_kind.search_tool }}`.";
        self.tool_bridge_handle()
            .render_prompt(hint_template, &serde_json::json!({}))
            .await
    }
    /// The full MCP announcement for the current server set, for `/context` accounting.
    /// Returns `None` when no servers are connected, or when the active template carries MCP in its first user message rather than in reminders.
    /// Known approximations: the default reminder mode is `Delta`, which injects incremental texts rather than this full listing.
    pub(super) async fn mcp_announcement_snapshot(&self) -> Option<McpAnnouncementSnapshot> {
        let server_summaries = self.connected_server_summaries();
        let mut text =
            distill_tools::implementations::search_tool::build_server_reminder(&server_summaries)?;
        if let Some(hint) = self.rendered_mcp_hint().await {
            text.push_str(&hint);
        }
        Some(McpAnnouncementSnapshot {
            text,
            server_count: server_summaries.len(),
        })
    }
}
/// The MCP server announcement as rendered by `mcp_announcement_snapshot`.
/// The MCP counterpart of `SkillListingSnapshot`.
pub(super) struct McpAnnouncementSnapshot {
    /// The announcement body: server listing plus the tool usage hint.
    pub(super) text: String,
    pub(super) server_count: usize,
}
/// On surfaces that declare delivery tools the user sees output only through those MCP tools.
/// Keying on the explicit opt-in rather than on `nonInteractive` keeps the default for every other client.
/// The right guidance there is to keep working and deliver through the tool, not to skip it.
pub(super) fn format_mcp_connecting_reminder(
    connecting: &[String],
    delivery_tools: &[String],
) -> String {
    let mut text =
        "MCP servers currently connecting (tools will become available shortly):\n".to_string();
    for name in connecting {
        text.push_str(&format!("- {name}\n"));
    }
    if delivery_tools.is_empty() {
        text.push_str(
            "\nDo not attempt to use tools from these servers yet. \
             If the user's request likely requires one of these servers, \
             mention that the server is still connecting and proceed with \
             what you can do in the meantime.",
        );
    } else {
        text.push_str(&format!(
            "\nThese servers are being awaited and their tools are expected \
             to become available as you work — use them normally, and if a \
             call reports the tool as unavailable, retry it after your other \
             work rather than giving up. User-visible output from this \
             session is delivered ONLY through: {}. Do NOT end the turn \
             without delivering your answer through the appropriate \
             delivery tool.",
            delivery_tools.join(", ")
        ));
    }
    text
}
