// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
use super::*;
use super::turn_end::{
    ErrorTurnArtifacts, TraceCaptures, TurnEndCapture, TurnEndOutcome, TurnResultArgs,
    run_detached_turn_end, run_registry_turn_end, run_trace_completion,
    upload_error_turn_artifacts,
};
use distill_telemetry::instrument_task;
use distill_telemetry::region;
use distill_telemetry::region::Parent;
use distill_telemetry::startup;
use tracing::Instrument;
use distill_login::{CachedTokenState, SilentRefresh};
use crate::upload::trace::PromptMetadataParams;
use crate::leader::protocol::InternalMethod;
/// Which `x_search` sub-tools enforce the date cutoff, sent in `initialize`. `x_user_search` and
/// `x_thread_fetch` are `false`: they don't honor it yet.
#[derive(serde::Serialize)]
struct ToolOverridesCapability {
    x_keyword_search: bool,
    x_semantic_search: bool,
    x_user_search: bool,
    x_thread_fetch: bool,
}
const TOOL_OVERRIDES_CAPABILITY: ToolOverridesCapability = ToolOverridesCapability {
    x_keyword_search: true,
    x_semantic_search: true,
    x_user_search: false,
    x_thread_fetch: false,
};
fn tool_overrides_capability() -> serde_json::Value {
    serde_json::to_value(TOOL_OVERRIDES_CAPABILITY)
        .expect("ToolOverridesCapability is always serializable")
}
impl MvpAgent {
    pub(crate) async fn set_model_gated(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let model = match self.resolve_model_id(&args.model_id) {
            Ok(model) => model,
            Err(_) => {
                self.models_manager.wait_for_first_catalog().await;
                self.resolve_model_id(&args.model_id)?
            }
        };
        if !model.info.user_selectable {
            return Err(
                acp::Error::invalid_params()
                    .data(
                        crate::agent::remote_config::allowlist_denied_message(
                                &self.cfg.borrow(),
                            )
                            .to_string(),
                    ),
            );
        }
        let session_id = args.session_id.clone();
        let switch_effort = match parse_reasoning_effort_meta(args.meta.as_ref()) {
            Some(effort) => {
                crate::agent::handlers::model_switch::SwitchEffort::Set(Some(effort))
            }
            None => crate::agent::handlers::model_switch::SwitchEffort::Preserve,
        };
        let res = crate::agent::handlers::model_switch::apply(
                self,
                args,
                switch_effort,
                crate::agent::handlers::model_switch::ConfigNotice::Send,
            )
            .await;
        if res.is_ok()
            && let Some(unavailable) = self
                .session_registry
                .take_unavailable_model(&session_id)
        {
            tracing::info!(
                session_id = %session_id.0,
                previously_unavailable_model = %unavailable.0,
                "set_session_model: user model switch cleared the model-unavailable block"
            );
        }
        res
    }
}
#[async_trait::async_trait(?Send)]
impl acp::Agent for MvpAgent {
    /// The response meta carries `model_state` so the client can display the available models and the default model. SINGLE-CALL INVARIANT: this method is the sole writer of `self.auth_method_id` during initialization.
    /// It is called exactly once per agent process by the ACP server before any session-creating requests. At that point `auth_method_id` is still `None` (initialized at `MvpAgent::new`).
    /// The auth-method block below relies on that invariant when it unconditionally writes the default id from `auth_method::build_auth_methods`.
    #[tracing::instrument(name = "agent.acp_initialize", skip_all)]
    async fn initialize(
        &self,
        arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        // Session creation/restoration must see the account's catalog before
        // deciding that a saved ChatGPT model is unavailable.
        self.models_manager.refresh_chatgpt_models().await;
        if let Some(meta) = arguments.meta.as_ref() {
            distill_otel::link_current_span_to_meta(
                &serde_json::Value::Object(meta.clone()),
            );
        }
        tracing::debug!(target: "sampling_log", "Received initialize request");
        distill_telemetry::unified_log::info("agent initialized", None, None);
        startup::mark_agent_serving();
        let _t = distill_telemetry::instrumentation::timer(
            "startup.acp_initialize.handler",
        );
        self.start_subagent_coordinator();
        if self.cfg.borrow().remote_settings.is_none() {
            self.spawn_settings_reapply();
        }
        let remote_settled = self.remote_settings_settled();
        let auto_gc_policy = self.cfg.borrow().resolve_worktree_auto_gc();
        if !remote_settled {
            tracing::debug!(
                "auto worktree gc and session search deferred until remote_settings arrive"
            );
        }
        let distill_home = distill_fast_worktree::resolve_distill_home();
        tokio::task::spawn_blocking(move || {
            crate::session::worktree_pool::cleanup_stale_pool_worktrees(None);
            if !remote_settled {
                return;
            }
            Self::reclaim_worktrees(distill_home, auto_gc_policy);
        });
        if remote_settled {
            self.start_search_index_once();
        }
        const PERMISSION_CLEANUP_TTL_DAYS: u64 = 30;
        static CLEANUP_PERMISSIONS_ONCE: std::sync::Once = std::sync::Once::new();
        CLEANUP_PERMISSIONS_ONCE
            .call_once(|| {
                tokio::task::spawn(
                    distill_workspace::permission::cleanup_stale_permission_state(
                        std::time::Duration::from_secs(
                            PERMISSION_CLEANUP_TTL_DAYS * 24 * 60 * 60,
                        ),
                    ),
                );
            });
        distill_workspace::trust::migrate_legacy_hook_trust();
        if let Some(auth) = self.auth_manager.current() {
            let user_id = auth.user_id.trim();
            let needs_user_info = user_id.is_empty()
                || user_id.eq_ignore_ascii_case("unknown");
            distill_telemetry::unified_log::info(
                "auth init user_info check",
                None,
                Some(
                    serde_json::json!({
                    "user_id": user_id,
                    "needs_user_info": needs_user_info,
                    "key_prefix": distill_auth::bearer_suffix(&auth.key),
                    "rt_prefix": auth.refresh_token.as_deref().map(distill_auth::bearer_suffix),
                }),
                ),
            );
            if needs_user_info {
                let _t = distill_telemetry::instrumentation::timer(
                    "startup.acp_initialize.user_info",
                );
                let _s = region!("startup.acp_initialize.user_info", Parent::Inherit);
                if let Err(e) = self.auth_manager.update(auth).await {
                    tracing::warn!(
                        "Failed to refresh user info from proxy during new_session: {}",
                        e
                    );
                }
            }
        }
        if !self.tier_allowed.get() {
            self.spawn_tier_recheck();
        }
        self.maybe_sync_bundle_in_background(false);
        let mut client_type = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientType"))
            .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
            .unwrap_or_default();
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref id) = client_identifier {
            tracing::info!("Client identifier set to: {}", id);
        }
        if client_type == ClientType::Generic {
            client_type = ClientType::from_client_identifier(
                client_identifier.as_deref(),
            );
        }
        *self.client_type.borrow_mut() = client_type;
        tracing::info!("Client type set to: {:?}", client_type);
        let code_nav_enabled = Self::parse_code_nav_capability(&arguments);
        self.code_nav_enabled.set(code_nav_enabled);
        tracing::info!(
            code_nav_enabled,
            client_type = ?client_type,
            event = "code_nav_capability_parsed",
            "code-nav capability initialized from initialize request; \
             index will start lazily on first x.ai/code/* request if eligible"
        );
        let interactive_trust_client = Self::parse_interactive_trust_capability(
            &arguments,
        );
        self.interactive_trust_client.set(interactive_trust_client);
        let client_supports_mcp_apps = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mcpApps"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if client_supports_mcp_apps {
            tracing::info!("Client supports MCP Apps");
        }
        let buffering_settings = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("bufferingSettings"))
            .map(|value| serde_json::from_value::<
                update_chunk_merge::BufferingSettings,
            >(value.clone()))
            .transpose()
            .map_err(|err| {
                tracing::warn!(
                    error = ?err,
                    "Failed to parse buffering settings from init meta"
                );
                err
            })
            .unwrap_or(None);
        tracing::info!(?buffering_settings, "Buffering settings from init");
        *self.buffering_settings.borrow_mut() = buffering_settings;
        if self.initialize_request.set(arguments).is_err() {
            tracing::info!("Initialize called on reconnect (already initialized)");
        }
        let pre = self
            .auth_manager
            .current()
            .map(|a| (
                distill_auth::bearer_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| distill_auth::bearer_suffix(t).to_owned()),
            ));
        {
            let _t = distill_telemetry::instrumentation::timer(
                "startup.acp_initialize.auth_reload",
            );
            let _s = region!("startup.acp_initialize.auth_reload", Parent::Inherit);
            self.auth_manager.force_reload_from_disk();
        }
        let post = self
            .auth_manager
            .current()
            .map(|a| (
                distill_auth::bearer_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| distill_auth::bearer_suffix(t).to_owned()),
            ));
        distill_telemetry::unified_log::info(
            "auth init disk refresh",
            None,
            Some(
                serde_json::json!({
                "pre_key": pre.as_ref().map(|p| &p.0),
                "pre_rt": pre.as_ref().and_then(|p| p.1.as_deref()),
                "post_key": post.as_ref().map(|p| &p.0),
                "post_rt": post.as_ref().and_then(|p| p.1.as_deref()),
                "changed": pre.as_ref().map(|p| &p.0) != post.as_ref().map(|p| &p.0),
            }),
            ),
        );
        distill_telemetry::unified_log::info(
            "auth: initialize() refreshed auth state from disk",
            None,
            Some(
                serde_json::json!({
                "has_current": self.auth_manager.current().is_some(),
                "is_expired": self.auth_manager.is_expired(),
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
            }),
            ),
        );
        if !self.cfg.borrow().grok_com_config.api_key_auth_disabled()
            && auth_method::read_xai_api_key_env().is_err()
            && let Some(api_key) = distill_login::read_api_key(
                &crate::util::distill_home::distill_home(),
            )
        {
            unsafe { std::env::set_var("XAI_API_KEY", &api_key) };
            tracing::info!("auth: loaded API key from auth.json (xai::api_key scope)");
            distill_telemetry::unified_log::info(
                "auth: loaded API key from auth.json (xai::api_key scope)",
                None,
                None,
            );
        }
        let disable_api_key_auth = self
            .cfg
            .borrow()
            .grok_com_config
            .api_key_auth_disabled();
        {
            let cfg = self.cfg.borrow();
            let gc = &cfg.grok_com_config;
            if disable_api_key_auth || gc.force_login_team_uuid.is_some() {
                distill_telemetry::unified_log::info(
                    "auth: enterprise login policy active",
                    None,
                    Some(
                        serde_json::json!({
                        "force_login_team_uuid": gc.force_login_team_uuid.as_ref().map(|t| format!("{t:?}")),
                        "disable_api_key_auth_knob": gc.disable_api_key_auth,
                        "api_key_auth_disabled": disable_api_key_auth,
                    }),
                    ),
                );
            }
        }
        let preferred_method_early = self.cfg.borrow().grok_com_config.preferred_method;
        let xai_api_base_url = self.cfg.borrow().endpoints.xai_api_base_url.clone();
        let has_byok = self
            .models_manager
            .models()
            .values()
            .any(crate::agent::config::ModelEntry::has_own_credentials);
        let first_party_env_ok = if distill_login::should_probe_first_party_env_key(
            disable_api_key_auth,
            has_byok,
            auth_method::has_xai_api_key_env(),
            preferred_method_early.is_some(),
        ) {
            distill_login::first_party_env_key_allows_advertise(
                    &xai_api_base_url,
                    distill_login::DEFAULT_PROBE_TIMEOUT,
                )
                .await
        } else {
            true
        };
        self.auth_manager.set_first_party_env_api_key_ok(first_party_env_ok);
        let has_external_api_key = auth_method::should_advertise_xai_api_key_with_env_ok(
            disable_api_key_auth,
            self.models_manager.models().values(),
            first_party_env_ok,
        );
        let init_token_state = self.auth_manager.cached_token_state();
        let init_has_current = matches!(init_token_state, CachedTokenState::Valid(_));
        let init_is_expired = matches!(init_token_state, CachedTokenState::Expired);
        distill_telemetry::unified_log::info(
            "auth init token state",
            None,
            Some(
                serde_json::json!({
                "has_current": init_has_current,
                "is_expired": init_is_expired,
            }),
            ),
        );
        let mut has_cached_token = init_has_current;
        if !init_has_current && init_is_expired {
            let _t = distill_telemetry::instrumentation::timer(
                "startup.acp_initialize.silent_refresh",
            );
            let _s = region!("startup.acp_initialize.silent_refresh", Parent::Inherit);
            has_cached_token = match self.auth_manager.silent_refresh().await {
                SilentRefresh::Renewed(_) => true,
                SilentRefresh::Failed(remedy) => remedy.is_self_healing(),
            };
        }
        let (
            login_label,
            has_auth_provider,
            has_enterprise_oidc,
            enterprise_oidc_issuer,
        ) = {
            let cfg = self.cfg.borrow();
            let issuer = cfg.grok_com_config.oidc.as_ref().map(|o| o.issuer.clone());
            (
                cfg.grok_com_config.auth_provider_label.clone(),
                cfg.grok_com_config.auth_provider_command.is_some(),
                cfg.grok_com_config.oidc.is_some(),
                issuer,
            )
        };
        if has_enterprise_oidc {
            let issuer = enterprise_oidc_issuer
                .as_deref()
                .expect(
                    "enterprise_oidc_issuer must be Some when has_enterprise_oidc is true",
                );
            tracing::info!(
                issuer = %issuer,
                "auth: advertising enterprise OIDC auth method",
            );
            distill_telemetry::unified_log::info(
                "auth: advertising enterprise OIDC auth method",
                None,
                Some(serde_json::json!({ "issuer": issuer })),
            );
        } else {
            tracing::info!(
                label = ?login_label,
                has_auth_provider,
                "auth: advertising grok.com auth method",
            );
        }
        let preferred_method = preferred_method_early;
        let has_external_api_key = match preferred_method {
            Some(distill_login::PreferredAuthMethod::Oidc) => false,
            _ => has_external_api_key,
        };
        let has_cached_token = match preferred_method {
            Some(distill_login::PreferredAuthMethod::ApiKey) => false,
            _ => has_cached_token,
        };
        let built = {
            let _t = distill_telemetry::instrumentation::timer(
                "startup.acp_initialize.auth_methods",
            );
            let _s = region!("startup.acp_initialize.auth_methods", Parent::Inherit);
            auth_method::build_auth_methods(auth_method::AuthMethodsBuildInputs {
                has_external_api_key,
                has_cached_token,
                has_enterprise_oidc,
                enterprise_oidc_issuer: enterprise_oidc_issuer.as_deref(),
                login_label: login_label.as_deref(),
                has_auth_provider_command: has_auth_provider,
                preferred_method,
            })
        };
        let auth_methods = built.methods;
        distill_telemetry::unified_log::info(
            "auth: initialize() built auth_methods for ACP response",
            None,
            Some(
                serde_json::json!({
                "distill_home": crate::util::distill_home::distill_home().display().to_string(),
                "HOME": std::env::var("HOME").unwrap_or_else(|_| "(unset)".into()),
                "has_external_api_key": has_external_api_key,
                "first_party_env_api_key_ok": first_party_env_ok,
                "disable_api_key_auth": disable_api_key_auth,
                "has_cached_token": has_cached_token,
                "has_enterprise_oidc": has_enterprise_oidc,
                "init_has_current": init_has_current,
                "init_is_expired": init_is_expired,
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
                "methods": auth_methods.iter().map(|m| m.id().0.as_ref()).collect::<Vec<_>>(),
                "default_auth_method_id": built.default_auth_method_id.as_ref().map(|id| id.0.as_ref()),
            }),
            ),
        );
        debug_assert!(
            !has_external_api_key
                || matches!(
                    auth_methods
                        .first()
                        .map(|m| auth_method::AuthMethodKind::from_id(m.id())),
                    Some(auth_method::AuthMethodKind::XaiApiKey)
                ),
            "BYOK invariant violated: xai.api_key MUST be auth_methods.first() \
             when has_external_api_key is true; got {:?}",
            auth_methods.first().map(|m| m.id()),
        );
        let default_auth_method_id_wire: Option<String> = built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.to_string());
        if let Some(default_id) = built.default_auth_method_id {
            distill_telemetry::unified_log::info(
                "auth method selection",
                None,
                Some(
                    serde_json::json!({
                    "default_auth_method_id": default_id.0.as_ref(),
                    "has_external_api_key": has_external_api_key,
                    "has_cached_token": has_cached_token,
                    "methods_first": auth_methods.first().map(|m| m.id().0.as_ref()),
                    "methods_count": auth_methods.len(),
                }),
                ),
            );
            self.set_auth_method(default_id);
        }
        self.sync_process_static_api_key(None);
        let current_working_directory = self.launch_cwd.clone();
        let hostname = gethostname::gethostname();
        let mcp_servers: Vec<crate::extensions::mcp::McpServerEntry> = Vec::new();
        self.spawn_initialize_launch_mcp_setup();
        self.spawn_managed_gateway_tool_catalog_fetch();
        {
            let agent_ref = LocalRef::new(self);
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                agent_ref.get().emit_announcements(AnnouncementsPushMode::SeedNewClient);
            });
        }
        self.spawn_announcements_refresh();
        self.spawn_heap_profile_monitor();
        let init_model_state = {
            let _t = distill_telemetry::instrumentation::timer(
                "startup.acp_initialize.model_state",
            );
            let _s = region!("startup.acp_initialize.model_state", Parent::Inherit);
            if crate::agent::chat_modes::process_chat_mode_enabled() {
                self.chat_modes.model_state().await
            } else {
                self.model_state(None)
            }
        };
        let session_capabilities = acp::SessionCapabilities::new()
            .close(acp::SessionCloseCapabilities::new());
        let session_capabilities = if crate::agent::chat_modes::process_chat_mode_enabled() {
            session_capabilities
        } else {
            session_capabilities
                    .list(acp::SessionListCapabilities::new())
                    .resume(acp::SessionResumeCapabilities::new())
        };
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::V1)
                .agent_capabilities(
                    acp::AgentCapabilities::new()
                        .load_session(true)
                        .meta(
                            serde_json::json!({
                    "x.ai/fs_notify": true,
                    // Advertised so SDKs can warn when a registration depends on hook behavior this agent doesn't honor
                    "x.ai/hooks": {
                        "blockingEvents": crate::extensions::hooks::ADVERTISED_BLOCKING_EVENTS,
                        "decisions": crate::extensions::hooks::ADVERTISED_DECISIONS,
                        "stopSignals": crate::extensions::hooks::ADVERTISED_STOP_SIGNALS,
                    },
                    "x.ai/capabilities": {
                        "toolOverrides": tool_overrides_capability(),
                    },
                })
                                .as_object()
                                .cloned(),
                        )
                        .prompt_capabilities(
                            acp::PromptCapabilities::new().embedded_context(true),
                        )
                        .mcp_capabilities(
                            acp::McpCapabilities::new().http(true).sse(true),
                        )
                        .session_capabilities(session_capabilities),
                )
                .auth_methods(auth_methods)
                .meta({
                    let metadata = crate::util::parse_json_object_env(
                        "GROK_AGENT_METADATA",
                    );
                    serde_json::json!({
                    "grokShell": true,
                    // Re-deriving this precedence client-side has regressed OIDC refresh, so clients consume the agent's choice from here
                    "defaultAuthMethodId": default_auth_method_id_wire,
                    // The agent can drive in-process SDK MCP servers over the ACP reverse channel (`x.ai/mcp/sdk_call`)
                    // The SDK reads this to enable transport="acp"
                    (distill_mcp::wire::MCP_SDK): true,
                    // `session/new` / `session/load` accept per-session plugin roots in `_meta.pluginDirs`
                    // The SDKs gate `GrokOptions.plugins` on this
                    (SESSION_PLUGIN_DIRS_CAPABILITY_KEY): true,
                    "currentWorkingDirectory": current_working_directory.to_string_lossy().to_string(),
                    "agentVersion": distill_version::VERSION,
                    "agentId": agent_id(),
                    "agentInstanceId": agent_instance_id(),
                    "hostname": hostname.to_string_lossy().to_string(),
                    "modelState": init_model_state,
                    "mcpServers": mcp_servers,
                    "mcpApps": client_supports_mcp_apps,
                    "metadata": metadata,
                    "availableCommands": crate::session::slash_commands::builtin_commands(self.command_availability()),
                    "cancelRewind": self
                        .cfg
                        .borrow()
                        .is_feature_enabled(crate::agent::config::Feature::CancelRewind),
                    // Resolved session-recap state (remote settings / config / env; default ON)
                    // The client gates BOTH its automatic away-recap poll and the manual `/recap` on this
                    // A disabled feature produces zero `x.ai/recap` traffic
                    "sessionRecap": self.cfg.borrow().is_session_recap_enabled(),
                    "feedbackTraceOffer": self.feedback_trace_offer(),
                    "voiceMode": self.cfg.borrow().is_voice_mode_enabled(),
                })
                        .as_object()
                        .cloned()
                }),
        )
    }
    async fn authenticate(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        tracing::info!(method = %arguments.method_id.0, "auth: authenticate request");
        distill_telemetry::unified_log::info(
            "auth started",
            None,
            Some(serde_json::json!({"method": arguments.method_id.0.as_ref()})),
        );
        if let Some(preferred) = self.cfg.borrow().grok_com_config.preferred_method {
            let kind = auth_method::AuthMethodKind::from_id(&arguments.method_id);
            let allowed = match preferred {
                distill_login::PreferredAuthMethod::ApiKey => kind.is_api_key(),
                distill_login::PreferredAuthMethod::Oidc => kind.is_session_based(),
            };
            if !allowed {
                let msg = match preferred {
                    distill_login::PreferredAuthMethod::ApiKey => {
                        auth_method::PREFERRED_API_KEY_UNAVAILABLE
                    }
                    distill_login::PreferredAuthMethod::Oidc => {
                        "preferred_method=oidc; API-key auth is not allowed."
                    }
                };
                emit_login_span(
                    false,
                    arguments.method_id.0.as_ref(),
                    None,
                    Some("preferred_method_mismatch"),
                );
                return Err(acp::Error::auth_required().data(msg));
            }
        }
        match arguments.method_id.0.as_ref() {
            auth_method::XAI_API_KEY_METHOD_ID => {
                if self.cfg.borrow().grok_com_config.api_key_auth_disabled() {
                    emit_login_span(false, "api_key", None, Some("disabled_by_admin"));
                    return Err(
                        acp::Error::auth_required()
                            .data("API-key auth is disabled by your administrator."),
                    );
                }
                let mut sampling_config = self.sampling_config.borrow_mut();
                if sampling_config.api_key.is_none() {
                    if let Ok(api_key) = auth_method::read_xai_api_key_env() {
                        sampling_config.api_key = Some(api_key.clone());
                        if let Err(e) = distill_login::store_api_key(
                            &crate::util::distill_home::distill_home(),
                            &api_key,
                        ) {
                            tracing::warn!("failed to persist API key to auth.json: {e}");
                            distill_telemetry::unified_log::warn(
                                "failed to persist API key to auth.json",
                                None,
                                Some(serde_json::json!({ "error" : e.to_string() })),
                            );
                        }
                    } else if !self
                        .models_manager
                        .models()
                        .values()
                        .any(|m| m.has_own_credentials())
                    {
                        emit_login_span(false, "api_key", None, Some("no_credentials"));
                        return Err(
                            acp::Error::auth_required()
                                .data(
                                    "Set XAI_API_KEY or add api_key/env_key to config.toml.",
                                ),
                        );
                    }
                }
                self.set_auth_method(arguments.method_id.clone());
                self.sync_process_static_api_key(None);
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(true, "api_key", None, None);
                log_event(distill_telemetry::events::Login {
                    auth_method: "api_key".to_string(),
                    user_id: None,
                });
                Ok(Default::default())
            }
            auth_method::CACHED_TOKEN_AUTH_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                if auth_meta.force_interactive {
                    return self
                        .authenticate(
                            acp::AuthenticateRequest::new(
                                    acp::AuthMethodId::new(auth_method::OIDC_METHOD_ID),
                                )
                                .meta(arguments.meta),
                        )
                        .await;
                }
                let current_auth = self.auth_manager.current();
                let has_current = current_auth.is_some();
                let is_expired = self.auth_manager.is_expired();
                let is_legacy = current_auth
                    .as_ref()
                    .is_some_and(|a| a.auth_mode == distill_login::AuthMode::WebLogin);
                let check_payload = serde_json::json!({
                    "has_current": has_current,
                    "is_expired": is_expired,
                    "is_legacy": is_legacy,
                });
                distill_telemetry::unified_log::info(
                    "auth cached_token check",
                    None,
                    Some(check_payload),
                );
                let token_state = self.auth_manager.cached_token_state();
                let was_expired = matches!(token_state, CachedTokenState::Expired);
                let resolved = match token_state {
                    CachedTokenState::Valid(auth) => Some(*auth),
                    CachedTokenState::Missing => None,
                    CachedTokenState::Expired => {
                        match self.auth_manager.silent_refresh().await {
                            SilentRefresh::Renewed(auth) => Some(*auth),
                            SilentRefresh::Failed(remedy) if remedy.is_self_healing() => {
                                self.auth_manager.current_or_expired()
                            }
                            SilentRefresh::Failed(_) => None,
                        }
                    }
                };
                let Some(auth) = resolved else {
                    let message = if was_expired {
                        "Session expired, re-authentication required"
                    } else {
                        "No cached auth token found"
                    };
                    tracing::info!(%message, "cached_token missing/expired, falling through");
                    distill_telemetry::unified_log::warn(
                        "auth cached_token fallthrough",
                        None,
                        Some(serde_json::json!({ "reason": message })),
                    );
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                };
                if auth.auth_mode == distill_login::AuthMode::WebLogin {
                    tracing::info!("auth: rejecting legacy WebLogin token");
                    distill_telemetry::unified_log::warn(
                        "auth cached_token legacy rejected",
                        None,
                        Some(
                            serde_json::json!({ "auth_mode": format!("{:?}", auth.auth_mode) }),
                        ),
                    );
                    self.auth_manager.clear_in_memory();
                    if let Err(e) = self
                        .auth_manager
                        .remove_scope(distill_login::LEGACY_AUTH_SCOPE)
                    {
                        tracing::warn!(error = ?e, "auth: failed to remove legacy scope during WebLogin rejection (non-fatal)");
                    }
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                }
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                let auth_for_settings = auth.clone();
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key);
                    tracing::debug!("auth: cached_token handler set api_key (SessionToken)");
                    distill_telemetry::unified_log::debug(
                        "auth: cached_token handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.set_auth_method(arguments.method_id.clone());
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                let uid = self.auth_manager.current().map(|a| a.user_id);
                emit_login_span(true, "cached_token", uid.as_deref(), None);
                log_event(distill_telemetry::events::Login {
                    auth_method: "cached_token".to_string(),
                    user_id: uid,
                });
                self.spawn_post_auth_settings(auth_for_settings);
                Ok(self.auth_response_with_meta())
            }
            auth_method::GROK_COM_METHOD_ID | auth_method::OIDC_METHOD_ID => {
                let grok_ctx = self.auth_manager.grok_com_config();
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                tracing::info!(
                    method = arguments.method_id.0.as_ref(),
                    headless = auth_meta.headless,
                    reauth = auth_meta.reauth,
                    use_oauth = auth_meta.use_oauth,
                    "auth: inline auth flow",
                );
                distill_telemetry::unified_log::info(
                    "auth: inline auth flow",
                    None,
                    Some(
                        serde_json::json!({
                        "method": arguments.method_id.0.as_ref(),
                        "headless": auth_meta.headless,
                        "reauth": auth_meta.reauth,
                        "use_oauth": auth_meta.use_oauth,
                    }),
                    ),
                );
                if auth_meta.reauth {
                    let _ = self.auth_manager.clear();
                }
                let cli_oauth = auth_meta.use_oauth.then_some(true);
                let use_oidc = self.cfg.borrow().resolve_grok_oauth(cli_oauth);
                tracing::debug!(resolved = use_oidc.value, source = ?use_oidc.source, "auth: method resolved");
                distill_telemetry::unified_log::debug(
                    "auth: method resolved",
                    None,
                    Some(
                        serde_json::json!({
                        "use_oidc": use_oidc.value,
                        "source": format!("{:?}", use_oidc.source),
                    }),
                    ),
                );
                let login_override = auth_meta.login_override();
                let config_device_flow = self.cfg.borrow().login_device_flow;
                let mut cancelled = false;
                let client_seq = auth_meta.request_seq;
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self
                        .interactive_auth
                        .begin(
                            Some(
                                distill_login::single_flight::AttemptChannels::new(
                                    code_tx,
                                    url_rx,
                                ),
                            ),
                            client_seq,
                        );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = distill_login::run_auth_flow_with_stderr_bridge(
                            &self.auth_manager,
                            grok_ctx,
                            config_device_flow,
                            distill_login::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            },
                            auth_meta.reauth,
                            auth_meta.force_interactive,
                            login_override,
                        ) => r,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = distill_login::run_auth_flow(
                            &self.auth_manager,
                            grok_ctx,
                            config_device_flow,
                            auth_meta.reauth,
                            None,
                            None,
                            None,
                            login_override,
                        ) => r,
                    }
                };
                let (auth, _did_auth) = auth_result
                    .map_err(|e| {
                        emit_login_span(
                            false,
                            arguments.method_id.0.as_ref(),
                            None,
                            Some(
                                if cancelled {
                                    "login_cancelled"
                                } else {
                                    "login_flow_failed"
                                },
                            ),
                        );
                        let mut err = acp::Error::auth_required();
                        err.message = e.to_string();
                        err
                    })?;
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key.clone());
                    tracing::debug!("auth: grok.com/oidc handler set api_key (SessionToken)");
                    distill_telemetry::unified_log::debug(
                        "auth: grok.com/oidc handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.auth_manager.hot_swap(auth.clone());
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                tokio::task::spawn_local(
                    crate::managed_config::post_login_sync(Some(auth.clone())),
                );
                self.set_auth_method(arguments.method_id.clone());
                self.models_manager.on_auth_changed().await;
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(
                    true,
                    arguments.method_id.0.as_ref(),
                    Some(auth.user_id.as_str()),
                    None,
                );
                log_event(distill_telemetry::events::Login {
                    auth_method: arguments.method_id.0.as_ref().to_string(),
                    user_id: Some(auth.user_id.clone()),
                });
                self.spawn_post_auth_settings(auth);
                Ok(self.auth_response_with_meta())
            }
            _ => {
                Err(
                    acp::Error::invalid_params()
                        .data(
                            format!(
                "unsupported auth method: {}",
                arguments.method_id.0
            ),
                        ),
                )
            }
        }
    }
    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let span = tracing::info_span!("agent.new_session");
        if let Some(meta) = arguments.meta.as_ref() {
            distill_otel::link_span_to_meta(
                &span,
                &serde_json::Value::Object(meta.clone()),
            );
        }
        self.new_session_inner(arguments).instrument(span).await
    }
    async fn load_session(
        &self,
        arguments: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        self.load_session_inner(arguments).await
    }
    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        crate::agent::handlers::session::handle_list_sessions(self, args).await
    }
    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        self.resume_session_inner(args).await
    }
    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        self.close_session_inner(args).await
    }
    #[tracing::instrument(
        name = "agent.prompt",
        skip_all,
        fields(session_id = %arguments.session_id.0, turn_number = tracing::field::Empty)
    )]
    #[allow(unused_mut)]
    async fn prompt(
        &self,
        mut arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        use crate::session::plan_mode::PromptMode;
        if let Some(meta) = arguments.meta.as_ref() {
            distill_otel::link_current_span_to_meta(
                &serde_json::Value::Object(meta.clone()),
            );
        }
        let preamble_span = region!("prompt.preamble", Parent::Inherit);
        tracing::debug!(
            target: "sampling_log",
            session_id = %arguments.session_id.0,
            "Received prompt request"
        );
        distill_telemetry::unified_log::info(
            "prompt received",
            Some(arguments.session_id.0.as_ref()),
            None,
        );
        let handle = self
            .session_handle_waiting_for_load(&arguments.session_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
        if self.models_manager.allowlist_excludes_all() {
            let deny = crate::agent::remote_config::allowlist_excludes_all_message(
                &self.cfg.borrow(),
            );
            self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    &deny,
                )
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        let latched_model = self
            .session_registry
            .unavailable_model(&arguments.session_id);
        if let Some(unavailable_model) = latched_model {
            let models = self.models_manager.models();
            let available = self.models_manager.available();
            let restore_model_id = selectable_catalog_key_for_persisted(
                    &models,
                    &available,
                    &unavailable_model,
                )
                .unwrap_or(unavailable_model.clone());
            if available.contains_key(&restore_model_id) {
                tracing::info!(
                    session_id = %arguments.session_id.0,
                    model_id = %restore_model_id.0,
                    "prompt: previously-unavailable model is back in the catalog; restoring it and unblocking the session"
                );
                distill_telemetry::unified_log::info(
                    "prompt: previously-unavailable model recovered, unblocking session",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!({
                        "model_id": restore_model_id.0.as_ref(),
                    }),
                    ),
                );
                self.session_registry.take_unavailable_model(&arguments.session_id);
                if let Err(e) = crate::agent::handlers::model_switch::apply(
                        self,
                        acp::SetSessionModelRequest::new(
                            arguments.session_id.clone(),
                            restore_model_id.clone(),
                        ),
                        crate::agent::handlers::model_switch::SwitchEffort::Preserve,
                        crate::agent::handlers::model_switch::ConfigNotice::Send,
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        model_id = %restore_model_id.0,
                        error = ?e,
                        "prompt: failed to restore previously-unavailable model; continuing with the session's current model"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %arguments.session_id.0,
                    unavailable_model = %unavailable_model.0,
                    available_count = available.len(),
                    available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
                    "prompt blocked: session model unavailable since load and still missing from the catalog"
                );
                distill_telemetry::unified_log::warn(
                    "prompt blocked: model unavailable",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!({
                        "unavailable_model": unavailable_model.0.as_ref(),
                        "available_count": available.len(),
                    }),
                    ),
                );
                self.send_model_auto_switched(
                        &arguments.session_id,
                        &acp::ModelId::new(String::new()),
                        &acp::ModelId::new(String::new()),
                        "Your previous model is no longer available and could not \
                     be switched to a compatible model. Please start a new session.",
                    )
                    .await;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
        }
        let dispatch_lock = self.dispatch_lock(&arguments.session_id);
        let dispatch_guard = dispatch_lock.lock().await;
        crate::agent::mvp_agent::test_hooks::park_forever_if_blackholed(
                &arguments.session_id,
            )
            .await;
        let meta_prompt_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mode"))
            .and_then(|v| v.as_str())
            .map(PromptMode::from_meta_str);
        let prompt_mode = if let Some(mode) = meta_prompt_mode {
            mode
        } else {
            let (mode_tx, mode_rx) = oneshot::channel();
            let _ = handle
                .cmd_tx
                .send(crate::session::SessionCommand::GetCurrentPromptMode {
                    responds_to: mode_tx,
                });
            mode_rx.await.unwrap_or_default()
        };
        let turn_started_at = chrono::Utc::now().to_rfc3339();
        let prompt_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("promptId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let turn_number = self.allocate_turn_number(&arguments.session_id);
        tracing::Span::current().record("turn_number", turn_number);
        tracing::info!("Setting up prompt tracing");
        let trace_context = self.get_trace_context(&handle.info, turn_number).await;
        let (wait_for_uploads, upload_flush_timeout) = crate::util::config::load_upload_wait_config_sync();
        crate::upload::drain::capture_flush_timeout_pre_exit(upload_flush_timeout);
        let turn_end_uploads = if wait_for_uploads {
            TurnEndUploads::Wait {
                budget: upload_flush_timeout,
            }
        } else {
            TurnEndUploads::Background
        };
        let (model_tx, model_rx) = oneshot::channel();
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::GetCurrentModel {
                responds_to: model_tx,
            });
        let model = model_rx
            .await
            .map(|current| current.id)
            .unwrap_or_else(|_| self.sampling_config.borrow().model.clone());
        let mut parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>> = None;
        let verbatim = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("verbatim"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let send_now = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sendNow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut before_upload_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Some(ctx) = trace_context.clone() {
            let (tx, parsed_prompt_rx) = oneshot::channel::<ParsedPromptInfo>();
            parsed_prompt_tx = Some(tx);
            let auth = self.auth_manager.current();
            let user_id = auth.as_ref().map(|a| a.user_id.clone());
            let team_id = auth.as_ref().and_then(|a| a.team_id.clone());
            let user_email = auth.and_then(|a| a.email);
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|req| req.meta.as_ref());
            let client_source = init_meta
                .and_then(|meta| {
                    meta
                        .get("clientSource")
                        .or_else(|| meta.get("clientType"))
                        .or_else(|| meta.get("clientIdentifier"))
                })
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let client_version = init_meta
                .and_then(|meta| meta.get("clientVersion"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| self.cfg.borrow().client_version.clone());
            let plugin_registry = self.plugin_registry_snapshot();
            let prompt_images: Vec<agent_client_protocol::ImageContent> = arguments
                .prompt
                .iter()
                .filter_map(|block| {
                    if let agent_client_protocol::ContentBlock::Image(img) = block {
                        Some(img.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let mut prompt_metadata = PromptMetadata::new(PromptMetadataParams {
                schema_version: GCS_SCHEMA_VERSION.to_string(),
                session_id: ctx.session_info.id.0.to_string(),
                turn_number: ctx.turn_number,
                request_id: prompt_id.clone(),
                turn_started_at: turn_started_at.clone(),
                user_id,
                user_email,
                team_id,
                client_source,
                client_version,
                model: model.to_owned(),
                reasoning_effort: ctx
                    .session_handle
                    .reasoning_effort
                    .map(|e| e.as_ref().to_string()),
                experiment_id: None,
                host_os: std::env::consts::OS.to_string(),
                host_arch: std::env::consts::ARCH.to_string(),
                prompt_has_image: Some(!prompt_images.is_empty()),
                prompt_was_truncated: Some(false),
                prompt_verbatim: if verbatim { Some(true) } else { None },
                cwd: Some(ctx.session_info.cwd.clone()),
                agent_type: Some(ctx.session_handle.agent_name.clone()),
                shell_version: Some(distill_version::VERSION.to_string()),
                sandbox: local_sandbox_telemetry(),
                ..Default::default()
            });
            prompt_metadata.attempt_id = ctx.attempt_id.clone();
            let (session_copy_tx, session_copy_rx) = oneshot::channel();
            let copy_sent = ctx
                .session_handle
                .cmd_tx
                .send(SessionCommand::CopyFile {
                    respond_to: session_copy_tx,
                })
                .is_ok();
            if !copy_sent {
                tracing::warn!(
                    session_id = %ctx.session_info.id.0,
                    turn_number = ctx.turn_number,
                    "Failed to send CopyFile command, skipping session state upload"
                );
            }
            before_upload_handles
                .push(
                    spawn_upload_task(
                        "prompt_metadata",
                        {
                            let ctx = ctx.clone();
                            async move {
                                if let Ok(Ok(info)) = tokio::time::timeout(
                                        crate::session::commands::PARSED_PROMPT_WAIT,
                                        parsed_prompt_rx,
                                    )
                                    .await && !info.text.is_empty()
                                {
                                    prompt_metadata.prompt_was_truncated = Some(
                                        info.full_text.is_some(),
                                    );
                                    if let Some(full_text) = &info.full_text {
                                        upload_full_prompt_txt(&ctx, full_text, UploadWait::Confirm)
                                            .await;
                                    }
                                }
                                upload_metadata(&ctx, prompt_metadata, UploadWait::Confirm)
                                    .await;
                            }
                        },
                    ),
                );
            before_upload_handles
                .push(
                    spawn_upload_task(
                        "before_uploads",
                        async move {
                            let before_workspace_fut = async {};
                            futures::join!(
                    upload_session_state(&ctx, "before", session_copy_rx, UploadWait::Confirm),
                    before_workspace_fut,
                    upload_images(&ctx, &prompt_images),
                    upload_plugin_state(&ctx, plugin_registry.as_deref()),
                );
                        },
                    ),
                );
        }
        let next_trace_turn = self
            .session_turn_number(&arguments.session_id)
            .unwrap_or_else(|| turn_number.saturating_add(1));
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: Some(prompt_id.clone()),
            });
        let (tx, rx) = oneshot::channel();
        let prompt_client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let prompt_screen_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("screenMode"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let json_schema = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("outputSchema"))
            .cloned();
        if json_schema.as_ref().is_some_and(|schema| !schema.is_object()) {
            return Err(
                acp::Error::invalid_params()
                    .data("outputSchema must be a JSON object describing a JSON Schema"),
            );
        }
        let tool_overrides_update = match arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("toolOverrides"))
        {
            None => None,
            Some(value) => {
                match distill_sampling_types::ToolOverridesUpdate::parse(value) {
                    Ok(update) => Some(update),
                    Err(reason) => {
                        return Err(
                            acp::Error::invalid_params()
                                .data(format!("toolOverrides: {reason}")),
                        );
                    }
                }
            }
        };
        let prompt_blocks = arguments.prompt.clone();
        let artifact_upload_ctx = trace_context
            .as_ref()
            .map(|ctx| ctx.artifact_upload_context());
        let traceparent = distill_otel::current_traceparent();
        let dispatch_result: Result<(), acp::Error> = if send_now {
            handle
                .cmd_tx
                .send(SessionCommand::Prompt {
                    prompt_id: prompt_id.clone(),
                    prompt_blocks,
                    prompt_mode,
                    artifact_upload_ctx,
                    client_identifier: prompt_client_identifier,
                    screen_mode: prompt_screen_mode,
                    verbatim,
                    traceparent,
                    json_schema,
                    send_now: true,
                    admission: None,
                    tool_overrides_update,
                    respond_to: tx,
                    prompt_admitted: None,
                    persist_ack: None,
                    parsed_prompt_tx,
                })
                .map_err(|e| {
                    acp::Error::internal_error()
                        .data(format!("failed to dispatch prompt to session: {e}"))
                })
        } else {
            let envelope = distill_message_delivery_core::DeliveryEnvelope::from_human(
                distill_message_delivery_core::Operation::Queue,
                crate::session::message_delivery::HumanPromptContent {
                    prompt_blocks,
                    prompt_mode,
                    artifact_upload_ctx,
                    client_identifier: prompt_client_identifier,
                    screen_mode: prompt_screen_mode,
                    verbatim,
                    traceparent,
                    json_schema,
                    tool_overrides_update,
                    respond_to: tx,
                    parsed_prompt_tx,
                },
                crate::session::message_delivery::human_delivery_identity(
                    prompt_id.clone(),
                ),
                crate::session::message_delivery::ResidentHumanGrant::new(
                    handle.info.id.0.to_string(),
                ),
            );
            handle
                .message_delivery()
                .send_human(envelope)
                .map_err(|error| match error {
                    crate::session::message_delivery::HumanDeliveryError::ChannelClosed(
                        error,
                    ) => {
                        acp::Error::internal_error()
                            .data(
                                format!("failed to dispatch prompt to session: {error}"),
                            )
                    }
                    crate::session::message_delivery::HumanDeliveryError::Rejected
                    | crate::session::message_delivery::HumanDeliveryError::Unsupported => {
                        unreachable!("resident human Queue is target-bound and supported")
                    }
                })
        };
        dispatch_result?;
        drop(dispatch_guard);
        self.push_roster_activity_delta(
            &arguments.session_id,
            crate::agent::roster::RosterActivity::Working,
        );
        preamble_span.close();
        let await_turn_span = region!("prompt.await_turn", Parent::Inherit);
        let stop_result = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("session failed to respond")
            })?;
        await_turn_span.close();
        let removed_from_queue = matches!(
            &stop_result,
            Ok(ok) if matches!(ok.completion_kind, crate::session::commands::PromptCompletionKind::RemovedFromQueue)
        );
        let capture = trace_context.is_some() && !removed_from_queue
            && stop_result.is_ok();
        let turn_end_capture: Option<TurnEndCapture> = capture
            .then(|| TurnEndCapture::begin(&handle, handle.info.cwd.clone()));
        let finalize_span = region!("prompt.finalize", Parent::Inherit);
        let turn_usage_span = region!("finalize.turn_usage", Parent::Explicit(finalize_span.span()));
        let last_turn_usage_for_meta = handle
            .chat_state_handle
            .get_last_turn_usage()
            .await;
        turn_usage_span.close();
        let applied_tool_overrides = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| ok.tool_overrides.clone());
        if matches!(
            stop_result,
            Ok(crate::session::commands::PromptTurnOk {
                completion_kind: crate::session::commands::PromptCompletionKind::RemovedFromQueue,
                ..
            })
        ) {
            let meta = build_prompt_response_meta(PromptResponseMetaArgs {
                session_id: &arguments.session_id.to_string(),
                prompt_id: &prompt_id,
                total_tokens: 0,
                model_id: &model,
                last_turn_usage: None,
                prompt_usage: None,
                cancellation_category: None,
                cancellation_context: None,
                cancel_trigger: None,
                structured_output: None,
                tool_overrides: applied_tool_overrides.clone(),
                completion_kind: Some(
                    crate::session::commands::REMOVED_FROM_QUEUE_KIND.to_string(),
                ),
            });
            return Ok(
                acp::PromptResponse::new(acp::StopReason::Cancelled)
                    .meta(meta.as_object().cloned()),
            );
        }
        let resolved_model = handle.get_model_metadata().await.resolved_model_id;
        let cancel_trigger: Option<String> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| match &ok.completion_kind {
                crate::session::commands::PromptCompletionKind::Cancelled {
                    context: Some(ctx),
                    ..
                } => ctx.trigger.clone(),
                _ => None,
            });
        let cancellation_category: Option<String> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| ok.completion_kind.cancellation_category_meta());
        let cancellation_context: Option<serde_json::Value> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| ok.completion_kind.cancellation_context_meta());
        {
            let mapped = stop_result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let turn_id = arguments
                .meta
                .as_ref()
                .and_then(|m| m.get("turnId"))
                .and_then(|v| v.as_u64());
            let mut payload = crate::session::turn_completion::prompt_complete_payload(
                &arguments.session_id,
                prompt_id.as_str(),
                &mapped,
            );
            if let Some(obj) = payload.as_object_mut() {
                if let Some(tid) = turn_id {
                    obj.insert("turnId".into(), serde_json::json!(tid));
                }
                if let Some(ref t) = cancel_trigger {
                    obj.insert("cancelTrigger".into(), serde_json::json!(t));
                }
                if let Some(ref c) = cancellation_category {
                    obj.insert("cancellationCategory".into(), serde_json::json!(c));
                }
                if let Some(ref ctx) = cancellation_context {
                    obj.insert("cancellationContext".into(), ctx.clone());
                }
            }
            if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                self.gateway
                    .forward_fire_and_forget(
                        acp::ExtNotification::new(
                            "x.ai/session/prompt_complete",
                            params.into(),
                        ),
                    );
            }
        }
        {
            let end_activity = if handle
                .pending_interactions
                .lock()
                .map(|g| !g.is_empty())
                .unwrap_or(false)
            {
                crate::agent::roster::RosterActivity::NeedsInput
            } else {
                crate::agent::roster::RosterActivity::Idle
            };
            self.push_roster_activity_delta(&arguments.session_id, end_activity);
        }
        let harness_trace_turns = {
            let (tx, rx) = oneshot::channel();
            if handle
                .cmd_tx
                .send(SessionCommand::TakeHarnessTraceTurns {
                    respond_to: tx,
                })
                .is_ok()
            {
                rx.await.ok().unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        if trace_context.is_some() && !harness_trace_turns.is_empty() {
            self.upload_harness_trace_turns(
                    &arguments.session_id,
                    &handle.info,
                    &handle.cmd_tx,
                    &model,
                    harness_trace_turns,
                )
                .await;
        }
        match stop_result {
            Ok(turn_ok) => {
                let crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot,
                    completion_kind,
                    structured_output,
                    usage: prompt_usage,
                    tool_overrides: _,
                } = turn_ok;
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let permission_events = self
                    .collect_permission_events(&arguments.session_id);
                let turn_messages: Option<distill_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        crate::upload::turn::stop_reason_commits_turn(stop_reason),
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason
                            .get_or_insert_with(|| match &completion_kind {
                                crate::session::commands::PromptCompletionKind::Cancelled {
                                    category,
                                    ..
                                } => {
                                    match category {
                                        Some(cat) => format!("cancelled:{cat:?}"),
                                        None => "cancelled".to_string(),
                                    }
                                }
                                _ => "non_completed".to_string(),
                            });
                        cap
                    });
                if let Some(ctx) = trace_context && let Some(cap) = turn_end_capture {
                    let (head, head_branch, registry_claim, session_copy_rx) = cap
                        .finish()
                        .await;
                    let turn_result = TurnResultArgs {
                        request_id: prompt_id.clone(),
                        completed: crate::upload::turn::stop_reason_commits_turn(
                            stop_reason,
                        ),
                        stop_reason: format!("{stop_reason:?}"),
                        total_tokens: Some(total_tokens),
                        error: None,
                        finished_at: chrono::Utc::now().to_rfc3339(),
                        turn_snapshot,
                        prompt_mode: prompt_mode.to_string(),
                        subagents_spawned: subagent_refs,
                    };
                    let registry = self
                        .build_registry_turn_end_args(
                            &arguments.session_id,
                            turn_number,
                            &handle,
                            &arguments.prompt,
                            registry_claim,
                            head,
                            head_branch,
                        );
                    let captures = TraceCaptures {
                        permission_events,
                        session_copy_rx,
                        turn_messages,
                        streaming_partial,
                    };
                    match turn_end_uploads {
                        TurnEndUploads::Wait { budget } => {
                            let deadline = tokio::time::Instant::now() + budget;
                            for handle in std::mem::take(&mut before_upload_handles) {
                                let _ = tokio::time::timeout_at(deadline, handle).await;
                            }
                            let wait = UploadWait::Defer { deadline };
                            let (archive_confirmed_tx, archive_confirmed_rx) = oneshot::channel();
                            spawn_linked_upload_task(
                                "turn_end.registry",
                                &prompt_id,
                                &arguments.session_id.0,
                                run_registry_turn_end(registry, archive_confirmed_rx),
                            );
                            let result = turn_result.into_metadata(resolved_model);
                            upload_turn_result(&ctx, &result, wait)
                                .instrument(
                                    tracing::debug_span!("turn_end.turn_result_upload"),
                                )
                                .await;
                            let confirmed = run_trace_completion(&ctx, captures, wait)
                                .instrument(
                                    tracing::debug_span!("turn_end.trace_completion"),
                                )
                                .await;
                            let _ = archive_confirmed_tx.send(confirmed);
                        }
                        TurnEndUploads::Background => {
                            spawn_linked_upload_task(
                                "turn_end.finalize_detached",
                                &prompt_id,
                                &arguments.session_id.0,
                                run_detached_turn_end(
                                    ctx,
                                    turn_result,
                                    resolved_model,
                                    TurnEndOutcome::Completed {
                                        captures,
                                        registry: Box::new(registry),
                                    },
                                ),
                            );
                        }
                    }
                }
                let last_turn_usage = last_turn_usage_for_meta;
                Ok(
                    acp::PromptResponse::new(stop_reason)
                        .meta(
                            build_prompt_response_meta(PromptResponseMetaArgs {
                                    session_id: &arguments.session_id.to_string(),
                                    prompt_id: &prompt_id,
                                    total_tokens,
                                    model_id: &model,
                                    last_turn_usage: last_turn_usage.as_ref(),
                                    prompt_usage,
                                    cancellation_category,
                                    cancellation_context,
                                    cancel_trigger,
                                    structured_output,
                                    tool_overrides: applied_tool_overrides,
                                    completion_kind: None,
                                })
                                .as_object()
                                .cloned(),
                        ),
                )
            }
            Err(err) => {
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let turn_messages: Option<distill_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let err_kind_str = format!("{:?}", err.code);
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        false,
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason = Some(format!("sampler_error:{err_kind_str}"));
                        cap
                    });
                if let Some(ctx) = trace_context {
                    let turn_result = TurnResultArgs {
                        request_id: prompt_id.clone(),
                        completed: false,
                        stop_reason: crate::sampling::error::stop_reason_for_turn_error(
                                &err,
                            )
                            .to_string(),
                        total_tokens: None,
                        error: Some(format!("{err:?}")),
                        finished_at: chrono::Utc::now().to_rfc3339(),
                        turn_snapshot: None,
                        prompt_mode: prompt_mode.to_string(),
                        subagents_spawned: subagent_refs,
                    };
                    let artifacts = ErrorTurnArtifacts {
                        turn_messages,
                        streaming_partial,
                        upload_unified: matches!(
                            crate::sampling::error::http_status_from_error(&err),
                            Some(401 | 404),
                        ),
                    };
                    match turn_end_uploads {
                        TurnEndUploads::Wait { budget } => {
                            let deadline = tokio::time::Instant::now() + budget;
                            for handle in std::mem::take(&mut before_upload_handles) {
                                let _ = tokio::time::timeout_at(deadline, handle).await;
                            }
                            let result = turn_result.into_metadata(resolved_model);
                            upload_error_turn_artifacts(
                                    &ctx,
                                    &result,
                                    artifacts,
                                    UploadWait::Defer { deadline },
                                )
                                .await;
                        }
                        TurnEndUploads::Background => {
                            spawn_linked_upload_task(
                                "turn_end.finalize_detached_error",
                                &prompt_id,
                                &arguments.session_id.0,
                                run_detached_turn_end(
                                    ctx,
                                    turn_result,
                                    resolved_model,
                                    TurnEndOutcome::Failed(artifacts),
                                ),
                            );
                        }
                    }
                }
                let err = if crate::sampling::error::prompt_usage_from_error(&err)
                    .is_some()
                {
                    err
                } else {
                    let prompt_id = handle
                        .current_prompt_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let usage = if handle
                        .cmd_tx
                        .send(crate::session::commands::SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    };
                    crate::sampling::error::attach_prompt_usage(err, usage)
                };
                Err(err)
            }
        }
    }
    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        tracing::info!("Received cancel request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let cancel_trigger = args
            .meta
            .as_ref()
            .and_then(|m| m.get("cancelTrigger"))
            .and_then(|v| v.as_str())
            .map(crate::session::CancelTrigger::from_client);
        distill_telemetry::unified_log::info(
            "shell.cancel.received",
            Some(args.session_id.0.as_ref()),
            Some(
                serde_json::json!({
                "session_found": handle.is_some(),
                "trigger": cancel_trigger.as_ref().map(crate::session::CancelTrigger::as_str),
            }),
            ),
        );
        if let Some(handle) = handle {
            let cancel_subagents = args
                .meta
                .as_ref()
                .and_then(|m| m.get("cancelSubagents"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let rewind_if_no_output = args
                .meta
                .as_ref()
                .and_then(|m| {
                    m.get("rewindIfNoOutput").or_else(|| m.get("rewindIfPristine"))
                })
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let rewind_prompt_id = args
                .meta
                .as_ref()
                .and_then(|m| m.get("promptId"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let history = if rewind_if_no_output {
                crate::session::CancelHistoryDisposition::RewindIfNoOutput {
                    prompt_id: rewind_prompt_id,
                }
            } else {
                crate::session::CancelHistoryDisposition::Keep
            };
            let dispatch_lock = self.dispatch_lock(&args.session_id);
            let _dispatch_guard = dispatch_lock.lock().await;
            let user_initiated = cancel_trigger
                .as_ref()
                .is_none_or(crate::session::CancelTrigger::is_user_gesture);
            let _ = handle
                .cmd_tx
                .send(
                    SessionCommand::Cancel(crate::session::CancelOptions {
                        cancel_subagents,
                        history,
                        trigger: cancel_trigger,
                        user_initiated,
                        ..Default::default()
                    }),
                );
        }
        Ok(())
    }
    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        tracing::info!("Received set session mode request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let (tx, rx) = oneshot::channel();
        if let Some(handle) = handle {
            let _ = handle
                .cmd_tx
                .send(SessionCommand::SessionMode {
                    session_mode: args.mode_id,
                    responds_to: tx,
                });
        }
        let _ = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("response to set session failed")
            })?;
        Ok(acp::SetSessionModeResponse::new())
    }
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        self.set_model_gated(args).await
    }
    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        crate::agent::handlers::config_option::apply(self, args).await
    }
    #[tracing::instrument(
        name = "agent.ext_method",
        skip_all,
        fields(method = %args.method)
    )]
    async fn ext_method(
        &self,
        args: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        let request_meta = serde_json::from_str::<serde_json::Value>(args.params.get())
            .ok()
            .and_then(|v| v.get("_meta").cloned());
        if let Some(meta) = &request_meta {
            distill_otel::link_current_span_to_meta(meta);
        }
        tracing::info!("Received extension method call: method={}", args.method);
        #[allow(unused_mut)]
        let mut backend_no_bridge_err: Option<acp::Error> = None;
        let method = args.method.clone();
        let result = match method.as_ref() {
            "x.ai/getApiKey" | "x.ai/setApiKey" => {
                crate::extensions::auth::handle(self, &args).await
            }
            "x.ai/session/info" | "x.ai/session/close" | "x.ai/session/list"
            | "x.ai/sessions/list" => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            "x.ai/workspaces/list" => {
                crate::agent::handlers::workspaces::handle(self, &args).await
            }
            "x.ai/models/list" => {
                crate::agent::handlers::models::handle(self, &args).await
            }
            "x.ai/session/updates" => {
                crate::extensions::session_updates::handle(&args, &self.gateway).await
            }
            "x.ai/session/state" => {
                crate::extensions::session_state::handle_state(&args).await
            }
            "x.ai/session/import" => {
                crate::extensions::session_state::handle_import(&args).await
            }
            "x.ai/session/load_history" => {
                crate::extensions::chat_conversation_history::handle(self, &args).await
            }
            "x.ai/session/search" => {
                crate::extensions::session_search::handle(self, &args).await
            }
            "x.ai/session/resolve_local_for_worktree_resume"
            | "x.ai/session/rehydrate" => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            #[cfg(feature = "local-workspace")]
            "x.ai/session/add_local_workspace" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "x.ai/session/rename" | "x.ai/session/delete"
            | "x.ai/session/update_mcp_servers" | "x.ai/session/fork"
            | "x.ai/plugins/reload" | "x.ai/commands/list" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            m if InternalMethod::from_name(m).is_some() => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "x.ai/session/repair" => crate::extensions::repair::handle(self, &args).await,
            "x.ai/session/usage" => crate::extensions::usage::handle(self, &args).await,
            crate::extensions::memory::MEMORY_FLUSH_METHOD
            | crate::extensions::memory::MEMORY_DREAM_METHOD
            | "x.ai/memory/rewrite"
            | crate::extensions::memory::MEMORY_LIST_METHOD
            | crate::extensions::memory::MEMORY_TOGGLE_METHOD
            | crate::extensions::memory::MEMORY_FORGET_METHOD => {
                crate::extensions::memory::handle(self, &args).await
            }
            "x.ai/skills/refresh-baseline" => {
                self.refresh_skill_baseline_for_all_sessions();
                crate::extensions::to_ext_response(
                    Ok(serde_json::json!({"ok": true})),
                )
            }
            "x.ai/interject" => crate::extensions::interject::handle(self, &args).await,
            "x.ai/feedback" | "x.ai/feedback/dismiss" | "x.ai/feedback/drafts/list"
            | "x.ai/feedback/drafts/get" | "x.ai/feedback/drafts/delete"
            | "x.ai/feedback/drafts/update" | "x.ai/feedback/upload-trace"
            | "x.ai/btw" => crate::extensions::feedback::handle(self, &args).await,
            "x.ai/recap" => crate::extensions::recap::handle(self, &args).await,
            "x.ai/cloud/terminate" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_id = params
                    .get("sandbox_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing sandbox_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .terminate_session(
                        sandbox_id,
                        &crate::remote::SandboxTerminateRequest {
                            environment_id: None,
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to terminate sandbox: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/cloud/env/list" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .list_environments(
                        &crate::remote::SandboxListEnvironmentsRequest::default(),
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to list environments: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environments": resp.environments,
                }),
                )
            }
            "x.ai/cloud/env/create" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .create_environment(
                        &crate::remote::SandboxCreateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            workspace_directory: Some("/workspace".to_string()),
                            internet_enabled: Some(true),
                            domain_allowlist_preset: Some("common".to_string()),
                            allowed_http_methods: Some("all".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to create environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/update" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .update_environment(
                        environment_id,
                        &crate::remote::SandboxUpdateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to update environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/delete" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .delete_environment(environment_id)
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to delete environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/billing" => crate::extensions::billing::handle(self, &args).await,
            "x.ai/auto-topup-rule" => {
                crate::extensions::billing::handle(self, &args).await
            }
            "x.ai/share_session" => crate::extensions::share::handle(self, &args).await,
            "x.ai/privacy/setCodingDataRetention" => {
                crate::extensions::privacy::handle(self, &args).await
            }
            "x.ai/consent/record" => {
                crate::extensions::consent::handle(self, &args).await
            }
            "x.ai/rollout/survey" => {
                crate::extensions::rollout::handle(self, &args).await
            }
            "x.ai/prompt_history" => {
                crate::extensions::prompt_history::handle(self, &args).await
            }
            "x.ai/suggest" => crate::extensions::suggest::handle(self, &args).await,
            "x.ai/suggestPrompt" => crate::extensions::suggest::handle(self, &args).await,
            s if s.starts_with("x.ai/auth/") => {
                crate::extensions::auth::handle(self, &args).await
            }
            s if s.starts_with("x.ai/session_summaries/") => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            s if s.starts_with("x.ai/git/worktree/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/git/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::git::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/compact_conversation") => {
                crate::extensions::memory::handle(self, &args).await
            }
            s if s.starts_with("x.ai/plugins/") => {
                crate::extensions::plugins::handle(self, &args).await
            }
            s if s.starts_with("x.ai/marketplace/") => {
                crate::extensions::marketplace::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hooks/") => {
                crate::extensions::hooks::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hunk-tracker/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::hunk_tracker::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/pr/") => {
                crate::extensions::pr::handle(self, &args).await
            }
            s if s.starts_with(crate::extensions::mcp::mcp_methods::PREFIX) => {
                crate::extensions::mcp::handle(self, &args).await
            }
            s if s.starts_with("x.ai/task/") => {
                crate::extensions::task::handle(self, &args).await
            }
            s if s.starts_with("x.ai/scheduler/") => {
                crate::extensions::task::handle_scheduler(self, &args).await
            }
            s if s.starts_with("x.ai/subagent/") => {
                crate::extensions::task::handle_subagent(self, &args).await
            }
            s if s.starts_with("x.ai/terminal/") => {
                crate::extensions::terminal::handle(self, &args).await
            }
            s if crate::extensions::fs::is_fs_method(s) => {
                crate::extensions::fs::handle(self, &args).await
            }
            s if s.starts_with("x.ai/search/") => {
                crate::extensions::search::handle(self, &args).await
            }
            s if s.starts_with("x.ai/bundle/") => {
                crate::extensions::bundle::handle(self, &args).await
            }
            s if s.starts_with("x.ai/code/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::code_nav::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/skills/") || s == "x.ai/workflows/list" => {
                let compat = self.cfg.borrow().compat_resolved;
                let cwd = crate::extensions::skills::request_cwd(&args);
                let registry = self.plugin_registry_for_cwd(cwd.as_deref()).await;
                crate::extensions::skills::handle(
                        self,
                        &args,
                        registry.as_deref(),
                        compat,
                    )
                    .await
            }
            s if s.starts_with("x.ai/review") => {
                crate::extensions::feedback::handle(self, &args).await
            }
            s if s.starts_with("x.ai/debug/") => {
                crate::extensions::debug::handle(self, &args).await
            }
            s if s.starts_with("x.ai/rewind") => {
                crate::extensions::rewind::handle(self, &args).await
            }
            other => {
                Err(
                    acp::Error::method_not_found()
                        .data(format!("unknown ACP extension method: {other}")),
                )
            }
        };
        if let Some(err) = backend_no_bridge_err
            && matches!(&result, Err(e) if e.code == acp::Error::method_not_found().code)
        {
            return Err(err);
        }
        result
    }
    async fn ext_notification(
        &self,
        args: acp::ExtNotification,
    ) -> Result<(), acp::Error> {
        tracing::info!("Received extension notification: method={}", args.method);
        if args.method.as_ref() == "x.ai/yolo_mode_changed"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let sender_id = params.get("clientIdentifier").and_then(|v| v.as_str());
            let permission_mode = params
                .get("permission_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let yolo_signal = params.get("yolo_mode").and_then(|v| v.as_bool());
            if let Some(yolo_mode) = yolo_signal {
                let mut updated_sessions = 0;
                self.session_registry
                    .for_each_resident_mut(|_, handle| {
                        updated_sessions
                            += apply_yolo_mode_to_matching_sessions(
                                std::iter::once(handle),
                                sender_id,
                                yolo_mode,
                            );
                    });
                tracing::info!(
                    yolo_mode,
                    sender = ?sender_id,
                    target_sessions = updated_sessions,
                    total_sessions = self.resident_count(),
                    "Setting YOLO mode for matching sessions"
                );
            }
            let auto_mode_explicit = params.get("auto_mode").and_then(|v| v.as_bool());
            let want_auto = auto_mode_explicit == Some(true)
                || permission_mode == "auto";
            let clear_auto = auto_mode_explicit == Some(false)
                || (matches!(permission_mode, "always-approve" | "ask" | "default")
                    && !want_auto);
            let enable_auto = want_auto && yolo_signal != Some(true);
            if enable_auto || clear_auto {
                let enabled = enable_auto;
                let matches_sender = |h: &crate::session::SessionHandle| -> bool {
                    sender_id.is_none()
                        || h.origin_client.as_ref().map(|c| c.product.as_str())
                            == sender_id
                };
                let total_sessions = self.resident_count();
                let mut updated = 0;
                self.session_registry
                    .for_each_resident_mut(|_, h| {
                        if !matches_sender(h) {
                            return;
                        }
                        if h
                            .cmd_tx
                            .send(crate::session::SessionCommand::SetAutoMode {
                                enabled,
                            })
                            .is_ok()
                        {
                            if enabled {
                                h.yolo_mode = false;
                            }
                            updated += 1;
                        }
                    });
                tracing::info!(
                    auto_mode = enabled,
                    sender = ?sender_id,
                    target_sessions = updated,
                    total_sessions,
                    "Setting auto permission mode for matching sessions"
                );
            }
        }
        if args.method.as_ref() == "x.ai/permissions/reset" {
            let mut updated = 0;
            self.session_registry
                .for_each_resident(|_, h| {
                    if h
                        .cmd_tx
                        .send(crate::session::SessionCommand::ResetPermissionState)
                        .is_ok()
                    {
                        updated += 1;
                    }
                });
            tracing::info!(
                target_sessions = updated,
                total_sessions = self.resident_count(),
                "Permission state reset for matching sessions"
            );
        }
        if args.method.as_ref() == InternalMethod::EvictSessions.name() {
            self.handle_evict_sessions(&args.params).await;
        }
        if args.method.as_ref() == "x.ai/toggle_plan_mode"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let handle = self.resident_handle(&acp::SessionId::new(session_id_str));
            if let Some(handle) = handle {
                let is_engaged = handle.plan_mode.lock().state()
                    != crate::session::plan_mode::PlanModeState::Inactive;
                let next_mode_id = acp::SessionModeId::new(
                    if is_engaged { "default" } else { "plan" },
                );
                let (tx, rx) = oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SessionMode {
                        session_mode: next_mode_id.clone(),
                        responds_to: tx,
                    });
                if rx.await.is_err() {
                    tracing::warn!(
                        session_id = %session_id_str,
                        mode_id = %next_mode_id.0,
                        "toggle_plan_mode: session mode update failed"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %session_id_str,
                    "toggle_plan_mode: session not found"
                );
            }
        }
        if args.method.as_ref().starts_with("x.ai/queue/")
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let owner = params
                .get("owner")
                .or_else(|| params.get("clientIdentifier"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(cmd) = crate::agent::ext_parsers::parse_queue_edit_command(
                args.method.as_ref(),
                &params,
                owner,
            ) {
                let session_id_str = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(handle) = self
                    .resident_handle(&acp::SessionId::new(session_id_str))
                {
                    if handle.cmd_tx.send(cmd).is_err() {
                        tracing::warn!(
                            session_id = %session_id_str,
                            method = %args.method,
                            "queue edit: failed to forward SessionCommand (session actor gone)"
                        );
                    }
                } else {
                    tracing::warn!(
                        session_id = %session_id_str,
                        method = %args.method,
                        "queue edit: session not found"
                    );
                }
            }
        }
        if args.method.as_ref() == "x.ai/terminal/pty/input"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            crate::extensions::terminal::handle_pty_input(&params).await;
        }
        if args.method.as_ref() == "_x.ai/session/update" {
            if let Ok(notification) = serde_json::from_str::<
                SessionNotification,
            >(args.params.get()) {
                tracing::info!(
                    "Storing xAI session notification: session_id={}",
                    notification.session_id.0
                );
                if let Some(handle) = self.resident_handle(&notification.session_id) {
                    let _ = handle
                        .cmd_tx
                        .send(crate::session::SessionCommand::XaiSessionNotification {
                            notification,
                        });
                } else {
                    tracing::warn!(
                        "Received xAI session notification for unknown session: {}",
                        notification.session_id.0
                    );
                }
            } else {
                tracing::warn!("Failed to parse xAI session notification params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/non_git_decision" {
            #[derive(serde::Deserialize)]
            struct NonGitDecisionParams {
                decision: String,
                session_id: String,
                #[serde(default)]
                client_version: Option<String>,
            }
            if let Ok(params) = serde_json::from_str::<
                NonGitDecisionParams,
            >(args.params.get()) {
                tracing::info!(
                    decision = %params.decision,
                    session_id = %params.session_id,
                    client_version = ?params.client_version,
                    "non_git_decision",
                );
                distill_telemetry::session_ctx::log_event(distill_telemetry::events::NonGitDecisionEvent {
                    decision: params.decision,
                    session_id: params.session_id,
                    client_version: params.client_version,
                });
            } else {
                tracing::warn!("Failed to parse non_git_decision telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_followup" {
            #[derive(serde::Deserialize)]
            struct MultiAgentFollowupParams {
                preferred_agent_label: char,
                preferred_agent_session_id: Option<String>,
                preferred_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                other_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentFollowupParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent followup telemetry: preferred_agent={}",
                    params.preferred_agent_label
                );
                let total_agents = 1 + params.other_agents.len();
                distill_telemetry::session_ctx::log_event(distill_telemetry::events::MultiAgentFollowup {
                    preferred_agent_label: params.preferred_agent_label.to_string(),
                    preferred_agent_session_id: params.preferred_agent_session_id,
                    preferred_agent_model_id: params.preferred_agent_model_id,
                    other_agents: params
                        .other_agents
                        .into_iter()
                        .map(|(l, s, m)| distill_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent followup telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_apply" {
            #[derive(serde::Deserialize)]
            struct MultiAgentApplyParams {
                applied_agent_label: char,
                applied_agent_session_id: Option<String>,
                applied_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentApplyParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent apply telemetry: applied_agent={}",
                    params.applied_agent_label
                );
                let total_agents = 1 + params.discarded_agents.len();
                distill_telemetry::session_ctx::log_event(distill_telemetry::events::MultiAgentApply {
                    applied_agent_label: params.applied_agent_label.to_string(),
                    applied_agent_session_id: params.applied_agent_session_id,
                    applied_agent_model_id: params.applied_agent_model_id,
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| distill_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent apply telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_discard" {
            #[derive(serde::Deserialize)]
            struct MultiAgentDiscardParams {
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentDiscardParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent discard telemetry: {} agents discarded",
                    params.discarded_agents.len()
                );
                let total = params.discarded_agents.len();
                distill_telemetry::session_ctx::log_event(distill_telemetry::events::MultiAgentDiscard {
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| distill_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents_discarded: total,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent discard telemetry params");
            }
        }
        if args.method.as_ref() == distill_telemetry::unified_log::LOG_METHOD
            && let Ok(params) = serde_json::from_str::<
                distill_telemetry::unified_log::LogNotificationParams,
            >(args.params.get())
        {
            distill_telemetry::unified_log::ingest_client_entries(
                params.src,
                &params.entries,
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tool_overrides_capability_tests {
    use super::tool_overrides_capability;
    #[test]
    fn capability_wire_shape_is_pinned() {
        assert_eq!(
            tool_overrides_capability(),
            serde_json::json!({
                "x_keyword_search": true,
                "x_semantic_search": true,
                "x_user_search": false,
                "x_thread_fetch": false,
            }),
        );
    }
}
