//! Session bring-up for `acp_session`: `spawn_session_actor` and the per-session OS thread (`SessionThread` / `spawn_session_on_thread`).
//! Also holds the MCP auto-restart wiring (`SessionRestartActions`).
//!
//! The chat+local `own` supervisor lives on `MvpAgent` (started in `session/new`), not `SessionActor`; crash-restart reaches it through the bridge slot seeded here.
#![allow(clippy::items_after_test_module)]
use super::*;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use tracing::Instrument;
use xai_grok_telemetry::region;
use xai_grok_telemetry::region::Parent as SpanParent;
use xai_grok_telemetry::subagent_spawn::phase_region_under;
static SESSIONS_ACTIVE: xai_grok_telemetry::activity::ActivityGauge =
    xai_grok_telemetry::activity::ActivityGauge::residency(
        xai_grok_telemetry::activity::SESSIONS_ACTIVE_KEY,
    );
/// Drop catch-all `--allow` rules (the `--yolo` substitute, see `resolution::is_catchall_allow`) when `policy_block` is set.
fn drop_cli_catchall_allows(
    rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    policy_block: Option<&'static str>,
) -> (
    Vec<xai_grok_workspace::permission::types::PermissionRule>,
    Vec<xai_grok_workspace::permission::types::PermissionRule>,
) {
    if policy_block.is_none() {
        return (rules, Vec::new());
    }
    let mut kept = Vec::with_capacity(rules.len());
    let mut dropped = Vec::new();
    for rule in rules {
        if xai_grok_workspace::permission::resolution::is_catchall_allow(&rule) {
            dropped.push(rule);
        } else {
            kept.push(rule);
        }
    }
    (kept, dropped)
}
/// Construction acquires fds (epoll/kqueue, waker) and fails with `EMFILE`/`EAGAIN` under resource pressure.
/// Only caps the blocking pool; pre-warming is reserved for process-lifetime runtimes (`xai_tty_utils::runtime`).
pub(crate) fn build_session_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    xai_tty_utils::runtime::apply_blocking_pool(builder.enable_all()).build()
}
fn configured_memory_retrieval_mode(
    config: Option<&crate::config::MemoryConfig>,
) -> xai_grok_telemetry::events::MemoryRetrievalMode {
    use xai_grok_telemetry::events::MemoryRetrievalMode::*;
    match config.filter(|config| config.enabled && config.mode.is_legacy()) {
        None => Disabled,
        Some(config)
            if config
                .embedding
                .model
                .as_ref()
                .is_some_and(|model| !model.is_empty()) =>
        {
            Hybrid
        }
        Some(_) => FtsOnly,
    }
}
/// One 429 layer per role, never stacked, never zero: an active subagent pacer disables the sampler retry, and a
/// disabled pacer falls back to it (a true rollback). A per-model sampler threshold overrides this policy elsewhere.
fn subagent_sampler_rate_limit_threshold(is_subagent: bool, pacer_max_attempts: u32) -> u32 {
    if is_subagent && pacer_max_attempts > 0 {
        xai_grok_sampler::RATE_LIMIT_RETRY_DISABLED
    } else {
        xai_grok_sampler::RATE_LIMIT_RETRY_THRESHOLD
    }
}
/// Prefer the model-resolved config; the separate argument remains for legacy spawn call sites.
fn session_max_retries_source(
    sampling_config_max_retries: Option<u32>,
    spawn_max_retries: Option<u32>,
) -> Option<u32> {
    sampling_config_max_retries.or(spawn_max_retries)
}
#[cfg(all(test, unix))]
#[path = "spawn_runtime_containment_tests.rs"]
mod runtime_containment_tests;
#[cfg(test)]
mod cli_catchall_drop_tests {
    use super::{configured_memory_retrieval_mode, drop_cli_catchall_allows};
    use xai_grok_workspace::permission::resolution::YoloPinReason;
    const PIN: &str = YoloPinReason::DisableBypassPermissionsMode.message();
    use xai_grok_workspace::permission::rules::parse_permission_rule;
    use xai_grok_workspace::permission::types::{PermissionRule, RuleAction, ToolFilter};
    fn allow(rule: &str) -> PermissionRule {
        parse_permission_rule(rule, RuleAction::Allow).expect("rule parses")
    }
    #[test]
    fn disabled_memory_config_has_disabled_retrieval_mode() {
        assert_eq!(
            configured_memory_retrieval_mode(Some(&Default::default())),
            xai_grok_telemetry::events::MemoryRetrievalMode::Disabled
        );
    }
    #[test]
    fn v2_memory_config_has_disabled_retrieval_mode() {
        let config = crate::config::MemoryConfig {
            enabled: true,
            mode: crate::config::MemoryMode::V2,
            ..Default::default()
        };
        assert_eq!(
            configured_memory_retrieval_mode(Some(&config)),
            xai_grok_telemetry::events::MemoryRetrievalMode::Disabled
        );
    }
    /// Under the pin, CLI catch-all `--allow` rules (`*`, `**`) are dropped while a scoped rule (`Bash(touch *)`) survives.
    #[test]
    fn pin_drops_cli_catchalls_keeps_scoped() {
        let rules = vec![allow("*"), allow("Bash(touch *)"), allow("**")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, Some(PIN));
        assert_eq!(kept.len(), 1, "only the scoped Bash rule survives");
        let [kept_rule] = kept.as_slice() else {
            panic!("only the scoped Bash rule survives: {kept:?}");
        };
        assert_eq!(kept_rule.tool, ToolFilter::Bash);
        assert_eq!(dropped.len(), 2, "both catch-alls are dropped");
    }
    #[test]
    fn no_pin_keeps_everything() {
        let rules = vec![allow("*"), allow("Bash(touch *)"), allow("**")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, None);
        assert_eq!(kept.len(), 3);
        assert!(dropped.is_empty());
    }
    #[test]
    fn pin_drops_cli_bare_and_prefix_bash_keeps_scoped() {
        let rules = vec![allow("Bash"), allow("Bash(?*)"), allow("Bash(git *)")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, Some(PIN));
        assert_eq!(kept.len(), 1, "only the scoped Bash rule survives");
        let [kept_rule] = kept.as_slice() else {
            panic!("only the scoped Bash rule survives: {kept:?}");
        };
        assert_eq!(kept_rule.pattern.as_deref(), Some("git *"));
        assert_eq!(dropped.len(), 2, "bare Bash and ?* are dropped");
        let rules = vec![allow("Bash"), allow("Bash(?*)"), allow("Bash(git *)")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, None);
        assert_eq!(kept.len(), 3);
        assert!(dropped.is_empty());
    }
}
#[cfg(test)]
mod session_retry_policy_tests {
    use super::{session_max_retries_source, subagent_sampler_rate_limit_threshold};
    use xai_grok_sampler::{RATE_LIMIT_RETRY_DISABLED, RATE_LIMIT_RETRY_THRESHOLD};
    #[test]
    fn main_session_always_keeps_sampler_retry() {
        assert_eq!(
            subagent_sampler_rate_limit_threshold(false, 0),
            RATE_LIMIT_RETRY_THRESHOLD
        );
        assert_eq!(
            subagent_sampler_rate_limit_threshold(false, 8),
            RATE_LIMIT_RETRY_THRESHOLD
        );
    }
    #[test]
    fn subagent_with_disabled_pacer_falls_back_to_sampler_retry() {
        assert_eq!(
            subagent_sampler_rate_limit_threshold(true, 0),
            RATE_LIMIT_RETRY_THRESHOLD
        );
    }
    #[test]
    fn subagent_with_active_pacer_disables_sampler_retry() {
        assert_eq!(
            subagent_sampler_rate_limit_threshold(true, 1),
            RATE_LIMIT_RETRY_DISABLED
        );
        assert_eq!(
            subagent_sampler_rate_limit_threshold(true, 8),
            RATE_LIMIT_RETRY_DISABLED
        );
    }
    #[test]
    fn model_retry_budget_wins_over_legacy_spawn_budget() {
        assert_eq!(session_max_retries_source(Some(6), Some(3)), Some(6));
        assert_eq!(session_max_retries_source(Some(6), None), Some(6));
        assert_eq!(session_max_retries_source(None, Some(3)), Some(3));
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_session_actor(
    session_info: SessionInfo,
    gateway: GatewaySender,
    sampling_config: SamplingConfig,
    credentials: xai_chat_state::Credentials,
    auth_method_id: crate::agent::auth_method::SharedAuthMethodId,
    auth_manager: Option<Arc<AuthManager>>,
    attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback>,
    mut tool_context: ToolContext,
    mcp_servers: Vec<acp::McpServer>,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    mcp_meta_config_map: McpMetaConfigMap,
    parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    acp_mcp_servers: Vec<crate::session::mcp_servers::AcpServerEntry>,
    support_permission: bool,
    telemetry_enabled: bool,
    auto_update: Option<bool>,
    persistence: PersistenceHandle,
    mut conversation: Vec<ConversationItem>,
    rewind_points_path: Option<std::path::PathBuf>,
    initial_last_compaction: Option<usize>,
    initial_prompt_texts: Vec<String>,
    fs_notify_config: Option<ClientFsConfig>,
    initial_total_tokens: u64,
    mut startup_hints: StartupHints,
    client_type: ClientType,
    auto_compact_threshold_percent: u8,
    system_prompt_label: String,
    compaction_mode: xai_chat_state::CompactionMode,
    compaction_verbatim_input: bool,
    compaction_tool_choice: crate::util::config::CompactionToolChoice,
    two_pass_enabled: bool,
    buffering_settings: Option<BufferingSettings>,
    origin_client: Option<crate::http::OriginClientInfo>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    code_nav_enabled: bool,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
    client_caps: crate::session::notifications::SessionClientCaps,
    feedback_proxy_url: Option<String>,
    feedback_user_token: Option<String>,
    feedback_alpha_test_key: Option<String>,
    deployment_key: Option<String>,
    client_terminal_capable: bool,
    client_fs_capable: bool,
    gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    agent_definition: AgentDefinition,
    session_default_agent_profile: Option<String>,
    skills_config: SkillsConfig,
    preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    compat: CompatConfig,
    paths_config: xai_grok_agent::prompt::paths::PathsConfig,
    incremental_bash_output: bool,
    persisted_signals: Option<crate::session::signals::SessionSignals>,
    persisted_plan_mode: Option<crate::session::plan_mode::PlanModeSnapshot>,
    persisted_goal_mode: Option<crate::session::goal_tracker::GoalOrchestration>,
    persisted_workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
    persisted_announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    memory_config: Option<crate::config::MemoryConfig>,
    loc_tracking_enabled: bool,
    feedback_flags: crate::session::feedback_manager::FeedbackFlags,
    managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    managed_mcp_proxy_base_url: String,
    session_model_id: acp::ModelId,
    session_yolo_mode: bool,
    session_auto_mode: bool,
    session_client_identifier: Option<String>,
    inference_idle_timeout_secs: u64,
    max_retries: Option<u32>,
    subagent_rate_limit_max_attempts: u32,
    web_search_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
    web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig,
    image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig,
    video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig,
    app_builder_deployer_config: xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig,
    write_file_enabled: bool,
    active_agent_messages_enabled: bool,
    agent_message_sender: Option<
        xai_grok_tools::implementations::grok_build::task::types::AgentMessageSender,
    >,
    goal_enabled: bool,
    background_workflows_enabled: bool,
    subagents_enabled: bool,
    subagents_max_depth: u32,
    workflow_max_concurrent_agents: usize,
    media_gen_batch_limits: xai_grok_tools::media_gen_limits::MediaGenBatchLimits,
    ask_user_question_enabled: bool,
    client_hooks: crate::extensions::hooks::ClientHooks,
    prompt_display_cwd: Option<String>,
    subagent_toggle: std::collections::HashMap<String, bool>,
    persona_summaries: Vec<String>,
    prompt_audience: xai_grok_agent::prompt::context::PromptAudience,
    role_instructions: Option<String>,
    persona_instructions: Option<String>,
    disable_web_search: bool,
    backend_tools_enabled: bool,
    respect_gitignore: bool,
    path_not_found_hints: bool,
    tool_params_json: crate::session::agent_rebuild::ResolvedToolParamsJson,
    mut prefetch: crate::session::session_create_prefetch::SessionCreatePrefetch,
    plugin_registry_handle: Option<xai_grok_agent::plugins::SharedPluginRegistryHandle>,
    models_manager: crate::agent::remote_config::ModelsManager,
    inherited_permission_handle: Option<xai_grok_workspace::permission::PermissionHandle>,
    api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    image_description_model: String,
    hook_registry_override: Option<std::sync::Arc<xai_grok_hooks::discovery::HookRegistry>>,
    workspace_ops: xai_grok_workspace::WorkspaceOps,
    cli_permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    todo_gate: bool,
    remote_settings: Option<crate::util::config::RemoteSettings>,
    laziness_debug_log: Option<std::path::PathBuf>,
    parent_terminal_backend: Option<
        std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
    >,
    parent_scheduler_handle: Option<
        xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle,
    >,
    max_turns: Option<usize>,
    forked_tool_override: Option<Vec<ToolSpec>>,
    is_chat_kind: bool,
    spawn_ctx: Option<xai_grok_telemetry::subagent_spawn::SpawnPhaseContext>,
    sampling_gate: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<(SessionInitResult, tokio::sync::oneshot::Receiver<()>), xai_grok_agent::AgentBuildError>
{
    if max_turns == Some(0) {
        return Err(xai_grok_agent::AgentBuildError::InvalidConfig(
            "max_turns must be greater than 0".to_string(),
        ));
    }
    let wf_sid: String = session_info.id.0.to_string();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tracing::info!(
        "Session '{}' created with {} MCP servers",
        session_info.id.0,
        mcp_servers.len()
    );
    let _ = support_permission;
    let owns_permission_manager = inherited_permission_handle.is_none();
    let permission_setup_timer =
        crate::instrumentation_timer!("session.spawn_actor.permission_setup");
    let (permissions, permission_events_rx, deny_read_globs) = async {
        use xai_grok_workspace::permission::resolution as permission_resolution;
        if let Some(handle) = inherited_permission_handle {
            let (_dummy_tx, dummy_rx) = mpsc::unbounded_channel::<PermissionEvent>();
            let deny_read_globs = handle.deny_read_globs();
            (handle, dummy_rx, deny_read_globs)
        } else {
            let web_fetch_allowed_domains = match &web_fetch_config {
                WebFetchConfig::Enabled { params } => params.allowed_domains(),
                WebFetchConfig::Disabled => vec![],
            };
            let project_trusted = if prefetch.scan().is_gathered() {
                prefetch
                    .resolve_trust(tool_context.cwd.as_path(), remote_settings.as_ref())
            } else {
                crate::agent::folder_trust::project_scope_allowed(
                        tool_context.cwd.as_path(),
                    )
            };
            let yolo_lock = permission_resolution::yolo_policy_lock();
            let yolo_pin = yolo_lock.as_ref().map(|lock| lock.reason.message());
            let mut permission_config = permission_resolution::resolve_permission_config_with_fallback_pinned(
                    tool_context.cwd.as_path(),
                    project_trusted,
                    yolo_lock.as_ref(),
                )
                .instrument(tracing::info_span!("spawn.permission_config_load"))
                .await;
            let (cli_permission_rules, dropped_catchalls) = drop_cli_catchall_allows(
                cli_permission_rules,
                yolo_pin,
            );
            if let Some(reason) = yolo_pin && !dropped_catchalls.is_empty() {
                tracing::warn!(
                    reason,
                    dropped = dropped_catchalls.len(),
                    "CLI --allow catch-all ignored: always-approve disabled by managed policy"
                );
                if startup_hints.non_interactive {
                    eprintln!("grok: --allow catch-all ignored: {reason}");
                }
            }
            if !cli_permission_rules.is_empty() {
                match &mut permission_config {
                    Some(config) => {
                        let mut merged = cli_permission_rules;
                        merged.append(&mut config.rules);
                        config.rules = merged;
                    }
                    None => {
                        permission_config = Some(
                            xai_grok_workspace::permission::types::PermissionConfig::new(
                                cli_permission_rules,
                            ),
                        );
                    }
                }
            }
            permission_resolution::apply_permission_mode_hint(
                &mut permission_config,
                startup_hints.permission_mode.as_deref(),
                yolo_pin,
            );
            let deny_read_globs = permission_config
                .as_ref()
                .map(permission_resolution::deny_read_globs_from_config)
                .unwrap_or_default();
            let hub_permission = if xai_grok_workspace::permission::hitl_permission_live_enabled() {
                let server = match workspace_ops.workspace_handle() {
                    Some(handle) => handle.hub_server_blocking().await,
                    None => None,
                };
                let transport = server
                    .and_then(|server| xai_grok_workspace::permission::ToolServerPermissionTransport::from_session_id(
                        server,
                        session_info.id.0.as_ref(),
                    ))
                    .map(|t| {
                        std::sync::Arc::new(t)
                            as std::sync::Arc<
                                dyn xai_grok_workspace::permission::PermissionHookTransport,
                            >
                    });
                if transport.is_none() {
                    tracing::debug!(
                        session_id = %session_info.id.0,
                        "hitl permission live enabled but no remote transport available; using local prompt"
                    );
                }
                transport
            } else {
                None
            };
            let (permissions, permission_events_rx) = xai_grok_workspace::permission::spawn_permission_manager_with_pin(
                session_info.id.clone(),
                gateway.clone(),
                tool_context.cwd.clone(),
                client_type,
                permission_config,
                deny_read_globs.clone(),
                web_fetch_allowed_domains,
                session_yolo_mode,
                session_client_identifier.clone(),
                crate::util::config::remember_tool_approvals_from_disk(),
                yolo_pin,
                hub_permission,
            );
            if crate::util::config::auto_mode_session_active(
                crate::util::config::auto_permission_mode_enabled_from_disk(),
                session_auto_mode,
                session_yolo_mode,
            ) {
                permissions.set_auto_mode(true);
                refresh_classifier_transcript(&permissions, &conversation);
            }
            (permissions, permission_events_rx, deny_read_globs)
        }
    }
        .instrument(tracing::info_span!("spawn.permission_setup"))
        .await;
    drop(permission_setup_timer);
    let history_scan_span = tracing::info_span!(
        "spawn.history_scan",
        history_items = conversation.len() as i64,
        history_turns = tracing::field::Empty,
    )
    .entered();
    let initial_prompt_index = conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::User(_)))
        .count();
    history_scan_span.record("history_turns", initial_prompt_index as i64);
    let initial_conversation_len = conversation.len();
    let title_session_dir = crate::session::persistence::session_dir(&session_info);
    let title_refresh_watermark =
        crate::session::helpers::session_summary::load_title_refresh_watermark(&title_session_dir);
    let title_refresh_turns_at_spawn =
        crate::session::helpers::session_recap::main_turn_count(&conversation);
    let initial_last_recap_main_turn = crate::session::helpers::session_recap::load_recap_watermark(
        &crate::session::persistence::session_dir(&session_info),
    );
    let initial_user_count = initial_prompt_index.saturating_sub(1) as u32;
    let initial_assistant_count = conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::Assistant(_)))
        .count() as u32;
    let (initial_tool_call_count, initial_tools_used, initial_models_used) =
        if persisted_signals.is_none() {
            let mut tool_call_count: u32 = 0;
            let mut tools_used = std::collections::HashSet::new();
            let mut models_used = std::collections::HashSet::new();
            for item in &conversation {
                if let ConversationItem::Assistant(assistant) = item {
                    tool_call_count += assistant.tool_calls.len() as u32;
                    for tc in &assistant.tool_calls {
                        tools_used.insert(tc.name.clone());
                    }
                    if let Some(model_id) = &assistant.model_id {
                        models_used.insert(model_id.clone());
                    }
                }
            }
            (
                tool_call_count,
                tools_used.into_iter().collect::<Vec<_>>(),
                models_used.into_iter().collect::<Vec<_>>(),
            )
        } else {
            (0, Vec::new(), Vec::new())
        };
    drop(history_scan_span);
    let primary_model_id = sampling_config.model.clone();
    let web_search_config_span = tracing::info_span!("spawn.web_search_config_load").entered();
    let web_search_domains = if disable_web_search {
        None
    } else {
        crate::util::config::resolve_web_search_domains_from_disk()
    };
    let web_search_config = if disable_web_search {
        xai_grok_tools::implementations::WebSearchConfig::Disabled
    } else if let Some(cfg) = web_search_sampling_config {
        if let Some(api_key) = cfg.api_key {
            xai_grok_tools::implementations::WebSearchConfig::Enabled {
                api_key,
                base_url: cfg.base_url,
                model: cfg.model,
                extra_headers: cfg.extra_headers,
                alpha_test_key: credentials.alpha_test_key.clone(),
                allowed_domains: web_search_domains
                    .as_ref()
                    .and_then(|o| o.allowed_domains.clone()),
                excluded_domains: web_search_domains
                    .as_ref()
                    .and_then(|o| o.excluded_domains.clone()),
            }
        } else {
            tracing::warn!("web_search disabled: resolved config has no API key");
            xai_grok_tools::implementations::WebSearchConfig::Disabled
        }
    } else {
        tracing::warn!("web_search disabled: configured model could not be resolved");
        xai_grok_tools::implementations::WebSearchConfig::Disabled
    };
    drop(web_search_config_span);
    let embed_base_url = sampling_config.base_url.clone();
    let embed_api_key = sampling_config.api_key.clone();
    let session_pruning_config: crate::config::PruningConfig = memory_config.as_ref().map_or_else(
        || crate::config::PruningConfig {
            enabled: false,
            ..Default::default()
        },
        |mc| mc.pruning.clone(),
    );
    let context_window_override = std::env::var("GROK_DEBUG_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(std::num::NonZeroU64::new);
    let baseline_context_window = std::num::NonZeroU64::new(sampling_config.context_window)
        .unwrap_or_else(|| {
            std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
        });
    if let Some(cw) = context_window_override {
        tracing::warn!(
            override_context_window = cw.get(),
            original_context_window = baseline_context_window.get(),
            "GROK_DEBUG_CONTEXT_WINDOW override active"
        );
    }
    let resolved_max_retries = xai_grok_sampler::resolve_max_retries(session_max_retries_source(
        sampling_config.max_retries,
        max_retries,
    ));
    let chat_state_span = tracing::info_span!("spawn.chat_state_init");
    let chat_state_guard = chat_state_span.enter();
    let chat_state_sampling_config = xai_grok_sampling_types::SamplingConfig {
        base_url: sampling_config.base_url.clone(),
        mtls_cert_dir: sampling_config.mtls_cert_dir.clone(),
        model: sampling_config.model.clone(),
        max_completion_tokens: sampling_config.max_completion_tokens,
        temperature: sampling_config.temperature,
        top_p: sampling_config.top_p,
        max_retries: Some(resolved_max_retries),
        rate_limit_retry_threshold: sampling_config.rate_limit_retry_threshold,
        api_backend: sampling_config.api_backend.clone(),
        extra_headers: sampling_config.extra_headers.clone(),
        conversation_group_id: sampling_config.conversation_group_id.clone(),
        query_params: sampling_config.query_params.clone(),
        env_http_headers: sampling_config.env_http_headers.clone(),
        context_window: context_window_override.unwrap_or(baseline_context_window),
        reasoning_effort: sampling_config.reasoning_effort,
        reasoning_summary: sampling_config.reasoning_summary,
        stream_tool_calls: Some(sampling_config.stream_tool_calls),
    };
    let actor_pruning_config = xai_chat_state::PruningConfig {
        enabled: session_pruning_config.enabled,
        keep_last_n_turns: session_pruning_config.keep_last_n_turns,
        soft_trim_threshold: session_pruning_config.soft_trim_threshold,
        soft_trim_head: session_pruning_config.soft_trim_head,
        soft_trim_tail: session_pruning_config.soft_trim_tail,
        hard_clear_age_turns: session_pruning_config.hard_clear_age_turns,
    };
    let (chat_state_event_tx, chat_state_event_rx) = mpsc::unbounded_channel();
    let chat_state_handle = xai_chat_state::ChatStateActor::spawn_with_pruning(
        conversation.clone(),
        chat_state_sampling_config,
        actor_pruning_config,
        Box::new(super::chat_persistence::ChannelChatPersistence::new(
            persistence.tx.clone(),
        )),
        chat_state_event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    drop(chat_state_guard);
    async {
        if (!initial_prompt_texts.is_empty()
            || initial_total_tokens > 0
            || initial_last_compaction.is_some())
            && let Some(mut snap) = chat_state_handle.snapshot().await
        {
            snap.prompt_index = initial_prompt_texts.len();
            snap.prompt_texts = initial_prompt_texts;
            if initial_total_tokens > 0 {
                snap.total_tokens = initial_total_tokens;
            }
            snap.last_compaction_prompt_index = initial_last_compaction;
            chat_state_handle.restore_snapshot(snap);
        }
    }
    .instrument(chat_state_span)
    .await;
    chat_state_handle.update_credentials(credentials);
    let state = TokioMutex::new(State {
        running_task: None,
        finalization_gate: Default::default(),
        message_delivery: Default::default(),
        pending_inputs: VecDeque::new(),
        edit_holds: HashMap::new(),
        pending_notifications: Vec::new(),
        notifications_suppressed: false,
        rewindable: false,
        front_message_committed: false,
        hook_block_hold: Default::default(),
        nudges_used_this_session: 0,
    });
    let mcp_strategy = startup_hints.resolve_mcp_strategy();
    let file_state_tracker = Arc::new(match rewind_points_path {
        Some(path) => FileStateTracker::with_lazy_source(path),
        None => FileStateTracker::new(),
    });
    let file_state_handle = FileStateHandle::new(file_state_tracker.clone());
    let task_completion_reservations =
        xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    let task_wake_suppressed =
        xai_grok_tools::reminders::task_completion::TaskWakeSuppressed::default();
    tool_context.task_completion_reservations = Some(task_completion_reservations.clone());
    tool_context.task_wake_suppressed = Some(task_wake_suppressed.clone());
    let synthetic_trace_tx_shared: std::sync::Arc<
        std::sync::Mutex<
            Option<
                tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>,
            >,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    *synthetic_trace_tx_shared.lock().unwrap() = tool_context.synthetic_trace_tx.clone();
    tool_context.synthetic_trace_tx_shared = Some(synthetic_trace_tx_shared.clone());
    let mut tool_context = tool_context.with_file_state_handle(file_state_handle);
    let index_root_for_session =
        xai_grok_workspace::session::git::find_git_root_from_path(tool_context.cwd.as_path())
            .unwrap_or_else(|_| tool_context.cwd.to_path_buf());
    let chat_state_handle_for_handle = chat_state_handle.clone();
    let hunk_tracker_handle_for_bridge = tool_context.hunk_tracker_handle.clone();
    let hunk_tracker_handle = tool_context.hunk_tracker_handle.clone();
    let prompt_index_for_bridge = tool_context.prompt_index.clone();
    let plan_mode = {
        let session_dir = crate::session::persistence::session_dir(&session_info);
        let tracker = if let Some(snapshot) = persisted_plan_mode {
            crate::session::plan_mode::PlanModeTracker::from_snapshot(session_dir, snapshot)
        } else {
            crate::session::plan_mode::PlanModeTracker::new(session_dir)
        };
        Arc::new(parking_lot::Mutex::new(tracker))
    };
    let goal_was_restored = persisted_goal_mode.is_some();
    let goal_tracker = {
        let session_dir = crate::session::persistence::session_dir(&session_info);
        let tracker = if let Some(snapshot) = persisted_goal_mode {
            crate::session::goal_tracker::GoalTracker::from_snapshot(session_dir, snapshot)
        } else {
            crate::session::goal_tracker::GoalTracker::new(session_dir)
        };
        Arc::new(parking_lot::Mutex::new(tracker))
    };
    let restored_prompt_mode = plan_mode.lock().session_prompt_mode();
    let current_prompt_mode = Arc::new(parking_lot::Mutex::new(restored_prompt_mode));
    let turn_prompt_mode = Arc::new(parking_lot::Mutex::new(restored_prompt_mode));
    let task_output_tool_name = Arc::new(std::sync::OnceLock::new());
    let read_tool_name = Arc::new(std::sync::OnceLock::new());
    let queue_exit_reminder_on_approved_exit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let emit_local_background_tasks = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let background_tasks_snapshot_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools_notification_handle = crate::tools::notification_bridge::spawn_notification_bridge(
        crate::tools::notification_bridge::NotificationBridgeConfig {
            gateway: gateway.clone(),
            session_id: session_info.id.clone(),
            hunk_tracker_handle: hunk_tracker_handle_for_bridge.clone(),
            file_state_tracker: file_state_tracker.clone(),
            prompt_index: prompt_index_for_bridge,
            cwd: tool_context.cwd.as_path().to_path_buf(),
            gateway_enabled: gateway_enabled.clone(),
            persistence: persistence.clone(),
            incremental_bash_output,
            plan_mode: plan_mode.clone(),
            current_prompt_mode: current_prompt_mode.clone(),
            turn_prompt_mode: turn_prompt_mode.clone(),
            session_cmd_tx: cmd_tx.clone(),
            task_completion_reservations: task_completion_reservations.clone(),
            task_wake_suppressed: task_wake_suppressed.clone(),
            synthetic_trace_tx: synthetic_trace_tx_shared.clone(),
            task_output_tool_name: task_output_tool_name.clone(),
            read_tool_name: read_tool_name.clone(),
            auto_wake_enabled: tool_context.auto_wake_enabled,
            queue_exit_reminder_on_approved_exit: queue_exit_reminder_on_approved_exit.clone(),
            goal_loop_active: tool_context.goal_loop_active_gate.clone(),
            background_tasks_snapshot_pending: background_tasks_snapshot_pending.clone(),
            emit_local_background_tasks: emit_local_background_tasks.clone(),
        },
    );
    let cursor_harness = false;
    let terminal_backend_span = tracing::info_span!("spawn.terminal_backend");
    let terminal_backend_guard = terminal_backend_span.enter();
    let terminal_backend_kind = select_terminal_backend_kind(
        startup_hints.is_subagent,
        parent_terminal_backend.is_some(),
        client_terminal_capable,
        tool_context.gateway.is_some(),
        cursor_harness,
    );
    let effective_cfg = matches!(
        terminal_backend_kind,
        TerminalBackendKind::LocalPersistent | TerminalBackendKind::LocalNonPersistent
    )
    .then(crate::config::load_effective_config)
    .and_then(Result::ok);
    let resolve_search_shadows = || {
        let requirements = crate::config::load_merged_requirements();
        let (find_bfs, grep_ugrep) = crate::util::config::resolve_search_tools_enabled(
            requirements.as_ref(),
            effective_cfg.as_ref(),
            None,
        );
        xai_grok_tools::computer::local::SearchShadowConfig {
            find_bfs,
            grep_ugrep,
        }
    };
    let resolve_policy = || crate::util::config::resolve_shell_env_policy(effective_cfg.as_ref());
    let terminal_backend: std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend> =
        match terminal_backend_kind {
            TerminalBackendKind::ReuseParent => parent_terminal_backend
                .expect("ReuseParent is only selected when a parent backend is present"),
            TerminalBackendKind::AcpClient => {
                std::sync::Arc::new(crate::terminal::AcpTerminalAdapter::new(
                    tool_context.gateway.clone().unwrap(),
                    tool_context.session_id.clone().unwrap(),
                ))
                    as std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>
            }
            TerminalBackendKind::LocalPersistent => {
                std::sync::Arc::new(LocalTerminalBackend::new_local_with_persistent_shell(
                    resolve_search_shadows(),
                    resolve_policy(),
                    tool_context.process_scope.clone(),
                ))
            }
            TerminalBackendKind::LocalNonPersistent => {
                let login_shell_capture = crate::util::config::resolve_login_shell_capture(
                    remote_settings.as_ref().and_then(|r| r.login_shell_capture),
                );
                std::sync::Arc::new(LocalTerminalBackend::new_local_with_login_shell_capture(
                    resolve_search_shadows(),
                    login_shell_capture,
                    resolve_policy(),
                    tool_context.process_scope.clone(),
                ))
            }
        };
    drop(terminal_backend_guard);
    if matches!(
        terminal_backend_kind,
        TerminalBackendKind::LocalPersistent | TerminalBackendKind::LocalNonPersistent
    ) {
        terminal_backend
            .warm_shell(tool_context.cwd.as_path())
            .instrument(terminal_backend_span)
            .await;
    } else {
        drop(terminal_backend_span);
    }
    let fs_backend: std::sync::Arc<dyn xai_grok_tools::computer::types::AsyncFileSystem> =
        if client_fs_capable && tool_context.gateway.is_some() {
            std::sync::Arc::new(xai_grok_workspace::file_system::AcpFsAdapter::new(
                tool_context.gateway.clone().unwrap(),
                tool_context.session_id.clone().unwrap(),
            ))
        } else {
            std::sync::Arc::new(xai_grok_tools::computer::local::LocalFs)
        };
    let bridge_state_path =
        crate::session::persistence::session_dir(&session_info).join("tool_state.json");
    let initial_agent_name = agent_definition.name.clone();
    let initial_agent_type = Some(initial_agent_name.clone());
    let compaction_policy = xai_grok_agent::CompactionPolicy {
        auto_compact_threshold_percent: auto_compact_threshold_percent as u32,
        compact_model: None,
        memory_flush_enabled: memory_config
            .as_ref()
            .is_some_and(|mc| mc.mode.is_legacy() && mc.flush.enabled),
        wall_clock_budget_secs: crate::util::config::resolve_compaction_wall_clock_budget_secs(
            remote_settings
                .as_ref()
                .and_then(|r| r.compaction_wall_clock_budget_secs),
        ),
        two_pass_enabled,
    };
    let reminder_policy = resolve_reminder_policy(remote_settings.as_ref(), todo_gate);
    let (user_question_tx, user_question_rx) = tokio::sync::mpsc::unbounded_channel::<
        xai_grok_tools::implementations::grok_build::ask_user_question::types::UserQuestionRequest,
    >();
    let attribution_callback_for_spec = auth_manager.as_ref().map(|am| {
        xai_grok_login::attribution::ShellAttribution::new_tool_callback(
            am.clone(),
            Some(session_info.id.0.to_string()),
        )
    });
    let memory_init_span = tracing::info_span!(
        "spawn.memory_init",
        memory_chunks = tracing::field::Empty,
        memory_files = tracing::field::Empty,
    )
    .entered();
    let mut configured_memory_storage = memory_config.as_ref().map(|mc| {
        if mc.mode.is_legacy()
            && mc.flat_memory_root
            && let Some(ref root) = mc.root_dir_override
        {
            return crate::session::memory::MemoryStorage::new_flat(
                tool_context.cwd.as_path(),
                root,
            );
        }
        crate::session::memory::MemoryStorage::new_for_mode(
            tool_context.cwd.as_path(),
            mc.root_dir_override.as_deref(),
            mc.mode,
        )
    });
    let mut memory_storage_for_session = None;
    match select_memory_storage(configured_memory_storage.as_ref(), memory_config.as_ref()) {
        MemoryStorageSelection::Enabled => {
            memory_storage_for_session = configured_memory_storage.clone();
        }
        MemoryStorageSelection::DisabledByConfig => {}
        MemoryStorageSelection::UnavailableForEphemeralWorkspace => {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                cwd = %tool_context.cwd.as_path().display(),
                "MEMORY_INIT: memory-v2 is disabled for ephemeral (temp-dir) workspaces"
            );
            configured_memory_storage = None;
        }
    }
    let memory_initial_injection_config = memory_config
        .as_ref()
        .filter(|mc| mc.mode.is_legacy())
        .map_or_else(
            || crate::config::MemoryInitialInjectionConfig {
                enabled: false,
                ..Default::default()
            },
            |mc| mc.initial_injection.clone(),
        );
    let mut memory_backend_params_for_session: Option<crate::session::memory::MemoryBackendParams> =
        None;
    let mut memory_search_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>> = None;
    let memory_v2_legacy_carryover = memory_config
        .as_ref()
        .is_some_and(|mc| mc.root_dir_override.is_none() && mc.v2.can_run_maintenance());
    let mut memory_v2_access = None;
    if let Some(storage) = memory_storage_for_session.clone() {
        if storage.mode().is_v2() {
            match memory_control::initialize_v2_memory(&storage, memory_v2_legacy_carryover).await {
                Ok(access) => memory_v2_access = Some(access),
                Err(failure) => {
                    tracing::warn!(
                        target: xai_grok_telemetry::memory_log::TARGET,
                        stage = failure.stage,
                        error = %failure.error,
                        "MEMORY_INIT: memory-v2 {} failed; memory is disabled for this session. \
                         Restart to retry, or start with `--no-memory` / `GROK_MEMORY=0` to skip memory.",
                        failure.stage,
                    );
                    memory_storage_for_session = None;
                    configured_memory_storage = None;
                }
            }
        } else if let Err(error) =
            crate::session::memory_state::initialize_memory_storage(storage.clone()).await
        {
            tracing::warn!(
                target: xai_grok_telemetry::memory_log::TARGET,
                error = %error,
                mode = ?storage.mode(),
                "MEMORY_INIT: storage initialization failed"
            );
        }
    }
    let memory_backend_for_spec: Option<
        std::sync::Arc<dyn xai_grok_tools::types::memory_backend::MemoryBackend>,
    > = if let Some(storage) = memory_storage_for_session
        .as_ref()
        .filter(|storage| storage.mode().is_legacy())
    {
        {
            let gc_storage = storage.clone();
            let gc_max_age = memory_config.as_ref().map_or(30, |mc| mc.gc.max_age_days);
            let gc_span = region!(debug, "memory.gc", SpanParent::Root);
            tokio::task::spawn_blocking(move || {
                let _gc_span = gc_span;
                match gc_storage.gc(gc_max_age) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            removed,
                            "MEMORY_GC: cleaned orphaned workspace directories"
                        );
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            error = %e,
                            "MEMORY_GC: failed"
                        );
                    }
                    _ => {}
                }
            });
        }
        let watcher_config = memory_config
            .as_ref()
            .map(|mc| &mc.watcher)
            .cloned()
            .unwrap_or_default();
        let watcher = if watcher_config.enabled {
            crate::session::memory::watcher::MemoryFileWatcher::start(storage.global_dir())
                .map(std::sync::Arc::new)
        } else {
            None
        };
        let embed_credentials = xai_grok_login::credential_provider::embedding_session_credentials(
            &embed_base_url,
            auth_manager.as_ref(),
            api_key_provider.clone(),
        );
        let params = crate::session::memory::MemoryBackendParams {
            session_id: session_info.id.to_string(),
            embed_config: memory_config.as_ref().map(|mc| mc.embedding.clone()),
            embed_base_url: embed_base_url.clone(),
            embed_api_key: embed_api_key.clone(),
            search_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.search.clone()),
            watcher,
            stale_claim_secs: watcher_config.stale_claim_secs,
            search_source: crate::session::memory::MemorySearchSource::Tool,
            observation_sink: std::sync::Arc::new(
                crate::session::memory_observation::TelemetryMemoryObservationSink {
                    session_id: session_info.id.to_string(),
                },
            ),
            embedding_credentials: embed_credentials,
        };
        let backend = crate::session::memory::MemoryBackendImpl::from_session_params(
            storage.clone(),
            &params,
        );
        memory_search_counter = Some(backend.search_counter.clone());
        let watcher_started = params.watcher.is_some();
        let backend: std::sync::Arc<dyn xai_grok_tools::types::memory_backend::MemoryBackend> =
            std::sync::Arc::new(backend);
        memory_backend_params_for_session = Some(params);
        if watcher_config.enabled && !watcher_started {
            tracing::warn!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_INIT: watcher was configured but failed to start \
                 (directory may not exist or OS watcher unavailable)"
            );
        }
        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            workspace = %storage.workspace_dir().display(),
            global = %storage.global_dir().display(),
            watcher_config_enabled = watcher_config.enabled,
            watcher_started,
            "MEMORY_INIT: storage + backend created"
        );
        let mc = memory_config.as_ref();
        let total_chunks = storage.total_chunk_count();
        let total_files = storage.list_memory_files().map_or(0, |f| f.len());
        memory_init_span.record("memory_chunks", total_chunks as i64);
        memory_init_span.record("memory_files", total_files as i64);
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::memory_telemetry::MemorySessionInit {
                session_id: session_info.id.to_string(),
                memory_enabled: true,
                memory_mode: xai_grok_telemetry::memory_telemetry::MemoryMode::Legacy,
                watcher_config_enabled: watcher_config.enabled,
                watcher_started,
                temporal_decay_enabled: mc.is_none_or(|c| c.search.temporal_decay.enabled),
                mmr_enabled: mc.is_some_and(|c| c.search.mmr.enabled),
                mmr_lambda: mc.map_or(0.7, |c| c.search.mmr.lambda),
                half_life_days: mc.map_or(30.0, |c| c.search.temporal_decay.half_life_days),
                embedding_dimensions: mc.map_or(1024, |c| c.embedding.dimensions),
                total_chunks,
                total_files,
                has_global_memory_md: storage.global_memory_file().exists(),
                has_workspace_memory_md: storage.workspace_memory_file().exists(),
            },
        );
        Some(backend)
    } else {
        if let Some(storage) = memory_storage_for_session.as_ref() {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                workspace = %storage.workspace_dir().display(),
                global = %storage.global_dir().display(),
                "MEMORY_INIT: isolated v2 storage created"
            );
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::memory_telemetry::MemorySessionInit {
                    session_id: session_info.id.to_string(),
                    memory_enabled: true,
                    memory_mode: xai_grok_telemetry::memory_telemetry::MemoryMode::V2,
                    total_chunks: storage.total_chunk_count(),
                    total_files: storage.list_memory_files().map_or(0, |files| files.len()),
                    has_global_memory_md: storage.global_memory_file().exists(),
                    has_workspace_memory_md: storage.workspace_memory_file().exists(),
                    ..Default::default()
                },
            );
            let controls = memory_config
                .as_ref()
                .map_or_else(crate::config::MemoryV2Config::default, |config| config.v2);
            let rollout = match controls.rollout {
                crate::config::MemoryV2Rollout::Off => {
                    xai_grok_telemetry::memory_telemetry::MemoryV2Rollout::Off
                }
                crate::config::MemoryV2Rollout::RecordOnly => {
                    xai_grok_telemetry::memory_telemetry::MemoryV2Rollout::RecordOnly
                }
                crate::config::MemoryV2Rollout::Shadow => {
                    xai_grok_telemetry::memory_telemetry::MemoryV2Rollout::Shadow
                }
                crate::config::MemoryV2Rollout::Active => {
                    xai_grok_telemetry::memory_telemetry::MemoryV2Rollout::Active
                }
            };
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::memory_telemetry::MemoryV2ControlsPinned {
                    rollout,
                    capture_enabled: controls.can_capture(),
                    automatic_dream_enabled: controls.can_run_automatic_dream(),
                    manual_dream_enabled: controls.can_run_manual_dream(),
                    file_writes_enabled: controls.file_writes_enabled,
                },
            );
        } else {
            tracing::debug!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_INIT: memory disabled, no storage created"
            );
        }
        None
    };
    drop(memory_init_span);
    let context_window_tokens = context_window_override
        .map(|c| c.get())
        .unwrap_or(sampling_config.context_window);
    let managed_gateway_tool_client = auth_manager.as_ref().map(|am| {
        xai_grok_tools::types::resources::ManagedGatewayToolClient(Arc::new(
            ShellManagedGatewayToolClient {
                proxy_base_url: managed_mcp_proxy_base_url.clone(),
                auth_manager: am.clone(),
            },
        ))
    });
    let mcp_state = {
        let mut state = McpState::new_with_meta(mcp_servers.clone(), mcp_meta_config_map);
        if let Some(ref pool) = parent_mcp_pool {
            state.import_shared_clients(pool);
            tracing::info!(
                session_id = %session_info.id.0,
                shared_clients = state.shared_clients.len(),
                "Imported shared MCP clients from parent pool"
            );
        }
        if !acp_mcp_servers.is_empty() {
            let invoker = std::sync::Arc::new(crate::session::acp_mcp::GatewayAcpInvoker::new(
                gateway.clone(),
            ));
            let acp_server_count = acp_mcp_servers.len();
            state.set_acp_servers(acp_mcp_servers, invoker);
            tracing::info!(
                session_id = %session_info.id.0,
                acp_mcp_servers = acp_server_count,
                "Registered in-process SDK MCP servers (x.ai/mcp/sdk_call)"
            );
        }
        Arc::new(TokioMutex::new(state))
    };
    let plugin_registry = prefetch
        .join_plugin_registry()
        .instrument(tracing::info_span!("spawn.plugin_registry_wait"))
        .await;
    let rebuild_spec = std::sync::Arc::new(crate::session::agent_rebuild::AgentRebuildSpec {
        working_directory: tool_context.cwd.as_path().to_path_buf(),
        terminal_backend: terminal_backend.clone(),
        fs_backend: fs_backend.clone(),
        tools_notification_handle: tools_notification_handle.clone(),
        bridge_state_path: bridge_state_path.clone(),
        session_env: tool_context.session_env.clone(),
        models_manager: models_manager.clone(),
        compaction_policy,
        reminder_policy,
        memory_enabled: memory_config
            .as_ref()
            .is_some_and(|mc| mc.enabled && mc.mode.is_legacy()),
        memory_global_path: memory_storage_for_session.as_ref().map(|storage| {
            if storage.mode().is_v2() {
                storage.global_dir().to_string_lossy().into_owned()
            } else {
                storage.global_memory_file().to_string_lossy().into_owned()
            }
        }),
        memory_workspace_path: memory_storage_for_session.as_ref().map(|storage| {
            if storage.mode().is_v2() {
                storage.workspace_dir().to_string_lossy().into_owned()
            } else {
                storage
                    .workspace_memory_file()
                    .to_string_lossy()
                    .into_owned()
            }
        }),
        memory_backend: memory_backend_for_spec,
        memory_v2_access: crate::session::agent_rebuild::MemoryV2AccessSlot::new(memory_v2_access),
        memory_v2_exposed: memory_v2_exposed(memory_config.as_ref()),
        web_search_config: web_search_config.clone(),
        web_search_domains,
        backend_search: backend_tools_enabled,
        web_fetch_config: web_fetch_config.clone(),
        image_gen_config: image_gen_config.clone(),
        video_gen_config: video_gen_config.clone(),
        app_builder_deployer_config: app_builder_deployer_config.clone(),
        media_gen_batch_limits,
        write_file_enabled,
        active_agent_messages_enabled,
        agent_message_sender,
        subagents_enabled,
        subagent_toggle: subagent_toggle.clone(),
        background_workflows_enabled,
        ask_user_question_enabled,
        persona_summaries: persona_summaries.clone(),
        prompt_audience,
        role_instructions: role_instructions.clone(),
        persona_instructions: persona_instructions.clone(),
        skills_config: skills_config.clone(),
        compat,
        paths_config: paths_config.clone(),
        context_window_tokens,
        prompt_working_directory: prompt_display_cwd.clone(),
        lsp: tool_context.lsp.clone(),
        plugin_registry: plugin_registry.clone(),
        api_key_provider: api_key_provider.clone(),
        attribution_callback: attribution_callback_for_spec,
        tool_params_json: tool_params_json.clone(),
        subagent_event_tx: tool_context.subagent_event_tx.clone(),
        subagent_coordinator_sender: tool_context.subagent_coordinator_sender.clone(),
        monitor_event_buffer: tool_context.monitor_event_buffer.clone(),
        user_question_tx: user_question_tx.clone(),
        subagent_depth: tool_context.subagent_depth,
        subagents_max_depth,
        session_id_str: session_info.id.0.to_string(),
        blocking_wait_depth: tool_context.blocking_wait_depth.clone(),
        respect_gitignore,
        path_not_found_hints,
        mcp_state: mcp_state.clone(),
        managed_gateway_tool_client: managed_gateway_tool_client.clone(),
        is_non_interactive: startup_hints.non_interactive,
        owner_session_id: Some(session_info.id.0.to_string()),
        parent_scheduler_handle: if startup_hints.is_subagent {
            parent_scheduler_handle
        } else {
            None
        },
    });
    use xai_grok_telemetry::subagent_spawn::SubagentSpawnPhase;
    let builder_started_at = std::time::Instant::now();
    let agent_build_timer = crate::instrumentation_timer!("session.spawn_actor.agent_build");
    let agent_build_span = spawn_ctx
        .as_ref()
        .map(|ctx| phase_region_under(SubagentSpawnPhase::AgentBuild, &ctx.parent));
    let (agent, agent_build_elapsed) = rebuild_spec
        .build_agent_with_initial_overrides(
            agent_definition,
            system_prompt_label,
            persisted_announcement_state
                .as_ref()
                .filter(|s| !s.announced_skill_names.is_empty())
                .map(|s| s.announced_skill_names.clone()),
            preloaded_skills,
        )
        .instrument(tracing::info_span!("spawn.harness_build"))
        .await
        .map_err(|e| {
            tracing::error!(
                session_id = %session_info.id.0,
                error = %e,
                "Agent building failed, please check your config"
            );
            e
        })?;
    drop(agent_build_timer);
    drop(agent_build_span);
    let tool_setup_span = spawn_ctx
        .as_ref()
        .map(|ctx| phase_region_under(SubagentSpawnPhase::ToolSetup, &ctx.parent));
    let (harness_metrics, scheduler_handle_for_handle, toolset) = async {
        let reservations_for_bridge = task_completion_reservations.clone();
        agent
            .tool_bridge()
            .update_resources_with(|resources| {
                resources.insert(reservations_for_bridge);
                resources.insert(task_wake_suppressed);
            })
            .await;
        let memory_retrieval_mode = configured_memory_retrieval_mode(memory_config.as_ref());
        let harness_metrics = if !startup_hints.is_subagent
            && (telemetry_enabled || xai_grok_telemetry::external::is_active())
        {
            let plugin_names = plugin_registry
                .as_ref()
                .map(|reg| {
                    reg.active_plugins()
                        .iter()
                        .map(|p| p.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            Some(super::telemetry::SessionHarnessMetrics {
                session_id: session_info.id.0.to_string(),
                client_identifier: session_client_identifier.clone(),
                model_id: session_model_id.0.to_string(),
                agent_name: initial_agent_name,
                permission_mode: if session_yolo_mode {
                    xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
                } else if session_auto_mode
                    && crate::util::config::auto_permission_mode_enabled_from_disk()
                {
                    xai_grok_telemetry::enums::PermissionMode::Auto
                } else {
                    xai_grok_telemetry::enums::PermissionMode::Ask
                },
                mcp_server_names: mcp_servers
                    .iter()
                    .map(|s| mcp_server_name(s).to_owned())
                    .collect(),
                lsp_server_names: tool_context.lsp_server_names.clone(),
                memory_enabled: memory_config.as_ref().is_some_and(|config| config.enabled),
                memory_retrieval_mode,
                auto_update,
                cwd: tool_context.cwd.as_str().to_owned(),
                skill_names: agent.tool_bridge().skill_discovery_snapshot_names().await,
                compat,
                paths_config,
                plugin_registry: plugin_registry.clone(),
                plugin_names,
            })
        } else {
            None
        };
        let resolved_task_output =
            xai_grok_tools::reminders::task_completion::resolve_task_output_tool_name(
                agent.tool_bridge(),
            )
            .await;
        let resolved_read =
            xai_grok_tools::reminders::task_completion::resolve_read_tool_name(agent.tool_bridge())
                .await;
        let resolved_scheduler_delete =
            xai_grok_tools::reminders::task_completion::resolve_scheduler_delete_tool_name(
                agent.tool_bridge(),
            )
            .await;
        let resolved_scheduler_create =
            xai_grok_tools::reminders::task_completion::resolve_scheduler_create_tool_name(
                agent.tool_bridge(),
            )
            .await;
        let _ = task_output_tool_name.set(resolved_task_output.clone());
        let _ = read_tool_name.set(resolved_read);
        tool_context.task_output_tool_name = resolved_task_output.unwrap_or_else(|| {
            xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string()
        });
        tool_context.scheduler_delete_tool_name = resolved_scheduler_delete;
        tool_context.scheduler_create_tool_name = resolved_scheduler_create;
        let scheduler_handle_for_handle = {
            let toolset = agent.tool_bridge().toolset();
            let res = toolset.resources.lock().await;
            res.get::<
                    xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle,
                >()
                .cloned()
        };
        let toolset = agent.tool_bridge().toolset();
        (harness_metrics, scheduler_handle_for_handle, toolset)
    }
    .instrument(tracing::info_span!("spawn.tool_setup"))
    .await;
    if let Some(ctx) = &spawn_ctx {
        ctx.timer
            .record(SubagentSpawnPhase::AgentBuild, agent_build_elapsed);
        ctx.timer.record(
            SubagentSpawnPhase::ToolSetup,
            builder_started_at
                .elapsed()
                .saturating_sub(agent_build_elapsed),
        );
    }
    drop(tool_setup_span);
    crate::waterfall::mark(&wf_sid, crate::waterfall::stage::SB_AGENT_BUILT);
    let prefix_build_span = tracing::info_span!("spawn.prefix_build").entered();
    let system_prompt = agent.system_prompt().to_string();
    let mut prompt_context = agent.prompt_context().clone();
    prompt_context.normalize_for_persistence();
    save_prompt_context(&session_info, &prompt_context);
    let is_subagent_spawn = startup_hints.is_subagent;
    let session_non_interactive = startup_hints.non_interactive;
    install_system_prompt(
        &mut conversation,
        &mut startup_hints.inherited_prefix_len,
        is_subagent_spawn,
        startup_hints.preserve_inherited_system,
        &system_prompt,
    );
    if !is_subagent_spawn && reconcile_resumed_memory_section(&mut conversation, &system_prompt) {
        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            memory_enabled = memory_storage_for_session.is_some(),
            "MEMORY_INIT: resumed system prompt's memory section did not match this session's \
             memory state; replaced with the fresh prompt"
        );
    }
    if !startup_hints.preserve_inherited_system
        && !conversation_has_project_instructions(&conversation)
        && let Some(agents_md_reminder) = agent.agents_md_user_reminder()
    {
        let insert_at = conversation.len().min(1);
        conversation.insert(
            insert_at,
            ConversationItem::project_instructions(agents_md_reminder),
        );
        if let Some(ref mut len) = startup_hints.inherited_prefix_len {
            *len += 1;
        }
    }
    if let Some(section) = agent.agents_md_section()
        && should_set_classifier_project_instructions(
            owns_permission_manager,
            Some(section.as_str()),
        )
    {
        let body = agents_md_classifier_body(&section);
        if !body.is_empty() {
            permissions.set_project_instructions(Some(body));
        }
    }
    if let Some(ConversationItem::System(sys)) = conversation.first() {
        save_system_prompt(&session_info, &sys.content);
    } else {
        save_system_prompt(&session_info, &system_prompt);
    }
    let initial_prefix_carries_fallback_date = resumed_prefix_carries_fallback_date(
        agent
            .definition()
            .user_message_template
            .surfaces_local_date(),
        &conversation,
    );
    drop(prefix_build_span);
    {
        let _persist_span = tracing::info_span!(
            "spawn.history_persist",
            history_items = conversation.len() as i64
        )
        .entered();
        persist_chat_history_jsonl_sync(&session_info, &conversation);
    }
    chat_state_handle.replace_conversation(conversation);
    let feedback_client = feedback_proxy_url.map(|base_url| {
        let mut client =
            crate::agent::feedback_client::FeedbackClient::new(base_url, feedback_user_token)
                .with_alpha_test_key(feedback_alpha_test_key)
                .with_deployment_key(deployment_key);
        if let Some(am) = auth_manager.as_ref() {
            client = client.with_auth_manager(am.clone());
        }
        client
    });
    let has_feedback_client = feedback_client.is_some();
    tracing::info!(
        session_id = %session_info.id.0,
        has_feedback_client = has_feedback_client,
        "Creating feedback manager"
    );
    let feedback_client_type = match client_type {
        ClientType::GrokTUI => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
        ClientType::GrokWeb => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Web,
        ClientType::Nebula => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Nebula,
        ClientType::Extension => {
            prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Extension
        }
        ClientType::Generic => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Agent,
        ClientType::Desktop => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Desktop,
        ClientType::GrokPager => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
    };
    let user_cfg = feedback_flags.user;
    let feedback_config = FeedbackManagerConfig {
        feedback_enabled: feedback_flags.enabled,
        telemetry_enabled,
        client_type: feedback_client_type,
        loc_tracking_enabled,
        user: user_cfg.clone(),
        ..Default::default()
    };
    if feedback_flags.enabled
        && let Some(user_cfg) = user_cfg
    {
        tokio::spawn(async move {
            let _ = crate::util::user_identity::cached_identity(Some(&user_cfg)).await;
        });
    }
    let feedback_manager = Arc::new(FeedbackManager::new(
        session_info.id.0.to_string(),
        feedback_client,
        feedback_config,
    ));
    let signals_handle = feedback_manager.signals_handle();
    if let Some(persisted) = persisted_signals {
        signals_handle.restore_signals(persisted);
    } else {
        signals_handle.seed_counts(
            initial_user_count,
            initial_assistant_count,
            initial_tool_call_count,
            initial_tools_used,
            initial_models_used,
        );
    }
    signals_handle.set_primary_model(&primary_model_id);
    signals_handle.set_tracing_config(inference_idle_timeout_secs);
    let sync_loop_cancel = if has_feedback_client {
        Some(tokio_util::sync::CancellationToken::new())
    } else {
        None
    };
    let force_compact = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resolved_workspace_root = xai_grok_workspace::session::git::find_git_root_from_path(
        std::path::Path::new(&session_info.cwd),
    )
    .ok()
    .map(|p| p.to_string_lossy().to_string())
    .unwrap_or_else(|| session_info.cwd.clone());
    let current_prompt_id = std::sync::Arc::new(std::sync::Mutex::new(None));
    let pending_interactions: crate::session::pending_interaction::PendingInteractions =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let permissions_for_handle = permissions.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let mut sampler_config_initial = sampling_config.clone();
    sampler_config_initial.idle_timeout_secs = Some(inference_idle_timeout_secs);
    let task_output_budgeted = tool_context.task_output_token_budget.is_some();
    let retry_only_before_output =
        task_output_budgeted || tool_context.sampler_retry_only_before_output;
    if retry_only_before_output {
        sampler_config_initial.doom_loop_recovery = None;
    }
    let sampler_retry_policy = xai_grok_sampler::RetryPolicy {
        max_retries: resolved_max_retries,
        rate_limit_retry_threshold: subagent_sampler_rate_limit_threshold(
            is_subagent_spawn,
            subagent_rate_limit_max_attempts,
        ),
        retry_only_before_output,
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_sampler::SamplingEvent>();
    let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
        sampler_config_initial,
        sampler_retry_policy,
        sampler_event_tx,
    );
    let attribution_callback_for_handle = attribution_callback.clone();
    let agent_name_for_handle = initial_agent_type
        .as_deref()
        .unwrap_or(crate::agent::config::DEFAULT_AGENT_TYPE)
        .to_owned();
    let allowed_subagent_types_for_handle = agent.definition().allowed_subagent_types.clone();
    let mut hook_discovery_errors: Vec<xai_grok_hooks::error::HookError> = Vec::new();
    let hooks_discovery_span = tracing::info_span!("spawn.hooks_discovery").entered();
    let built_hook_registry: Option<Arc<xai_grok_hooks::discovery::HookRegistry>> =
        if let Some(override_reg) = hook_registry_override {
            Some(override_reg)
        } else {
            let cwd_path = std::path::Path::new(&session_info.cwd);
            let project_trusted = if prefetch.scan().is_gathered() {
                prefetch.resolve_trust(cwd_path, remote_settings.as_ref())
            } else {
                crate::agent::folder_trust::resolve_and_record(
                    cwd_path,
                    remote_settings.as_ref(),
                    false,
                )
            };
            let git_root = xai_grok_workspace::session::git::find_git_root_from_path(cwd_path).ok();
            let (registry, errors) = crate::util::hooks::discover_hooks(
                git_root.as_deref(),
                &rebuild_spec.compat,
                project_trusted,
            );
            for e in &errors {
                tracing::warn!(error = ?e, "hook loading error");
            }
            hook_discovery_errors = errors;
            if registry.is_empty() {
                None
            } else {
                tracing::info!(hook_count = registry.len(), "loaded hooks");
                Some(Arc::new(registry))
            }
        };
    drop(hooks_discovery_span);
    let hook_registry_for_handle = built_hook_registry.clone();
    let workspace_ops_for_handle = workspace_ops.clone();
    #[allow(clippy::arc_with_non_send_sync)]
    let mut _hook_load_errors: Vec<String> = hook_discovery_errors
        .iter()
        .map(|e| e.to_string())
        .collect();
    let upload_queue = Arc::new(std::sync::OnceLock::new());
    let (goal_update_tx, goal_update_rx) = tokio::sync::mpsc::unbounded_channel::<
        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalEnvelope,
    >();
    let workflow_restore_span = tracing::info_span!("spawn.workflow_restore").entered();
    crate::session::workflow::registry::warm_builtin_cache();
    let workflow_session_dir = crate::session::persistence::session_dir(&session_info);
    let resume_workflows: Vec<crate::session::resume_status::ResumeWorkflow> =
        persisted_workflow_runs
            .iter()
            .filter(|run| {
                use crate::session::workflow::tracker::WorkflowRunStatus;
                let status = run.manifest.state.status;
                status == WorkflowRunStatus::Active
                    || status == WorkflowRunStatus::Interrupted
                    || status.is_paused()
            })
            .map(|run| {
                let state = &run.manifest.state;
                crate::session::resume_status::ResumeWorkflow {
                    run_id: state.run_id.clone(),
                    objective: state.objective.clone(),
                }
            })
            .collect();
    let resume_goal = goal_tracker.lock().snapshot().and_then(|g| {
        use crate::session::goal_tracker::GoalStatus;
        if matches!(g.status, GoalStatus::Complete | GoalStatus::BudgetLimited) {
            None
        } else {
            Some(crate::session::resume_status::ResumeGoal {
                objective: g.objective.clone(),
            })
        }
    });
    if !startup_hints.is_subagent && !crate::session::resume_status::exists(&workflow_session_dir) {
        let snapshot = crate::session::resume_status::reconstruct_from_disk(
            &workflow_session_dir,
            session_info.id.0.as_ref(),
            resume_workflows,
            resume_goal,
        );
        crate::session::resume_status::persist(&workflow_session_dir, &snapshot);
    }
    let (workflow_store, workflow_snapshots) =
        crate::session::workflow::store::WorkflowRunStore::from_restored(
            Some(workflow_session_dir.clone()),
            persistence.tx.clone(),
            persisted_workflow_runs,
        );
    let workflow_tracker = Arc::new(parking_lot::Mutex::new(
        crate::session::workflow::tracker::WorkflowTracker::from_snapshot(workflow_snapshots),
    ));
    let active_work = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workflow_notify = crate::session::workflow::notify::WorkflowNotifySender::new(
        session_info.id.clone(),
        gateway.clone(),
        persistence.tx.clone(),
        workflow_store.clone(),
    );
    for state in workflow_tracker.lock().snapshot() {
        workflow_notify.emit(&state, state.elapsed_ms_floor, 0);
    }
    drop(workflow_restore_span);
    let workflow_manager = Arc::new(tokio::sync::Mutex::new(
        crate::session::workflow::manager::WorkflowManager::new(
            session_info.id.0.to_string(),
            Some(workflow_session_dir),
            std::path::PathBuf::from(session_info.cwd.as_str()),
            workflow_tracker.clone(),
            active_work.clone(),
            workflow_store,
            workflow_notify,
            tool_context.subagent_event_tx.clone().unwrap_or_else(|| {
                tracing::warn!(
                    "workflow manager: no subagent coordinator; agent() spawns will fail"
                );
                tokio::sync::mpsc::unbounded_channel().0
            }),
            Arc::new(|name: &str, fields: &serde_json::Value, replayed: bool| {
                if !replayed {
                    tracing::info!(event = name, %fields, "workflow telemetry");
                }
            }),
            cmd_tx.clone(),
            std::collections::HashMap::new(),
            workflow_max_concurrent_agents,
        ),
    ));
    let (workflow_launch_tx, workflow_launch_rx) = tokio::sync::mpsc::unbounded_channel::<
        xai_grok_tools::implementations::grok_build::workflow::WorkflowLaunchEnvelope,
    >();
    crate::session::workflow::request_service::spawn_request_service(
        workflow_launch_rx,
        crate::session::workflow::request_service::RequestServiceOptions {
            manager: workflow_manager.clone(),
            cwd: std::path::PathBuf::from(session_info.cwd.as_str()),
            session_dir: crate::session::persistence::session_dir(&session_info),
            enabled: background_workflows_enabled,
        },
    );
    let obs_bridge = {
        let sid = xai_tool_protocol::SessionId::new(&*session_info.id.0)
            .unwrap_or_else(|_| xai_tool_protocol::SessionId::new("unknown").expect("valid"));
        xai_computer_hub_sdk::ObservabilityBridge::new(None, sid)
    };
    let config_load_span = tracing::info_span!("spawn.config_load").entered();
    let mut effective_config = crate::config::load_effective_config()
        .ok()
        .and_then(|raw| crate::agent::config::Config::new_from_toml_cfg(&raw).ok())
        .unwrap_or_default();
    effective_config.remote_settings = remote_settings.clone();
    let goal_classifier_max_runs = effective_config.resolve_goal_classifier_max_runs().value;
    let goal_strategist_every = effective_config
        .resolve_goal_strategist_every(goal_classifier_max_runs)
        .value;
    let goal_reverify_after = effective_config.resolve_goal_reverify_after().value;
    let goal_use_current_model_only = effective_config.resolve_goal_use_current_model_only().value;
    let goal_role_models = {
        let planner = effective_config
            .resolve_goal_planner_model(goal_use_current_model_only)
            .value;
        let strategist = effective_config
            .resolve_goal_strategist_model(goal_use_current_model_only)
            .value;
        let skeptic_pool = effective_config
            .resolve_goal_skeptic_models(goal_use_current_model_only)
            .value
            .into_iter()
            .filter_map(|c| match c {
                crate::agent::config::GoalRoleModelChoice::Explicit(p) => Some(p),
                crate::agent::config::GoalRoleModelChoice::InheritCurrent => None,
            })
            .collect();
        GoalRoleModelConfig {
            planner,
            strategist,
            skeptic_pool,
        }
    };
    let doom_loop_recovery = effective_config.resolve_doom_loop_recovery();
    let resolved_tool_overrides: std::sync::Arc<
        arc_swap::ArcSwapOption<xai_grok_sampling_types::ToolOverrides>,
    > = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());
    let title_refresh_enabled = effective_config.is_title_refresh_enabled();
    let initial_title_refresh_idx =
        crate::session::helpers::session_summary::initial_title_refresh_idx(
            title_refresh_watermark,
            title_refresh_enabled,
            title_refresh_turns_at_spawn,
        );
    if title_refresh_watermark.is_none() && initial_title_refresh_idx == 0 {
        crate::session::helpers::session_summary::save_title_refresh_watermark(
            &title_session_dir,
            0,
        );
    }
    drop(config_load_span);
    let git_scan_span = tracing::info_span!("spawn.git_scan").entered();
    let vcs_kind = {
        let root = std::path::Path::new(&session_info.cwd);
        match xai_grok_workspace::session::git::discover_git_root(root) {
            xai_grok_workspace::session::git::GitDiscoveryResult::Found(git_root) => {
                xai_grok_workspace::session::git::detect_vcs_kind(&git_root)
            }
            _ => xai_grok_workspace::session::git::VcsKind::None,
        }
    };
    use crate::session::repo_status_prefix::{
        RepoStatusInputs, RepoStatusPlan, RepoStatusPrefetch, discover_vcs_root,
    };
    let suppress_status_body =
        !crate::util::config::resolve_repo_status_in_system_prompt(remote_settings.as_ref())
            || startup_hints.skip_git_status;
    let starts_fresh = initial_conversation_len == 0;
    let repo_status_plan = if matches!(vcs_kind, xai_grok_workspace::session::git::VcsKind::None) {
        RepoStatusPlan::NoRepo
    } else {
        let prefix_cwd = std::path::PathBuf::from(
            prompt_display_cwd
                .clone()
                .unwrap_or_else(|| session_info.cwd.clone()),
        );
        if suppress_status_body {
            RepoStatusPlan::RootOnly {
                root: discover_vcs_root(&prefix_cwd),
                vcs_kind,
            }
        } else {
            let inputs = RepoStatusInputs::new(prefix_cwd, vcs_kind);
            let prefetch = starts_fresh
                .then(|| RepoStatusPrefetch::spawn(inputs.clone()))
                .flatten();
            RepoStatusPlan::Gather {
                inputs,
                prefetch: std::cell::RefCell::new(prefetch),
            }
        }
    };
    drop(git_scan_span);
    let actor_build_span = tracing::info_span!("spawn.actor_build").entered();
    let session = Arc::new_cyclic(|weak: &std::sync::Weak<SessionActor>| SessionActor {
        status_wake: Default::default(),
        session_info: session_info.clone(),
        transient_retry_enabled: !startup_hints.is_subagent
            && crate::util::config::resolve_turn_transient_retry(
                remote_settings
                    .as_ref()
                    .and_then(|r| r.turn_transient_retry),
            ),
        transient_retries_prompt_total: std::cell::Cell::new(0),
        transient_episode_start: std::cell::Cell::new(None),
        auth_method_id,
        model_auth_memo: std::cell::RefCell::new(None),
        jev_ledger: std::cell::RefCell::new(Default::default()),
        attribution_callback,
        auth_manager,
        is_chat_kind,
        state,
        notifications: NotificationSender {
            gateway: gateway.clone(),
            gateway_enabled: gateway_enabled.clone(),
            persistence_tx: persistence.tx.clone(),
            disk_full: persistence.subscribe_disk_full(),
            client_caps: client_caps.clone(),
        },
        permissions,
        tool_context,
        deny_read_globs,
        mcp_state: mcp_state.clone(),
        mcp_strategy: std::cell::Cell::new(mcp_strategy),
        initial_client_mcp_servers: initial_client_mcp_servers.clone(),
        chat_state_handle,
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        current_prompt_id: current_prompt_id.clone(),
        active_work: active_work.clone(),
        pending_interactions: pending_interactions.clone(),
        telemetry_enabled,
        supports_backend_search: std::cell::Cell::new(sampling_config.supports_backend_search),
        tool_overrides: std::cell::RefCell::new(None),
        resolved_tool_overrides: resolved_tool_overrides.clone(),
        compactions_remaining: std::cell::Cell::new(sampling_config.compactions_remaining),
        compaction_at_tokens: std::cell::Cell::new(sampling_config.compaction_at_tokens),
        doom_loop_recovery,
        doom_loop_turn_tally: Default::default(),
        file_state_tracker,
        rewind_pending_prompt: std::sync::Mutex::new(None),
        delivery_tools: std::cell::RefCell::new(startup_hints.delivery_tools.clone()),
        attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(
            startup_hints.non_interactive,
        )),
        startup_hints,
        forked_tool_override,
        compaction: super::compaction_config::CompactionConfig {
            threshold_percent: std::cell::Cell::new(auto_compact_threshold_percent),
            force_compact: force_compact.clone(),
            context_window_override,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode,
            verbatim_input: compaction_verbatim_input,
            tool_choice: compaction_tool_choice,
            prefire: crate::session::compaction_config::PrefireState::default(),
            prefix_released: std::sync::atomic::AtomicBool::new(false),
            cancel: Default::default(),
        },
        memory: super::memory_state::SessionMemory {
            configured_mode: memory_config.as_ref().map(|mc| mc.mode),
            v2_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.v2),
            configured_storage: configured_memory_storage.filter(|storage| {
                storage.mode().is_v2()
                    || memory_config.as_ref().is_some_and(|config| config.enabled)
            }),
            process_disabled: memory_config
                .as_ref()
                .is_some_and(|config| config.force_disabled),
            config_opt_out: memory_config
                .as_ref()
                .is_some_and(|config| !config.enabled && !config.force_disabled),
            v2_legacy_carryover: memory_v2_legacy_carryover,
            prompt_sync_pending: std::sync::atomic::AtomicBool::new(false),
            flush_config: memory_config.as_ref().map_or_else(
                || crate::config::MemoryFlushConfig {
                    enabled: false,
                    ..Default::default()
                },
                |mc| {
                    if mc.mode.is_legacy() {
                        mc.flush.clone()
                    } else {
                        crate::config::MemoryFlushConfig {
                            enabled: false,
                            idle_timeout_secs: None,
                            ..Default::default()
                        }
                    }
                },
            ),
            is_flushing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            capture_worker: std::cell::RefCell::new(None),
            dream_workers: crate::session::memory_state::V2DreamWorkers::default(),
            last_capture_failure: std::cell::RefCell::new(None),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(memory_storage_for_session),
            save_on_end: memory_config
                .as_ref()
                .is_none_or(|mc| mc.mode.is_legacy() && mc.session.save_on_end),
            backend_params: memory_backend_params_for_session,
            initial_injection_config: memory_initial_injection_config,
            context_injected: std::sync::atomic::AtomicBool::new(false),
            flush_count: std::sync::atomic::AtomicU64::new(0),
            last_flush_content: std::cell::RefCell::new(None),
            flush_success_count: std::sync::atomic::AtomicU64::new(0),
            flush_error_count: std::sync::atomic::AtomicU64::new(0),
            search_counter: std::cell::RefCell::new(memory_search_counter),
            injection_count: std::sync::atomic::AtomicU64::new(0),
            compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
            chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            init_reindex_handle: std::cell::RefCell::new(None),
            dream_config: memory_config.as_ref().map_or_else(Default::default, |mc| {
                if mc.mode.is_legacy() {
                    mc.dream
                } else {
                    crate::config::MemoryDreamConfig {
                        enabled: false,
                        check_interval_secs: None,
                        ..Default::default()
                    }
                }
            }),
            dream_count: std::sync::atomic::AtomicU64::new(0),
            dream_success_count: std::sync::atomic::AtomicU64::new(0),
            dream_error_count: std::sync::atomic::AtomicU64::new(0),
            token_totals: Default::default(),
        },
        session_start: std::time::Instant::now(),
        inference_idle_timeout: Duration::from_secs(inference_idle_timeout_secs),
        uncharged_401_park_enabled: crate::util::config::resolve_uncharged_401_park(
            remote_settings.as_ref().and_then(|r| r.uncharged_401_park),
        ),
        max_turns,
        max_retries: resolved_max_retries,
        rate_limit_waits: RateLimitWaitConfig::with_max_attempts(subagent_rate_limit_max_attempts),
        pending_interjections: InterjectionBuffer::new(),
        pending_skill_reminders: Mutex::new(Vec::new()),
        idle_flush_timeout: memory_config
            .as_ref()
            .filter(|mc| mc.mode.is_legacy())
            .and_then(|mc| mc.flush.idle_timeout_secs)
            .map(std::time::Duration::from_secs),
        dream_check_timeout: memory_config
            .as_ref()
            .filter(|mc| mc.mode.is_legacy() && mc.dream.enabled)
            .and_then(|mc| mc.dream.check_interval_secs)
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs),
        last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(
            initial_conversation_len,
        ),
        event_tx,
        buffering_settings,
        client_identifier: session_client_identifier.clone(),
        origin_client: origin_client.clone(),
        feedback_manager: feedback_manager.clone(),
        upload_queue: upload_queue.clone(),
        sync_loop_cancel: sync_loop_cancel.clone(),
        agent: std::cell::RefCell::new(agent),
        last_reported_branch: Arc::new(Mutex::new(None)),
        git_head_enabled: fs_watch_caps.git_head,
        status_line_enabled: client_caps.status_line.clone(),
        models_manager,
        display_cwd: {
            let lock = std::sync::OnceLock::new();
            if let Some(ref cwd) = prompt_display_cwd {
                let _ = lock.set(cwd.clone());
            }
            lock
        },
        active_agent_type: parking_lot::Mutex::new(initial_agent_type),
        queue_exit_reminder_on_approved_exit,
        emit_local_background_tasks: emit_local_background_tasks.clone(),
        active_skill: parking_lot::Mutex::new(None),
        current_prompt_mode: current_prompt_mode.clone(),
        turn_start_prompt_mode: parking_lot::Mutex::new(restored_prompt_mode),
        turn_prompt_mode: turn_prompt_mode.clone(),
        plan_mode: plan_mode.clone(),
        goal_enabled,
        background_workflows_enabled,
        goal_harness_enabled: std::sync::atomic::AtomicBool::new(if background_workflows_enabled {
            goal_enabled
        } else {
            false
        }),
        goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(false),
        goal_tracker,
        goal_turn_task_ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
        goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
        goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
        goal_update_rx: std::cell::RefCell::new(Some(goal_update_rx)),
        goal_update_tx,
        workflow_manager: workflow_manager.clone(),
        workflow_launch_tx: workflow_launch_tx.clone(),
        goal_classifier_enabled: effective_config
            .resolve_goal_classifier_enabled(goal_enabled)
            .value,
        goal_planner_enabled: effective_config
            .resolve_goal_planner_enabled(goal_enabled)
            .value,
        goal_summary_enabled: effective_config
            .resolve_goal_summary_enabled(goal_enabled)
            .value,
        length_salvage_remote_budget: effective_config
            .remote_settings
            .as_ref()
            .and_then(|r| r.length_salvage_budget),
        goal_verifier_skeptic_count: effective_config.resolve_goal_verifier_count().value,
        goal_role_models,
        goal_use_current_model_only,
        goal_classifier_max_runs,
        goal_strategist_every,
        goal_reverify_after,
        goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
        pending_classifier_completions: parking_lot::Mutex::new(VecDeque::new()),
        goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
        managed_mcp_handle,
        tool_metadata_snapshot: Arc::new(std::sync::Mutex::new(Default::default())),
        mcp_announcements: Mutex::new(
            persisted_announcement_state
                .map(crate::session::announcement_state::McpAnnounced::from_persisted)
                .unwrap_or_default(),
        ),
        mcp_reminder_mode: McpReminderMode::from_env(),
        mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        mcp_connecting_reminder_injected: std::cell::Cell::new(false),
        mcp_refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
        user_input_generation: std::sync::atomic::AtomicU64::new(0),
        laziness_debug_log: laziness_debug_log.map(|p| std::sync::Arc::from(p.as_path())),
        last_live_orphan_reconcile: std::cell::Cell::new(None),
        deferred_prefix: DeferredPrefix::new(),
        mcp_startup_waits: Default::default(),
        mcp_init_tasks: Default::default(),
        repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::new(
            repo_status_plan,
        ),
        extension_registry: session_extension_registry(weak.clone()),
        weak_self: weak.clone(),
        startup_tasks: Default::default(),
        last_announced_local_date: std::cell::Cell::new(chrono::Local::now().date_naive()),
        prefix_carries_fallback_date: std::cell::Cell::new(initial_prefix_carries_fallback_date),
        last_search_prompt_index: std::sync::atomic::AtomicI64::new(-1),
        last_api_request_at: std::sync::atomic::AtomicI64::new(0),
        hook_registry: std::cell::RefCell::new(built_hook_registry),
        hook_disabled: std::cell::RefCell::new(Arc::new(
            xai_grok_hooks::trust::DisabledHooks::load(),
        )),
        turn_report: Default::default(),
        turn_abort: Default::default(),
        turn_end_tx: Default::default(),
        client_hooks: std::cell::RefCell::new(client_hooks),
        hook_resolved_workspace_root: resolved_workspace_root,
        vcs_kind,
        hook_load_errors: std::cell::RefCell::new(_hook_load_errors),
        plugin_registry: std::cell::RefCell::new(plugin_registry.clone()),
        plugin_registry_handle,
        events: crate::session::events::EventTracker::new(
            &crate::session::persistence::session_dir(&session_info),
        ),
        observability_bridge: obs_bridge,
        current_turn_number: std::cell::Cell::new(0),
        turn_phases: std::sync::Arc::default(),
        last_recap_main_turn: std::cell::Cell::new(initial_last_recap_main_turn),
        recap_in_flight: std::cell::Cell::new(false),
        recap_epoch: std::cell::Cell::new(0),
        turn_summary_task: std::cell::RefCell::new(None),
        turn_summary_generation: std::cell::Cell::new(0),
        turn_summary_enabled: effective_config.is_turn_summary_enabled(),
        title_refresh_enabled,
        title_refresh_task: std::cell::RefCell::new(None),
        title_refresh_generation: std::cell::Cell::new(0),
        next_title_refresh_idx: std::cell::Cell::new(initial_title_refresh_idx),
        session_turn_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        streaming_turn_capture: parking_lot::Mutex::new(StreamingTurnCapture::default()),
        stream_apply_span: parking_lot::Mutex::new(None),
        current_turn_span_id: parking_lot::Mutex::new(None),
        turn_stream_drained: parking_lot::Mutex::new(std::collections::HashMap::new()),
        pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
        image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
        sampler_handle,
        sampling_gate,
        rebuild_spec: rebuild_spec.clone(),
        image_description_model,
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: workspace_ops.clone(),
        trace_config_template: std::cell::RefCell::new(None),
    });
    drop(actor_build_span);
    async {
        if owns_permission_manager {
            session.wire_permission_prompt_notification();
        }
        if goal_was_restored {
            let current_tokens = session.chat_state_handle.get_total_tokens().await as i64;
            let (tokens_used, finished_marginal) = session.goal_tokens(current_tokens);
            session.goal_notify_sender().emit_goal_updated(
                &mut session.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }
        session.emit_resolved_tool_overrides();
        {
            let drainer_session = session.clone();
            let mut sampler_event_rx = sampler_event_rx;
            tokio::task::spawn_local(async move {
                while let Some(event) = sampler_event_rx.recv().await {
                    drainer_session.handle_sampling_event(event).await;
                }
                tracing::debug!("sampler event drainer exiting (channel closed)");
            });
        }
        if !background_workflows_enabled {
            let drainer_session = session.clone();
            let Some(mut goal_update_rx) = session.goal_update_rx.borrow_mut().take() else {
                unreachable!("goal_update_rx must be Some at session spawn");
            };
            tokio::task::spawn_local(async move {
                while let Some(envelope) = goal_update_rx.recv().await {
                    let current_tokens =
                        drainer_session.chat_state_handle.get_total_tokens().await as i64;
                    drainer_session
                        .drain_goal_updates_with_extra(
                            current_tokens,
                            DrainPurpose::MidTurn,
                            vec![envelope],
                        )
                        .await;
                }
                tracing::debug!("goal update drainer exiting (channel closed)");
            });
        }
        {
            let snapshot = session.tool_metadata_snapshot.clone();
            let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
            session
                .agent
                .borrow()
                .tool_bridge()
                .update_resource(xai_grok_tools::types::tool_index::ToolIndex(
                    std::sync::Arc::new(tool_index),
                ))
                .await;
        }
        if let Some(client) = managed_gateway_tool_client.clone() {
            session
                .agent
                .borrow()
                .tool_bridge()
                .update_resource(client)
                .await;
        }
        {
            let plan_path = session.plan_mode.lock().plan_file_path().to_path_buf();
            session
                .agent
                .borrow()
                .tool_bridge()
                .update_resource(xai_grok_tools::types::resources::PlanFilePath(plan_path))
                .await;
        }
        session.inject_deny_read_globs().await;
        if session.permissions.is_auto_mode() {
            session.wire_permission_auto_llm_classifier().await;
        }
        // A session that starts in always-approve gets the Jev brake right away.
        session.wire_jev_veto_classifier(session.permissions.is_yolo_mode());
        session
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(
                xai_grok_tools::implementations::grok_build::workflow::WorkflowLaunchHandle(
                    session.workflow_launch_tx.clone(),
                ),
            )
            .await;
        if !background_workflows_enabled {
            session
                .agent
                .borrow()
                .tool_bridge()
                .update_resource(
                    xai_grok_tools::implementations::grok_build::update_goal::GoalUpdateHandle(
                        session.goal_update_tx.clone(),
                    ),
                )
                .await;
        }
        if let Some(ref display_cwd) = prompt_display_cwd {
            session
                .agent
                .borrow()
                .tool_bridge()
                .set_display_cwd(std::path::PathBuf::from(display_cwd))
                .await;
        }
    }
    .instrument(tracing::info_span!("spawn.actor_setup"))
    .await;
    if let Some(storage) = session
        .memory
        .storage()
        .filter(|storage| storage.mode().is_legacy())
    {
        crate::session::memory::init_sqlite_vec();
        let index_config = memory_config
            .as_ref()
            .map_or_else(Default::default, |mc| mc.index.clone());
        let embed_config = memory_config
            .as_ref()
            .map(|mc| mc.embedding.clone())
            .unwrap_or_default();
        let embed_dims = embed_config.dimensions;
        let sampling_base_url = embed_base_url.clone();
        let sampling_api_key = embed_api_key.clone();
        let session_id_for_reindex = session_info.id.to_string();
        let chunks_added_counter = session.memory.chunks_added.clone();
        let reindex_handle = tokio::task::spawn_local(async move {
            let db_path = storage.workspace_dir().join("index.sqlite");
            if let Ok(mut index) = crate::session::memory::MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                index_config,
                embed_dims,
            ) && let Ok(files) = storage.list_memory_files()
            {
                let reindex_start = std::time::Instant::now();
                let (mut total_added, mut total_updated, mut total_removed) = (0, 0, 0);
                for file in &files {
                    let source = storage.classify_source(file);
                    if let Ok(stats) = index.reindex_file(file, source) {
                        total_added += stats.added;
                        total_updated += stats.updated;
                        total_removed += stats.removed;
                    }
                }
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    files = files.len(),
                    "MEMORY_REINDEX: background reindex complete"
                );
                let embedded_count = if let Some(api_key) = sampling_api_key {
                    if let Some(provider) =
                        crate::session::memory::embedding::ApiEmbeddingProvider::from_session(
                            &embed_config,
                            sampling_base_url,
                            api_key,
                        )
                    {
                        crate::session::memory::embed_missing_chunks(&index, &provider).await
                    } else {
                        0
                    }
                } else {
                    0
                };
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::memory_telemetry::MemoryReindex {
                        session_id: session_id_for_reindex.clone(),
                        source: "init".to_owned(),
                        added: total_added,
                        updated: total_updated,
                        removed: total_removed,
                        embedded: embedded_count,
                        duration_ms: reindex_start.elapsed().as_millis() as u64,
                        trigger: "init".to_owned(),
                    },
                );
                chunks_added_counter
                    .fetch_add(total_added as u64, std::sync::atomic::Ordering::Relaxed);
            }
        });
        *session.memory.init_reindex_handle.borrow_mut() = Some(reindex_handle);
    }
    if let Some(cancel) = sync_loop_cancel {
        tracing::info!(
            session_id = %session_info.id.0,
            "Spawning feedback sync loop"
        );
        let fm = feedback_manager.clone();
        tokio::spawn(async move {
            fm.run_sync_loop(cancel).await;
        });
    } else {
        tracing::debug!(
            session_id = %session_info.id.0,
            "No feedback client available, skipping sync loop"
        );
    }
    {
        use agent_client_protocol::Client as _;
        use xai_grok_tools::implementations::grok_build::ask_user_question::{
            AskUserQuestionExtRequest, AskUserQuestionExtResponse, UserQuestionError,
            UserQuestionResponse,
        };
        let gateway = session.notifications.gateway.clone();
        let session_id = session.session_info.id.clone();
        let current_prompt_mode = session.current_prompt_mode.clone();
        let pending_interactions = session.pending_interactions.clone();
        let session_for_hooks = session.clone();
        let mut user_question_rx = user_question_rx;
        tokio::task::spawn_local(async move {
            while let Some(mut request) = user_question_rx.recv().await {
                use xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionMode;
                let mode = match *current_prompt_mode.lock() {
                    PromptMode::Plan => AskUserQuestionMode::Plan,
                    _ => AskUserQuestionMode::Default,
                };
                let ext_req = AskUserQuestionExtRequest {
                    session_id: session_id.0.to_string(),
                    tool_call_id: request.tool_call_id.clone(),
                    questions: request.questions.clone(),
                    mode,
                };
                debug_assert!(
                    !ext_req.session_id.is_empty(),
                    "ask_user_question reverse-request must carry a non-empty sessionId (design §5.4)"
                );
                let ext_request = agent_client_protocol::ExtRequest::new(
                    "x.ai/ask_user_question",
                    serde_json::value::to_raw_value(&ext_req)
                        .expect("AskUserQuestionExtRequest serialization should not fail")
                        .into(),
                );
                session_for_hooks
                    .dispatch_notification_hook(
                        "elicitation_dialog",
                        Some("User question requested".into()),
                        None,
                        Some("info".into()),
                    )
                    .await;
                let questions_for_response = request.questions.clone();
                let tool_call_id = request.tool_call_id.clone();
                let result = {
                    let _pending_guard =
                        crate::session::pending_interaction::PendingInteractionGuard::new(
                            pending_interactions.clone(),
                            gateway.clone(),
                            session_id.clone(),
                            tool_call_id.clone(),
                            crate::session::pending_interaction::PendingKind::Question,
                        );
                    tokio::select! {
                        biased;
                        () = request.result_tx.closed() => {
                            tracing::info!(
                                %tool_call_id,
                                "ask_user_question tool receiver closed (timeout or cancel); abandoning ACP wait"
                            );
                            Ok(UserQuestionResponse::Cancelled)
                        }
                        acp_result = gateway.ext_method(ext_request) => {
                            match acp_result {
                                Ok(raw) => {
                                    match serde_json::from_str::<AskUserQuestionExtResponse>(
                                        raw.0.get(),
                                    ) {
                                        Ok(typed) => {
                                            Ok(typed.into_response(questions_for_response))
                                        }
                                        Err(e) => Err(UserQuestionError::MalformedResponse(
                                            e.to_string(),
                                        )),
                                    }
                                }
                                Err(e) => Err(UserQuestionError::TransportError(e.to_string())),
                            }
                        }
                    }
                };
                let _ = request.result_tx.send(result);
            }
        });
    }
    let (session_done_tx, session_done_rx) = tokio::sync::oneshot::channel::<()>();
    let spawn_snapshot = crate::session::SpawnSnapshot {
        applied_tool_overrides: session.effective_tool_overrides(),
        memory_mode: session.memory.mode(),
    };
    let tool_context_for_handle = session.tool_context.clone();
    let telemetry_ctx = xai_grok_telemetry::session_ctx::TelemetryCtx::new(
        session.session_info.id.0.to_string(),
        session.tool_context.prompt_index.clone(),
    );
    if let Some(metrics) = harness_metrics {
        let hooks: Vec<super::telemetry::HookRegInfo> = session
            .hook_registry
            .borrow()
            .as_ref()
            .map(|reg| {
                reg.all_hooks()
                    .iter()
                    .map(|s| super::telemetry::HookRegInfo::from_spec(s))
                    .collect()
            })
            .unwrap_or_default();
        let telemetry_enabled = session.telemetry_enabled;
        tokio::spawn(async move {
            let ev = metrics.into_event(hooks).await;
            xai_grok_telemetry::session_ctx::log_event_dual(telemetry_enabled, ev);
        });
    }
    let hosting = SESSIONS_ACTIVE.enter();
    tokio::task::spawn_local(async move {
        let _hosting = hosting;
        xai_grok_telemetry::session_ctx::with_session_ctx(
            telemetry_ctx,
            run_session(
                session,
                cmd_rx,
                chat_state_event_rx,
                event_rx,
                fs_notify_config,
                codebase_indexes,
                index_root_for_session,
                fs_watch_caps,
            ),
        )
        .await;
        let _ = session_done_tx.send(());
    });
    let handle = SessionHandle {
        cmd_tx,
        persistence_tx: persistence.tx.clone(),
        current_prompt_id,
        registry_write_order: Default::default(),
        pending_interactions,
        active_work: active_work.clone(),
        info: session_info,
        max_turns,
        resolved_tool_overrides,
        spawn_snapshot,
        hunk_tracker_handle,
        chat_state_handle: chat_state_handle_for_handle,
        signals_handle,
        gateway_enabled,
        emit_local_background_tasks,
        client_caps,
        mcp_servers,
        initial_client_mcp_servers,
        display_cwd: None,
        feedback_manager: feedback_manager.clone(),
        upload_queue: upload_queue.clone(),
        upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        tool_context: tool_context_for_handle,
        model_id: session_model_id,
        reasoning_effort: sampling_config.reasoning_effort,
        yolo_mode: session_yolo_mode,
        origin_client: origin_client.clone(),
        code_nav_enabled,
        ask_user_question_enabled,
        non_interactive: session_non_interactive,
        plan_mode: plan_mode.clone(),
        force_compact,
        permission_handle: permissions_for_handle,
        attribution_callback: attribution_callback_for_handle,
        agent_name: agent_name_for_handle,
        managed_mcp_proxy_base_url,
        session_default_agent_profile,
        allowed_subagent_types: allowed_subagent_types_for_handle,
        hook_registry: hook_registry_for_handle,
        workspace_ops: workspace_ops_for_handle,
        terminal_backend: Some(terminal_backend.clone()),
        tools_notification_handle: Some(tools_notification_handle.clone()),
        scheduler_handle: scheduler_handle_for_handle,
    };
    Ok((
        SessionInitResult {
            handle,
            permission_events_rx,
            system_prompt,
            toolset,
        },
        session_done_rx,
    ))
}
/// Stored separately from `SessionHandle` (which derives `Clone`) because `JoinHandle` is not `Clone`.
pub struct SessionThread {
    join_handle: std::thread::JoinHandle<()>,
}
impl SessionThread {
    pub fn is_finished(&self) -> bool {
        self.join_handle.is_finished()
    }
    #[cfg(test)]
    pub fn from_handle(handle: std::thread::JoinHandle<()>) -> Self {
        Self {
            join_handle: handle,
        }
    }
}
/// What a spawned actor hands back to the caller that installs it.
pub(crate) struct SessionInitResult {
    pub(crate) handle: SessionHandle,
    pub(crate) permission_events_rx: mpsc::UnboundedReceiver<PermissionEvent>,
    pub(crate) system_prompt: String,
    /// The actor's toolset, for the installer to bind with [`bind_installed_toolset`].
    pub(crate) toolset: Arc<xai_grok_tools::registry::types::FinalizedToolset>,
}
/// Bind an installed actor's toolset to its workspace session so local `call_tool` dispatches through
/// it. The installer calls this once it owns the id, so two installs racing for one id never both
/// bind. `cwd` and `hunk_tracker` are the actor's own (`ToolContext`). `Some` only when a local
/// binding was taken; proxy mode binds nothing.
pub(crate) fn bind_installed_toolset(
    workspace_ops: &xai_grok_workspace::WorkspaceOps,
    session_id: &acp::SessionId,
    cwd: &std::path::Path,
    hunk_tracker: &xai_hunk_tracker::HunkTrackerHandle,
    toolset: &Arc<xai_grok_tools::registry::types::FinalizedToolset>,
) -> Option<Arc<xai_grok_tools::registry::types::FinalizedToolset>> {
    match workspace_ops.bind_local_session(
        session_id.0.as_ref(),
        cwd.to_path_buf(),
        hunk_tracker.clone(),
        Arc::clone(toolset),
        None,
    ) {
        Ok(bound) => bound.then(|| Arc::clone(toolset)),
        Err(e) => {
            tracing::warn!(
                session_id = %session_id.0,
                error = %e,
                "failed to bind local session toolset"
            );
            None
        }
    }
}
/// The entire `spawn_session_actor` body runs on the session thread: the `!Send` `SessionActor` never crosses a
/// thread boundary, and the `Send` results come back to the caller over a oneshot.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn spawn_session_on_thread(
    session_info: SessionInfo,
    gateway: GatewaySender,
    sampling_config: SamplingConfig,
    credentials: xai_chat_state::Credentials,
    auth_method_id: crate::agent::auth_method::SharedAuthMethodId,
    auth_manager: Option<Arc<AuthManager>>,
    attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback>,
    tool_context: ToolContext,
    mcp_servers: Vec<acp::McpServer>,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    mcp_meta_config_map: McpMetaConfigMap,
    parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    acp_mcp_servers: Vec<crate::session::mcp_servers::AcpServerEntry>,
    support_permission: bool,
    telemetry_enabled: bool,
    auto_update: Option<bool>,
    persistence: PersistenceHandle,
    conversation: Vec<ConversationItem>,
    rewind_points_path: Option<std::path::PathBuf>,
    fs_notify_config: Option<ClientFsConfig>,
    initial_total_tokens: u64,
    startup_hints: StartupHints,
    client_type: ClientType,
    auto_compact_threshold_percent: u8,
    system_prompt_label: String,
    compaction_mode: xai_chat_state::CompactionMode,
    compaction_verbatim_input: bool,
    compaction_tool_choice: crate::util::config::CompactionToolChoice,
    two_pass_enabled: bool,
    buffering_settings: Option<BufferingSettings>,
    origin_client: Option<crate::http::OriginClientInfo>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    code_nav_enabled: bool,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
    client_caps: crate::session::notifications::SessionClientCaps,
    feedback_proxy_url: Option<String>,
    feedback_user_token: Option<String>,
    feedback_alpha_test_key: Option<String>,
    deployment_key: Option<String>,
    client_terminal_capable: bool,
    client_fs_capable: bool,
    gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    agent_definition: AgentDefinition,
    session_default_agent_profile: Option<String>,
    skills_config: SkillsConfig,
    preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    compat: CompatConfig,
    paths_config: xai_grok_agent::prompt::paths::PathsConfig,
    incremental_bash_output: bool,
    persisted_signals: Option<crate::session::signals::SessionSignals>,
    persisted_plan_mode: Option<crate::session::plan_mode::PlanModeSnapshot>,
    persisted_goal_mode: Option<crate::session::goal_tracker::GoalOrchestration>,
    persisted_workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
    persisted_announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    memory_config: Option<crate::config::MemoryConfig>,
    loc_tracking_enabled: bool,
    feedback_flags: crate::session::feedback_manager::FeedbackFlags,
    managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    managed_mcp_proxy_base_url: String,
    session_model_id: acp::ModelId,
    session_yolo_mode: bool,
    session_auto_mode: bool,
    session_client_identifier: Option<String>,
    inference_idle_timeout_secs: u64,
    max_retries: Option<u32>,
    subagent_rate_limit_max_attempts: u32,
    web_search_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
    web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig,
    image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig,
    video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig,
    app_builder_deployer_config: xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig,
    write_file_enabled: bool,
    active_agent_messages_enabled: bool,
    agent_message_sender: Option<
        xai_grok_tools::implementations::grok_build::task::types::AgentMessageSender,
    >,
    goal_enabled: bool,
    background_workflows_enabled: bool,
    subagents_enabled: bool,
    subagents_max_depth: u32,
    workflow_max_concurrent_agents: usize,
    media_gen_batch_limits: xai_grok_tools::media_gen_limits::MediaGenBatchLimits,
    ask_user_question_enabled: bool,
    client_hooks: crate::extensions::hooks::ClientHooks,
    prompt_display_cwd: Option<String>,
    subagent_toggle: std::collections::HashMap<String, bool>,
    persona_summaries: Vec<String>,
    prompt_audience: xai_grok_agent::prompt::context::PromptAudience,
    role_instructions: Option<String>,
    persona_instructions: Option<String>,
    disable_web_search: bool,
    backend_tools_enabled: bool,
    respect_gitignore: bool,
    path_not_found_hints: bool,
    tool_params_json: crate::session::agent_rebuild::ResolvedToolParamsJson,
    prefetch: crate::session::session_create_prefetch::SessionCreatePrefetch,
    plugin_registry_handle: Option<xai_grok_agent::plugins::SharedPluginRegistryHandle>,
    models_manager: crate::agent::remote_config::ModelsManager,
    parent_traceparent: Option<String>,
    inherited_permission_handle: Option<xai_grok_workspace::permission::PermissionHandle>,
    api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    image_description_model: String,
    hook_registry_override: Option<std::sync::Arc<xai_grok_hooks::discovery::HookRegistry>>,
    workspace_ops: xai_grok_workspace::WorkspaceOps,
    cli_permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    todo_gate: bool,
    remote_settings: Option<crate::util::config::RemoteSettings>,
    laziness_debug_log: Option<std::path::PathBuf>,
    parent_terminal_backend: Option<
        std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
    >,
    parent_scheduler_handle: Option<
        xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle,
    >,
    max_turns: Option<usize>,
    forked_tool_override: Option<Vec<ToolSpec>>,
    is_chat_kind: bool,
    spawn_ctx: Option<xai_grok_telemetry::subagent_spawn::SpawnPhaseContext>,
    sampling_gate: Option<Arc<tokio::sync::Semaphore>>,
    spawn_trace: Option<xai_grok_telemetry::startup::SpawnTraceContext>,
) -> Result<(SessionInitResult, SessionThread), acp::Error> {
    let (init_tx, init_rx) = tokio::sync::oneshot::channel::<
        Result<SessionInitResult, xai_grok_agent::AgentBuildError>,
    >();
    let sid = session_info.id.0.to_string();
    let sid_prefix = match sid.get(..sid.len().min(8)) {
        Some(prefix) => prefix,
        None => sid.as_str(),
    };
    let thread_name = format!("ses-{sid_prefix}");
    const SESSION_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
    let history_load_span = match spawn_trace.as_ref() {
        Some(ctx) => tracing::info_span!(parent: &ctx.parent, "spawn.history_load"),
        None => tracing::info_span!("spawn.history_load"),
    };
    let join_handle = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(SESSION_THREAD_STACK_SIZE)
        .spawn(move || {
            let (initial_last_compaction, initial_prompt_texts) = {
                let _history_load_span = history_load_span.entered();
                let session_dir = crate::session::persistence::session_dir(
                    &session_info,
                );
                let updates_path = session_dir.join("updates.jsonl");
                let initial_last_compaction = {
                    let _timer = crate::instrumentation_timer!(
                        "session.spawn_actor.find_compaction_checkpoint"
                    );
                    crate::session::helpers::replay::find_latest_compaction_checkpoint(
                            &updates_path,
                        )
                        .ok()
                        .flatten()
                        .map(|cp| cp.prompt_index_at_compaction)
                };
                let initial_prompt_texts = {
                    let _timer = crate::instrumentation_timer!("session.spawn_actor.load_user_prompts");
                    SessionActor::load_user_prompts_from_updates(&updates_path)
                        .unwrap_or_default()
                };
                (initial_last_compaction, initial_prompt_texts)
            };
            let rt = match build_session_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to build session runtime (resource exhaustion?)"
                    );
                    let _ = init_tx
                        .send(Err(xai_grok_agent::AgentBuildError::RuntimeBuild(e)));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let actor_main = async move {
                let _trace_span = parent_traceparent
                    .as_ref()
                    .map(|tp| {
                        let meta = serde_json::json!({ "traceparent": tp })
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        let span = xai_grok_otel::span_from_meta_traceparent(&meta);
                        span.entered()
                    });
                let session_spawn_span = match spawn_trace {
                    Some(ctx) => {
                        tracing::info_span!(
                        parent: &ctx.parent,
                        "session.spawn",
                        session_id = %session_info.id.0,
                        client_type = ?client_type,
                        start_type = if initial_prompt_texts.is_empty() { "new" } else { "resumed" },
                    )
                    }
                    None => {
                        tracing::info_span!(
                        "session.spawn",
                        session_id = %session_info.id.0,
                        client_type = ?client_type,
                        start_type = if initial_prompt_texts.is_empty() { "new" } else { "resumed" },
                    )
                    }
                };
                let actor_fut = spawn_session_actor(
                    session_info,
                    gateway,
                    sampling_config,
                    credentials,
                    auth_method_id,
                    auth_manager,
                    attribution_callback,
                    tool_context,
                    mcp_servers,
                    initial_client_mcp_servers,
                    mcp_meta_config_map,
                    parent_mcp_pool,
                    acp_mcp_servers,
                    support_permission,
                    telemetry_enabled,
                    auto_update,
                    persistence,
                    conversation,
                    rewind_points_path,
                    initial_last_compaction,
                    initial_prompt_texts,
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
                    origin_client,
                    codebase_indexes,
                    code_nav_enabled,
                    fs_watch_caps,
                    client_caps,
                    feedback_proxy_url,
                    feedback_user_token,
                    feedback_alpha_test_key,
                    deployment_key,
                    client_terminal_capable,
                    client_fs_capable,
                    gateway_enabled,
                    agent_definition,
                    session_default_agent_profile,
                    skills_config,
                    preloaded_skills,
                    compat,
                    paths_config,
                    incremental_bash_output,
                    persisted_signals,
                    persisted_plan_mode,
                    persisted_goal_mode,
                    persisted_workflow_runs,
                    persisted_announcement_state,
                    memory_config,
                    loc_tracking_enabled,
                    feedback_flags,
                    managed_mcp_handle,
                    managed_mcp_proxy_base_url,
                    session_model_id,
                    session_yolo_mode,
                    session_auto_mode,
                    session_client_identifier,
                    inference_idle_timeout_secs,
                    max_retries,
                    subagent_rate_limit_max_attempts,
                    web_search_sampling_config,
                    web_fetch_config,
                    image_gen_config,
                    video_gen_config,
                    app_builder_deployer_config,
                    write_file_enabled,
                    active_agent_messages_enabled,
                    agent_message_sender,
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
                    persona_summaries,
                    prompt_audience,
                    role_instructions,
                    persona_instructions,
                    disable_web_search,
                    backend_tools_enabled,
                    respect_gitignore,
                    path_not_found_hints,
                    tool_params_json,
                    prefetch,
                    plugin_registry_handle,
                    models_manager,
                    inherited_permission_handle,
                    api_key_provider,
                    image_description_model,
                    hook_registry_override,
                    workspace_ops,
                    cli_permission_rules,
                    todo_gate,
                    remote_settings,
                    laziness_debug_log,
                    parent_terminal_backend,
                    parent_scheduler_handle,
                    max_turns,
                    forked_tool_override,
                    is_chat_kind,
                    spawn_ctx,
                    sampling_gate,
                );
                let actor_result = actor_fut.instrument(session_spawn_span).await;
                let (init, session_done_rx) = match actor_result {
                    Ok(result) => result,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                let _ = init_tx.send(Ok(init));
                let _ = session_done_rx.await;
            };
            local.block_on(&rt, actor_main);
            rt.block_on(xai_grok_telemetry::session_ctx::drain_at_session_exit());
        });
    let join_handle = match join_handle {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                error = %e,
                "failed to spawn session thread (thread/PID limit or memory pressure?)"
            );
            return Err(
                acp::Error::internal_error().data(format!("failed to spawn session thread: {e}"))
            );
        }
    };
    let init = init_rx
        .await
        .map_err(|_| {
            tracing::error!("Session thread panicked during initialization");
            acp::Error::internal_error().data("session thread panicked during initialization")
        })?
        .map_err(|e| {
            acp::Error::internal_error().data(format!("session initialization failed: {e}"))
        })?;
    Ok((init, SessionThread { join_handle }))
}
/// Production [`crate::session::mcp_restart::RestartActions`] impl, captured by the dispatcher task when `mcp.auto_restart=true`.
pub(crate) struct SessionRestartActions {
    session: Arc<SessionActor>,
    shutdown: crate::session::mcp_dispatcher::SharedShutdownState,
}
impl SessionRestartActions {
    pub(crate) fn new(
        session: Arc<SessionActor>,
        shutdown: crate::session::mcp_dispatcher::SharedShutdownState,
    ) -> Self {
        Self { session, shutdown }
    }
}
#[async_trait::async_trait(?Send)]
impl crate::session::mcp_restart::RestartActions for SessionRestartActions {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn is_stdio_server_configured(&self, server: &str) -> bool {
        self.session.is_stdio_server_configured(server).await
    }
    fn is_in_shutting_down(&self, server: &str) -> bool {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_shutting_down(server)
    }
    #[tracing::instrument(level = "debug", skip_all)]
    async fn respawn_stdio(
        &self,
        server: &str,
    ) -> Result<crate::session::mcp_restart::Respawn, String> {
        self.session.respawn_stdio(server).await
    }
    #[tracing::instrument(level = "debug", skip_all)]
    async fn is_http_server_configured(&self, server: &str) -> bool {
        self.session.is_http_server_configured(server).await
    }
    #[tracing::instrument(level = "debug", skip_all)]
    async fn reset_http_client(&self, server: &str) -> Result<(), String> {
        self.session.reset_http_client(server).await
    }
    fn unregister_server_tools(&self, server: &str) {
        self.session.unregister_server_tools(server);
    }
    fn push_status(&self, payload: &crate::session::mcp_dispatcher::McpServerStatusPayload) {
        crate::session::mcp_restart::forward_status(&self.session.notifications.gateway, payload);
    }
    fn begin_restart(&self, server: &str) -> bool {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_restart(server.to_string())
    }
    fn end_restart(&self, server: &str) {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end_restart(server);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryStorageSelection {
    Enabled,
    DisabledByConfig,
    UnavailableForEphemeralWorkspace,
}
fn select_memory_storage(
    storage: Option<&crate::session::memory::MemoryStorage>,
    memory_config: Option<&crate::config::MemoryConfig>,
) -> MemoryStorageSelection {
    let Some((storage, config)) = storage.zip(memory_config) else {
        return MemoryStorageSelection::DisabledByConfig;
    };
    if storage.mode().is_v2() && storage.is_ephemeral() {
        return MemoryStorageSelection::UnavailableForEphemeralWorkspace;
    }
    let enabled = config.enabled
        && (storage.mode().is_legacy()
            || (config.v2.rollout != crate::config::MemoryV2Rollout::Off
                && config.v2.file_writes_enabled));
    if !enabled {
        return MemoryStorageSelection::DisabledByConfig;
    }
    MemoryStorageSelection::Enabled
}
fn memory_v2_exposed(memory_config: Option<&crate::config::MemoryConfig>) -> bool {
    memory_config.is_some_and(|config| config.mode.is_v2() && config.v2.can_expose_memory())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalBackendKind {
    ReuseParent,
    AcpClient,
    LocalPersistent,
    LocalNonPersistent,
}
fn select_terminal_backend_kind(
    is_subagent: bool,
    has_parent_backend: bool,
    client_terminal_capable: bool,
    has_gateway: bool,
    cursor_harness: bool,
) -> TerminalBackendKind {
    if is_subagent && has_parent_backend {
        TerminalBackendKind::ReuseParent
    } else if client_terminal_capable && has_gateway {
        TerminalBackendKind::AcpClient
    } else if cursor_harness {
        TerminalBackendKind::LocalPersistent
    } else {
        TerminalBackendKind::LocalNonPersistent
    }
}
/// Resume skips the prefix rebuild, so recover the flag from the loaded prefix. Fail-safe: any user item with both
/// `<user_info>` and the date marker counts as stamped (may over-keep the reminder, never suppresses a dated session).
fn resumed_prefix_carries_fallback_date(
    template_surfaces_local_date: bool,
    conversation: &[ConversationItem],
) -> bool {
    if template_surfaces_local_date {
        return false;
    }
    conversation.iter().any(|item| {
        let ConversationItem::User(u) = item else {
            return false;
        };
        let contains = |needle: &str| {
            u.content.iter().any(|part| {
                matches!(
                    part,
                    xai_grok_sampling_types::conversation::ContentPart::Text { text }
                        if text.contains(needle)
                )
            })
        };
        contains("<user_info>") && contains(crate::session::user_message::USER_INFO_DATE_MARKER)
    })
}
#[cfg(test)]
mod resumed_prefix_fallback_tests {
    use super::resumed_prefix_carries_fallback_date;
    use crate::session::user_message::USER_INFO_DATE_MARKER;
    use xai_grok_sampling_types::conversation::ConversationItem;
    #[test]
    fn resumed_prefix_fallback_detection_is_fail_safe() {
        let with_date = vec![ConversationItem::user(format!(
            "<user_info>\n{USER_INFO_DATE_MARKER} 2024-01-01\n</user_info>"
        ))];
        let without_date = vec![ConversationItem::user(
            "<user_info>\nWorkspace: /x\n</user_info>",
        )];
        let spoofed_leading_user_info = vec![
            ConversationItem::user("<user_info>\nWorkspace: /x\n</user_info>"),
            ConversationItem::user(format!(
                "<user_info>\n{USER_INFO_DATE_MARKER} 2024-01-01\n</user_info>"
            )),
        ];
        let leading_noise = vec![
            ConversationItem::user("project instructions: do the thing"),
            ConversationItem::user(format!(
                "<user_info>\n{USER_INFO_DATE_MARKER} 2024-01-01\n</user_info>"
            )),
        ];
        assert!(resumed_prefix_carries_fallback_date(false, &with_date));
        assert!(!resumed_prefix_carries_fallback_date(false, &without_date));
        assert!(resumed_prefix_carries_fallback_date(
            false,
            &spoofed_leading_user_info
        ));
        assert!(resumed_prefix_carries_fallback_date(false, &leading_noise));
        assert!(!resumed_prefix_carries_fallback_date(true, &with_date));
    }
}
#[cfg(test)]
mod memory_storage_select_tests {
    use super::{MemoryStorageSelection, memory_v2_exposed, select_memory_storage};
    use crate::config::{MemoryConfig, MemoryMode, MemoryV2Rollout};
    use crate::session::memory::MemoryStorage;
    use std::path::Path;
    fn config(mode: MemoryMode) -> MemoryConfig {
        MemoryConfig {
            enabled: true,
            mode,
            ..MemoryConfig::default()
        }
    }
    #[test]
    fn memory_v2_prompt_exposure_follows_rollout_stage() {
        let with_rollout = |rollout| {
            let mut config = config(MemoryMode::V2);
            config.v2.rollout = rollout;
            config
        };
        assert!(!memory_v2_exposed(None));
        assert!(!memory_v2_exposed(Some(&config(MemoryMode::Legacy))));
        assert!(!memory_v2_exposed(Some(&with_rollout(
            MemoryV2Rollout::Shadow
        ))));
        assert!(memory_v2_exposed(Some(&with_rollout(
            MemoryV2Rollout::Active
        ))));
    }
    #[test]
    fn v2_storage_is_unavailable_only_for_ephemeral_workspaces() {
        let temp_cwd = std::env::temp_dir().join("qa-scratch");
        let v2_temp = MemoryStorage::new_for_mode(&temp_cwd, None, MemoryMode::V2);
        assert!(v2_temp.is_ephemeral());
        assert_eq!(
            select_memory_storage(Some(&v2_temp), Some(&config(MemoryMode::V2))),
            MemoryStorageSelection::UnavailableForEphemeralWorkspace
        );
        let mut disabled_v2 = config(MemoryMode::V2);
        disabled_v2.enabled = false;
        assert_eq!(
            select_memory_storage(Some(&v2_temp), Some(&disabled_v2)),
            MemoryStorageSelection::UnavailableForEphemeralWorkspace
        );
        let legacy_temp = MemoryStorage::new_for_mode(&temp_cwd, None, MemoryMode::Legacy);
        assert_eq!(
            select_memory_storage(Some(&legacy_temp), Some(&config(MemoryMode::Legacy))),
            MemoryStorageSelection::Enabled
        );
        let v2_project =
            MemoryStorage::new_for_mode(Path::new("/home/user/project"), None, MemoryMode::V2);
        assert_eq!(
            select_memory_storage(Some(&v2_project), Some(&config(MemoryMode::V2))),
            MemoryStorageSelection::Enabled
        );
    }
}
#[cfg(test)]
mod terminal_backend_select_tests {
    use super::{TerminalBackendKind, select_terminal_backend_kind};
    #[test]
    fn subagent_with_parent_reuses_parent() {
        assert_eq!(
            select_terminal_backend_kind(true, true, true, true, true),
            TerminalBackendKind::ReuseParent
        );
    }
    #[test]
    fn subagent_without_parent_falls_through() {
        assert_eq!(
            select_terminal_backend_kind(true, false, true, true, true),
            TerminalBackendKind::AcpClient
        );
        assert_eq!(
            select_terminal_backend_kind(true, false, false, true, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn non_subagent_never_reuses_parent() {
        assert_eq!(
            select_terminal_backend_kind(false, true, false, false, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn client_terminal_uses_acp_only_with_gateway() {
        assert_eq!(
            select_terminal_backend_kind(false, false, true, true, true),
            TerminalBackendKind::AcpClient
        );
        assert_eq!(
            select_terminal_backend_kind(false, false, true, false, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn local_session_cursor_harness_selects_persistent_backend() {
        assert_eq!(
            select_terminal_backend_kind(false, false, false, false, true),
            TerminalBackendKind::LocalPersistent
        );
        assert_eq!(
            select_terminal_backend_kind(false, false, false, false, false),
            TerminalBackendKind::LocalNonPersistent
        );
    }
}
