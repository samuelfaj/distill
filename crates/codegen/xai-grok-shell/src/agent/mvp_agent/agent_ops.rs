#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! Inherent [`MvpAgent`] helpers (MCP/clients/gateway, settings/models, session ops, spawn).
//! Co-located child of `mvp_agent` (`use super::*`).
use super::*;
use crate::sampling::EffortTarget;
use xai_grok_login::PreferredAuthMethod;
use crate::upload::trace::PromptMetadataParams;
use xai_grok_tools::implementations::grok_build::task::backend::SubagentBackend;
use xai_tty_utils::ProcessScope;
struct SessionConfigInputs {
    model_id: acp::ModelId,
    effort_options: Vec<ReasoningEffortOption>,
    current_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
}
/// `preferred` model, else catalog `current`, else first with own credentials.
fn byok_from_models(
    models: &indexmap::IndexMap<String, ModelEntry>,
    preferred: Option<&str>,
    current: &str,
) -> Option<String> {
    preferred
        .and_then(|id| models.get(id))
        .and_then(|m| m.own_credential())
        .or_else(|| models.get(current).and_then(|m| m.own_credential()))
        .or_else(|| models.values().find_map(|m| m.own_credential()))
}
struct MissingSessionCtx {
    has_session_key: bool,
    has_own_credentials: bool,
    is_session_based_auth: bool,
    preferred: Option<PreferredAuthMethod>,
}
/// Warn only when a missing session is a real failure, not on API-key hosts.
fn should_warn_missing_session(ctx: MissingSessionCtx) -> bool {
    if ctx.has_session_key || ctx.has_own_credentials {
        return false;
    }
    match ctx.preferred {
        Some(PreferredAuthMethod::Oidc) => true,
        Some(PreferredAuthMethod::ApiKey) => false,
        None => ctx.is_session_based_auth,
    }
}
/// The stored fields a settings poll may replace. Snapshotted before the fetch so a full reapply landing mid-fetch makes the poll skip.
type PolledFields = (
    Option<Vec<xai_grok_announcements::RemoteAnnouncement>>,
    Vec<xai_grok_config_types::RemoteRequestEncoding>,
);
fn polled_fields(settings: &crate::util::config::RemoteSettings) -> PolledFields {
    (settings.announcements.clone(), settings.accept_request_encodings.clone())
}
impl MvpAgent {
    /// Announce a session's new title over ACP.
    /// ACP scopes `session/update` to sessions the client established, and a rename can name a history row it never loaded.
    /// So the liveness check belongs here rather than at each call site.
    pub(crate) fn notify_session_info_update(
        &self,
        session_id: &agent_client_protocol::SessionId,
        title: &str,
    ) {
        if self.is_resident(session_id) {
            self.gateway
                .forward_fire_and_forget(
                    crate::session::summary::session_info_update_manual(
                        session_id.clone(),
                        title,
                    ),
                );
        }
    }
    pub fn reload_skills_all_sessions(&self) -> usize {
        let session_ids = self.resident_ids();
        for sid in &session_ids {
            if let Some(handle) = self.resident_handle(sid) {
                let _ = handle.cmd_tx.send(SessionCommand::ReloadSkills);
            }
        }
        session_ids.len()
    }
    pub fn advertise_commands_all_sessions(&self) -> usize {
        let session_ids = self.resident_ids();
        for session_id in &session_ids {
            if let Some(handle) = self.resident_handle(session_id) {
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::AdvertiseCommands {
                        trigger: crate::session::AdvertiseTrigger::WorkflowsChanged,
                    });
            }
        }
        session_ids.len()
    }
    pub(super) fn resolve_image_description_model(&self) -> String {
        self.cfg
            .borrow()
            .image_description_model
            .as_deref()
            .unwrap_or(crate::models::default_image_description_model())
            .to_owned()
    }
    fn resolve_session_summary_model(&self) -> String {
        self.cfg
            .borrow()
            .session_summary_model
            .as_deref()
            .unwrap_or(crate::models::default_session_summary_model())
            .to_owned()
    }
    pub(super) fn build_summary_client(
        &self,
        primary: &SamplingConfig,
    ) -> Result<(OaiCompatClient, String), acp::Error> {
        let slug = self.resolve_session_summary_model();
        let session_key = self.auth_manager.current_or_expired().map(|a| a.key.clone());
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let (disable_api_key_auth, alpha_test_key, client_version) = {
            let cfg = self.cfg.borrow();
            (
                cfg.grok_com_config.api_key_auth_disabled(),
                cfg.endpoints.alpha_test_key.clone(),
                cfg.client_version.clone(),
            )
        };
        let config = match crate::agent::config::resolve_aux_model_sampling_config(
            &slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            alpha_test_key,
            client_version,
        ) {
            Some(mut cfg) => {
                crate::agent::config::stamp_session_local_sampler_fields(
                    &mut cfg,
                    primary,
                    primary.client_identifier.clone(),
                    primary.max_retries,
                );
                cfg
            }
            None => {
                let mut fallback = primary.clone();
                fallback.model = slug;
                fallback
            }
        };
        let model = config.model.clone();
        let client = OaiCompatClient::new(config).map_err(map_sampling_err_to_acp)?;
        Ok((client, model))
    }
    fn has_proxy_credentials(&self) -> bool {
        self.cfg.borrow().endpoints.deployment_key.is_some()
            || self.auth_manager.current_or_expired().is_some_and(|a| a.is_xai_auth())
    }
    /// `true` for session-based ACP auth methods.
    fn is_session_based_auth(&self) -> bool {
        self.auth_method_id
            .load()
            .as_deref()
            .is_some_and(crate::agent::auth_method::is_session_based_method)
    }
    /// Publish the current ACP auth method into the shared live handle so every running session's per-turn auth gate observes it on its next turn.
    pub(super) fn set_auth_method(&self, id: acp::AuthMethodId) {
        self.auth_method_id.store(Some(std::sync::Arc::new(id)));
    }
    /// Publish model-owned credentials for voice/tools static fallthrough.
    /// Only [`ModelEntry::own_credential`], not `sampling_config.api_key` (which may be a session JWT).
    pub(crate) fn sync_process_static_api_key(&self, preferred_model_id: Option<&str>) {
        if self.cfg.borrow().grok_com_config.api_key_auth_disabled() {
            self.auth_manager.set_process_static_api_key(None);
            return;
        }
        let models = self.models_manager.models();
        let current = self.models_manager.current_model_id();
        self.auth_manager
            .set_process_static_api_key(
                byok_from_models(&models, preferred_model_id, current.0.as_ref()),
            );
    }
    /// Return auth for sync config construction.
    pub(super) fn current_or_buffered_auth(&self) -> Option<xai_grok_login::GrokAuth> {
        self.auth_manager
            .current()
            .or_else(|| {
                if self.is_session_based_auth() {
                    let auth = self.auth_manager.expired_auth();
                    if auth.is_some() {
                        xai_grok_telemetry::unified_log::info(
                            "auth buffered token fallback",
                            None,
                            None,
                        );
                    }
                    auth
                } else {
                    None
                }
            })
    }
    fn has_managed_mcp_auth(&self) -> bool {
        self.auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_managed_mcp_eligible())
    }
    fn can_fetch_managed_mcp_gateway_tools(&self) -> bool {
        self.cfg.borrow().managed_mcp_gateway_tools_enabled
            && self.has_managed_mcp_auth()
    }
    pub(crate) async fn get_managed_mcp_gateway_tool_catalog(
        &self,
    ) -> Option<crate::session::managed_mcp::GatewayToolCatalog> {
        if !self.can_fetch_managed_mcp_gateway_tools() {
            self.managed_mcp_cache.lock().await.disable_gateway_tools();
            return None;
        }
        self.managed_mcp_cache.lock().await.enable_gateway_tools();
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let auth_key = self
            .auth_manager
            .get_valid_token()
            .await
            .ok()
            .or_else(|| self.auth_manager.current_or_expired().map(|a| a.key));
        crate::session::managed_mcp::get_or_fetch_gateway_tool_catalog(
                &self.managed_mcp_cache,
                &proxy_url,
                auth_key.as_deref(),
            )
            .await
    }
    pub(crate) fn managed_mcp_cache(
        &self,
    ) -> &crate::session::managed_mcp::ManagedMcpStateHandle {
        &self.managed_mcp_cache
    }
    pub(crate) fn disable_managed_gateway_tools_and_refresh_sessions(&self) {
        self.disable_managed_gateway_tools_and_refresh_sessions_with_txs({
            let mut txs = Vec::new();
            self.session_registry
                .for_each_resident(|_, handle| {
                    txs.push(handle.cmd_tx.clone());
                });
            txs
        });
    }
    fn disable_managed_gateway_tools_and_refresh_sessions_with_txs(
        &self,
        session_txs: Vec<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    ) {
        let cache = self.managed_mcp_cache.clone();
        tokio::task::spawn_local(async move {
            cache.lock().await.disable_gateway_tools();
            for tx in session_txs {
                let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
            }
        });
    }
    pub(crate) fn spawn_managed_gateway_tool_catalog_fetch(&self) {
        let session_txs = self.resident_cmd_txs();
        if !self.can_fetch_managed_mcp_gateway_tools() {
            self.disable_managed_gateway_tools_and_refresh_sessions_with_txs(
                session_txs,
            );
            return;
        }
        let cache = self.managed_mcp_cache.clone();
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let auth_manager = self.auth_manager.clone();
        tokio::task::spawn_local(async move {
            let auth_key = auth_manager
                .get_valid_token()
                .await
                .ok()
                .or_else(|| auth_manager.current_or_expired().map(|a| a.key));
            if !auth_manager
                .current_or_expired()
                .is_some_and(|a| a.is_managed_mcp_eligible())
            {
                cache.lock().await.disable_gateway_tools();
                for tx in session_txs {
                    let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
                }
                return;
            }
            cache.lock().await.enable_gateway_tools();
            crate::session::managed_mcp::get_or_fetch_gateway_tool_catalog(
                    &cache,
                    &proxy_url,
                    auth_key.as_deref(),
                )
                .await;
            for tx in session_txs {
                let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
            }
        });
    }
    /// Rebuild `search_tool` in every live session after a fresh gateway tool catalog committed.
    /// Gateway tools live in the agent-level catalog, not per-session `McpServers`.
    /// Callers skip on a failed refetch so the last-good index stays.
    pub(crate) fn refresh_mcp_search_index_in_sessions(&self) {
        let session_txs = self.resident_cmd_txs();
        for tx in session_txs {
            let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
        }
    }
    /// `mcp/list` catalog fetch: optional cache bust, then gateway list, then fan `RefreshMcpSearchIndex` only when a fresh catalog commits.
    /// Gateway off (or no eligible auth) goes through [`Self::get_managed_mcp_gateway_tool_catalog`].
    /// That path disables the cache the same way initialize does.
    pub(crate) async fn fetch_gateway_catalog_for_mcp_list(
        &self,
        cache: bool,
    ) -> Option<crate::session::managed_mcp::GatewayToolCatalog> {
        if !cache {
            crate::session::managed_mcp::invalidate_gateway_tool_cache(
                    self.managed_mcp_cache(),
                )
                .await;
        }
        let catalog = self.get_managed_mcp_gateway_tool_catalog().await;
        if !cache && catalog.is_some() {
            self.refresh_mcp_search_index_in_sessions();
        }
        catalog
    }
    /// Resolve the launch dir's project-scope trust verdict ONCE and return it with its path. Memoizes the single [`folder_trust::resolve_launch_dir_trust`] gather (see it for the dedup and TOCTOU contract).
    /// Backs `ensure_local_workspace_ops`. The plugin registry builds deliberately do not reuse it: they read disk config through the trust gate, so they resolve (and record) afresh right before the read — see `registry_build_inputs`.
    pub(super) fn prime_launch_dir_trust(&self) -> (&std::path::Path, bool) {
        let trust = *self
            .launch_dir_trust
            .get_or_init(|| {
                let remote_settings = self.cfg.borrow().remote_settings.clone();
                folder_trust::resolve_launch_dir_trust(
                    &self.launch_cwd,
                    remote_settings.as_ref(),
                )
            });
        (&self.launch_cwd, trust)
    }
    /// Resolve folder trust and load launch-dir MCP configs after `initialize` returns.
    /// The walks are synchronous and expensive in large monorepos.
    /// They must not block the ACP response (grok-desktop sends `initialize` immediately).
    pub(super) fn spawn_initialize_launch_mcp_setup(&self) {
        let cwd = self.launch_cwd.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let gateway = self.gateway.clone();
        let agent_mcp_state = self.agent_mcp_state.clone();
        tokio::task::spawn_local(async move {
            let local_mcp_servers = match tokio::task::spawn_blocking(move || {
                    let local = crate::util::config::load_mcp_servers(&cwd, &compat);
                    folder_trust::resolve_and_record(
                        &cwd,
                        remote_settings.as_ref(),
                        false,
                    );
                    let local = folder_trust::filter_untrusted_project_mcp(&cwd, local);
                    crate::session::managed_mcp::filter_policy_blocked_agent_mcp(
                        local,
                        &cwd,
                    )
                })
                .await
            {
                Ok(servers) => servers,
                Err(e) => {
                    tracing::warn!(error = %e, "initialize MCP setup task failed");
                    return;
                }
            };
            if !local_mcp_servers.is_empty() {
                agent_mcp_state.lock().await.update_configs(local_mcp_servers.clone());
            }
            crate::extensions::mcp::notify_servers_updated(&gateway, &local_mcp_servers)
                .await;
        });
    }
    pub(crate) fn agent_mcp_state(
        &self,
    ) -> std::sync::Arc<tokio::sync::Mutex<crate::session::mcp_servers::McpState>> {
        self.agent_mcp_state.clone()
    }
    /// Build the launch-dir plugin registry snapshot on first use. Boot-time discovery was deferred past ACP `initialize`, leaving `plugin_registry_handle` empty.
    /// The cwd-to-git-root and user/marketplace walks stalled grok-desktop's first `initialize`. That shared snapshot still backs the launch-dir plugin MCP/LSP merges read in `resolve_mcp_servers` and the session LSP build.
    /// So populate it lazily, off the `initialize` critical path, on the first session-creating call. Runs the discovery walk once; per-session `build_for_cwd` still re-resolves project-scoped plugins for each session's own cwd.
    pub(crate) fn ensure_plugin_registry(&self) {
        if self.plugin_registry_initialized.replace(true) {
            return;
        }
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let (trusted, disk_config) = Self::registry_build_inputs(
            &self.launch_cwd,
            remote_settings.as_ref(),
        );
        let count = self
            .plugin_registry_handle
            .reload(Some(&self.launch_cwd), &disk_config, trusted, false);
        tracing::debug!(
            plugin_count = count,
            "lazily populated plugin registry snapshot"
        );
    }
    /// Inputs for a registry build scoped to `cwd`: a fresh real-remote trust verdict, then
    /// `[plugins]` from disk. Self-free so callers can run it on a blocking thread.
    /// `cfg.plugins` is boot-time state that `config.toml` edits never refresh, so a snapshot built
    /// from it reports stale `enabled` flags to session-less `x.ai/plugins/list` / `x.ai/skills/list`.
    /// Order is load-bearing: the disk read consults the folder-trust gate, whose cold-key backstop
    /// resolves remote-less and records a durable verdict that would make an org kill-switch
    /// unliftable for the process; resolving first (recording, with the real `RemoteSettings`) makes
    /// the read a cache hit.
    /// The verdict is never the startup primer's: that memoizes a point-in-time answer, including
    /// the non-durable no-configs allow, which also leaves the launch dir cold for the gate
    /// (`plugins_reload_rechecks_launch_dir_trust`).
    pub(crate) fn registry_build_inputs(
        cwd: &std::path::Path,
        remote_settings: Option<&crate::util::config::RemoteSettings>,
    ) -> (bool, xai_grok_agent::plugins::discovery::DiscoveryConfig) {
        let trusted = folder_trust::resolve_and_record(cwd, remote_settings, false);
        (
            trusted,
            crate::config::resolve_effective_plugins_config(cwd).to_discovery_config(),
        )
    }
    /// The directory the agent was launched in; the shared registry snapshot is built for it.
    pub(crate) fn launch_cwd(&self) -> &std::path::Path {
        &self.launch_cwd
    }
    /// An explicit rebuild populated the shared snapshot; the lazy boot build must not run after it
    /// (it would rebuild with the startup primer's memoized trust, undoing a fresh reload verdict).
    pub(crate) fn mark_plugin_registry_initialized(&self) {
        self.plugin_registry_initialized.set(true);
    }
    /// [`Self::ensure_plugin_registry`] for async callers: the trust gather, config read and
    /// discovery walk run on a blocking thread so a pre-session pull never stalls the runtime. The
    /// result is published back on the runtime, and only if nothing else (a concurrent explicit
    /// `x.ai/plugins/reload`) initialized the registry meanwhile — the newer build wins.
    pub(crate) async fn ensure_plugin_registry_async(&self) {
        if self.plugin_registry_initialized.get() {
            return;
        }
        let launch_cwd = self.launch_cwd.clone();
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let handle = self.plugin_registry_handle.clone();
        let built = tokio::task::spawn_blocking(move || {
                let (trusted, disk_config) = Self::registry_build_inputs(
                    &launch_cwd,
                    remote_settings.as_ref(),
                );
                handle.refresh_and_build_for_cwd(&launch_cwd, &disk_config, &[], trusted)
            })
            .await;
        match built {
            Ok(_) if self.plugin_registry_initialized.get() => {}
            Ok(registry) => {
                tracing::debug!(
                    plugin_count = registry.as_ref().map_or(0, |r| r.len()),
                    "lazily populated plugin registry snapshot"
                );
                self.plugin_registry_handle.publish(registry);
                self.plugin_registry_initialized.set(true);
            }
            Err(err) => {
                tracing::warn!(error = %err, "plugin registry build task failed")
            }
        }
    }
    /// Plugin registry for a pre-session pull (`x.ai/skills/*`, `x.ai/commands/list`) scoped to
    /// `cwd`: a fresh, non-shared build for that cwd when one is given — the launch dir is unrelated
    /// to the user's workspace in desktop-to-docker and ssh setups — else the shared launch-dir
    /// snapshot, built on demand. Trust is resolved before the disk read either way, and the walks
    /// run on a blocking thread.
    pub(crate) async fn plugin_registry_for_cwd(
        &self,
        cwd: Option<&std::path::Path>,
    ) -> Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>> {
        let Some(cwd) = cwd else {
            self.ensure_plugin_registry_async().await;
            return self.plugin_registry_handle.snapshot();
        };
        let cwd = cwd.to_path_buf();
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let handle = self.plugin_registry_handle.clone();
        match tokio::task::spawn_blocking(move || {
                let (trusted, disk_cfg) = Self::registry_build_inputs(
                    &cwd,
                    remote_settings.as_ref(),
                );
                handle.build_for_cwd(&cwd, &disk_cfg, &[], trusted)
            })
            .await
        {
            Ok(registry) => registry,
            Err(err) => {
                tracing::warn!(error = %err, "plugin registry build task failed");
                None
            }
        }
    }
    /// Admit client servers and merge local / plugin / client sources.
    ///
    /// Plugin registry is ensured first; admit and merge then share one compat snapshot.
    pub(super) async fn resolve_mcp_servers(
        &self,
        client_servers: Vec<acp::McpServer>,
        cwd: &std::path::Path,
    ) -> (Vec<acp::McpServer>, Vec<acp::McpServer>) {
        self.report_setup_phase(super::session_setup::SessionSetupPhase::PluginRegistry);
        self.ensure_plugin_registry();
        let compat = self.cfg.borrow().compat_resolved;
        let admitted = crate::session::managed_mcp::admit_client_mcp_servers(
            client_servers,
            cwd,
            &compat,
        );
        self.report_setup_phase(super::session_setup::SessionSetupPhase::McpMerge);
        let merged = crate::session::managed_mcp::merge_managed_mcp_servers(
            admitted.clone(),
            cwd,
            self.plugin_registry_handle.snapshot().as_deref(),
            &compat,
        );
        (admitted, merged)
    }
    /// Set the memory configuration (called from TUI after config resolution).
    pub fn set_memory_config(&mut self, config: crate::config::MemoryConfig) {
        *self.memory_config.get_mut() = Some(config);
    }
    /// Clone the memory configuration pinned by the next session or subagent spawn.
    pub(super) fn memory_config_snapshot(&self) -> Option<crate::config::MemoryConfig> {
        self.memory_config.borrow().clone()
    }
    fn publish_memory_config(&self, mut config: crate::config::MemoryConfig) {
        if let Some(current) = self.memory_config.borrow().as_ref() {
            config.root_dir_override = current.root_dir_override.clone();
            config.flat_memory_root = current.flat_memory_root;
        }
        self.memory_config.replace(Some(config));
    }
    /// Apply the current remote settings to memory for sessions spawned after
    /// this point without changing any running session's pinned snapshot.
    fn sync_memory_config_from_agent_config(&self) {
        let memory_config = {
            let cfg = self.cfg.borrow();
            cfg.resolve_memory(cfg.memory_enabled_override, cfg.remote_settings.as_ref())
        };
        self.publish_memory_config(memory_config);
    }
    /// Re-resolve runtime config and publish its memory result to future spawns.
    pub(super) fn re_resolve_runtime_fields_and_sync_memory(
        &self,
        raw_config: &toml::Value,
    ) {
        let memory_config = {
            let mut cfg = self.cfg.borrow_mut();
            cfg.re_resolve_runtime_fields(raw_config);
            cfg.memory_config.clone()
        };
        if let Some(memory_config) = memory_config {
            self.publish_memory_config(memory_config);
        }
    }
    /// Adopt the leader's [`AgentActivity`].
    /// The auto-update checker then sees the agent's live view of running turns/subagents and can flush sessions at shutdown.
    /// Must be called right after construction: entries registered on the constructor-created default instance are NOT migrated.
    pub(crate) fn set_activity(
        &mut self,
        activity: crate::agent::activity::AgentActivity,
    ) {
        self.activity = activity;
    }
    /// Send [`SessionCommand::Shutdown`] to every live session actor and wait up to `grace` for them to exit (SessionEnd hooks, memory save, etc.).
    /// Call on non-leader process quit **after** the cancel token fires but **before** dropping the agent / exiting the process. Otherwise session actors are killed mid-hook.
    /// Mirrors the leader auto-update / relaunch flush path ([`crate::agent::activity::AgentActivity::flush_all_sessions`]).
    pub async fn flush_all_sessions(&self, grace: std::time::Duration) {
        self.activity.flush_all_sessions(grace).await;
    }
    /// Install the channel that fans new session cwds into the leader's `ConfigFileWatcher::watch_path`.
    /// Called once after the watcher is constructed in `agent/app.rs`.
    /// In simple / non-leader mode the channel is never wired and `notify_session_cwd_for_watch` is a no-op.
    pub(crate) fn set_config_watcher_path_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
    ) {
        self.config_watcher_path_tx = Some(tx);
    }
    /// Best-effort fan-out of a new session's `cwd` to the leader's `ConfigFileWatcher` for dynamic non-recursive registration.
    /// No-op if `set_config_watcher_path_tx` was never called (simple mode, tests) or if the receiver has been dropped.
    /// Watcher errors are logged inside the spawned task and do NOT propagate here.
    pub(crate) fn notify_session_cwd_for_watch(&self, cwd: &std::path::Path) {
        if let Some(tx) = self.config_watcher_path_tx.as_ref()
            && tx.send(cwd.to_path_buf()).is_err()
        {
            tracing::debug!(
                cwd = %cwd.display(),
                "config watcher path channel closed; session cwd not registered"
            );
        }
    }
    /// Returns `(base_url, user_token, optional_extra_access_key, deployment_key)`.
    /// Used by both [`feedback_client`] and session spawning.
    #[allow(clippy::type_complexity)]
    fn feedback_credentials(
        &self,
    ) -> Option<(String, Option<String>, Option<String>, Option<String>)> {
        if !self.has_proxy_credentials() {
            return None;
        }
        let user_token = self
            .auth_manager
            .current_or_expired()
            .filter(|a| a.is_xai_auth())
            .map(|a| a.key.clone());
        let cfg = self.cfg.borrow();
        let base_url = cfg.endpoints.resolve_feedback_base_url();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let deployment_key = cfg.endpoints.deployment_key.clone();
        Some((base_url, user_token, alpha_test_key, deployment_key))
    }
    pub(super) fn ensure_telemetry_client(&self) {
        xai_grok_login::credential_provider::sync_external_otel_identity();
        let cfg = self.cfg.borrow();
        let mode = cfg.resolve_telemetry_mode().value;
        if !mode.is_disabled() {
            let Some(auth) = self
                .auth_manager
                .current()
                .filter(|a| {
                    a.is_xai_auth() || a.auth_mode == xai_grok_login::AuthMode::ApiKey
                }) else {
                return;
            };
            let subscription_tier = resolve_subscription_tier_for_telemetry(
                cfg
                    .remote_settings
                    .as_ref()
                    .and_then(|rs| rs.subscription_tier_display.clone()),
                Some(&auth),
            );
            let (user_id, team_id) = if auth.is_xai_auth() {
                (Some(auth.user_id), auth.team_id)
            } else {
                (None, auth.team_id)
            };
            xai_grok_telemetry::client::init_if_needed(
                cfg.telemetry.clone(),
                mode,
                user_id,
                team_id,
                cfg.endpoints.deployment_key.clone(),
                self.origin_client_info_from_meta(None),
                xai_grok_version::VERSION.to_owned(),
                subscription_tier,
                crate::http::shared_client(),
            );
        }
    }
    pub(crate) fn feedback_client(&self) -> Option<FeedbackClient> {
        let (base_url, user_token, alpha_test_key, deployment_key) = self
            .feedback_credentials()?;
        Some(
            FeedbackClient::new(base_url, user_token)
                .with_alpha_test_key(alpha_test_key)
                .with_deployment_key(deployment_key)
                .with_auth_manager(self.auth_manager.clone()),
        )
    }
    /// Build a `RegistryConfig` if the feature is enabled (for passing to persistence actor).
    pub(super) fn build_registry_config(
        &self,
    ) -> Option<crate::session::RegistryConfig> {
        let remote = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.session_registry_enabled);
        if !self.session_registry_local.or(remote).unwrap_or(false) {
            return None;
        }
        let auth = self.auth_manager.current_or_expired()?;
        if !auth.is_xai_auth() {
            return None;
        }
        let key = auth.key.clone();
        let cfg = self.cfg.borrow();
        Some(crate::session::RegistryConfig {
            base_url: cfg.endpoints.proxy_url(),
            user_token: key,
            deployment_key: cfg.endpoints.deployment_key.clone(),
            alpha_test_key: cfg.endpoints.alpha_test_key.clone(),
        })
    }
    /// Delegates to `build_registry_config()` for the enabled check and config.
    pub(crate) fn session_registry_client(
        &self,
    ) -> Option<crate::agent::session_registry_client::SessionRegistryClient> {
        let cfg = self.build_registry_config()?;
        Some(
            crate::agent::session_registry_client::SessionRegistryClient::new(
                    cfg.base_url,
                    cfg.user_token,
                )
                .with_deployment_key(cfg.deployment_key)
                .with_alpha_test_key(cfg.alpha_test_key)
                .with_auth(self.auth_manager.clone()),
        )
    }
    pub(crate) fn conversations_client(
        &self,
    ) -> Option<crate::remote::ConversationsClient> {
        if !crate::session::unified_list::conversations_lane_active() {
            return None;
        }
        Some(crate::remote::ConversationsClient::new(self.auth_manager.clone()))
    }
    pub(crate) fn workspaces_client(&self) -> crate::remote::WorkspacesClient {
        crate::remote::WorkspacesClient::new(self.auth_manager.clone())
    }
    /// Pre-session command availability snapshot. Used by the `x.ai/commands/list` ext method and the `InitializeResponse._meta` path (`builtin_commands()`). Both fire before any session exists.
    /// The eventual agent's toolset is unknown (it depends on the model the user picks). So runtime/tool-dependent gates (`/flush`, `/loop`, `/memory`, …) fail closed.
    /// The session-scoped `available_commands_update` in `acp_session.rs` fills in the real per-model gating as soon as a session starts.
    pub(crate) fn command_availability(
        &self,
    ) -> crate::session::slash_commands::CommandAvailability {
        crate::session::slash_commands::CommandAvailability {
            goal: self.cfg.borrow().resolve_goal().value,
            workflows: self.cfg.borrow().resolve_workflows().value,
            ..crate::session::slash_commands::CommandAvailability::default()
        }
    }
    /// `true` when data collection should be suppressed (team ZDR or coding-data-retention opt-out).
    /// Delegates to [`AuthManager::is_data_collection_disabled`].
    pub(crate) fn is_data_collection_disabled(&self) -> bool {
        self.auth_manager.is_data_collection_disabled()
    }
    /// Telemetry enabled and not ZDR. Same gate as session `telemetry_enabled`.
    pub(crate) fn product_analytics_enabled(&self) -> bool {
        self.cfg
            .borrow()
            .product_analytics_enabled(self.auth_manager.current_or_expired().as_ref())
    }
    /// Re-sync the `Send` mirror of `cfg.is_trace_upload_enabled()` that the per-session collection gates read.
    /// `cfg` is `!Send`; the gates run on the tokio pool.
    /// Must be called after any mid-session config change that can flip the switch, i.e. every `remote_settings` rewrite.
    pub(super) fn sync_collection_config_gate(&self) {
        self.trace_upload_live
            .store(
                self.cfg.borrow().is_trace_upload_enabled(),
                std::sync::atomic::Ordering::Relaxed,
            );
    }
    /// Current client type as set by the most recent `initialize()` call.
    pub(crate) fn client_type(&self) -> ClientType {
        *self.client_type.borrow()
    }
    /// Most recently allocated turn number for `sid`, or `None` if the session has not started a turn yet.
    pub(crate) fn session_turn_number(&self, sid: &acp::SessionId) -> Option<u64> {
        self.session_registry.turn_number(sid)
    }
    /// Return the current GrokAuth credentials, if authenticated and not expired.
    pub(crate) fn current_auth(&self) -> Option<xai_grok_login::GrokAuth> {
        self.auth_manager.current()
    }
    /// Shared plugin registry handle used by extensions for snapshot/reload.
    pub(crate) fn plugin_registry_handle(
        &self,
    ) -> &xai_grok_agent::plugins::SharedPluginRegistryHandle {
        &self.plugin_registry_handle
    }
    /// `true` when the agent runs in writeback storage mode.
    pub(crate) fn is_writeback_storage(&self) -> bool {
        matches!(self.storage_mode.get(), StorageMode::Writeback)
    }
    /// Resolved cli-chat-proxy base for session features (via `proxy_url`).
    /// Not for the deployment-config fetch.
    pub(crate) fn cli_chat_proxy_base_url(&self) -> String {
        self.cfg.borrow().endpoints.proxy_url()
    }
    pub(crate) fn alpha_test_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.alpha_test_key.clone()
    }
    #[cfg(all(feature = "local-workspace", unix))]
    /// Spawn owned `workspace_server` for chat+local `own` intent.
    /// Mints `server_id` into `_meta` before handshake parse.
    pub(crate) async fn start_own_local_workspace_if_needed(
        &self,
        meta: &mut Option<acp::Meta>,
        session_cwd: &std::path::Path,
    ) -> Result<
        Option<crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceHandle>,
        acp::Error,
    > {
        use crate::gateway_bridge::local_workspace_supervisor::{
            parse_local_workspace_intent, stamp_server_id_into_meta, start_own,
            StartOwnConfig, LocalWorkspaceIntent,
        };
        let Some(LocalWorkspaceIntent::Own { cwd }) = parse_local_workspace_intent(
            meta.as_ref(),
        ) else {
            return Ok(None);
        };
        let cwd = if cwd.as_os_str().is_empty() {
            session_cwd.to_path_buf()
        } else {
            cwd
        };
        crate::gateway_bridge::local_workspace_supervisor::validate_cwd(&cwd)
            .map_err(|e| e.into_acp_error())?;
        let hub_url = {
            let cfg = self.cfg.borrow();
            crate::gateway_bridge::local_workspace_supervisor::resolve_hub_url(
                cfg.hub.url.as_deref(),
            )
        };
        let handle = start_own(StartOwnConfig {
                cwd,
                hub_url,
                auth_config: None,
                binary: None,
                ready_timeout: crate::gateway_bridge::local_workspace_supervisor::READY_TIMEOUT,
                allow_missing_auth: false,
            })
            .await
            .map_err(|e| e.into_acp_error())?;
        let meta_map = meta.get_or_insert_with(acp::Meta::new);
        stamp_server_id_into_meta(meta_map, &handle.server_id);
        Ok(Some(handle))
    }
    #[cfg(all(feature = "local-workspace", unix))]
    pub(crate) fn register_local_workspace_supervisor(
        &self,
        session_id: acp::SessionId,
        handle: crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceHandle,
    ) {
        let server_id = handle.server_id.clone();
        self.arm_local_workspace_watcher(session_id.clone(), handle);
        tracing::info!(
            session_id = %session_id.0,
            server_id = %server_id,
            "local_workspace_supervisor: registered own workspace_server"
        );
    }
    #[cfg(all(feature = "local-workspace", unix))]
    pub(crate) fn new_local_workspace_reap_guard(
        &self,
        session_id: acp::SessionId,
        armed: bool,
    ) -> LocalWorkspaceReapGuard {
        LocalWorkspaceReapGuard {
            supervisors: self.local_workspace_supervisors.clone(),
            generations: self.local_workspace_generations.clone(),
            session_id,
            armed,
        }
    }
    /// Prefer live supervisor `server_id` over the parse-time stamp (pre-bridge crash).
    #[cfg(all(feature = "local-workspace", unix))]
    pub(crate) fn refresh_sessions_from_supervisor(
        &self,
        session_id: &acp::SessionId,
        sessions: Option<Vec<crate::gateway_bridge::ComputerSession>>,
    ) -> Option<Vec<crate::gateway_bridge::ComputerSession>> {
        use crate::gateway_bridge::ComputerSession;
        let supervisors = self.local_workspace_supervisors.borrow();
        let Some(handle) = supervisors.get(session_id) else {
            return sessions;
        };
        let server_id = handle.server_id.clone();
        let cwd = Some(handle.cwd.to_string_lossy().into_owned());
        match sessions {
            None => Some(vec![ComputerSession::ExistingWorkspace { server_id, cwd }]),
            Some(mut list) => {
                for session in &mut list {
                    if let ComputerSession::ExistingWorkspace {
                        server_id: sid,
                        cwd: existing_cwd,
                    } = session {
                        *sid = server_id.clone();
                        if existing_cwd.is_none() {
                            *existing_cwd = cwd.clone();
                        }
                    }
                }
                Some(list)
            }
        }
    }
    /// Wait out an in-flight crash restart before refreshing handshake sessions.
    #[cfg(all(feature = "local-workspace", unix))]
    pub(crate) async fn await_refresh_sessions_from_supervisor(
        &self,
        session_id: &acp::SessionId,
        sessions: Option<Vec<crate::gateway_bridge::ComputerSession>>,
    ) -> Option<Vec<crate::gateway_bridge::ComputerSession>> {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let pending = self
                .local_workspace_restart_pending
                .borrow()
                .contains(session_id);
            let live = self
                .local_workspace_supervisors
                .borrow()
                .contains_key(session_id);
            if live || !pending {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    "timed out waiting for local-workspace crash restart before handshake refresh"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        self.refresh_sessions_from_supervisor(session_id, sessions)
    }
    #[cfg(all(feature = "local-workspace", unix))]
    fn arm_local_workspace_watcher(
        &self,
        session_id: acp::SessionId,
        handle: crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceHandle,
    ) {
        let cwd = handle.cwd.clone();
        let sessions = self.session_registry.clone();
        let supervisors = self.local_workspace_supervisors.clone();
        let generations = self.local_workspace_generations.clone();
        let sid = session_id.clone();
        let hub_url = {
            let cfg = self.cfg.borrow();
            crate::gateway_bridge::local_workspace_supervisor::resolve_hub_url(
                cfg.hub.url.as_deref(),
            )
        };
        let auth_path = xai_grok_workspace::hub_auth::default_auth_path().ok();
        let binary = crate::gateway_bridge::local_workspace_supervisor::resolve_workspace_server_bin()
            .ok();
        let agent_ref = LocalRef::new(self);
        let generation = {
            let mut gens = generations.borrow_mut();
            let e = gens.entry(session_id.clone()).or_insert(0);
            *e = e.saturating_add(1);
            *e
        };
        self.local_workspace_supervisors.borrow_mut().insert(session_id.clone(), handle);
        let mut supervisors_mut = self.local_workspace_supervisors.borrow_mut();
        let Some(handle_mut) = supervisors_mut.get_mut(&session_id) else {
            return;
        };
        let _ = handle_mut
            .spawn_exit_watcher(move || {
                let sessions = sessions.clone();
                let supervisors = supervisors.clone();
                let generations = generations.clone();
                let sid = sid.clone();
                let hub_url = hub_url.clone();
                let auth_path = auth_path.clone();
                let binary = binary.clone();
                let cwd = cwd.clone();
                let agent_ref = agent_ref.clone();
                tokio::task::spawn_local(async move {
                    if generations.borrow().get(&sid) != Some(&generation) {
                        return;
                    }
                    agent_ref
                        .get()
                        .local_workspace_restart_pending
                        .borrow_mut()
                        .insert(sid.clone());
                    let prev = supervisors.borrow_mut().remove(&sid);
                    let Some(prev) = prev else {
                        agent_ref
                            .get()
                            .local_workspace_restart_pending
                            .borrow_mut()
                            .remove(&sid);
                        return;
                    };
                    let Some(binary) = binary else {
                        tracing::warn!(
                        session_id = %sid.0,
                        "local workspace crash restart skipped: binary missing"
                    );
                        prev.shutdown().await;
                        agent_ref
                            .get()
                            .local_workspace_restart_pending
                            .borrow_mut()
                            .remove(&sid);
                        return;
                    };
                    let auth = auth_path
                        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent"));
                    let restart_count = prev.restart_count;
                    let prev_cwd = prev.cwd.clone();
                    prev.shutdown().await;
                    match crate::gateway_bridge::local_workspace_supervisor::restart_own_from(
                            restart_count,
                            prev_cwd,
                            &binary,
                            &hub_url,
                            &auth,
                            false,
                        )
                        .await
                    {
                        Ok(new_handle) => {
                            if generations.borrow().get(&sid) != Some(&generation) {
                                new_handle.shutdown().await;
                                agent_ref
                                    .get()
                                    .local_workspace_restart_pending
                                    .borrow_mut()
                                    .remove(&sid);
                                return;
                            }
                            let new_id = new_handle.server_id.clone();
                            let cwd_str = cwd.to_string_lossy().into_owned();
                            agent_ref
                                .get()
                                .arm_local_workspace_watcher(sid.clone(), new_handle);
                            agent_ref
                                .get()
                                .local_workspace_restart_pending
                                .borrow_mut()
                                .remove(&sid);
                            let armed_generation = generations
                                .borrow()
                                .get(&sid)
                                .copied();
                            let bridge = sessions.bridge(&sid);
                            if let Some(bridge) = bridge {
                                let _ = crate::gateway_bridge::local_workspace_supervisor::push_computer_sessions_update(
                                        &bridge,
                                        new_id.clone(),
                                        Some(cwd_str.clone()),
                                        false,
                                    )
                                    .await;
                                let _ = bridge.wait_until_ready().await;
                                if generations.borrow().get(&sid)
                                    != armed_generation.as_ref()
                                {
                                    tracing::debug!(
                                    session_id = %sid.0,
                                    "skip stale local-workspace session.update after superseded restart"
                                );
                                    return;
                                }
                                let _ = crate::gateway_bridge::local_workspace_supervisor::push_computer_sessions_update(
                                        &bridge,
                                        new_id,
                                        Some(cwd_str),
                                        false,
                                    )
                                    .await;
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                            session_id = %sid.0,
                            error = %err,
                            "local workspace crash restart failed"
                        );
                            agent_ref
                                .get()
                                .local_workspace_restart_pending
                                .borrow_mut()
                                .remove(&sid);
                        }
                    }
                });
            });
    }
    /// Add-only: bind a local workspace mid-session via the ACP extension and session.update. Refuses if a local existing workspace is already bound (no remove until session end). Own mode requires unix (supervisor spawn).
    /// Attach is platform-agnostic.
    #[cfg(feature = "local-workspace")]
    pub(crate) async fn add_local_workspace_mid_session(
        &self,
        session_id: &acp::SessionId,
        mut meta: Option<acp::Meta>,
        session_cwd: &std::path::Path,
    ) -> Result<serde_json::Value, acp::Error> {
        use crate::gateway_bridge::ComputerSession;
        use crate::gateway_bridge::local_workspace_supervisor::{
            parse_local_workspace_intent, LocalWorkspaceIntent, SupervisorError,
        };
        if self.local_workspace_already_bound(session_id) {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        serde_json::json!({
                "code": "local_workspace_already_bound",
                "message": "local workspace already bound; remove is not supported until session end",
            }),
                    ),
            );
        }
        self.mark_local_workspace_bound(session_id.clone());
        let mut bind_guard = LocalWorkspaceBindGuard {
            bound: self.local_workspace_bound.clone(),
            session_id: session_id.clone(),
            keep: false,
        };
        let Some(intent) = parse_local_workspace_intent(meta.as_ref()) else {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        serde_json::json!({
                "code": "local_workspace_intent_missing",
                "message": "x.ai/local_workspace intent required for mid-session add",
            }),
                    ),
            );
        };
        let mode = match &intent {
            LocalWorkspaceIntent::Own { .. } => "own",
            LocalWorkspaceIntent::Attach { .. } => "attach",
        };
        #[cfg_attr(not(unix), allow(unused_variables))]
        let pending: Option<
            crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceHandle,
        > = match intent {
            LocalWorkspaceIntent::Own { .. } => {
                #[cfg(unix)]
                {
                    self.start_own_local_workspace_if_needed(&mut meta, session_cwd)
                        .await?
                }
                #[cfg(not(unix))]
                {
                    let _ = session_cwd;
                    return Err(SupervisorError::UnsupportedPlatform.into_acp_error());
                }
            }
            LocalWorkspaceIntent::Attach { cwd, .. } => {
                if let Some(ref cwd) = cwd {
                    crate::gateway_bridge::local_workspace_supervisor::validate_cwd(cwd)
                        .map_err(|e| e.into_acp_error())?;
                }
                Self::ensure_attach_fs_only_advertised_tools()
                    .map_err(|msg| {
                        acp::Error::invalid_params()
                            .data(
                                serde_json::json!({
                        "code": "local_workspace_fs_only_required",
                        "message": msg,
                    }),
                            )
                    })?;
                None
            }
        };
        let sessions = resolve_session_computer_sessions(meta.as_ref())?;
        let Some(sessions) = sessions.filter(|s| !s.is_empty()) else {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        serde_json::json!({
                "code": "local_workspace_stamp_failed",
                "message": "failed to resolve existing_workspace stamp for mid-session add",
            }),
                    ),
            );
        };
        if !sessions
            .iter()
            .any(|s| matches!(s, ComputerSession::ExistingWorkspace { .. }))
        {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        serde_json::json!({
                "code": "local_workspace_stamp_failed",
                "message": "mid-session add did not produce existing_workspace",
            }),
                    ),
            );
        }
        #[cfg(unix)]
        let mut reap_guard = self
            .new_local_workspace_reap_guard(session_id.clone(), false);
        #[cfg(unix)]
        if let Some(handle) = pending {
            self.register_local_workspace_supervisor(session_id.clone(), handle);
            reap_guard = self.new_local_workspace_reap_guard(session_id.clone(), true);
        }
        let Some(bridge) = self.gateway_bridge_for(session_id) else {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        serde_json::json!({
                "code": "gateway_bridge_missing",
                "message": "session has no gateway bridge for session.update computer_sessions",
            }),
                    ),
            );
        };
        match tokio::time::timeout(BRIDGE_READY_TIMEOUT, bridge.wait_until_ready()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(err.into_acp_error());
            }
            Err(_) => {
                return Err(
                    acp::Error::internal_error()
                        .data(
                            "gateway bridge not ready for mid-session add_local_workspace",
                        ),
                );
            }
        }
        let sessions = sessions;
        let server_id = match sessions.first() {
            Some(ComputerSession::ExistingWorkspace { server_id, .. }) => {
                server_id.clone()
            }
            _ => {
                return Err(
                    acp::Error::internal_error()
                        .data("expected existing_workspace as first computer session"),
                );
            }
        };
        let cwd = match sessions.first() {
            Some(ComputerSession::ExistingWorkspace { cwd, .. }) => cwd.clone(),
            _ => None,
        };
        if let Some(ref stamped_cwd) = cwd {
            crate::gateway_bridge::local_workspace_supervisor::validate_cwd(
                    std::path::Path::new(stamped_cwd),
                )
                .map_err(|e| e.into_acp_error())?;
        }
        if let Err(err) = crate::gateway_bridge::local_workspace_supervisor::push_computer_sessions_update(
                &bridge,
                server_id.clone(),
                cwd,
                true,
            )
            .await
        {
            return Err(err.into_acp_error());
        }
        if let Some(handle) = self.resident_handle(session_id) {
            handle
                .emit_local_background_tasks
                .store(false, std::sync::atomic::Ordering::Release);
        }
        #[cfg(unix)] reap_guard.disarm();
        bind_guard.keep = true;
        Ok(serde_json::json!({
            "ok": true,
            "server_id": server_id,
            "mode": mode,
        }))
    }
    #[cfg(feature = "local-workspace")]
    pub(crate) fn local_workspace_already_bound(
        &self,
        session_id: &acp::SessionId,
    ) -> bool {
        if self.local_workspace_bound.borrow().contains(session_id) {
            return true;
        }
        #[cfg(unix)]
        if self.local_workspace_supervisors.borrow().contains_key(session_id) {
            return true;
        }
        false
    }
    #[cfg(feature = "local-workspace")]
    pub(crate) fn mark_local_workspace_bound(&self, session_id: acp::SessionId) {
        self.local_workspace_bound.borrow_mut().insert(session_id);
    }
    /// Operator-attested FS-only toolset for mid-session attach.
    #[cfg(feature = "local-workspace")]
    fn ensure_attach_fs_only_advertised_tools() -> Result<(), String> {
        const ENV: &str = "GROK_CHAT_LOCAL_WORKSPACE_ADVERTISED_TOOLS";
        const ALLOW: &[&str] = &[
            "workspace.fs_list",
            "workspace.fs_exists",
            "workspace.fs_read_file",
            "workspace.fs_write_file",
            "workspace.fs_delete_file",
            "workspace.put_files",
            "workspace.get_files",
        ];
        let Some(raw) = std::env::var(ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()) else {
            return Err(
                "attached workspace_server advertised toolset is uncheckable; refuse attach \
                 (set GROK_CHAT_LOCAL_WORKSPACE_ADVERTISED_TOOLS to a comma-separated FS-only catalog)"
                    .into(),
            );
        };
        let ids: Vec<&str> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(
                "attached workspace_server advertised an empty toolset; refuse attach"
                    .into(),
            );
        }
        let forbidden: Vec<&str> = ids
            .into_iter()
            .filter(|id| !ALLOW.contains(id))
            .collect();
        if forbidden.is_empty() {
            Ok(())
        } else {
            Err(
                    format!(
                "attached workspace_server advertises tools outside the FS-only allowlist: {}",
                forbidden.join(", ")
            ),
                )
        }
    }
    #[cfg(feature = "local-workspace")]
    /// After chat+local stamp, wait for handshake success.
    /// Only fail-closed for `x.ai/local_workspace` intent (not generic GatewayAttach).
    /// Handshake errors propagate; the session and bridge are reaped on failure / timeout.
    pub(crate) async fn await_existing_workspace_handshake(
        &self,
        session_id: &acp::SessionId,
        local_workspace_intent: bool,
    ) -> Result<(), acp::Error> {
        if !local_workspace_intent {
            return Ok(());
        }
        #[cfg(feature = "local-workspace")]
        self.mark_local_workspace_bound(session_id.clone());
        let Some(bridge) = self.gateway_bridge_for(session_id) else {
            return Ok(());
        };
        match tokio::time::timeout(BRIDGE_READY_TIMEOUT, bridge.wait_until_ready()).await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                tracing::warn!(
                    session_id = %session_id.0,
                    error = %err,
                    kind = "existing_workspace_handshake_failed",
                    "chat+local handshake failed; reaping session"
                );
                self.request_session_shutdown(session_id);
                self.remove_session(session_id);
                Err(err.into_acp_error())
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %session_id.0,
                    kind = "existing_workspace_handshake_timeout",
                    "chat+local handshake timed out; reaping session"
                );
                self.request_session_shutdown(session_id);
                self.remove_session(session_id);
                Err(
                    acp::Error::internal_error().data("gateway bridge connect timed out"),
                )
            }
        }
    }
    /// Build the process-lifetime local `WorkspaceOps` on first use.
    /// Deferred past ACP wiring so `initialize` can respond before folder-trust scans and `WorkspaceHandle::new_minimal` run.
    /// This is the same boot stall as plugin discovery on grok-desktop Windows.
    fn ensure_local_workspace_ops(
        &self,
    ) -> Result<xai_grok_workspace::WorkspaceOps, acp::Error> {
        if let Some(ops) = self.workspace_ops.borrow().clone() {
            return Ok(ops);
        }
        let (cwd, project_lsp_trusted) = self.prime_launch_dir_trust();
        let workspace_identity = self
            .auth_manager
            .current_or_expired()
            .map(|a| match a.team_id.filter(|t| !t.is_empty()) {
                Some(team) => {
                    xai_grok_workspace::WorkspaceIdentity::team(a.user_id, team)
                }
                None => {
                    xai_grok_workspace::WorkspaceIdentity::new(
                        a.user_id,
                        a.principal_type,
                        a.principal_id,
                    )
                }
            })
            .unwrap_or_default();
        let ops = match xai_grok_workspace::handle::WorkspaceHandle::new_minimal(
            cwd.to_path_buf(),
            workspace_identity,
            project_lsp_trusted,
        ) {
            Ok(handle) => xai_grok_workspace::WorkspaceOps::local(handle),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to create local WorkspaceHandle"
                );
                return Err(
                    acp::Error::internal_error().data("workspace not initialized"),
                );
            }
        };
        *self.workspace_ops.borrow_mut() = Some(ops.clone());
        Ok(ops)
    }
    /// Resolve the workspace ops, returning `Err` if not yet initialized.
    /// Only `None` before the first lazy local build via [`Self::ensure_local_workspace_ops`].
    /// Called at the `ext_method` dispatch boundary and in session spawn; extensions receive the resolved `&WorkspaceOps` directly.
    pub(crate) fn resolve_workspace_ops(
        &self,
    ) -> Result<xai_grok_workspace::WorkspaceOps, acp::Error> {
        let ops = self.ensure_local_workspace_ops()?;
        if let Some(handle) = ops.workspace_handle() && !handle.has_client_ext_sink() {
            let gw = self.gateway.clone();
            handle
                .set_client_ext_sink(
                    std::sync::Arc::new(move |method: String, params: serde_json::Value| {
                        if let Ok(raw) = serde_json::value::to_raw_value(&params) {
                            gw.forward_fire_and_forget(
                                acp::ExtNotification::new(method, raw.into()),
                            );
                        }
                    }),
                );
        }
        Ok(ops)
    }
    /// Derive the current `AuthType` from auth method and auth manager state. Conceptually, `AuthType` describes *which authentication mechanism this session uses*, not *whether we currently have a live bearer*.
    /// Bearer liveness is tracked by the auth manager; the mechanism is fixed by `auth_method_id`.
    /// Returns `ApiKey` only when the auth method is BYOK (`xai.api_key`) or no auth method has been selected yet AND no live credential exists. The session-based clause is load-bearing. Without it, chat_state can get locked into `auth_type = ApiKey` and skip token refresh on later prompts.
    pub(crate) fn auth_type(&self) -> xai_chat_state::AuthType {
        if self.auth_manager.current().is_some() || self.is_session_based_auth() {
            xai_chat_state::AuthType::SessionToken
        } else {
            xai_chat_state::AuthType::ApiKey
        }
    }
    /// Fall through to `xai.api_key` if the startup probe still allows it, else `grok.com`.
    /// `None` when `preferred_method` is pinned.
    pub(super) fn cached_token_fallthrough_method_id(
        &self,
    ) -> Option<acp::AuthMethodId> {
        let preferred = self.cfg.borrow().grok_com_config.preferred_method;
        let id = auth_method::method_id_after_cached_token_unavailable(
            auth_method::should_advertise_xai_api_key_with_env_ok(
                self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
                self.models_manager.models().values(),
                self.auth_manager.first_party_env_api_key_ok(),
            ),
            preferred,
        )?;
        Some(acp::AuthMethodId::new(id))
    }
    /// Shared exit for missing/expired/legacy `cached_token`: fall through with `use_oauth` only when the target is interactive `grok.com`.
    /// When `preferred_method` is pinned, fail instead of falling through.
    pub(super) async fn authenticate_after_cached_token_unavailable(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        let Some(method_id) = self.cached_token_fallthrough_method_id() else {
            let preferred = self.cfg.borrow().grok_com_config.preferred_method;
            let msg = match preferred {
                Some(xai_grok_login::PreferredAuthMethod::ApiKey) => {
                    auth_method::PREFERRED_API_KEY_UNAVAILABLE
                }
                _ => auth_method::PREFERRED_OIDC_UNAVAILABLE,
            };
            tracing::info!(%msg, "cached_token unavailable; preferred_method forbids fallthrough");
            xai_grok_telemetry::unified_log::warn(
                "auth cached_token fallthrough blocked by preferred_method",
                None,
                Some(
                    serde_json::json!({
                    "preferred_method": preferred.map(|p| format!("{p:?}")),
                }),
                ),
            );
            return Err(acp::Error::auth_required().data(msg));
        };
        let meta = if method_id.0.as_ref() == auth_method::GROK_COM_METHOD_ID {
            serde_json::json!({ "use_oauth": true }).as_object().cloned()
        } else {
            arguments.meta
        };
        tracing::info!(fallback = %method_id.0, "cached_token fallthrough");
        xai_grok_telemetry::unified_log::warn(
            "auth cached_token fallthrough",
            None,
            Some(serde_json::json!({ "fallback": method_id.0.as_ref() })),
        );
        acp::Agent::authenticate(
                self,
                acp::AuthenticateRequest::new(method_id).meta(meta),
            )
            .await
    }
    pub(crate) fn deployment_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.deployment_key.clone()
    }
    /// Apply settings side effects and push `x.ai/settings/update` to clients.
    /// Shared tail for every settings-arrival site.
    pub(super) fn on_remote_settings_changed(&self) {
        self.sync_memory_config_from_agent_config();
        {
            let cfg = self.cfg.borrow();
            crate::agent::config::apply_remote_settings_side_effects(
                cfg.remote_settings.as_ref(),
                &cfg.endpoints.proxy_url(),
            );
        }
        if let Some(identity) = self
            .auth_manager
            .current_or_expired()
            .filter(|a| a.is_xai_auth())
            .map(|a| a.user_id)
        {
            self.tier_allowed
                .set(
                    super::settings_allow_access(
                        self.cfg.borrow().remote_settings.as_ref(),
                    ),
                );
            *self.allow_access_resolved_for.borrow_mut() = Some(identity);
        }
        self.reapply_storage_mode();
        self.reapply_official_marketplace();
        {
            let cfg_snapshot = self.cfg.borrow().clone();
            if self.session_registry.resident_count() == 0 {
                self.models_manager.apply_config_reselecting_default(cfg_snapshot);
            } else {
                self.models_manager.apply_config(cfg_snapshot);
            }
        }
        self.sync_collection_config_gate();
        self.emit_settings_update_notification();
        self.emit_announcements(AnnouncementsPushMode::IfChanged);
        self.reconfigure_heap_profile_monitor();
    }
    /// Re-evaluates the official-marketplace auto-register gate now that remote settings exist.
    /// `init_process` ran the same gate at boot without them, so a settings-targeted (not env-set) team would otherwise never register.
    /// Idempotent: a no-op once installed.
    fn reapply_official_marketplace(&self) {
        if self.cfg.borrow().resolve_official_marketplace_auto_register().value {
            let task = tokio_util::task::AbortOnDropHandle::new(
                tokio::task::spawn_blocking(|| {
                    crate::extensions::marketplace::ensure_official_marketplace_source(
                        &crate::util::grok_home::grok_home(),
                    );
                }),
            );
            *self.official_marketplace_register.borrow_mut() = Some(task);
        }
    }
    /// Upgrade storage mode from newly-arrived remote settings.
    /// Mirrors the `resolve_config` gate: only upgrades from `Local`, writeback needs xai auth.
    fn reapply_storage_mode(&self) {
        if self.storage_mode.get() != StorageMode::Local {
            return;
        }
        let resolved_mode = {
            let cfg = self.cfg.borrow();
            if cfg.mode == crate::agent::config::AgentMode::Generic {
                return;
            }
            let has_xai_auth = self
                .auth_manager
                .current_or_expired()
                .is_some_and(|a| a.is_xai_auth());
            StorageMode::from_remote_gated(cfg.remote_settings.as_ref(), has_xai_auth)
        };
        if resolved_mode == self.storage_mode.get() {
            return;
        }
        tracing::info!(?resolved_mode, "storage mode upgraded from remote settings");
        self.storage_mode.set(resolved_mode);
        if resolved_mode == StorageMode::Writeback {
            self.session_registry
                .for_each_resident(|_, handle| {
                    let _ = handle
                        .persistence_tx
                        .send(crate::session::persistence::PersistenceMsg::UpgradeToWriteback {
                            auth_manager: self.auth_manager.clone(),
                        });
                });
        }
    }
    /// Live `/v1/settings` read. Does not join the startup load and does not
    /// accept a warm disk cache. `Rejected` is preserved for 401 self-heal.
    async fn fetch_settings(
        &self,
        auth: &xai_grok_login::GrokAuth,
    ) -> crate::remote::SettingsFetch {
        let (origin, alpha, auth_config) = {
            let cfg = self.cfg.borrow();
            (
                cfg.endpoints.proxy_url(),
                cfg.endpoints.alpha_test_key.clone(),
                cfg.grok_com_config.clone(),
            )
        };
        let query = crate::agent::remote_config::settings_get::SettingsQuery::from_parts(
            Some(auth.clone()),
            origin,
            alpha,
            Some(auth_config),
        );
        crate::agent::remote_config::settings_get::fetch_settings_live(query).await
    }
    /// Fetch remote settings for `auth` and drive the external-OTEL gate from the outcome. Re-closes the gate first only on an account switch, then hands the outcome to [`OtelGate::resolve`].
    /// That returns the settings only on a successful fetch for the still-live identity. Both post-auth callers funnel through here. [`OtelGate::resolve`]: crate::agent::otel_gate::OtelGate::resolve
    pub(super) async fn fetch_settings_resolving_gate(
        &self,
        auth: &xai_grok_login::GrokAuth,
    ) -> Option<crate::util::config::RemoteSettings> {
        let identity = auth.user_id.clone();
        let channel = {
            let proxy_url = self.cfg.borrow().endpoints.proxy_url();
            crate::agent::otel_gate::policy_channel_for(&proxy_url)
        };
        self.otel_gate.rearm_on_switch(&identity, channel);
        let outcome = self
            .settings_refresh
            .refresh(auth, || self.fetch_settings_self_healing_401(auth))
            .await;
        let live = self.auth_manager.current_or_expired().map(|a| a.user_id);
        match outcome {
            Some(outcome) => self.otel_gate.resolve(&identity, outcome, live.as_deref()),
            None => None,
        }
    }
    /// Fetch settings; on a `401` try one self-healing [`AuthManager::auth`] refresh and re-fetch if it yields a *different* token. This recovers a 401 from a token that expired mid-fetch.
    /// The caller waits at most `STARTUP_AUTH_REFRESH_TIMEOUT`, but the refresh is spawned and runs to completion past the deadline. Dropping it mid-exchange could abandon an IdP response carrying the rotated refresh token.
    /// On timeout or error the original `Rejected` stands.
    async fn fetch_settings_self_healing_401(
        &self,
        auth: &xai_grok_login::GrokAuth,
    ) -> crate::remote::SettingsFetch {
        let outcome = self.fetch_settings(auth).await;
        if matches!(outcome, crate::remote::SettingsFetch::Rejected) {
            let manager = self.auth_manager.clone();
            let attempt = tokio::spawn(async move { manager.auth().await });
            if let Ok(Ok(Ok(fresh))) = tokio::time::timeout(
                    crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
                    attempt,
                )
                .await && fresh.key != auth.key
            {
                return self.fetch_settings(&fresh).await;
            }
        }
        outcome
    }
    /// Writes remote settings into `cfg` along with the fields derived from them, so no derived field drifts between post-fetch callers.
    pub(super) fn store_remote_settings(
        &self,
        settings: crate::util::config::RemoteSettings,
    ) {
        let mut cfg = self.cfg.borrow_mut();
        cfg.remote_settings = Some(settings);
        crate::util::config::sync_campaign_fields(&mut cfg);
        if let Some(v) = cfg
            .remote_settings
            .as_ref()
            .and_then(|s| s.path_not_found_hints)
        {
            cfg.path_not_found_hints = v;
        }
    }
    /// Stores settings and fans out side effects via [`Self::on_remote_settings_changed`].
    /// Shared tail for callers that do not also re-init the telemetry client (those use [`Self::refresh_remote_settings`]).
    pub(super) fn install_remote_settings(
        &self,
        settings: crate::util::config::RemoteSettings,
    ) {
        self.store_remote_settings(settings);
        self.on_remote_settings_changed();
    }
    /// Re-fetch remote settings, re-init the telemetry client, apply side effects, and push `x.ai/settings/update` to clients. Called from both auth handlers (first install and reauth/account switch).
    /// Agent-level fields resolved at startup (`worktree_type`, `restore_code`) are NOT re-resolved here. That requires a broader refactor of the init path.
    pub(super) async fn refresh_remote_settings(&self, auth: &xai_grok_login::GrokAuth) {
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::debug!("post-auth settings refresh skipped: remote_fetch disabled");
            return;
        }
        let is_xai = auth.is_xai_auth();
        let user_id = auth.user_id.clone();
        let team_id = auth.team_id.clone();
        let remote_was_absent = self.cfg.borrow().remote_settings.is_none();
        let Some(settings) = self.fetch_settings_resolving_gate(auth).await else {
            if remote_was_absent {
                self.run_deferred_remote_work();
            }
            return;
        };
        tracing::info!("post-auth settings refreshed");
        self.store_remote_settings(settings);
        let (
            telemetry_config,
            telemetry_mode,
            grok_user_id,
            grok_team_id,
            deployment_key,
            subscription_tier,
        ) = {
            let cfg = self.cfg.borrow();
            crate::util::config::cache_remote_mcp_startup_timeout_secs(
                cfg.remote_settings.as_ref().and_then(|s| s.mcp_startup_timeout_secs),
            );
            let telemetry_mode = cfg.resolve_telemetry_mode();
            let trace_upload = cfg.resolve_trace_upload();
            tracing::info!(
                telemetry = %telemetry_mode,
                trace_upload = %trace_upload,
                "post-auth data capture config re-resolved",
            );
            let grok_user_id = is_xai.then(|| user_id.clone());
            let grok_team_id = is_xai.then(|| team_id.clone()).flatten();
            let telemetry_config = cfg.telemetry.clone();
            let deployment_key = cfg.endpoints.deployment_key.clone();
            let subscription_tier_display = cfg
                .remote_settings
                .as_ref()
                .and_then(|rs| rs.subscription_tier_display.clone());
            (
                telemetry_config,
                telemetry_mode.value,
                grok_user_id,
                grok_team_id,
                deployment_key,
                subscription_tier_display,
            )
        };
        let subscription_tier = resolve_subscription_tier_for_telemetry(
            subscription_tier,
            self.auth_manager.current_or_expired().as_ref(),
        );
        xai_grok_telemetry::client::init(
            telemetry_config,
            telemetry_mode,
            grok_user_id,
            grok_team_id,
            deployment_key,
            self.origin_client_info_from_meta(None),
            xai_grok_version::VERSION.to_owned(),
            subscription_tier,
            crate::http::shared_client(),
        );
        xai_grok_login::credential_provider::sync_external_otel_identity();
        self.on_remote_settings_changed();
        if remote_was_absent {
            self.run_deferred_remote_work();
        }
    }
    /// Refresh remote settings and re-resolve eagerly-resolved config fields. Called on `/new` session creation so feature flags reflect the latest remote settings state without requiring a TUI restart.
    /// Extends [`refresh_remote_settings`] by also re-running [`resolve_runtime_fields`] with the fresh settings. In-flight sessions are unaffected; they snapshot config at creation.
    pub(super) async fn refresh_settings_and_reapply(
        &self,
        auth: &xai_grok_login::GrokAuth,
    ) {
        self.refresh_remote_settings(auth).await;
        {
            let mut cfg = self.cfg.borrow_mut();
            crate::util::config::sync_campaign_fields(&mut cfg);
        }
        let raw_config = crate::config::load_effective_config()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "config reload failed during settings refresh");
                toml::Value::Table(toml::map::Map::new())
            });
        self.re_resolve_runtime_fields_and_sync_memory(&raw_config);
        self.sync_collection_config_gate();
        self.emit_settings_update_notification();
        self.emit_announcements(AnnouncementsPushMode::Force);
        self.reconfigure_heap_profile_monitor();
    }
    /// Spawns a background task coalesced on `in_flight`: a request while one is in flight is dropped.
    /// The task is bounded by `SETTINGS_REAPPLY_TIMEOUT`.
    /// Returns whether a task was spawned.
    fn spawn_coalesced_settings_task(
        &self,
        in_flight: &std::rc::Rc<std::cell::Cell<bool>>,
        task: impl std::future::Future<Output = ()> + 'static,
    ) -> bool {
        if in_flight.replace(true) {
            return false;
        }
        let in_flight = in_flight.clone();
        tokio::task::spawn_local(async move {
            struct ClearOnDrop(std::rc::Rc<std::cell::Cell<bool>>);
            impl Drop for ClearOnDrop {
                fn drop(&mut self) {
                    self.0.set(false);
                }
            }
            let _clear = ClearOnDrop(in_flight);
            let _ = tokio::time::timeout(crate::http::SETTINGS_REAPPLY_TIMEOUT, task)
                .await;
        });
        true
    }
    /// Fire-and-forget remote settings refresh for new sessions (at most one in flight).
    pub(super) fn spawn_settings_reapply(&self) {
        let agent_ref = LocalRef::new(self);
        let auth_manager = self.auth_manager.clone();
        let _spawned = self
            .spawn_coalesced_settings_task(
                &self.settings_reapply_in_flight,
                async move {
                    let auth_result = tokio::time::timeout(
                            crate::http::STARTUP_FETCH_TIMEOUT,
                            auth_manager.auth(),
                        )
                        .await;
                    let mut deferred_to_sibling = false;
                    if let Ok(Ok(auth)) = auth_result {
                        let agent = agent_ref.get();
                        if agent.post_auth_settings_in_flight.get() {
                            deferred_to_sibling = true;
                        } else {
                            agent.refresh_settings_and_reapply(&auth).await;
                        }
                    }
                    if !deferred_to_sibling {
                        agent_ref.get().run_deferred_remote_work();
                    }
                },
            );
        #[cfg(test)]
        if _spawned {
            self.settings_reapply_spawn_count
                .set(self.settings_reapply_spawn_count.get() + 1);
        }
    }
    /// Resolve post-auth remote settings in the background. A slow or hung `/settings` then can't gate `authenticate` (and thus the client's first draw).
    /// The external-OTEL gate stays fail-closed until this resolves; the result reaches clients via `x.ai/settings/update`. Its own guard keeps an in-flight reapply from coalescing away the authenticated identity.
    pub(super) fn spawn_post_auth_settings(&self, auth: xai_grok_login::GrokAuth) {
        let agent_ref = LocalRef::new(self);
        let _spawned = self
            .spawn_coalesced_settings_task(
                &self.post_auth_settings_in_flight,
                async move {
                    let agent = agent_ref.get();
                    agent.refresh_remote_settings(&auth).await;
                    agent.maybe_fetch_post_auth_settings().await;
                },
            );
        #[cfg(test)]
        if _spawned {
            self.post_auth_settings_spawn_count
                .set(self.post_auth_settings_spawn_count.get() + 1);
        }
    }
    /// Spawn the periodic remote-settings poll that pushes mid-session announcement changes to connected clients. Idempotent.
    /// Plain loop (no cancellation) like `ensure_session_supervisor`; the LocalSet drop at process exit ends it. Skipped under `cfg!(test)` like the managed-config sync (PTY e2e runs the real binary and is unaffected).
    pub(super) fn spawn_announcements_refresh(&self) {
        if cfg!(test) || self.announcements_refresh_started.replace(true) {
            return;
        }
        let agent_ref = LocalRef::new(self);
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(announcements_refresh_interval());
            interval.tick().await;
            loop {
                interval.tick().await;
                let result = futures::FutureExt::catch_unwind(
                        std::panic::AssertUnwindSafe(
                            agent_ref.get().poll_announcements_refresh_once(),
                        ),
                    )
                    .await;
                if result.is_err() {
                    tracing::error!("announcements refresh tick panicked; continuing");
                }
            }
        });
    }
    /// One poll cycle. With no settings baseline, first population is delegated to the fill-if-missing path (which emits on success). Otherwise refresh the stored announcements best-effort, then run the emit gate.
    /// The gate runs even when the fetch was skipped or failed, so a pure expiry crossing still clears client banners on time.
    async fn poll_announcements_refresh_once(&self) {
        if self.cfg.borrow().remote_settings.is_none() {
            self.maybe_fetch_post_auth_settings().await;
            return;
        }
        self.fetch_and_store_polled_settings().await;
        self.emit_announcements(AnnouncementsPushMode::IfChanged);
    }
    /// Fetch half of a poll cycle: fresh settings from the proxy, then the poll-only apply.
    /// Every failure path is a silent skip; the next tick retries.
    async fn fetch_and_store_polled_settings(&self) {
        let Ok(auth) = self.auth_manager.auth().await else {
            tracing::debug!("announcements refresh skipped: not authenticated");
            return;
        };
        let pre_fetch = self.cfg.borrow().remote_settings.as_ref().map(polled_fields);
        let Some(settings) = self.fetch_remote_settings(auth).await else {
            tracing::debug!("announcements refresh skipped: settings fetch failed");
            return;
        };
        self.apply_polled_settings(settings, pre_fetch);
    }
    pub(super) fn apply_polled_settings(
        &self,
        fresh: crate::util::config::RemoteSettings,
        pre_fetch: Option<PolledFields>,
    ) {
        let mut cfg = self.cfg.borrow_mut();
        let origin = cfg.endpoints.proxy_url();
        let Some(stored) = cfg.remote_settings.as_mut() else {
            return;
        };
        if pre_fetch.as_ref() != Some(&polled_fields(stored)) {
            tracing::debug!("settings poll apply skipped: settings changed mid-fetch");
            return;
        }
        stored.announcements = fresh.announcements;
        stored.accept_request_encodings = fresh.accept_request_encodings;
        crate::util::config::cache_remote_accept_request_encodings(
            &origin,
            &stored.accept_request_encodings,
        );
    }
    /// The single announcements push gate: every `remote_settings` writer funnels through here. Emits `x.ai/announcements/update` and advances the last-emitted baseline per [`announcements_push_payload`].
    /// `mode` decides when an unchanged list still pushes. The baseline advances only once the gateway accepts the send. A failed enqueue leaves it untouched so the next gate call re-diffs and re-pushes.
    /// Synchronous by design: the decide, send, advance sequence cannot interleave with another gate call on the LocalSet.
    pub(super) fn emit_announcements(&self, mode: AnnouncementsPushMode) {
        let payload_list = {
            let cfg = self.cfg.borrow();
            let last = self.last_emitted_announcements.borrow();
            announcements_push_payload(
                cfg.remote_settings.as_ref().and_then(|s| s.announcements.as_deref()),
                &last,
                chrono::Utc::now(),
                mode,
            )
        };
        let Some(announcements) = payload_list else {
            return;
        };
        let payload = serde_json::json!({
            "gen": self.next_announcements_gen(),
            "announcements": announcements,
        });
        let Ok(params) = serde_json::value::to_raw_value(&payload) else {
            return;
        };
        let accepted = self
            .gateway
            .forward_fire_and_forget(
                acp::ExtNotification::new("x.ai/announcements/update", params.into()),
            );
        if !accepted {
            return;
        }
        *self.last_emitted_announcements.borrow_mut() = announcements.clone();
        tracing::info!(
            count = announcements.len(),
            mode = ?mode,
            "pushing announcements update to clients"
        );
    }
    /// Next generation for an `x.ai/announcements/update` push.
    /// Strictly increasing within the process, and seeded from unix-epoch seconds.
    /// So a restarted leader's pushes still clear pager watermarks that survived re-election (`AppView.announcements_last_gen` outlives the agent).
    pub(super) fn next_announcements_gen(&self) -> u64 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let next = now_secs.max(self.announcements_gen.get() + 1);
        self.announcements_gen.set(next);
        next
    }
    /// Shared fetch half of every settings refresh. Endpoint fields come from a scoped `cfg` borrow; failures normalize to `None`. `fetch_settings_blocking` runs off-executor (it already retries transient errors internally).
    /// Callers own their miss logging; the apply halves deliberately stay separate (full reapply vs announcements-only).
    pub(super) async fn fetch_remote_settings(
        &self,
        auth: xai_grok_login::GrokAuth,
    ) -> Option<crate::util::config::RemoteSettings> {
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::debug!("settings fetch skipped: remote_fetch disabled");
            return None;
        }
        let (base_url, alpha_test_key) = {
            let cfg = self.cfg.borrow();
            (cfg.endpoints.proxy_url(), cfg.endpoints.alpha_test_key.clone())
        };
        match tokio::task::spawn_blocking(move || crate::remote::fetch_settings_blocking(
                &base_url,
                &auth,
                alpha_test_key.as_deref(),
            ))
            .await
        {
            Ok(outcome) => outcome.into_option(),
            Err(e) => {
                tracing::warn!(error = %e, "settings fetch task panicked");
                None
            }
        }
    }
    pub(super) async fn send_model_auto_switched(
        &self,
        session_id: &acp::SessionId,
        previous: &acp::ModelId,
        new: &acp::ModelId,
        reason: &str,
    ) {
        let notification = crate::extensions::notification::SessionNotification {
            session_id: session_id.clone(),
            update: crate::extensions::notification::SessionUpdate::ModelAutoSwitched {
                previous_model_id: previous.0.to_string(),
                new_model_id: new.0.to_string(),
                reason: reason.to_string(),
            },
            meta: None,
        };
        if let Ok(params) = serde_json::value::to_raw_value(&notification) {
            let _ = self
                .gateway
                .ext_notification(
                    acp::ExtNotification::new("x.ai/session_notification", params.into()),
                )
                .await;
        }
    }
    /// Pure id-to-entry resolver (the `allowed_models` gate lives in `set_session_model`).
    pub(crate) fn resolve_model_id(
        &self,
        requested: &acp::ModelId,
    ) -> Result<ModelEntry, acp::Error> {
        let requested_str = requested.0.as_ref();
        let models = self.models_manager.models();
        let Some(catalog_key) = resolve_catalog_key(&models, requested) else {
            tracing::debug!(
                requested = %requested_str,
                model_count = models.len(),
                "resolve_model_id: unknown model id (not in models() by key or .model field)"
            );
            return Err(acp::Error::invalid_params().data("unknown model id"));
        };
        let entry = models
            .get(catalog_key.0.as_ref())
            .expect("resolve_catalog_key returns a key present in models");
        let match_kind = if catalog_key.0.as_ref() == requested_str {
            "map key"
        } else {
            "model field scan"
        };
        tracing::debug!(
            "resolve_model_id: matched by {}: requested={} model={}",
            match_kind,
            requested_str,
            entry.info.model
        );
        Ok(entry.clone())
    }
    pub(crate) fn prepare_sampling_config_for_model(
        &self,
        model: &ModelEntry,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        let preferred = self.cfg.borrow().grok_com_config.preferred_method;
        let prefers_oidc = preferred == Some(PreferredAuthMethod::Oidc);
        let is_session_based_auth = self.is_session_based_auth();
        let session = match preferred {
            Some(PreferredAuthMethod::ApiKey) => None,
            _ if is_session_based_auth => self.auth_manager.current_or_expired(),
            _ => None,
        };
        let has_session_key = session.is_some();
        let mut credentials = resolve_credentials(
            model,
            session.as_ref().map(|a| a.key.as_str()),
        );
        if prefers_oidc && !model.has_own_credentials()
            && credentials.auth_type == xai_chat_state::AuthType::ApiKey
        {
            credentials.api_key = None;
            credentials.auth_type = xai_chat_state::AuthType::SessionToken;
        }
        crate::agent::config::enforce_disable_api_key_auth(
            &mut credentials,
            self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
            session.as_ref().map(|a| a.key.as_str()),
        );
        if !has_session_key && credentials.auth_type == xai_chat_state::AuthType::ApiKey
            && !model.has_own_credentials() && is_session_based_auth
        {
            tracing::info!(
                model = model.info().model.as_str(),
                "auth: overriding auth_type to SessionToken (session-based auth method)",
            );
            xai_grok_telemetry::unified_log::info(
                "auth auth_type override to SessionToken",
                None,
                Some(serde_json::json!({ "model": model.info().model.as_str() })),
            );
            credentials.auth_type = xai_chat_state::AuthType::SessionToken;
        }
        if should_warn_missing_session(MissingSessionCtx {
            has_session_key,
            has_own_credentials: model.has_own_credentials(),
            is_session_based_auth,
            preferred,
        }) {
            tracing::warn!(
                model = model.info().model.as_str(),
                is_expired = self.auth_manager.is_expired(),
                auth_type = ?credentials.auth_type,
                "auth: prepare_sampling_config has no session key",
            );
            xai_grok_telemetry::unified_log::warn(
                "auth: prepare_sampling_config has no session key",
                None,
                Some(
                    serde_json::json!({
                    "model": model.info().model.as_str(),
                    "is_expired": self.auth_manager.is_expired(),
                    "auth_type": format!("{:?}", credentials.auth_type),
                }),
                ),
            );
        }
        let cfg = self.cfg.borrow();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let client_version = cfg.client_version.clone();
        let deployment_id = crate::managed_config::resolve_deployment_id(
            cfg.endpoints.deployment_key.as_deref(),
        );
        drop(cfg);
        let user_id = self
            .auth_manager
            .current_or_expired()
            .filter(|a| a.is_xai_auth())
            .map(|a| a.user_id);
        let mut config = crate::agent::config::sampling_config_for_model(
            model,
            credentials,
            alpha_test_key,
            client_version,
            deployment_id,
            user_id,
        );
        config.origin_client = origin_client;
        config
    }
    /// Resolve sampling config for a model by ID, falling back to the global default on resolution failure.
    /// API-key auth then routes to the public API (via resolve_credentials) instead of the global config's cli-chat-proxy base_url.
    pub(super) fn resolve_sampling_config_for_model(
        &self,
        model_id: &acp::ModelId,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        if let Ok(model) = self.resolve_model_id(model_id) {
            self.prepare_sampling_config_for_model(&model, origin_client.clone())
        } else {
            let mut c = self.sampling_config.borrow().clone();
            c.origin_client = origin_client;
            c
        }
    }
    /// Apply a profile's pinned-model override to the session's sampling config.
    /// `pinned_model` is resolved once by the caller (shared with harness inheritance).
    /// `None` (no override, or model not in catalog) keeps the session defaults.
    fn apply_agent_model_override(
        &self,
        pinned_model: Option<&(acp::ModelId, ModelEntry)>,
        default_model_id: acp::ModelId,
        default_sampling: SamplingConfig,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> (acp::ModelId, SamplingConfig) {
        let Some((id, model)) = pinned_model else {
            return (default_model_id, default_sampling);
        };
        let new_config = self.prepare_sampling_config_for_model(model, origin_client);
        tracing::info!(
            model = %id.0,
            "agent profile model override applied to parent session"
        );
        (id.clone(), new_config)
    }
    /// Whether the current session is a personal grok.com account on a gated tier (free / X Basic). The Imagine tools stay advertised to the model but are flagged tier-restricted.
    /// They then short-circuit at call time with the SuperGrok upsell prose (see `ImageGenConfig`/`VideoGenConfig`'s `tier_restricted`).
    /// Fails **open** (returns `false`) whenever we can't positively confirm a restricted personal tier. So this client gate is a UX optimization (a clean in-chat upsell instead of a doomed request), never the security boundary. The only difference is the absent-tier policy (the pager hides on `None`, we fail open on `None`).
    fn is_tier_restricted_capability(&self) -> bool {
        let Some(auth) = self.auth_manager.current() else {
            return false;
        };
        if !auth.is_xai_auth() || auth.team_id.is_some() {
            return false;
        }
        let tier = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|rs| rs.subscription_tier_display.clone())
            .or_else(|| jwt_tier_claim(&auth.key));
        tier.as_deref().is_some_and(crate::tier::is_restricted_tier_name)
    }
    /// Both BYOK and session (OAuth) users go direct to `xai_api_base_url`.
    /// `sampling_config.api_key` carries the OAuth bearer for session users (the `api_key_provider` refreshes it per request).
    /// So IC authenticates and meters Imagine usage per-user.
    pub(super) fn prepare_image_gen_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig {
        use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
        let sampling_config = self.sampling_config.borrow();
        let Some(ref api_key) = sampling_config.api_key else {
            return ImageGenConfig::Disabled;
        };
        let tier_restricted = self.is_tier_restricted_capability();
        let cfg = self.cfg.borrow();
        let base_url = cfg.endpoints.xai_api_base_url.clone();
        let version = cfg
            .client_version
            .clone()
            .unwrap_or_else(|| xai_grok_version::VERSION.to_string());
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let mut headers = indexmap::IndexMap::new();
        headers.insert("user-agent".to_string(), format!("xai-remote-code/{version}"));
        inject_proxy_headers(
            &mut headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &base_url,
        );
        ImageGenConfig::Enabled {
            api_key: api_key.clone(),
            base_url,
            extra_headers: headers,
            image_gen_enabled: cfg.resolve_image_gen().value,
            image_edit_enabled: cfg.resolve_image_edit().value,
            model_override: cfg.resolve_image_gen_model_override(),
            edit_model_override: cfg.resolve_image_edit_model_override(),
            tier_restricted,
        }
    }
    /// The tool talks directly to the deployer service.
    pub(super) fn prepare_app_builder_deployer_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig {
        use xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig;
        AppBuilderDeployerConfig::Disabled
    }
    /// Video tools call the Remote-Code API directly.
    pub(super) fn prepare_video_gen_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig {
        use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
        let cfg = self.cfg.borrow();
        if !cfg.resolve_video_gen().value {
            return VideoGenConfig::Disabled;
        }
        let Some(api_key) = self.sampling_config.borrow().api_key.clone() else {
            return VideoGenConfig::Disabled;
        };
        let tier_restricted = self.is_tier_restricted_capability();
        let zdr_video_output_s3 = cfg
            .disable_zdr_incompatible_tools
            .then(|| cfg.zdr_video_output_s3.clone())
            .flatten()
            .filter(|s3| s3.is_valid());
        let zdr_restricted = cfg.disable_zdr_incompatible_tools
            && zdr_video_output_s3.is_none();
        if zdr_restricted {
            tracing::info!("video_gen zdr-restricted by tools.disable_zdr_incompatible_tools");
        }
        let base_url = cfg.endpoints.xai_api_base_url.clone();
        let version = cfg
            .client_version
            .clone()
            .unwrap_or_else(|| xai_grok_version::VERSION.to_string());
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let mut headers = indexmap::IndexMap::new();
        headers.insert("user-agent".to_string(), format!("xai-remote-code/{version}"));
        inject_proxy_headers(
            &mut headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &base_url,
        );
        VideoGenConfig::Enabled {
            api_key,
            base_url,
            extra_headers: headers,
            zdr_video_output_s3: zdr_video_output_s3.map(Box::new),
            tier_restricted,
            zdr_restricted,
        }
    }
    pub(super) fn prepare_web_search_sampling_config(&self) -> Option<SamplingConfig> {
        let model_id = self.cfg.borrow().web_search_model.clone();
        let models = self.models_manager.models();
        let session = self.current_or_buffered_auth();
        let alpha_test_key = self.cfg.borrow().endpoints.alpha_test_key.clone();
        let client_version = self.cfg.borrow().client_version.clone();
        let mut cfg = config::resolve_web_search_sampling_config(
            &model_id,
            &models,
            session.as_ref().map(|a| a.key.as_str()),
            self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
            alpha_test_key.clone(),
            client_version,
            &self.cfg.borrow().endpoints,
        )?;
        inject_proxy_headers(
            &mut cfg.extra_headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        Some(cfg)
    }
    /// The caller at the process boundary renders the [`crate::agent::init::BootstrapError`] and exits.
    pub fn new(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        boot: Option<crate::agent::init::BootstrapPrefetch>,
    ) -> Result<Self, crate::agent::init::BootstrapError> {
        let (cfg, models_manager) = crate::agent::init::bootstrap_with_cancel(
            cfg,
            &auth_manager,
            prefetched_models,
            &tokio_util::sync::CancellationToken::new(),
            boot,
        )?;
        Ok(Self::with_models(gateway, &cfg, auth_manager, models_manager))
    }
    /// Prepare the web fetch configuration based on feature flags.
    /// Enabled gate: `disable_web_search` kill-switch > `GROK_WEB_FETCH` env > remote settings `web_fetch_enabled` > default (false).
    /// Params resolution (TOML > env > remote settings > default): `proxy_endpoint`: `[toolset.web_fetch] proxy_endpoint` > `GROK_WEB_FETCH_PROXY` > remote settings > None `allowed_domains`: `[toolset.web_fetch] allowed_domains` > remote settings > built-in defaults `allow_local`: `[toolset.web_fetch] allow_local` > `GROK_WEB_FETCH_ALLOW_LOCAL` > false
    pub(super) fn prepare_web_fetch_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig {
        use xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig;
        let cfg = self.cfg.borrow();
        if cfg.disable_web_search {
            return WebFetchConfig::Disabled;
        }
        let remote = cfg.remote_settings.as_ref();
        if !cfg.is_feature_enabled(crate::agent::config::Feature::WebFetch) {
            return WebFetchConfig::Disabled;
        }
        let context_window = Some(self.sampling_config.borrow().context_window);
        let params = cfg
            .toolset
            .web_fetch
            .resolve_params(
                remote.and_then(|s| s.web_fetch_proxy.as_deref()),
                remote.and_then(|s| s.web_fetch_allowed_domains.as_deref()),
                context_window,
            );
        if params.allowed_domains.as_ref().is_some_and(Vec::is_empty) {
            tracing::info!("web_fetch disabled: allowed_domains is explicitly empty");
            return WebFetchConfig::Disabled;
        }
        WebFetchConfig::Enabled { params }
    }
    /// Construct from pre-built components.
    /// Use when the caller needs the `ModelsManager` handle externally (e.g. `run_leader` wires it to the config watcher).
    /// Otherwise prefer [`Self::new`].
    pub fn with_models(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        models_manager: crate::agent::remote_config::ModelsManager,
    ) -> Self {
        models_manager.set_gateway(gateway.clone());
        let sampling_config = models_manager.sampling_config();
        if !cfg.grok_com_config.api_key_auth_disabled() {
            let models = models_manager.models();
            let current = models_manager.current_model_id();
            auth_manager
                .set_process_static_api_key(
                    byok_from_models(&models, None, current.0.as_ref()),
                );
        }
        crate::upload::trace::spawn_purge_stale_upload_scratch();
        let storage_mode = cfg.storage_mode;
        let default_yolo_mode = cfg.default_yolo_mode;
        let default_auto_mode = cfg.default_auto_mode;
        let tui_mode = cfg.mode == crate::agent::config::AgentMode::Tui;
        let relay_config_enabled = crate::util::config::load_relay_sync_enabled_sync();
        let has_xai_auth = auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_xai_auth());
        let relay_sync_enabled = tui_mode && relay_config_enabled && has_xai_auth;
        let config_root = crate::config::load_effective_config().ok();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let raw = config_root.as_ref().unwrap_or(&empty_config);
        let (worktree_type, wt_source) = crate::util::config::resolve_worktree_type(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let restore_code = crate::util::config::resolve_restore_code(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let session_registry_local = crate::util::config::session_registry_local_override(
            config_root.as_ref(),
        );
        tracing::info!(
            worktree_type = ?worktree_type,
            source = wt_source,
            "WORKTREE_CONFIG_SHELL: resolved worktree type at agent startup"
        );
        if relay_sync_enabled {
            tracing::info!("[grok] Relay sync: ENABLED");
        } else if tui_mode && relay_config_enabled && !has_xai_auth {
            tracing::info!("[grok] Relay sync: DISABLED (no auth - run 'grok login' first)");
        } else if tui_mode && !relay_config_enabled {
            tracing::debug!("Relay sync: DISABLED (not configured in config.toml or env)");
        } else {
            tracing::debug!("Relay sync: DISABLED (not in TUI mode)");
        }
        if cfg.telemetry.trace_upload == Some(false) {
            tracing::info!(
                enabled = false,
                reason = "feature_off",
                "trace_upload_status"
            );
        }
        let (subagent_event_tx, subagent_event_rx) = crate::agent::subagent::subagent_coordinator_channel();
        let activity = crate::agent::activity::AgentActivity::default();
        let instance = Self {
            activity,
            session_registry: SessionRegistry::default(),
            resident_roster_titles: RefCell::new(HashMap::new()),
            initialize_request: OnceLock::new(),
            gateway,
            launch_cwd: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            launch_dir_trust: std::cell::OnceCell::new(),
            plugin_registry_handle: xai_grok_agent::plugins::SharedPluginRegistryHandle::new(
                None,
                cfg.plugins.cli_plugin_dirs.clone(),
            ),
            plugin_registry_initialized: std::cell::Cell::new(false),
            models_manager,
            chat_modes: {
                let chat_modes = crate::agent::chat_modes::ChatModesManager::new(
                    auth_manager.clone(),
                );
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    chat_modes.warm_in_background();
                }
                chat_modes
            },
            cfg: RefCell::new(cfg.clone()),
            auth_method_id: crate::agent::auth_method::new_shared_auth_method_id(None),
            sampling_config: RefCell::new(sampling_config),
            auth_manager,
            interactive_auth: Default::default(),
            client_type: RefCell::new(ClientType::default()),
            code_nav_enabled: std::cell::Cell::new(false),
            interactive_trust_client: std::cell::Cell::new(false),
            interactive_trust_prompted: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            tier_allowed: std::cell::Cell::new(true),
            allow_access_resolved_for: std::cell::RefCell::new(None),
            official_marketplace_register: std::cell::RefCell::new(None),
            storage_mode: std::cell::Cell::new(storage_mode),
            otel_gate: crate::agent::otel_gate::OtelGate::default(),
            default_yolo_mode,
            default_auto_mode,
            trace_upload_live: Arc::new(
                std::sync::atomic::AtomicBool::new(cfg.is_trace_upload_enabled()),
            ),
            feedback_trace_upload_grants: RefCell::new(VecDeque::new()),
            memory_config: RefCell::new(None),
            config_watcher_path_tx: None,
            relay_sync_enabled,
            buffering_settings: RefCell::new(None),
            background_copy_context: BackgroundCopyContext::new(),
            codebase_indexes: Arc::new(
                parking_lot::Mutex::new(CodebaseIndexManager::new()),
            ),
            search_index: crate::session::storage::search::SharedSearchIndex::default(),
            worktree_type,
            restore_code,
            session_registry_local,
            managed_mcp_cache: Default::default(),
            agent_mcp_state: std::sync::Arc::new(
                tokio::sync::Mutex::new(
                    crate::session::mcp_servers::McpState::new(vec![]),
                ),
            ),
            subagent_event_tx,
            subagent_event_rx: RefCell::new(Some(subagent_event_rx)),
            subagent_presentation: RefCell::new(
                crate::agent::subagent::SubagentPresentation::new(),
            ),
            subagent_sampling_semaphore: Arc::new(
                tokio::sync::Semaphore::new(cfg.subagents_sampling_limit),
            ),
            monitor_event_buffer: xai_grok_tools::implementations::grok_build::monitor::types::MonitorEventBuffer::default(),
            bundle_sync_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            post_unblock_jwt_retry_in_flight: Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ),
            tier_recheck_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            workspace_ops: RefCell::new(None),
            #[cfg(all(feature = "local-workspace", unix))]
            local_workspace_supervisors: Rc::new(RefCell::new(HashMap::new())),
            #[cfg(all(feature = "local-workspace", unix))]
            local_workspace_generations: Rc::new(RefCell::new(HashMap::new())),
            #[cfg(all(feature = "local-workspace", unix))]
            local_workspace_restart_pending: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            #[cfg(feature = "local-workspace")]
            local_workspace_bound: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            supervisor_started: std::cell::Cell::new(false),
            settings_reapply_in_flight: std::rc::Rc::new(std::cell::Cell::new(false)),
            post_auth_settings_in_flight: std::rc::Rc::new(std::cell::Cell::new(false)),
            settings_refresh: crate::agent::remote_config::SettingsRefresh::default(),
            announcements_gen: std::cell::Cell::new(0),
            last_emitted_announcements: RefCell::new(Vec::new()),
            announcements_refresh_started: std::cell::Cell::new(false),
            heap_profile_monitor: RefCell::new(
                crate::heap_profile::HeapProfileMonitor::new(),
            ),
            heap_profile_started: std::cell::Cell::new(false),
            #[cfg(test)]
            finalize_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            roster_delta_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            supervisor_spawn_count: std::cell::Cell::new(0),
            #[cfg(test)]
            settings_reapply_spawn_count: std::cell::Cell::new(0),
            #[cfg(test)]
            auto_gc_spawn_count: std::cell::Cell::new(0),
            #[cfg(test)]
            post_auth_settings_spawn_count: std::cell::Cell::new(0),
            #[cfg(test)]
            tier_recheck_run_count: std::cell::Cell::new(0),
        };
        instance
            .auth_manager
            .configure_refresher(
                instance.cfg.borrow().grok_com_config.auth_provider_command.clone(),
                instance.diagnostic_upload_config(),
            );
        xai_grok_login::credential_provider::wire_otel_auth_manager(
            instance.auth_manager.clone(),
        );
        if let Some(ref dk) = instance.cfg.borrow().endpoints.deployment_key {
            xai_grok_login::credential_provider::wire_otel_deployment_key(dk.clone());
        }
        instance
    }
    /// Client disconnect: keep working sessions resident, idle-unload the rest (never destroy).
    pub(super) async fn handle_evict_sessions(
        &self,
        params: &serde_json::value::RawValue,
    ) {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct EvictParams {
            session_ids: Vec<String>,
        }
        let Ok(p) = serde_json::from_str::<EvictParams>(params.get()) else {
            tracing::warn!("Failed to parse evict_sessions params");
            return;
        };
        if p.session_ids.is_empty() {
            return;
        }
        tracing::info!(
            count = p.session_ids.len(),
            sessions = ?p.session_ids,
            "Client disconnected; detaching sessions (no-evict keystone)"
        );
        let (attaching, to_check): (Vec<_>, Vec<_>) = p
            .session_ids
            .iter()
            .map(|sid| acp::SessionId::new(sid.clone()))
            .partition(|id| self.session_registry.is_attaching(id));
        for id in &attaching {
            tracing::info!(
                session_id = %id.0,
                "kept session resident across client disconnect (attach in flight)"
            );
        }
        let checks = to_check
            .into_iter()
            .map(|id| async move {
                let measured = self.resident_handle(&id).map(|h| h.cmd_tx);
                let busy = self.session_is_busy(&id).await;
                (id, busy, measured)
            });
        let resolved = futures::future::join_all(checks).await;
        let mut kept_resident: usize = attaching.len();
        let mut unloaded: usize = 0;
        for (id, busy, measured) in resolved {
            if self.session_registry.is_attaching(&id) {
                kept_resident += 1;
                tracing::info!(
                    session_id = %id.0,
                    "kept session resident across client disconnect (attach in flight)"
                );
                continue;
            }
            let same_actor = self
                .resident_handle(&id)
                .zip(measured)
                .is_some_and(|(current, measured)| {
                    current.cmd_tx.same_channel(&measured)
                });
            if !same_actor {
                kept_resident += 1;
                tracing::info!(
                    session_id = %id.0,
                    "kept session resident across client disconnect (actor replaced mid-check)"
                );
                continue;
            }
            if busy {
                if let Some(handle) = self.resident_handle(&id) {
                    handle.set_status_line_wanted(false);
                }
                self.set_session_live_state(&id, SessionLiveState::Working);
                kept_resident += 1;
                tracing::info!(
                    session_id = %id.0,
                    "kept session resident across client disconnect (live work)"
                );
                continue;
            }
            if let Some(handle) = self.resident_handle(&id) {
                handle.persist_resume_status().await;
            }
            self.request_session_shutdown(&id);
            self.retire_root_session(&id);
            if self.take_session(&id).is_some() {
                self.session_registry.remove_retired_root(&id);
                self.session_registry.clear_resident(&id);
                self.set_session_live_state(&id, SessionLiveState::Dormant);
                unloaded += 1;
                tracing::debug!(session_id = %id.0, "idle session unloaded to disk on disconnect");
            }
        }
        tracing::info!(kept_resident, unloaded, "client-disconnect detach complete");
        self.sweep_dead_sessions();
    }
    /// Wait for an old session thread to finish before reloading the same session. When a client disconnects and a session is *idle*, `handle_evict_sessions` unloads it.
    /// That sends `Shutdown`, drops the `SessionHandle`, and keeps the `SessionThread`. (Sessions with live work stay fully resident and skip this path.)
    /// If the client reconnects and loads the same session, wait for the old actor to finish flushing to disk before replaying `updates.jsonl`. Uses async polling (never blocks the `LocalSet` runtime) with a 5s deadline to handle slow shutdowns (e.g., embedding API timeouts).
    pub(super) async fn drain_old_session_thread(&self, session_id: &acp::SessionId) {
        self.drain_old_session_thread_within(session_id, DRAIN_OLD_THREAD_WAIT).await;
    }
    /// [`Self::drain_old_session_thread`] under a caller-supplied budget.
    pub(super) async fn drain_old_session_thread_within(
        &self,
        session_id: &acp::SessionId,
        budget: std::time::Duration,
    ) {
        match self.session_registry.thread_is_finished(session_id) {
            None => return,
            Some(true) => {
                self.session_registry.clear_thread(session_id);
                return;
            }
            Some(false) => {}
        }
        tracing::info!(
            session_id = %session_id.0,
            "Waiting for old session thread to finish before reload"
        );
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match self.session_registry.thread_is_finished(session_id) {
                None => return,
                Some(true) => {
                    self.session_registry.clear_thread(session_id);
                    tracing::debug!(
                        session_id = %session_id.0,
                        "Old session thread finished cleanly"
                    );
                    return;
                }
                Some(false) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    budget_ms = budget.as_millis() as u64,
                    "Old session thread still running at the drain budget; proceeding. \
                     Session data may be incomplete if the old actor is still writing."
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    /// Mark a `session/load` as in flight for `session_id`.
    /// While it is alive, [`Self::wait_for_in_flight_session_load`] blocks racing session-scoped requests for the same session.
    /// Dropping the guard (every exit path of `load_session`, success or error) removes the marker and wakes all waiters via watch-channel closure.
    pub(super) fn begin_session_load(
        &self,
        session_id: &acp::SessionId,
    ) -> SessionLoadGuard<'_> {
        let (tx, rx) = self.session_registry.begin_attach(session_id);
        SessionLoadGuard {
            agent: self,
            session_id: session_id.clone(),
            rx,
            _tx: tx,
        }
    }
    /// Session lookup that tolerates an in-flight `session/load`. THE chokepoint for the post-leader-crash error class.
    /// Every user-facing session-scoped handler (`prompt`, `set_session_model`, `interject`, ...) resolves its handle through this.
    /// A bare `sessions` lookup would make a request racing the reconnect-replayed `session/load` fail with "unknown session id". This waits for the session to land instead. Returns `None` only when the session is genuinely absent: no load in flight (or the load failed / timed out). Those are exactly the cases where the legacy error is correct.
    pub(crate) async fn session_handle_waiting_for_load(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        let existing = self.resident_handle(session_id);
        if existing.is_some() {
            return existing;
        }
        self.wait_for_in_flight_session_load(session_id).await;
        self.resident_handle(session_id)
    }
    /// If a `session/load` for `session_id` is in flight, wait (bounded) for it to finish. This closes the load-vs-request race after a leader restart. Clients replay `session/load` on reconnect.
    /// A `session/prompt` arriving right behind it must wait for the session to land in `self.sessions`. Otherwise it fails with "unknown session id".
    /// The wait wakes when the load's [`SessionLoadGuard`] drops (success or failure) and re-checks. A failed load still gives the caller the original error.
    pub(crate) async fn wait_for_in_flight_session_load(
        &self,
        session_id: &acp::SessionId,
    ) {
        const LOAD_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
            60,
        );
        let deadline = tokio::time::Instant::now() + LOAD_WAIT_TIMEOUT;
        loop {
            if self.is_resident(session_id) {
                return;
            }
            let rx = self.session_registry.attach_waiter(session_id);
            let Some(mut rx) = rx else { return };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    "timed out waiting for in-flight session/load"
                );
                return;
            }
            if let Ok(Err(_)) = tokio::time::timeout(deadline - now, rx.changed()).await
                && self
                    .session_registry
                    .attach_waiter(session_id)
                    .is_some_and(|w| w.same_channel(&rx))
            {
                tracing::warn!(
                    session_id = %session_id.0,
                    "attach waiter closed without settling; abandoning the wait"
                );
                return;
            }
        }
    }
    /// A warm attach adopting an actor another attach installed waits for that actor's identity
    /// stamp; adopting before it lands would publish an identity disk never saw.
    /// A stamp that fails, or outlives the wait, takes the install back so nothing is published.
    pub(super) async fn await_adopted_identity_stamp(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), acp::Error> {
        const IDENTITY_STAMP_WAIT: std::time::Duration = std::time::Duration::from_secs(
            60,
        );
        let Some((install_id, mut stamp)) = self
            .session_registry
            .pending_identity_stamp(session_id) else {
            return Ok(());
        };
        let landed = stamp.wait_for(|outcome| *outcome != IdentityStamp::Pending);
        match tokio::time::timeout(IDENTITY_STAMP_WAIT, landed).await {
            Ok(Ok(outcome)) if *outcome == IdentityStamp::Durable => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                let failed = IdentityStamp::Failed;
                if let StampResolution::Withdrawn(withdrawn) = self
                    .session_registry
                    .resolve_identity_stamp(session_id, install_id, failed)
                {
                    self.roll_back_install(session_id, *withdrawn).await;
                }
            }
        }
        Err(acp::Error::invalid_params().data("session identity stamp failed"))
    }
    /// Wait until no attach is in flight for this id, up to `budget`.
    /// An attach registers its actor and keeps going, so handle presence is not enough.
    pub(crate) async fn wait_for_load_to_settle(
        &self,
        session_id: &acp::SessionId,
        budget: std::time::Duration,
    ) {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let Some(mut rx) = self.session_registry.attach_waiter(session_id) else {
                return;
            };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    "timed out waiting for session/load to settle"
                );
                return;
            }
            if let Ok(Err(_)) = tokio::time::timeout(deadline - now, rx.changed()).await
                && self
                    .session_registry
                    .attach_waiter(session_id)
                    .is_some_and(|w| w.same_channel(&rx))
            {
                tracing::warn!(
                    session_id = %session_id.0,
                    "attach waiter closed without settling; abandoning the wait"
                );
                return;
            }
        }
    }
    pub fn default_yolo_mode(&self) -> bool {
        self.default_yolo_mode
    }
    pub fn storage_mode(&self) -> StorageMode {
        self.storage_mode.get()
    }
    pub(crate) fn background_copy_context(&self) -> BackgroundCopyContext {
        self.background_copy_context.clone()
    }
    /// Move a foreground bash command to background.
    /// Routes through the session's tool bridge to unblock the agent loop.
    pub(crate) async fn background_foreground_command(
        &self,
        session_id: &str,
        tool_call_id: &str,
    ) -> bool {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.background_foreground_command(tool_call_id).await
        } else {
            false
        }
    }
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub(crate) async fn kill_background_task(
        &self,
        session_id: &str,
        task_id: &str,
        source: xai_grok_tools::types::KillSource,
    ) -> Result<xai_grok_tools::types::KillOutcome, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.kill_background_task(task_id, source).await
        } else {
            Err("session not found".to_string())
        }
    }
    pub(crate) async fn delete_scheduled_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<bool, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.delete_scheduled_task(task_id).await
        } else {
            Err("session not found".to_string())
        }
    }
    /// Cancel a subagent by id, returning a typed outcome that backs the pager's `x.ai/subagent/cancel`.
    /// Active/pending becomes cancelled (a finish follows); already-finished returns its terminal status; an unknown id returns `NotFound`.
    pub(crate) async fn cancel_subagent(
        &self,
        subagent_id: &str,
    ) -> xai_grok_tools::implementations::grok_build::task::types::SubagentCancelOutcome {
        xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                self.subagent_event_tx.event_sender().0,
            )
            .cancel(subagent_id)
            .await
    }
    pub(crate) async fn list_running_subagents(
        &self,
        parent_session_id: &str,
    ) -> Vec<
        xai_grok_tools::implementations::grok_build::task::types::SubagentInspection,
    > {
        let backend = xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
            self.subagent_event_tx.event_sender().0,
        );
        let sid = acp::SessionId::new(parent_session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            let session_dir = crate::session::persistence::session_dir(&handle.info);
            crate::agent::subagent::reconcile_live_orphaned_subagents(
                    &backend,
                    &session_dir,
                    parent_session_id,
                    &self.gateway,
                    Some(&handle.cmd_tx),
                    self.session_registry.live_orphan_heal_lock(&sid),
                )
                .await;
        }
        backend.list_running(parent_session_id).await
    }
    pub(crate) async fn inspect_subagent(
        &self,
        subagent_id: &str,
    ) -> Option<
        xai_grok_tools::implementations::grok_build::task::types::SubagentInspection,
    > {
        xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                self.subagent_event_tx.event_sender().0,
            )
            .inspect(subagent_id)
            .await
    }
    pub(crate) async fn query_subagent(
        &self,
        subagent_id: &str,
        block: bool,
        timeout_ms: Option<u64>,
    ) -> Option<
        xai_grok_tools::implementations::grok_build::task::types::SubagentSnapshot,
    > {
        xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                self.subagent_event_tx.event_sender().0,
            )
            .query(subagent_id, block, timeout_ms)
            .await
    }
    pub(super) async fn spawned_subagent_refs_for_prompt(
        &self,
        parent_session_id: &str,
        prompt_id: &str,
    ) -> Vec<crate::upload::trace::SubagentSpawnedRef> {
        xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                self.subagent_event_tx.event_sender().0,
            )
            .spawned_refs_for_prompt(parent_session_id, prompt_id)
            .await
            .into_iter()
            .map(|child| crate::upload::trace::SubagentSpawnedRef {
                subagent_id: child.subagent_id,
                child_session_id: child.child_session_id,
                subagent_type: child.subagent_type,
                description: child.description,
                persona: child.persona,
                resumed_from: child.resumed_from,
            })
            .collect()
    }
    /// List all background tasks for a session.
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub async fn list_tasks(
        &self,
        session_id: &str,
    ) -> Option<Vec<xai_grok_tools::types::TaskSnapshot>> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.list_tasks().await
        } else {
            None
        }
    }
    /// Flush a session's persistence buffer with a 5-second timeout.
    /// Sends `FlushComplete` to the session actor, which chains through to `FlushAndAck` on the persistence actor.
    /// That is a true sync barrier: it only resolves after all queued writes (chat messages, updates) hit disk.
    pub(crate) async fn flush_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), &'static str> {
        let cmd_tx = self.resident_handle(session_id).map(|h| h.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else {
            return Err("session not found");
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if cmd_tx
            .send(SessionCommand::FlushComplete {
                respond_to: tx,
            })
            .is_err()
        {
            return Err("send failed");
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(_))) => Err("flush failed"),
            Ok(Err(_)) => Err("channel closed"),
            Err(_) => Err("timeout"),
        }
    }
    /// RelaySync is only enabled when: Running in TUI interactive mode (cfg.enable_relay_sync)
    /// Config file/env enables it ([relay] enabled or GROK_RELAY_SYNC_ENABLED)
    /// User is authenticated
    pub(super) fn create_relay_sync(
        &self,
        session_id: &str,
        session_info: &crate::session::info::Info,
    ) -> Option<crate::relay::RelaySync> {
        if !self.relay_sync_enabled {
            return None;
        }
        let auth = self.auth_manager.current_or_expired()?;
        if auth.is_zdr_team() {
            tracing::debug!("ZDR team: skipping relay sync");
            return None;
        }
        let cfg = self.cfg.borrow();
        let relay_config = crate::agent::relay::RelayConfig::for_session(
            &auth,
            &cfg.grok_com_config,
            cfg.endpoints.alpha_test_key.clone(),
            None,
        )?;
        let session_dir = crate::session::persistence::session_dir(session_info);
        Some(
            crate::relay::RelaySync::new(
                session_id.to_string(),
                relay_config,
                crate::relay::AgentType::Tui,
                Some(session_dir),
                None,
            ),
        )
    }
    /// Spawn a local task that watches `ConnectionState` changes and forwards them to the TUI as `ExtNotification`s containing `RelaySyncStatus`.
    pub(super) fn spawn_relay_state_forwarder(
        mut state_rx: tokio::sync::watch::Receiver<crate::relay::ConnectionState>,
        session_id: String,
        gateway: GatewaySender,
    ) {
        use crate::extensions::notification::RelaySyncStatus;
        let session_id = acp::SessionId::new(session_id);
        tokio::task::spawn_local(async move {
            while state_rx.changed().await.is_ok() {
                let state = *state_rx.borrow_and_update();
                let status = match state {
                    crate::relay::ConnectionState::Connected => {
                        let share_url = crate::relay::sync::build_share_url(
                            &session_id.0,
                        );
                        RelaySyncStatus::Connected {
                            share_url,
                        }
                    }
                    crate::relay::ConnectionState::Disconnected => {
                        RelaySyncStatus::Disconnected
                    }
                    crate::relay::ConnectionState::Connecting => {
                        RelaySyncStatus::Reconnecting {
                            attempt: 0,
                        }
                    }
                };
                let notification = SessionNotification {
                    session_id: session_id.clone(),
                    update: SessionUpdate::RelaySyncStatus(status),
                    meta: None,
                };
                if let Ok(params) = serde_json::value::to_raw_value(&notification) {
                    let ext_notification = acp::ExtNotification::new(
                        "x.ai/session_notification",
                        params.into(),
                    );
                    let _ = gateway.ext_notification(ext_notification).await;
                }
            }
        });
    }
    /// Returns None if the session is not found.
    pub(crate) fn get_session_cwd(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<PathBuf> {
        self.resident_handle(session_id).map(|handle| PathBuf::from(&handle.info.cwd))
    }
    pub(crate) fn get_session_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        self.resident_handle(session_id)
    }
    /// Get hooks list for a session (for `x.ai/hooks/list` extension).
    pub(crate) async fn list_hooks(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<xai_hooks_plugins_types::HooksListResponse> {
        let handle = self.get_session_handle(session_id)?;
        handle.get_hooks_list().await
    }
    /// Execute a hooks management action (for `x.ai/hooks/action`).
    pub(crate) async fn execute_hooks_action(
        &self,
        session_id: &acp::SessionId,
        action: xai_hooks_plugins_types::HooksAction,
    ) -> Option<xai_hooks_plugins_types::ActionOutcome> {
        if matches!(action, xai_hooks_plugins_types::HooksAction::Untrust)
            && let Some(cwd) = self.get_session_cwd(session_id)
        {
            self.interactive_trust_prompted
                .borrow_mut()
                .remove(&xai_grok_workspace::trust::workspace_key(&cwd));
        }
        let handle = self.get_session_handle(session_id)?;
        handle.execute_hooks_action(action).await
    }
    /// Execute a plugins management action (for `x.ai/plugins/action`).
    pub(crate) async fn execute_plugins_action(
        &self,
        session_id: &acp::SessionId,
        action: xai_hooks_plugins_types::PluginsAction,
    ) -> Option<xai_hooks_plugins_types::ActionOutcome> {
        let is_reload = matches!(action, xai_hooks_plugins_types::PluginsAction::Reload);
        let handle = self.get_session_handle(session_id)?;
        let outcome = handle.execute_plugins_action(action).await;
        let succeeded = matches!(
            outcome.as_ref().map(|o| &o.status),
            Some(xai_hooks_plugins_types::OutcomeStatus::Success)
        );
        if is_reload && succeeded {
            self.broadcast_plugin_registry_to_sessions(Some(session_id));
        }
        outcome
    }
    /// Get a snapshot of the shared plugin registry (for `x.ai/plugins/list`).
    pub(crate) fn plugin_registry_snapshot(
        &self,
    ) -> Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>> {
        self.plugin_registry_handle.snapshot()
    }
    /// Returns an upload method, or `None` when trace uploads are disabled.
    pub(crate) async fn trace_upload_config(
        &self,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        let (method, _reason) = self.trace_upload_config_with_reason().await;
        method
    }
    pub(super) fn trace_upload_config_snapshot(
        &self,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        if self.is_data_collection_disabled()
            || !self.cfg.borrow().is_trace_upload_enabled()
        {
            return None;
        }
        let cfg = self.cfg.borrow();
        let auth_token = if cfg.endpoints.deployment_key.is_none() {
            self.auth_manager
                .current_or_expired()
                .filter(|auth| auth.is_xai_auth())
                .map(|auth| auth.key)
        } else {
            None
        };
        cfg.endpoints.resolve_upload_method(auth_token)
    }
    pub(super) fn diagnostic_upload_config(
        &self,
    ) -> Option<xai_grok_login::DiagnosticUploader> {
        self.sync_collection_config_gate();
        let cfg = self.cfg.borrow();
        if !cfg.is_trace_upload_enabled() {
            return None;
        }
        let proxy_base_url = cfg.endpoints.resolve_trace_upload_url();
        let deployment_key = cfg.endpoints.deployment_key.clone();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let auth_manager = self.auth_manager.clone();
        let trace_upload_live = self.trace_upload_live.clone();
        Some(
            std::sync::Arc::new(move |
                log_bytes: Vec<u8>,
                auth_token: String,
                user_id: String|
            {
                let proxy_base_url = proxy_base_url.clone();
                let deployment_key = deployment_key.clone();
                let alpha_test_key = alpha_test_key.clone();
                let auth_manager = auth_manager.clone();
                let trace_upload_live = trace_upload_live.clone();
                Box::pin(async move {
                    if !auth_manager.allows_data_collection()
                        || !trace_upload_live.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        tracing::debug!(
                            "skipping auth-diagnostics upload: data collection disabled"
                        );
                        return;
                    }
                    let upload_method = crate::session::repo_changes::UploadMethod::Proxy {
                        proxy_base_url,
                        user_token: auth_token,
                        deployment_key,
                        alpha_test_key,
                    };
                    crate::upload::gcs::upload_to_auth_diagnostics(
                            &log_bytes,
                            &user_id,
                            &upload_method,
                            auth_manager,
                        )
                        .await;
                })
            }),
        )
    }
    /// Like `trace_upload_config`, but also returns the reason why uploads are enabled or disabled for structured session events.
    async fn trace_upload_config_with_reason(
        &self,
    ) -> (
        Option<crate::session::repo_changes::UploadMethod>,
        crate::upload::turn::TraceUploadReason,
    ) {
        use crate::upload::turn::TraceUploadReason;
        if self.is_data_collection_disabled() {
            crate::upload::trace::spawn_startup_spill_reconcile(
                crate::util::grok_home::grok_home(),
                None,
            );
            return (None, TraceUploadReason::ZdrTeam);
        }
        if self.cfg.borrow().remote_settings.is_none()
            && let Ok(auth) = self.auth_manager.auth().await
        {
            self.refresh_remote_settings(&auth).await;
        }
        let (direct_method, has_deployment_key, endpoints) = {
            let cfg = self.cfg.borrow();
            if !cfg.is_trace_upload_enabled() {
                return (None, TraceUploadReason::FeatureOff);
            }
            (
                cfg.endpoints.resolve_direct_upload_method(),
                cfg.endpoints.deployment_key.is_some(),
                cfg.endpoints.clone(),
            )
        };
        let service_account_key = crate::util::config::load_gcs_service_account_key_sync();
        let method = if let Some(method) = direct_method {
            Some(method)
        } else {
            let auth_token = if has_deployment_key {
                None
            } else {
                self.auth_manager
                        .auth()
                        .await
                        .ok()
                        .filter(|auth| auth.is_xai_auth())
                        .map(|auth| auth.key)
            };
            if auth_token.is_some() || has_deployment_key {
                endpoints.resolve_upload_method(auth_token)
            } else if service_account_key.is_some() {
                Some(crate::session::repo_changes::UploadMethod::Direct {
                    service_account_key,
                })
            } else {
                None
            }
        };
        let reason = crate::upload::turn::TraceUploadReason::from_upload_method(&method);
        (method, reason)
    }
    /// Resolve client version: prefer the value from the initialize request _meta.
    /// Fall back to the agent's own version (VERSION_WITH_COMMIT set by the TUI launcher).
    pub(crate) fn client_version(&self) -> Option<String> {
        self.initialize_request
            .get()
            .and_then(|req| req.meta.as_ref())
            .and_then(|m| m.get("clientVersion"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.cfg.borrow().client_version.clone())
    }
    pub(super) fn origin_client_info_from_meta(
        &self,
        meta: Option<&acp::Meta>,
    ) -> Option<crate::http::OriginClientInfo> {
        crate::http::merge_origin_client_info(
                crate::http::origin_client_info_from_meta(meta),
                crate::http::origin_client_info_from_meta(
                        self.initialize_request.get().and_then(|req| req.meta.as_ref()),
                    )
                    .map(|mut origin| {
                        if origin.version.is_none() {
                            origin.version = self.client_version();
                        }
                        origin
                    }),
            )
            .map(|mut origin| {
                if origin.version.is_none() {
                    origin.version = self.client_version();
                }
                origin
            })
    }
    /// When `session_id` is `Some`, looks up the session's per-session model.
    /// Falls back to `current_model_id` (startup default) when no session is found or `session_id` is `None`.
    /// The `None` case covers `initialize`, before any session exists.
    pub fn model_state(
        &self,
        session_id: Option<&acp::SessionId>,
    ) -> acp::SessionModelState {
        let model_id = lookup_session_model(
            session_id
                .and_then(|sid| self.resident_handle(sid).map(|h| h.model_id.clone())),
            &self.models_manager.current_model_id(),
        );
        let mut available_models: Vec<acp::ModelInfo> = self
            .models_manager
            .available()
            .values()
            .cloned()
            .collect();
        let override_effort = session_id
            .and_then(|sid| self.resident_handle(sid).map(|h| h.reasoning_effort))
            .flatten()
            .or_else(|| self.models_manager.current_reasoning_effort());
        if let Some(override_effort) = override_effort
            && let Some(info) = available_models
                .iter_mut()
                .find(|info| info.model_id == model_id)
            && supports_reasoning_effort_meta(info.meta.as_ref())
        {
            let mut map = info.meta.clone().unwrap_or_default();
            map.insert(
                REASONING_EFFORT_META_KEY.to_string(),
                reasoning_effort_meta_value(override_effort),
            );
            info.meta = Some(map);
        }
        acp::SessionModelState::new(model_id, available_models)
    }
    pub(crate) async fn session_model_state(
        &self,
        session_id: &acp::SessionId,
    ) -> acp::SessionModelState {
        self.model_state(Some(session_id))
    }
    pub(crate) async fn acp_config_options_for_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Vec<acp::SessionConfigOption> {
        let state = self.session_model_state(session_id).await;
        self.acp_config_options(Some(session_id), &state)
    }
    pub(super) fn session_config_options(
        &self,
        session_id: Option<&acp::SessionId>,
        state: &acp::SessionModelState,
    ) -> Vec<session_config::SessionConfigOption> {
        let inputs = self.session_config_inputs(session_id, state);
        session_config::build_session_config_options(
            &state.available_models,
            &inputs.model_id,
            &inputs.effort_options,
            inputs.current_effort,
        )
    }
    pub(crate) fn acp_config_options(
        &self,
        session_id: Option<&acp::SessionId>,
        state: &acp::SessionModelState,
    ) -> Vec<acp::SessionConfigOption> {
        let inputs = self.session_config_inputs(session_id, state);
        session_config::build_acp_config_options(
            &state.available_models,
            &inputs.model_id,
            &inputs.effort_options,
            inputs.current_effort,
        )
    }
    /// Resolve an effort selector against `model_id`'s menu (not a fresh session lookup, which
    /// falls back to the global model when the session isn't resident). `session_id` still gates
    /// the gateway case, which offers no local menu.
    pub(crate) fn resolve_reasoning_effort_value(
        &self,
        session_id: &acp::SessionId,
        model_id: &acp::ModelId,
        value_id: &str,
    ) -> Option<xai_grok_sampling_types::ReasoningEffort> {
        let state = acp::SessionModelState::new(model_id.clone(), Vec::new());
        self.session_config_inputs(Some(session_id), &state)
            .effort_options
            .iter()
            .find(|option| option.id.as_str() == value_id)
            .map(|option| option.value)
    }
    fn session_config_inputs(
        &self,
        session_id: Option<&acp::SessionId>,
        state: &acp::SessionModelState,
    ) -> SessionConfigInputs {
        let gateway_session = false;
        if gateway_session {
            return SessionConfigInputs {
                model_id: state.current_model_id.clone(),
                effort_options: Vec::new(),
                current_effort: None,
            };
        }
        let model_id = resolve_catalog_key(
                &self.models_manager.models(),
                &state.current_model_id,
            )
            .unwrap_or_else(|| state.current_model_id.clone());
        let supports_effort = self
            .models_manager
            .model_supports_reasoning_effort(model_id.0.as_ref());
        let effort_options: Vec<ReasoningEffortOption> = if supports_effort {
            let options = self
                .models_manager
                .model_reasoning_efforts(model_id.0.as_ref());
            if options.is_empty() {
                session_config::legacy_session_effort_options()
            } else {
                options
            }
        } else {
            Vec::new()
        };
        let current_effort = if supports_effort {
            session_id
                .and_then(|sid| self.resident_handle(sid).map(|h| h.reasoning_effort))
                .flatten()
                .or_else(|| self.models_manager.current_reasoning_effort())
                .or_else(|| {
                    self
                        .models_manager
                        .model_default_reasoning_effort(model_id.0.as_ref())
                })
        } else {
            None
        };
        SessionConfigInputs {
            model_id,
            effort_options,
            current_effort,
        }
    }
    /// Insert the per-session `_meta` keys shared by `new_session` and `load_session`.
    /// The keys are `x.ai/sessionConfig`, `x.ai/sessionDetail`,
    /// and `x.ai/memoryMode`.
    /// Keeping both response paths on this one builder stops them drifting.
    pub(super) fn insert_session_config_meta(
        &self,
        meta: &mut serde_json::Map<String, serde_json::Value>,
        session_id: &acp::SessionId,
        cwd: String,
        title: Option<String>,
        model_state: &acp::SessionModelState,
    ) {
        let config_options = self.session_config_options(Some(session_id), model_state);
        let detail = session_config::GrokSessionDetail::build(
            session_id.0.to_string(),
            cwd,
            model_state.current_model_id.0.to_string(),
            title,
        );
        meta.insert(
            "x.ai/sessionConfig".to_string(),
            serde_json::json!({ "options": config_options }),
        );
        meta.insert("x.ai/sessionDetail".to_string(), serde_json::json!(detail));
        if let Some(memory_mode) = self
            .resident_handle(session_id)
            .and_then(|handle| handle.spawn_snapshot.memory_mode)
        {
            meta.insert(
                MEMORY_MODE_META_KEY.to_string(),
                serde_json::json!(memory_mode),
            );
        }
    }
    /// Seed the global sampling config with login auth when available. Only sets the `api_key` if missing. Does NOT resolve `base_url` from `current_model_id`. That is deferred to session creation time.
    /// It avoids cross-client contamination in leader mode, where `current_model_id` is shared mutable state.
    pub(super) fn seed_client_config_auth_if_available(&self) {
        let mut sampling_config = self.sampling_config.borrow_mut();
        if sampling_config.api_key.is_none() {
            if let Some(auth) = self.auth_manager.current_or_expired() {
                sampling_config.api_key = Some(auth.key);
                tracing::debug!("auth: seed_client_config set auth (SessionToken)");
                xai_grok_telemetry::unified_log::debug(
                    "auth: seed_client_config set auth (SessionToken)",
                    None,
                    None,
                );
            } else if !self
                .models_manager
                .models()
                .values()
                .any(|m| m.has_own_credentials())
            {
                tracing::warn!("No credentials found: no login token and no model api_key/env_key");
                xai_grok_telemetry::unified_log::warn(
                    "No credentials found: no login token and no model api_key/env_key",
                    None,
                    None,
                );
            }
        }
    }
    /// Build a `TraceExportConfig` for uploading JSON artifacts under a given prefix.
    ///
    /// Shared by comment uploads (`{session_id}/comments/...`), comparison metadata (`{session_id}/turn_{N}/...`), etc.
    pub(crate) async fn build_gcs_config(
        &self,
        gcs_prefix: String,
    ) -> Option<crate::session::repo_changes::TraceExportConfig> {
        let upload_method = self.trace_upload_config().await?;
        let bucket_url = {
            let cfg = self.cfg.borrow();
            match &upload_method {
                crate::session::repo_changes::UploadMethod::Direct { .. } => {
                    match cfg.endpoints.resolve_trace_bucket_url() {
                        Some(resolved) => Some(resolved.value),
                        None => {
                            tracing::debug!(
                                "no trace bucket configured; skipping direct GCS upload"
                            );
                            return None;
                        }
                    }
                }
                crate::session::repo_changes::UploadMethod::S3 { bucket, .. } => {
                    Some(format!("s3://{bucket}"))
                }
                crate::session::repo_changes::UploadMethod::Proxy { .. } => None,
            }
        };
        Some(crate::session::repo_changes::TraceExportConfig {
            bucket_url,
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(gcs_prefix),
            absolute_paths: false,
            archive_name_override: None,
            upload_method,
        })
    }
    pub(crate) fn team_blocks_one_shot_trace_upload(&self) -> bool {
        self.auth_manager
            .current_or_expired()
            .is_some_and(|auth| auth.team_name.is_some())
    }
    pub(crate) fn issue_feedback_trace_upload_grant(
        &self,
        session_id: acp::SessionId,
    ) -> String {
        const MAX_PENDING_GRANTS: usize = 8;
        let token = uuid::Uuid::new_v4().to_string();
        let mut grants = self.feedback_trace_upload_grants.borrow_mut();
        if grants.len() == MAX_PENDING_GRANTS {
            let _ = grants.pop_front();
        }
        grants.push_back((token.clone(), session_id));
        token
    }
    pub(crate) fn consume_feedback_trace_upload_grant(
        &self,
        token: &str,
        session_id: &acp::SessionId,
    ) -> bool {
        let mut grants = self.feedback_trace_upload_grants.borrow_mut();
        let Some(position) = grants
            .iter()
            .position(|(grant, granted_session)| {
                grant == token && granted_session == session_id
            }) else {
            return false;
        };
        let _ = grants.remove(position);
        true
    }
    /// Whether `/feedback` may offer to turn trace upload on.
    /// An individual coding-data opt-out still asks: the card is how opted-out users switch sharing back on.
    /// ZDR has no self-serve way back, so it never asks.
    pub(crate) fn feedback_trace_offer(&self) -> bool {
        if self.auth_manager.current_or_expired().is_some_and(|a| a.is_zdr_team()) {
            return false;
        }
        if self.team_blocks_one_shot_trace_upload() {
            return false;
        }
        let cfg = self.cfg.borrow();
        if !cfg.is_feature_enabled(crate::agent::config::Feature::FeedbackTraceCard) {
            return false;
        }
        if !Self::trace_upload_posture_allows_offer(&cfg) {
            return false;
        }
        if cfg.is_trace_upload_enabled() {
            return false;
        }
        if Self::has_custom_trace_destination(&cfg) {
            return false;
        }
        cfg.endpoints.deployment_key.is_none()
            && self.auth_manager.current_or_expired().is_some_and(|a| a.is_xai_auth())
    }
    /// Trace upload being off as *policy* (an MDM/requirements pin or a telemetry-disabled posture) must suppress the card. It must not invite the user to override the policy.
    /// The accepted consent persists at the config tier, which those postures cannot outrank. Trace upload being off via the remote `trace_upload_enabled` default is different: that is the card's audience.
    /// Individual consent overriding a fleet default is the feature (its own kill switch is `feedback_trace_card_enabled`).
    fn trace_upload_posture_allows_offer(cfg: &crate::agent::config::Config) -> bool {
        cfg.requirements.trace_upload.pinned() != Some(false)
            && cfg.is_telemetry_enabled()
    }
    fn has_custom_trace_destination(cfg: &crate::agent::config::Config) -> bool {
        cfg.endpoints.trace_upload_url.is_some()
            || cfg.endpoints.trace_upload_bucket.is_some()
            || cfg.endpoints.trace_upload_endpoint_url.is_some()
    }
    /// Upload method for a user-consented feedback trace archive. Blocks ZDR and custom destinations. Deliberately ignores the live `trace_upload` flag and the cached coding-data opt-out.
    /// The consent just granted may not have reached either cache yet. Fails closed on unknown privacy state.
    /// With no credential (and no deployment key) the ZDR / team predicates can't be evaluated, so nothing may leave the machine.
    pub(crate) async fn one_shot_feedback_gcs_config(
        &self,
        gcs_prefix: String,
    ) -> Option<crate::session::repo_changes::TraceExportConfig> {
        let cached_auth = self.auth_manager.current_or_expired()?;
        if cached_auth.is_zdr_team() {
            return None;
        }
        if self.team_blocks_one_shot_trace_upload() {
            return None;
        }
        {
            let cfg = self.cfg.borrow();
            if cfg.endpoints.deployment_key.is_some() {
                return None;
            }
            if !Self::trace_upload_posture_allows_offer(&cfg) {
                return None;
            }
            if Self::has_custom_trace_destination(&cfg) {
                return None;
            }
        }
        let auth_token = self
            .auth_manager
            .auth()
            .await
            .ok()
            .filter(|auth| auth.is_xai_auth())
            .map(|auth| auth.key);
        let cfg = self.cfg.borrow();
        let upload_method = cfg.endpoints.resolve_upload_method(auth_token)?;
        if !matches!(
            upload_method,
            crate::session::repo_changes::UploadMethod::Proxy { .. }
        ) {
            return None;
        }
        Some(crate::session::repo_changes::TraceExportConfig {
            bucket_url: None,
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(gcs_prefix),
            absolute_paths: false,
            archive_name_override: None,
            upload_method,
        })
    }
    /// Allocate the next monotonic telemetry turn number for a session. The counter is intentionally monotonic even across rewinds to avoid overwriting older telemetry docs in cloud storage.
    /// For sessions sharing a parent's trace counter, call this once with the **root session ID** and reuse the result. That way the root's counter does not advance more than once per logical turn.
    /// The cloud storage layout writes to `{session_id}/turn_{N}/`.
    pub(crate) fn allocate_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        let turn = self.peek_turn_number(session_id);
        self.set_turn_number(session_id, turn.saturating_add(1));
        turn
    }
    /// Read a session's next trace turn number without advancing the counter.
    fn peek_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        self.session_turn_number(session_id).unwrap_or(0u64)
    }
    /// Set a session's next trace turn number.
    pub(super) fn set_turn_number(&self, session_id: &acp::SessionId, next: u64) {
        self.session_registry.set_turn_number(session_id, next);
    }
    pub(crate) fn release_subagent_turn_number(&self, session_id: &acp::SessionId) {
        self.session_registry.clear_turn_number(session_id);
    }
    /// Upload each drained harness trace turn as its own `turn_{N}` artifact.
    /// Numbered from the same counter as model turns so subagents interleave correctly in remote clients.
    /// Best-effort and non-blocking.
    pub(super) async fn upload_harness_trace_turns(
        &self,
        session_id: &acp::SessionId,
        info: &crate::session::info::Info,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
        model: &str,
        turns: Vec<Vec<xai_grok_sampling_types::conversation::ConversationItem>>,
    ) {
        use crate::upload::manifest::{
            build_manifest, resolve_upload_method, write_upload_manifest,
        };
        let base = self.peek_turn_number(session_id);
        let uploads = self
            .build_harness_trace_uploads(session_id, info, model, base, turns)
            .await;
        if uploads.is_empty() {
            return;
        }
        let next_trace_turn = base.saturating_add(uploads.len() as u64);
        self.set_turn_number(session_id, next_trace_turn);
        let _ = cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: None,
            });
        for (ctx, metadata, capture) in uploads {
            spawn_upload_task(
                "harness_trace_turn",
                async move {
                    let (session_state, capture) = build_chat_history_then_move_capture(
                            capture,
                        )
                        .await;
                    futures::join!(
                    upload_metadata(&ctx, metadata, UploadWait::Confirm),
                    upload_turn_messages(&ctx, capture, UploadWait::Confirm),
                    upload_harness_session_archive(&ctx, session_state),
                );
                    let upload_method = resolve_upload_method(&ctx.gcs_config);
                    write_upload_manifest(
                            &ctx,
                            &build_manifest(&ctx.artifact_tracker, upload_method, None),
                        )
                        .await;
                },
            );
        }
    }
    /// Number the drained harness turns `base, base+1, …` and build their `(trace context, metadata, capture)` upload payloads.
    /// Stops at the first turn whose trace context is `None`: uploads are disabled (or the session is gone). That state is uniform across the batch since all turns share one `session_id`.
    /// A `None` *after* a `Some` would be a broken invariant, so it is logged rather than dropped silently.
    pub(super) async fn build_harness_trace_uploads(
        &self,
        session_id: &acp::SessionId,
        info: &crate::session::info::Info,
        model: &str,
        base: u64,
        turns: Vec<Vec<xai_grok_sampling_types::conversation::ConversationItem>>,
    ) -> Vec<(PromptTraceContext, PromptMetadata, xai_chat_state::TurnCapture)> {
        let mut uploads = Vec::with_capacity(turns.len());
        for (offset, items) in turns.into_iter().enumerate() {
            let turn_number = base.saturating_add(offset as u64);
            let Some(ctx) = self.get_trace_context(info, turn_number).await else {
                if offset > 0 {
                    tracing::warn!(
                        turn_number,
                        "harness trace: trace context unexpectedly None mid-batch; \
                         dropping the remaining drained turns"
                    );
                }
                break;
            };
            let metadata = PromptMetadata::new(PromptMetadataParams {
                schema_version: GCS_SCHEMA_VERSION.to_string(),
                session_id: session_id.0.to_string(),
                turn_number,
                request_id: format!("harness-trace-{turn_number}"),
                turn_started_at: chrono::Utc::now().to_rfc3339(),
                model: model.to_string(),
                reasoning_effort: ctx
                    .session_handle
                    .reasoning_effort
                    .map(|e| e.as_ref().to_string()),
                host_os: std::env::consts::OS.to_string(),
                host_arch: std::env::consts::ARCH.to_string(),
                prompt_has_image: Some(false),
                prompt_was_truncated: Some(false),
                prompt_verbatim: Some(true),
                cwd: Some(info.cwd.clone()),
                shell_version: Some(xai_grok_version::VERSION.to_string()),
                sandbox: local_sandbox_telemetry(),
                ..Default::default()
            });
            let capture = xai_chat_state::TurnCapture {
                messages: items,
                compaction_occurred: false,
            };
            uploads.push((ctx, metadata, capture));
        }
        uploads
    }
    pub(crate) async fn get_trace_context(
        &self,
        session_info: &crate::session::info::Info,
        turn_number: u64,
    ) -> Option<PromptTraceContext> {
        let (upload_method, upload_reason) = self
            .trace_upload_config_with_reason()
            .await;
        {
            let mut decision = self.cfg.borrow().trace_upload_decision_debug();
            if let Some(obj) = decision.as_object_mut() {
                obj.insert(
                    "uploads_enabled".into(),
                    serde_json::json!(upload_method.is_some()),
                );
                obj.insert(
                    "upload_reason".into(),
                    serde_json::json!(upload_reason.as_ref()),
                );
                obj.insert(
                    "data_collection_disabled".into(),
                    serde_json::json!(self.is_data_collection_disabled()),
                );
                obj.insert("turn_number".into(), serde_json::json!(turn_number));
            }
            xai_grok_telemetry::unified_log::info(
                "trace.upload.decision",
                Some(session_info.id.0.as_ref()),
                Some(decision),
            );
        }
        let upload_method = match upload_method {
            Some(method) => method,
            None => {
                xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::TraceUploadSkipped {
                    session_id: session_info.id.0.to_string(),
                    turn_number,
                    reason: upload_reason.as_ref().to_owned(),
                });
                return None;
            }
        };
        let bucket_url = {
            let cfg = self.cfg.borrow();
            match &upload_method {
                crate::session::repo_changes::UploadMethod::Direct { .. } => {
                    match cfg.endpoints.resolve_trace_bucket_url() {
                        Some(resolved) => Some(resolved.value),
                        None => {
                            xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::TraceUploadSkipped {
                                session_id: session_info.id.0.to_string(),
                                turn_number,
                                reason: "no_trace_bucket_configured".to_owned(),
                            });
                            return None;
                        }
                    }
                }
                crate::session::repo_changes::UploadMethod::S3 { bucket, .. } => {
                    Some(format!("s3://{bucket}"))
                }
                crate::session::repo_changes::UploadMethod::Proxy { .. } => None,
            }
        };
        let gcs_config = crate::session::repo_changes::TraceExportConfig {
            bucket_url,
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(format!("{}/turn_{}", session_info.id.0, turn_number)),
            absolute_paths: false,
            archive_name_override: None,
            upload_method,
        };
        let session_handle = self.resident_handle(&session_info.id)?;
        let queue = session_handle
            .upload_queue
            .get_or_init(|| {
                let grok_home = crate::util::grok_home::grok_home();
                let queue = crate::upload::trace::spawn_upload_queue(
                    &grok_home,
                    &gcs_config,
                    Some(xai_grok_version::VERSION),
                    self.auth_manager.clone(),
                );
                crate::upload::trace::spawn_startup_spill_reconcile(
                    grok_home,
                    Some(queue.clone()),
                );
                session_handle
                    .feedback_manager
                    .set_upload_queue_stats(queue.stats_arc());
                queue
            });
        let upload_queue = Some(queue.clone());
        let session_registry_enabled = self.build_registry_config().is_some();
        Some(PromptTraceContext {
            gcs_config,
            session_info: session_info.clone(),
            turn_number,
            attempt_id: None,
            memory_mode: session_handle
                .spawn_snapshot
                .memory_mode
                .or_else(|| self.memory_config_snapshot().map(|memory| memory.mode)),
            session_handle,
            session_registry_enabled,
            upload_queue,
            artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
            auth_manager: self.auth_manager.clone(),
        })
    }
    /// Resolve the agent definition for a session. Priority (highest to lowest): Model `agent_type` if it names a strict harness (codex, …). `acp_agent_profile` from ACP `_meta.agentProfile` (remote clients).
    /// `agent_profile_path` from CLI `--agent-profile`. `agent_config` from config.toml `[agent]`. `GROK_AGENT` env var. Built-in default agent. `GROK_AGENT` and an explicit `[agent] name` bypass step 1.
    /// Strict-harness classification is structural; see [`xai_grok_agent::config::is_strict_harness_agent_type`]. Harness inheritance for a profile that pins its own model is applied by the caller via [`inherited_harness_template`], not here.
    pub fn resolve_agent_definition(
        cwd: &std::path::Path,
        agent_profile_path: Option<&std::path::Path>,
        agent_config: &config::AgentSelectionConfig,
        acp_agent_profile: Option<xai_grok_agent::AgentDefinition>,
        model_agent_type: Option<&str>,
    ) -> xai_grok_agent::AgentDefinition {
        use xai_grok_agent::AgentDefinition;
        let grok_agent_env_set = std::env::var("GROK_AGENT")
            .ok()
            .is_some_and(|s| !s.trim().is_empty());
        let config_agent_explicitly_set = agent_config.name.is_some();
        let model_requires_strict_harness = model_agent_type
            .is_some_and(xai_grok_agent::config::is_strict_harness_agent_type);
        if !grok_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(required, cwd)
        {
            tracing::info!(
                agent_name = %def.name,
                "Using agent definition from model agent_type"
            );
            return def;
        }
        if let Some(def) = acp_agent_profile {
            tracing::info!(
                agent_name = %def.name,
                "Using agent profile from ACP _meta.agentProfile"
            );
            return def;
        }
        if let Some(path) = agent_profile_path {
            match AgentDefinition::from_file(path) {
                Ok(def) => return def,
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "Failed to load agent profile from --agent-profile path"
                    );
                    eprintln!(
                        "error: failed to load agent profile '{}': {}",
                        path.display(),
                        e
                    );
                    crate::instrumentation::finalize_and_exit(1);
                }
            }
        }
        if let Some(ref path) = agent_config.definition {
            match AgentDefinition::from_file(path) {
                Ok(def) => {
                    tracing::info!(
                        agent_name = %def.name,
                        path = %path.display(),
                        "Using agent definition from config.toml [agent] definition"
                    );
                    return def;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to load agent definition from config.toml [agent] definition, \
                         falling through to next source"
                    );
                }
            }
        }
        if let Some(ref name) = agent_config.name {
            tracing::info!(
                agent_name = %name,
                "Resolving agent definition from config.toml [agent] name"
            );
            if let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(name, cwd) {
                return def;
            }
            tracing::warn!(
                agent_name = %name,
                "Agent '{}' not found via discovery, falling through to next source",
                name
            );
        }
        let agent_name = std::env::var("GROK_AGENT").ok();
        let resolved = match agent_name.as_deref() {
            Some("browser-use") | Some("browser_use") => AgentDefinition::browser_use(),
            Some("remote-code-concise") | Some("grok_build_concise") => {
                AgentDefinition::grok_build_concise()
            }
            Some(path) if std::path::Path::new(path).is_absolute() => {
                match AgentDefinition::from_file(path) {
                    Ok(def) => def,
                    Err(e) => {
                        tracing::warn!(
                            path = path,
                            error = %e,
                            "Failed to load agent definition from file, falling back to default"
                        );
                        AgentDefinition::grok_build_plan()
                    }
                }
            }
            Some(name) => {
                xai_grok_agent::discovery::by_name_in_cwd(name, cwd)
                    .unwrap_or_else(AgentDefinition::grok_build_plan)
            }
            None => AgentDefinition::grok_build_plan(),
        };
        if !grok_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && resolved.name != required
        {
            tracing::info!(
                resolved_agent = %resolved.name,
                model_agent_type = %required,
                "resolve_agent_definition: model requires different agent, re-resolving"
            );
            if let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(required, cwd) {
                return def;
            }
            tracing::warn!(
                model_agent_type = %required,
                fallback_agent = %resolved.name,
                "resolve_agent_definition: model agent_type '{}' not found via discovery, \
                 keeping chain-resolved agent",
                required,
            );
        }
        resolved
    }
    /// Whether the requesting client will draw a status row.
    /// Session `_meta` first, for the same reason as [`Self::resolve_client_io_caps`].
    /// `init` holds whichever client started the process, and a leader multiplexes many.
    pub(super) fn resolve_status_line_capability(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) -> bool {
        Self::resolve_bool_capability(
            meta,
            init,
            xai_grok_status_line::CLIENT_STATUS_LINE_META,
            xai_grok_status_line::STATUS_LINE_CAPABILITY,
            false,
        )
    }
    fn resolve_bool_capability(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
        session_key: &str,
        init_key: &str,
        default: bool,
    ) -> bool {
        meta.and_then(|m| m.get(session_key))
            .or_else(|| {
                init.client_capabilities.meta.as_ref().and_then(|m| m.get(init_key))
            })
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }
    /// Switch the row on for the resident actor an attach reuses and ask it to fill it.
    /// The store precedes the request because the emitter re-reads the capability when the wake lands.
    pub(super) fn attach_status_line(
        &self,
        session_id: &acp::SessionId,
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) {
        let Some(handle) = self.resident_handle(session_id) else {
            return;
        };
        let wanted = Self::resolve_status_line_capability(meta, init);
        handle.set_status_line_wanted(wanted);
        if wanted {
            handle.request_status_snapshot();
        }
    }
    /// Whether the requesting client wants live `user_message_chunk` during a prompt.
    /// Session `_meta` first: a leader multiplexes many clients behind one `initialize`.
    pub(super) fn resolve_user_message_echo_capability(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) -> bool {
        Self::resolve_bool_capability(
            meta,
            init,
            crate::session::CLIENT_USER_MESSAGE_ECHO_META,
            crate::session::USER_MESSAGE_ECHO_CAPABILITY,
            false,
        )
    }
    /// Assign live prompt echo for the reused resident actor. Can turn it off.
    pub(super) fn attach_user_message_echo(
        &self,
        session_id: &acp::SessionId,
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) {
        let Some(handle) = self.resident_handle(session_id) else {
            return;
        };
        handle
            .set_user_message_echo_wanted(
                Self::resolve_user_message_echo_capability(meta, init),
            );
    }
    /// Extract per-client terminal/fs capabilities from request `_meta` (injected by the leader).
    /// Falls back to the shared `init` OnceCell.
    pub(super) fn resolve_client_io_caps(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) -> (bool, bool, bool) {
        let terminal = meta
            .and_then(|m| m.get("clientTerminal"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.terminal);
        let fs_read = meta
            .and_then(|m| m.get("clientFsRead"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.read_text_file);
        let fs_write = meta
            .and_then(|m| m.get("clientFsWrite"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.write_text_file);
        (terminal, fs_read, fs_write)
    }
    /// Spawn and register a session actor given a session id and session parameters.
    /// Parameters are bundled in [`SessionSpawnOptions`] (named fields) rather than passed positionally.
    /// There are too many same-typed args (`bool`s, `Option<…>`s) for positional calls to be transposition-safe.
    pub(super) async fn spawn_and_register_session(
        &self,
        init: &acp::InitializeRequest,
        spec: SessionSpawnOptions<'_>,
        spawn_trace: Option<xai_grok_telemetry::startup::SpawnTraceContext>,
    ) -> Result<bool, acp::Error> {
        let SessionSpawnOptions {
            session_info,
            cwd,
            mcp_servers,
            initial_client_mcp_servers,
            mcp_meta_config_map,
            persistence,
            root_identity,
            attach_waiter,
            mut chat_history,
            rewind_points_file_path,
            initial_total_tokens,
            origin_client: _origin_client,
            client_code_nav_enabled,
            client_terminal,
            client_fs_read,
            client_fs_write,
            envrc,
            persisted_signals,
            persisted_plan_mode,
            persisted_goal_mode,
            persisted_workflow_runs,
            persisted_announcement_state,
            session_meta,
            model_agent_type,
            session_model_id,
            initial_reasoning_effort,
            session_yolo_mode,
            session_auto_mode,
            prompt_display_cwd,
            is_headless,
            is_chat_kind,
            prefetch,
        } = spec;
        let _timer = crate::instrumentation_timer!("session.spawn_and_register");
        reject_direct_hub_cloud_meta(session_meta)?;
        let spawn_remote_settings = self.cfg.borrow().remote_settings.clone();
        let mut prefetch = match prefetch {
            Some(prefetch) => prefetch,
            None => {
                crate::session::session_create_prefetch::SessionCreatePrefetch::launch_from_meta(
                    cwd.as_path(),
                    folder_trust::TrustScan::skipped(),
                    self.plugin_registry_handle.clone(),
                    session_meta,
                )
            }
        };
        let project_env_trusted = prefetch
            .resolve_trust(cwd.as_path(), spawn_remote_settings.as_ref());
        let load_envrc = self.cfg.borrow().session.load_envrc.unwrap_or(true);
        let envrc = envrc
            .unwrap_or_else(|| xai_grok_workspace::envrc::spawn_envrc_load(
                cwd.as_path().to_path_buf(),
                load_envrc && project_env_trusted,
            ));
        let use_acp_fs = client_fs_read && client_fs_write;
        let fs_notify_config = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/fs_notify"))
            .and_then(|v| {
                use crate::session::{ClientFsConfig, ClientFsMode};
                use xai_fsnotify::FsConfig;
                if v.as_bool() == Some(true) {
                    return Some(ClientFsConfig::default());
                }
                let obj = v.as_object()?;
                if obj.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
                    return None;
                }
                let mode = if obj.get("index").and_then(|i| i.as_bool()) == Some(true) {
                    ClientFsMode::Index
                } else {
                    ClientFsMode::Events
                };
                let mut fs = FsConfig::default();
                if let Some(ms) = obj.get("debounce_ms").and_then(|v| v.as_u64()) {
                    fs.debounce_ms = ms;
                }
                if let Some(patterns) = obj.get("ignore").and_then(|v| v.as_array()) {
                    fs.ignore_patterns = patterns
                        .iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect();
                }
                Some(ClientFsConfig { fs, mode })
            });
        let fs: Arc<dyn xai_grok_workspace::file_system::AsyncFileSystem> = if use_acp_fs {
            let mut acp_fs = AcpSessionFs::new(
                cwd.to_path_buf(),
                session_info.id.clone(),
                self.gateway.clone(),
            );
            if let Some(ref display) = prompt_display_cwd {
                acp_fs = acp_fs.with_display_cwd(std::path::PathBuf::from(display));
            }
            Arc::new(acp_fs)
        } else {
            Arc::new(LocalFs::new(cwd.to_path_buf()))
        };
        let gateway_enabled = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        );
        let terminal: std::sync::Arc<dyn crate::terminal::AsyncTerminalRunner> = if client_terminal {
            std::sync::Arc::new(AcpTerminalRunner {
                gateway: self.gateway.clone(),
                session_id: session_info.id.clone(),
            })
        } else {
            let notifier: std::sync::Arc<
                dyn crate::terminal::SessionNotificationSender,
            > = std::sync::Arc::new(
                crate::terminal::GatedNotifier::new(
                    std::sync::Arc::new(self.gateway.clone()),
                    gateway_enabled.clone(),
                ),
            );
            std::sync::Arc::new(TerminalRunner::new(notifier, session_info.id.clone()))
        };
        let startup_hints = startup_hints_from_meta(session_meta, init.meta.as_ref());
        let hunk_plan = plan_hunk_tracking(
            init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("x.ai/hunkTracker"))
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
        );
        let incremental_bash_output = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/incrementalBashOutput"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let no_color = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/bashOutputNoColor"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hunk_tracking_enabled = hunk_plan.enabled();
        let (hunk_tracker_handle, hunk_event_rx) = match hunk_plan.actor_mode {
            Some(mode) => {
                let cancel = CancellationToken::new();
                let (hunk_event_tx, hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
                let handle = HunkTrackerActor::spawn(
                    session_info.id.0.to_string(),
                    cwd.as_path().to_path_buf(),
                    hunk_event_tx,
                    mode,
                    cancel.clone(),
                );
                (handle, Some((hunk_event_rx, cancel)))
            }
            None => (xai_hunk_tracker::HunkTrackerHandle::noop(), None),
        };
        let has_xai_auth = self.auth_manager.current().is_some_and(|a| a.is_xai_auth());
        let loc_tracking_enabled = hunk_tracking_enabled && has_xai_auth
            && (self
                .cfg
                .borrow()
                .remote_settings
                .as_ref()
                .and_then(|s| s.loc_tracking)
                .unwrap_or(false)
                || std::env::var("GROK_LOC_TRACKING")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false));
        let (feedback_resolved, feedback_flags) = {
            let cfg = self.cfg.borrow();
            let resolved = cfg.feature(crate::agent::config::Feature::Feedback);
            let flags = crate::session::feedback_manager::FeedbackFlags {
                enabled: resolved.value,
                user: cfg.feedback.user.clone(),
            };
            (resolved, flags)
        };
        tracing::info!(feedback = %feedback_resolved, "resolved feedback feature flag");
        let loc_aggregate_rx = match hunk_event_rx {
            Some((hunk_event_rx, loc_cancel)) if loc_tracking_enabled => {
                let (loc_agg_tx, loc_agg_rx) = tokio::sync::mpsc::unbounded_channel();
                let loc_path = crate::session::persistence::session_dir(&session_info)
                    .join("hunk_records.jsonl");
                let loc_writer = xai_hunk_tracker::JsonlHunkRecordWriter::new(loc_path);
                let loc_ctx = xai_hunk_tracker::LocSinkContext {
                    session_id: session_info.id.0.to_string(),
                    agent_id: agent_id(),
                    user_id: self.auth_manager.current().map(|a| a.user_id.clone()),
                    aggregate_tx: Some(loc_agg_tx),
                };
                tokio::spawn(
                    xai_hunk_tracker::run_loc_sink(
                        hunk_event_rx,
                        loc_writer,
                        loc_ctx,
                        loc_cancel,
                    ),
                );
                Some(loc_agg_rx)
            }
            _ => None,
        };
        let session_env_timer = crate::instrumentation_timer!("session.spawn_and_register.session_env");
        let mut session_env = xai_grok_workspace::permission::claude_settings::load_claude_env_with_project(
            cwd.as_path(),
            project_env_trusted,
        );
        session_env.extend(envrc.join().await);
        drop(session_env_timer);
        if no_color {
            session_env.extend(crate::terminal::no_color_env());
        } else {
            session_env.extend(crate::terminal::color_env());
        }
        let mut tool_ctx = ToolContext::with_preloaded_env(
                cwd.clone(),
                Some(self.gateway.clone()),
                Some(session_info.id.clone()),
                fs,
                terminal,
                hunk_tracker_handle,
                session_env,
            )
            .with_hunk_tracking_enabled(hunk_tracking_enabled);
        tool_ctx.process_scope = Some(ProcessScope::new());
        let workspace_ops = self
            .resolve_workspace_ops()
            .map_err(|_| {
                acp::Error::internal_error()
                    .data(
                        "Local workspace initialization failed; cannot create session. \
                 Check that a Tokio runtime is available.",
                    )
            })?;
        tool_ctx.subagent_event_tx = Some(self.subagent_event_tx.event_sender().0);
        tool_ctx.subagent_coordinator_sender = Some(self.subagent_event_tx.clone());
        tool_ctx.synthetic_trace_tx = self
            .subagent_presentation
            .borrow()
            .synthetic_trace_tx
            .clone();
        if let Some(ref shared) = tool_ctx.synthetic_trace_tx_shared {
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = self
                .subagent_presentation
                .borrow()
                .synthetic_trace_tx
                .clone();
        }
        tool_ctx.is_turn_active = Some(
            self.subagent_presentation.borrow().turn_active_flag(),
        );
        tool_ctx.monitor_event_buffer = Some(self.monitor_event_buffer.clone());
        tool_ctx.subagent_depth = 0;
        tool_ctx.auto_wake_enabled = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::AutoWake);
        let support_permission = self.cfg.borrow().features.support_permission;
        let telemetry_enabled = self.product_analytics_enabled();
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let sampling_config = self
            .resolve_sampling_config_for_model(&session_model_id, origin_client.clone());
        if self.auth_method_id.load().is_none() {
            return Err(acp::Error::auth_required().data("no auth method id provided"));
        }
        let auth_method_id = std::sync::Arc::clone(&self.auth_method_id);
        tracing::info!(
            session_id = %session_info.id.0,
            ?startup_hints,
            "startup hints"
        );
        let compaction_verbatim_input = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::CompactionVerbatimInput);
        let compaction_tool_choice = self.cfg.borrow().resolve_compaction_tool_choice();
        let auto_update = self.cfg.borrow().cli.auto_update;
        let client_type = *self.client_type.borrow();
        let buffering_settings = self.buffering_settings.borrow().clone();
        let (
            feedback_proxy_url,
            feedback_user_token,
            feedback_alpha_test_key,
            deployment_key,
        ) = if let Some((url, token, alpha, deploy)) = self.feedback_credentials() {
            (Some(url), token, alpha, deploy)
        } else {
            (None, None, None, None)
        };
        tracing::info!(
            session_id = %session_info.id.0,
            feedback_url = ?feedback_proxy_url,
            authenticated = feedback_user_token.is_some(),
            "Initializing feedback manager for session"
        );
        let skills = self.cfg.borrow().skills.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let paths_config = self.cfg.borrow().paths.clone();
        let acp_agent_profile = parse_agent_profile_from_meta(session_meta);
        let session_default_agent_profile = acp_agent_profile
            .as_ref()
            .map(|d| d.name.clone());
        let mut agent_definition = {
            let cfg = self.cfg.borrow();
            Self::resolve_agent_definition(
                cwd.as_path(),
                cfg.agent_profile_path.as_deref(),
                &cfg.agent,
                acp_agent_profile,
                model_agent_type,
            )
        };
        {
            let cfg = self.cfg.borrow();
            let overrides = &cfg.cli_agent_overrides;
            overrides.apply_to_definition(&mut agent_definition);
            if overrides.has_definition_overrides() {
                tracing::debug!(
                    agent = %agent_definition.name,
                    tools = ?overrides.tools,
                    disallowed = ?overrides.disallowed_tools,
                    permission_mode = ?overrides.permission_mode,
                    "cli agent overrides applied"
                );
            }
        }
        let pinned_model: Option<(acp::ModelId, ModelEntry)> = match &agent_definition
            .model
        {
            xai_grok_agent::config::ModelOverride::Override(id) => {
                let mid = acp::ModelId::new(Arc::from(id.as_str()));
                match self.resolve_model_id(&mid) {
                    Ok(entry) => Some((mid, entry)),
                    Err(_) => {
                        tracing::warn!(
                            agent = %agent_definition.name,
                            model = %id,
                            "agent profile model not in catalog, keeping session default"
                        );
                        None
                    }
                }
            }
            xai_grok_agent::config::ModelOverride::Inherit => None,
        };
        if let Some(template) = inherited_harness_template(
            &agent_definition.user_message_template,
            pinned_model.as_ref().map(|(_, e)| e.info().agent_type.as_str()),
            cwd.as_path(),
        ) {
            tracing::info!(
                agent = %agent_definition.name,
                "Inheriting harness wire-format from the profile model's agent_type"
            );
            agent_definition.user_message_template = template;
        }
        let pins = crate::session::cursor_compaction_pins(
            self.cfg.borrow().resolve_compaction_mode(),
            self.cfg.borrow().is_two_pass_compaction_enabled(),
            crate::session::is_cursor_user_template(
                &agent_definition.user_message_template,
            ),
        );
        let compaction_mode = pins.mode;
        let two_pass_enabled = pins.two_pass;
        let (session_model_id, mut sampling_config) = self
            .apply_agent_model_override(
                pinned_model.as_ref(),
                session_model_id,
                sampling_config,
                origin_client.clone(),
            );
        let (auto_compact_threshold_percent, system_prompt_label) = {
            let cfg = self.cfg.borrow();
            let models = self.models_manager.models();
            let model = config::find_model_by_id(&models, &session_model_id.0);
            (
                crate::util::config::resolve_auto_compact_threshold_percent(
                    &cfg,
                    &session_model_id.0,
                    model.map(|e| &e.info),
                ),
                crate::util::config::resolve_system_prompt_label(
                    &cfg,
                    &session_model_id.0,
                    model.map(|e| &e.info),
                ),
            )
        };
        self.models_manager
            .apply_supported_effort(
                &mut sampling_config,
                initial_reasoning_effort,
                &session_info.id,
                EffortTarget::NewSession,
            );
        sampling_config.conversation_group_id = Some(
            crate::sampling::derive_conversation_group_id(session_info.id.0.as_ref()),
        );
        let max_turns = {
            let cfg = self.cfg.borrow();
            cfg.cli_agent_overrides
                .max_turns
                .or(agent_definition.max_turns)
                .map(|v| v as usize)
        };
        {
            let cfg = self.cfg.borrow();
            let effective = cfg
                .toolset
                .resolve_file_toolset(cfg.remote_settings.as_ref());
            if effective != crate::tools::FileToolset::Standard {
                let file_tools = effective
                    .tool_configs(&cfg.toolset.hashline)
                    .map_err(|e| {
                        acp::Error::invalid_params()
                            .data(format!("invalid [toolset.hashline] config: {e}"))
                    })?;
                agent_definition.override_file_tools(file_tools);
            }
        }
        let lsp_tools_enabled = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::LspTools);
        if lsp_tools_enabled && tool_ctx.lsp.is_none() {
            let snapshot = self.plugin_registry_handle.snapshot();
            let active: Vec<_> = snapshot
                .iter()
                .flat_map(|reg| reg.active_plugins())
                .collect();
            let (plugin_lsp_paths, plugin_names): (Vec<std::path::PathBuf>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.lsp_config_path.clone().map(|path| (path, p.name.as_str()))
                })
                .unzip();
            let (
                plugin_inline_lsp,
                inline_names,
            ): (Vec<&serde_json::Value>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.inline_lsp_servers.as_ref().map(|v| (v, p.name.as_str()))
                })
                .unzip();
            let sourced = xai_grok_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
                tool_ctx.cwd.as_path(),
                &plugin_lsp_paths,
                &plugin_inline_lsp,
                &plugin_names,
                &inline_names,
            );
            let servers = folder_trust::filter_untrusted_project_lsp(
                tool_ctx.cwd.as_path(),
                sourced,
            );
            tool_ctx.lsp_server_names = servers.keys().cloned().collect();
            if servers.is_empty() {
                let user_path = xai_grok_tools::util::grok_home::grok_home()
                    .join("lsp.json");
                let project_path = tool_ctx.cwd.as_path().join(".grok").join("lsp.json");
                tracing::debug!(
                    cwd = %tool_ctx.cwd,
                    user_lsp_path = %user_path.display(),
                    project_lsp_path = %project_path.display(),
                    "LSP tools enabled, but no language servers are configured"
                );
            } else {
                use xai_grok_tools::implementations::lsp::{
                    LspBackend, LspBackendAdapter, LspManager,
                };
                let mgr = std::sync::Arc::new(
                    tokio::sync::Mutex::new(
                        LspManager::new(
                                servers,
                                tool_ctx.cwd.as_path().to_path_buf(),
                                true,
                                xai_grok_tools::notification::ToolNotificationHandle::noop(),
                            )
                            .with_process_scope(tool_ctx.process_scope.clone()),
                    ),
                );
                let adapter = std::sync::Arc::new(LspBackendAdapter::new(mgr));
                adapter.ensure_started_background();
                tool_ctx.lsp = Some(adapter as std::sync::Arc<dyn LspBackend>);
            }
        }
        let inference_idle_timeout_secs = {
            let models = self.models_manager.models();
            let cfg = self.cfg.borrow();
            resolve_inference_idle_timeout_secs(
                &models,
                &sampling_config.model,
                cfg.remote_settings.as_ref(),
            )
        };
        let subagent_rate_limit_max_attempts = {
            let models = self.models_manager.models();
            let per_model = crate::agent::config::find_model_by_id(
                    &models,
                    &sampling_config.model,
                )
                .and_then(|entry| entry.info.subagent_rate_limit_max_attempts);
            self.resolved_subagent_rate_limit_max_attempts(per_model)
        };
        let model_max_retries = self
            .models_manager
            .models()
            .values()
            .find(|entry| entry.info.has_model_id(&sampling_config.model))
            .and_then(|entry| entry.info.max_retries);
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let web_search_sampling_config = self.prepare_web_search_sampling_config();
        let image_gen_config = self.prepare_image_gen_config();
        let video_gen_config = self.prepare_video_gen_config();
        let app_builder_deployer_config = self.prepare_app_builder_deployer_config();
        let web_fetch_config = self.prepare_web_fetch_config();
        let write_file_enabled = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::WriteFile);
        let active_agent_messages_enabled = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::ActiveAgentMessages);
        let goal_enabled = self.cfg.borrow().resolve_goal().value;
        let background_workflows_enabled = self.cfg.borrow().resolve_workflows().value;
        let subagents_enabled = self.cfg.borrow().subagents_enabled;
        let subagents_max_depth = self.cfg.borrow().subagents_max_depth;
        let workflow_max_concurrent_agents = self
            .cfg
            .borrow()
            .workflow_max_concurrent_agents;
        let media_gen_batch_limits = self.cfg.borrow().media_gen_batch_limits;
        let ask_user_question_enabled = crate::upload::turn::parse_ask_user_question_from_meta(
                session_meta,
            )
            .unwrap_or_else(|| {
                self
                    .cfg
                    .borrow()
                    .is_feature_enabled(crate::agent::config::Feature::AskUserQuestion)
            });
        let client_hooks = crate::extensions::hooks::parse_client_hooks(session_meta);
        let disable_web_search = self.cfg.borrow().disable_web_search;
        let todo_gate = self.cfg.borrow().todo_gate;
        let remote_settings_for_spawn = self.cfg.borrow().remote_settings.clone();
        let laziness_debug_log_for_spawn = self.cfg.borrow().laziness_debug_log.clone();
        let respect_gitignore = self.cfg.borrow().respect_gitignore;
        let path_not_found_hints = self.cfg.borrow().path_not_found_hints;
        let subagent_toggle = self.cfg.borrow().subagent_toggle.clone();
        let handle_display_cwd = prompt_display_cwd.clone();
        let auth_manager = Some(self.auth_manager.clone());
        let bash_params_json = {
            let cfg = self.cfg.borrow();
            cfg.toolset
                .bash
                .to_bash_params_json_with_remote(cfg.remote_settings.as_ref())
        };
        let ask_user_question_params_json = {
            let cfg = self.cfg.borrow();
            let params = crate::util::config::resolve_ask_user_question_params_from_disk(
                cfg.remote_settings.as_ref(),
            );
            match serde_json::to_value(params) {
                Ok(serde_json::Value::Object(map)) => Some(map),
                _ => None,
            }
        };
        let tool_params_json = crate::session::agent_rebuild::ResolvedToolParamsJson {
            bash: Some(bash_params_json),
            ask_user_question: ask_user_question_params_json,
        };
        let backend_tools_enabled = self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::BackendTools);
        let managed_mcp_proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let init_meta = self
            .initialize_request
            .get()
            .and_then(|init| init.meta.as_ref());
        if let Some(override_prompt) = system_prompt_override_from_meta(
            session_meta,
            init_meta,
        ) && !chat_history.is_empty() && !startup_hints.preserve_inherited_system
        {
            let changed = replace_or_insert_system_head(
                &mut chat_history,
                override_prompt,
            );
            if changed {
                tracing::info!(
                    session_id = %session_info.id.0,
                    prompt_len = override_prompt.len(),
                    "cold-load: applied systemPromptOverride to loaded head"
                );
            } else {
                tracing::debug!(
                    session_id = %session_info.id.0,
                    "cold-load: systemPromptOverride already matches head, no-op"
                );
            }
        }
        let (init, session_thread) = {
            let _timer = crate::instrumentation_timer!("session.spawn_actor_call");
            let session_key = self.auth_manager.current_or_expired().map(|a| a.key);
            let credentials = xai_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type(
                    sampling_config.model.as_str(),
                    session_key.as_deref(),
                    self.auth_type(),
                ),
                alpha_test_key: self.alpha_test_key(),
                client_version: sampling_config.client_version.clone(),
            };
            let attribution_callback: Option<
                xai_grok_sampler::SharedAttributionCallback,
            > = Some(
                xai_grok_login::attribution::ShellAttribution::new(
                    self.auth_manager.clone(),
                    Some(session_info.id.0.to_string()),
                ),
            );
            let agent_hook_registry_override = agent_definition
                .hooks
                .as_ref()
                .and_then(|hooks_config| {
                    let hooks_val = hooks_config.as_value();
                    let (specs, errors) = xai_grok_hooks::config::parse_hooks_from_value_with_dir(
                        &hooks_val,
                        &format!(
                        "{}{}",
                        xai_grok_hooks::config::AGENT_HOOK_PREFIX,
                        agent_definition.name
                    ),
                        std::path::Path::new(&session_info.cwd),
                    );
                    for e in &errors {
                        tracing::warn!(agent = %agent_definition.name, error = ?e, "agent hook parse error");
                    }
                    if specs.is_empty() {
                        return None;
                    }
                    let cwd = std::path::Path::new(&session_info.cwd);
                    let hooks_trusted = project_env_trusted;
                    let git_root = {
                        let _timer = crate::instrumentation_timer!("session.spawn_git_root");
                        xai_grok_workspace::session::git::find_git_root_from_path(cwd)
                            .ok()
                    };
                    let (disk_registry, disk_errors) = {
                        let _timer = crate::instrumentation_timer!("session.spawn_hook_discovery");
                        crate::util::hooks::discover_hooks(
                            git_root.as_deref(),
                            &compat,
                            hooks_trusted,
                        )
                    };
                    for e in &disk_errors {
                        tracing::warn!(error = ?e, "hook loading error");
                    }
                    let mut merged = disk_registry;
                    if folder_trust::agent_inline_hooks_allowed(
                        agent_definition.scope,
                        || hooks_trusted,
                    ) {
                        merged.append_specs(specs);
                    }
                    Some(std::sync::Arc::new(merged))
                });
            let reasoning_effort_to_persist = chat_history
                .is_empty()
                .then_some(sampling_config.reasoning_effort);
            let _ = persistence
                .tx
                .send(crate::session::persistence::PersistenceMsg::CurrentModel {
                    model_id: session_model_id.clone(),
                    agent_name: Some(agent_definition.name.clone()),
                    reasoning_effort: reasoning_effort_to_persist,
                });
            let acp_mcp_servers = crate::session::acp_mcp::parse_acp_mcp_servers(
                session_meta,
            );
            let git_head_changed = init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("x.ai/gitHeadChanged"))
                .and_then(|v| v.as_bool());
            let client_caps = crate::session::notifications::SessionClientCaps::new(
                Self::resolve_status_line_capability(session_meta, init),
                Self::resolve_user_message_echo_capability(session_meta, init),
            );
            let fs_watch_caps = crate::session::fs_watch::FsWatchCapabilities::resolve(crate::session::fs_watch::CapabilityInputs {
                client_notify: fs_notify_config.is_some(),
                hunk_tracking: hunk_plan.enabled(),
                code_nav: client_code_nav_enabled,
                git_head_changed,
            });
            tool_ctx.live_orphan_heal_lock = self
                .session_registry
                .live_orphan_heal_lock(&session_info.id);
            let _spawn_on_thread_timer = crate::instrumentation_timer!("session.spawn_on_thread");
            spawn_session_on_thread(
                    session_info.clone(),
                    self.gateway.clone(),
                    sampling_config,
                    credentials,
                    auth_method_id,
                    auth_manager,
                    attribution_callback,
                    tool_ctx,
                    mcp_servers,
                    initial_client_mcp_servers,
                    mcp_meta_config_map,
                    None,
                    acp_mcp_servers,
                    support_permission,
                    telemetry_enabled,
                    auto_update,
                    persistence,
                    chat_history.clone(),
                    rewind_points_file_path,
                    fs_notify_config,
                    initial_total_tokens,
                    startup_hints,
                    client_type,
                    auto_compact_threshold_percent,
                    system_prompt_label,
                    compaction_mode,
                    compaction_verbatim_input,
                    compaction_tool_choice,
                    two_pass_enabled,
                    buffering_settings,
                    origin_client.clone(),
                    self.codebase_indexes.clone(),
                    client_code_nav_enabled,
                    fs_watch_caps,
                    client_caps,
                    feedback_proxy_url,
                    feedback_user_token,
                    feedback_alpha_test_key,
                    deployment_key,
                    client_terminal,
                    client_fs_read && client_fs_write,
                    gateway_enabled,
                    agent_definition,
                    session_default_agent_profile,
                    skills,
                    None,
                    compat,
                    paths_config,
                    incremental_bash_output,
                    persisted_signals,
                    persisted_plan_mode,
                    persisted_goal_mode,
                    persisted_workflow_runs,
                    persisted_announcement_state,
                    self.memory_config_snapshot(),
                    loc_tracking_enabled,
                    feedback_flags,
                    self.managed_mcp_cache.clone(),
                    managed_mcp_proxy_url,
                    session_model_id,
                    session_yolo_mode,
                    session_auto_mode,
                    origin_client.as_ref().map(|o| o.product.clone()),
                    inference_idle_timeout_secs,
                    model_max_retries,
                    subagent_rate_limit_max_attempts,
                    web_search_sampling_config,
                    web_fetch_config,
                    image_gen_config,
                    video_gen_config,
                    app_builder_deployer_config,
                    write_file_enabled,
                    active_agent_messages_enabled,
                    None,
                    goal_enabled,
                    background_workflows_enabled,
                    subagents_enabled,
                    subagents_max_depth,
                    workflow_max_concurrent_agents,
                    media_gen_batch_limits,
                    ask_user_question_enabled,
                    client_hooks,
                    prompt_display_cwd,
                    subagent_toggle,
                    Vec::new(),
                    xai_grok_agent::prompt::context::PromptAudience::Primary,
                    None,
                    None,
                    disable_web_search,
                    backend_tools_enabled,
                    respect_gitignore,
                    path_not_found_hints,
                    tool_params_json,
                    prefetch,
                    Some(self.plugin_registry_handle.clone()),
                    self.models_manager.clone(),
                    None,
                    None,
                    Some(
                        Arc::new(
                            xai_grok_login::manager::SharedAuthKeyProvider(
                                self.auth_manager.clone(),
                            ),
                        ),
                    ),
                    self.resolve_image_description_model(),
                    agent_hook_registry_override,
                    workspace_ops.clone(),
                    {
                        let cfg = self.cfg.borrow();
                        cfg.cli_agent_overrides.permission_rules.clone()
                    },
                    todo_gate,
                    remote_settings_for_spawn,
                    laziness_debug_log_for_spawn,
                    None,
                    None,
                    max_turns,
                    None,
                    is_chat_kind,
                    None,
                    None,
                    spawn_trace,
                )
                .await?
        };
        tracing::debug!(session_id = %session_info.id.0, "spawn_session_on_thread complete");
        let crate::session::SessionInitResult {
            mut handle,
            permission_events_rx,
            system_prompt: agent_system_prompt,
            toolset,
        } = init;
        #[cfg(test)]
        super::test_hooks::pause_at(super::test_hooks::AttachPause::AfterSpawn).await;
        self.set_session_live_state(&session_info.id, SessionLiveState::IdleResident);
        self.ensure_session_supervisor();
        self.heap_profile_set_session_id(&session_info.id.0);
        self.push_roster_delta_upserted(&session_info.id);
        if chat_history.is_empty() {
            let _timer = crate::instrumentation_timer!("session.system_prompt_inject");
            let system_prompt = build_spawn_system_prompt(
                session_meta,
                init_meta,
                &agent_system_prompt,
            );
            tracing::debug!(
                session_id = %session_info.id.0,
                "built system prompt"
            );
            let _ = handle
                .cmd_tx
                .send(SessionCommand::Initialize {
                    system_prompt,
                });
            tracing::debug!(session_id = %session_info.id.0, "enqueued SessionCommand::Initialize");
        }
        let _ = handle
            .cmd_tx
            .send(SessionCommand::AdvertiseCommands {
                trigger: crate::session::AdvertiseTrigger::SessionStart,
            });
        if let Some(mut loc_rx) = loc_aggregate_rx {
            let signals = handle.signals_handle.clone();
            tokio::spawn(async move {
                while let Some(agg) = loc_rx.recv().await {
                    match agg {
                        xai_hunk_tracker::LocAggregate::LinesChanged {
                            author_type,
                            lines_added,
                            lines_removed,
                            file_path,
                        } => {
                            let is_agent = author_type
                                == xai_hunk_tracker::AuthorType::Agent;
                            signals
                                .record_loc_change(
                                    is_agent,
                                    lines_added,
                                    lines_removed,
                                    file_path,
                                );
                        }
                        xai_hunk_tracker::LocAggregate::LinesReverted {
                            lines_added_reverted,
                            lines_removed_reverted,
                        } => {
                            signals
                                .record_loc_revert(
                                    lines_added_reverted,
                                    lines_removed_reverted,
                                );
                        }
                    }
                }
            });
        }
        self.session_registry
            .set_permission_receiver(&session_info.id, permission_events_rx);
        if handle_display_cwd.is_some() {
            handle.display_cwd = handle_display_cwd;
        }
        let source = if chat_history.is_empty() { "new" } else { "load" };
        let _ = handle
            .cmd_tx
            .send(SessionCommand::DispatchSessionStartHook {
                source: source.to_string(),
            });
        self.notify_session_cwd_for_watch(std::path::Path::new(&session_info.cwd));
        self.activity.register_session(&session_info.id.0, &handle);
        let bind_cwd = handle.tool_context.cwd.clone();
        let bind_tracker = handle.hunk_tracker_handle.clone();
        let (displaced, install_id) = match attach_waiter {
            Some(waiter) => {
                match self
                    .session_registry
                    .put_resident_for_attach(
                        &session_info.id,
                        handle,
                        session_thread,
                        root_identity,
                        waiter,
                    )
                {
                    Ok((displaced, install_id)) => (displaced, Some(install_id)),
                    Err(stale) => {
                        self.roll_back_install(&session_info.id, *stale).await;
                        return Ok(false);
                    }
                }
            }
            None => {
                self.session_registry.set_thread(&session_info.id, session_thread);
                (self.install_resident(&session_info.id, handle, root_identity), None)
            }
        };
        self.bind_accepted_install(
            &workspace_ops,
            &session_info.id,
            bind_cwd.as_path(),
            &bind_tracker,
            &toolset,
            install_id,
        );
        let mut stamp_guard = install_id
            .map(|install_id| PendingStampGuard::arm(
                self,
                session_info.id.clone(),
                install_id,
            ));
        let displaced_existing = if let Some(old) = displaced {
            if let Some(scope) = &old.tool_context.process_scope {
                scope.kill_all();
            }
            true
        } else {
            false
        };
        #[cfg(test)]
        super::test_hooks::pause_at(super::test_hooks::AttachPause::AfterInstall).await;
        if let Some(install_id) = install_id {
            let Some(identity) = self
                .session_registry
                .current_attach_identity(&session_info.id, install_id) else {
                return Ok(false);
            };
            let Some(current) = self.resident_handle(&session_info.id) else {
                return Ok(false);
            };
            let (respond_to, response) = tokio::sync::oneshot::channel();
            let sent = current
                .persistence_tx
                .send(crate::session::persistence::PersistenceMsg::StampSessionIdentity {
                    identity: identity.to_session_identity(),
                    respond_to,
                })
                .is_ok();
            let outcome = if sent && matches!(response.await, Ok(Ok(_))) {
                IdentityStamp::Durable
            } else {
                tracing::warn!(session = %session_info.id, "session identity stamp failed; attach fails closed");
                IdentityStamp::Failed
            };
            let resolution = self
                .session_registry
                .resolve_identity_stamp(&session_info.id, install_id, outcome);
            if let Some(guard) = &mut stamp_guard {
                guard.disarm();
            }
            match resolution {
                StampResolution::Landed => {}
                StampResolution::Withdrawn(withdrawn) => {
                    self.roll_back_install(&session_info.id, *withdrawn).await;
                    return Ok(false);
                }
                StampResolution::Superseded => return Ok(false),
            }
        }
        if is_headless {
            self.session_registry.mark_headless(&session_info.id);
        }
        self.spawn_managed_gateway_tool_catalog_fetch();
        let cwd_for_maintenance = session_info.cwd.clone();
        tokio::spawn(async move {
            crate::session::prompt_history::truncate_if_needed_async(cwd_for_maintenance)
                .await;
        });
        Ok(!displaced_existing)
    }
    /// Collects all pending permission events from a session's receiver.
    /// Returns only the events from the current turn (since last collection).
    pub(super) fn collect_permission_events(
        &self,
        session_id: &acp::SessionId,
    ) -> Vec<PermissionEvent> {
        self.session_registry.drain_permission_events(session_id)
    }
}
/// Withdraws an installed actor whose stamp the installer never resolved; a load cancelled mid
/// round-trip would otherwise leave a `Pending` slot every later adopter waits out.
struct PendingStampGuard<'a> {
    agent: &'a MvpAgent,
    session_id: acp::SessionId,
    install_id: u64,
    armed: bool,
}
impl<'a> PendingStampGuard<'a> {
    fn arm(agent: &'a MvpAgent, session_id: acp::SessionId, install_id: u64) -> Self {
        Self {
            agent,
            session_id,
            install_id,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for PendingStampGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let StampResolution::Withdrawn(withdrawn) = self
            .agent
            .session_registry
            .resolve_identity_stamp(
                &self.session_id,
                self.install_id,
                IdentityStamp::Failed,
            ) else {
            return;
        };
        self.agent.roll_back_install_sync(&self.session_id, *withdrawn);
    }
}
/// Rollback guard for mid-session bind reservation.
#[cfg(feature = "local-workspace")]
struct LocalWorkspaceBindGuard {
    bound: Rc<RefCell<std::collections::HashSet<acp::SessionId>>>,
    session_id: acp::SessionId,
    keep: bool,
}
#[cfg(feature = "local-workspace")]
impl Drop for LocalWorkspaceBindGuard {
    fn drop(&mut self) {
        if !self.keep {
            self.bound.borrow_mut().remove(&self.session_id);
        }
    }
}
/// Reap guard: if session/new fails after register, Drop kills the supervisor.
#[cfg(all(feature = "local-workspace", unix))]
pub(crate) struct LocalWorkspaceReapGuard {
    supervisors: Rc<
        RefCell<
            HashMap<
                acp::SessionId,
                crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceHandle,
            >,
        >,
    >,
    generations: Rc<RefCell<HashMap<acp::SessionId, u64>>>,
    session_id: acp::SessionId,
    armed: bool,
}
#[cfg(all(feature = "local-workspace", unix))]
impl LocalWorkspaceReapGuard {
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}
#[cfg(all(feature = "local-workspace", unix))]
impl Drop for LocalWorkspaceReapGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.generations.borrow_mut().remove(&self.session_id);
        if let Some(handle) = self.supervisors.borrow_mut().remove(&self.session_id) {
            tokio::spawn(async move {
                handle.shutdown().await;
            });
        }
    }
}
