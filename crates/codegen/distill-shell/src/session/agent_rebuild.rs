// Modified for Distill by Samuel Fajreldines, 2026.
//! `AgentRebuildSpec` is the canonical recipe for constructing an [`distill_agent::Agent`] for a given session.
//!
//! INVARIANT: This is the **only** place in the shell crate that calls [`distill_agent::AgentBuilder::new`].
//! Initial session spawn ([`crate::session::acp_session::spawn_session_actor`]) goes through [`AgentRebuildSpec::build_agent`].
//! So does the zero-turn harness rebuild ([`crate::session::acp_session::SessionActor::handle_rebuild_agent_for_definition`]).
//!
//! ## Why this exists
//!
//! [`distill_agent::Agent`] owns an [`distill_tools::bridge::ToolBridge`] that carries session-scoped channels.
//! Those are the notification handle, terminal/fs backends, subagent senders, scheduler set, plugin registry, and attribution callback.
//! The Agent is therefore session-bound: it cannot be shared across sessions and cannot be re-rendered from outside its session context.
//! A rebuild happens, for example, when the user picks a model with a different `agent_type` before sending any user message.
//! To rebuild, we must retain every input that the original `AgentBuilder` chain consumed.
//! `AgentRebuildSpec` is exactly that retained bag of inputs.
//!
//! ## WHEN ADDING A NEW [`distill_agent::AgentBuilder`]`::with_*` KNOB
//!
//! 1. Add the corresponding field to [`AgentRebuildSpec`].
//! 2. Pass it through in [`AgentRebuildSpec::build_agent`].
//!    The destructure pattern at the top of `build_agent` forces every field to be used.
//!    A forgotten field is a compile error (`#[deny(unused_variables)]`).
//! 3. Populate the field at the call site in `spawn_session_actor`.
//!
//! ## Why some fields are channel senders
//!
//! Several `ToolBridge` resources (e.g. `UserQuestionSender`, `SubagentBackendResource`) are backed by the `tx` halves of channels.
//! The `rx` halves are owned by long-lived coordinator tasks spawned in `spawn_session_actor`.
//! The subagent channels are wrapped in a `ChannelBackend` behind `SubagentBackendResource`.
//! On rebuild, we must reuse the **same** senders so the existing coordinator keeps receiving requests.
//! A fresh channel would orphan the running coordinator.
use distill_agent::config::AgentDefinition;
use distill_agent::error::AgentBuildError;
use distill_agent::prompt::context::PromptAudience;
use distill_agent::prompt::skills::SkillsConfig;
use distill_agent::{Agent, AgentBuilder, CompactionPolicy, ReminderPolicy};
use distill_tools::computer::types::{AsyncFileSystem, TerminalBackend};
use distill_tools::implementations::distill::app_builder::AppBuilderDeployerConfig;
use distill_tools::implementations::distill::ask_user_question::types::UserQuestionRequest;
use distill_tools::implementations::distill::image_gen::ImageGenConfig;
use distill_tools::implementations::distill::monitor::types::MonitorEventBuffer;
use distill_tools::implementations::distill::task::types::{
    AgentMessageSender, SubagentCapabilityModeExt, SubagentEvent, TaskModelValidator,
};
use distill_tools::implementations::distill::video_gen::VideoGenConfig;
use distill_tools::implementations::distill::web_fetch::WebFetchConfig;
use distill_tools::implementations::lsp::LspBackend;
use distill_tools::implementations::web_search::WebSearchConfig;
use distill_tools::notification::ToolNotificationHandle;
use distill_tools::types::SharedApiKeyProvider;
use distill_tools::types::compat::CompatConfig;
use distill_tools::types::memory_backend::MemoryBackend;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
/// Shell-resolved per-tool `ToolConfig.params` JSON maps.
/// The struct keeps the spawn functions to a single argument instead of adjacent identically-typed positional arguments a caller could transpose.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedToolParamsJson {
    /// `[toolset.bash]` overrides for the bash tool(s).
    pub bash: Option<serde_json::Map<String, serde_json::Value>>,
    /// `[toolset.ask_user_question]` timeout policy for the ask tool.
    pub ask_user_question: Option<serde_json::Map<String, serde_json::Value>>,
}
/// The live memory-v2 file-access policy shared by spawn, the `/memory` toggle, and rebuilds.
/// The lock never escapes: readers get a clone, writers replace the value.
pub(crate) struct MemoryV2AccessSlot(
    parking_lot::Mutex<Option<distill_tools::types::memory_v2::MemoryV2AccessResource>>,
);
impl MemoryV2AccessSlot {
    pub(crate) fn new(
        access: Option<distill_tools::types::memory_v2::MemoryV2AccessResource>,
    ) -> Self {
        Self(parking_lot::Mutex::new(access))
    }
    pub(crate) fn get(&self) -> Option<distill_tools::types::memory_v2::MemoryV2AccessResource> {
        self.0.lock().clone()
    }
    pub(crate) fn set(
        &self,
        access: Option<distill_tools::types::memory_v2::MemoryV2AccessResource>,
    ) {
        *self.0.lock() = access;
    }
}
/// Cached recipe for building a session-scoped [`Agent`].
/// See module docs for the invariant: this is the only construction site for `Agent` in the shell crate.
/// Cloning is intentionally not derived; the spec lives behind an [`Arc`] and is shared by cloning that `Arc`.
pub(crate) struct AgentRebuildSpec {
    pub working_directory: PathBuf,
    pub terminal_backend: Arc<dyn TerminalBackend>,
    pub fs_backend: Arc<dyn AsyncFileSystem>,
    pub tools_notification_handle: ToolNotificationHandle,
    pub bridge_state_path: PathBuf,
    pub session_env: Arc<HashMap<String, String>>,
    pub models_manager: crate::agent::remote_config::ModelsManager,
    pub compaction_policy: CompactionPolicy,
    pub reminder_policy: ReminderPolicy,
    pub memory_enabled: bool,
    pub memory_global_path: Option<String>,
    pub memory_workspace_path: Option<String>,
    pub memory_backend: Option<Arc<dyn MemoryBackend>>,
    /// Live v2 file-access policy. `None` while v2 memory is off; the `/memory` toggle
    /// replaces it so a later zero-turn rebuild renders the same prompt as a fresh spawn.
    pub memory_v2_access: MemoryV2AccessSlot,
    pub memory_v2_exposed: bool,
    pub web_search_config: WebSearchConfig,
    /// `[toolset.web_search]` domain policy, resolved once at spawn.
    /// It is applied to both search paths (the hosted `tool_overrides` merge and the client-side `WebSearchConfig`) so they never diverge.
    pub web_search_domains: Option<distill_sampling_types::WebSearchOptions>,
    pub backend_search: bool,
    pub web_fetch_config: WebFetchConfig,
    pub image_gen_config: ImageGenConfig,
    pub video_gen_config: VideoGenConfig,
    pub app_builder_deployer_config: AppBuilderDeployerConfig,
    pub media_gen_batch_limits: distill_tools::media_gen_limits::MediaGenBatchLimits,
    pub write_file_enabled: bool,
    pub active_agent_messages_enabled: bool,
    pub subagents_enabled: bool,
    pub subagent_toggle: HashMap<String, bool>,
    pub background_workflows_enabled: bool,
    pub ask_user_question_enabled: bool,
    pub persona_summaries: Vec<String>,
    pub prompt_audience: PromptAudience,
    pub role_instructions: Option<String>,
    pub persona_instructions: Option<String>,
    pub skills_config: SkillsConfig,
    /// Resolved vendor-compat config (from `Config::compat_resolved`), threaded into skills / rules / AGENTS.md discovery via the builder.
    pub compat: CompatConfig,
    /// `[paths]` config, threaded into rules discovery via the builder.
    pub paths_config: distill_agent::prompt::paths::PathsConfig,
    pub context_window_tokens: u64,
    pub prompt_working_directory: Option<String>,
    pub lsp: Option<Arc<dyn LspBackend>>,
    pub plugin_registry: Option<Arc<distill_agent::plugins::PluginRegistry>>,
    pub api_key_provider: Option<SharedApiKeyProvider>,
    pub attribution_callback: Option<distill_tools::SharedAttributionCallback>,
    pub tool_params_json: ResolvedToolParamsJson,
    pub subagent_event_tx: Option<UnboundedSender<SubagentEvent>>,
    pub subagent_coordinator_sender:
        Option<distill_tools::implementations::distill::task::backend::SubagentCoordinatorSender>,
    pub agent_message_sender: Option<AgentMessageSender>,
    pub monitor_event_buffer: Option<MonitorEventBuffer>,
    pub user_question_tx: UnboundedSender<UserQuestionRequest>,
    pub subagent_depth: u32,
    pub subagents_max_depth: u32,
    pub session_id_str: String,
    pub blocking_wait_depth: Arc<crate::tools::tool_context::BlockingWaitState>,
    pub respect_gitignore: bool,
    pub path_not_found_hints: bool,
    /// Fire side of the scheduler mode.
    /// Keep the two on one resolve.
    pub mcp_state: Arc<tokio::sync::Mutex<crate::session::mcp_servers::McpState>>,
    pub managed_gateway_tool_client:
        Option<distill_tools::types::resources::ManagedGatewayToolClient>,
    pub is_non_interactive: bool,
    pub owner_session_id: Option<String>,
    pub parent_scheduler_handle:
        Option<distill_tools::implementations::distill::scheduler::types::SchedulerHandle>,
}
impl AgentRebuildSpec {
    /// This is the canonical construction path; see module docs for the invariant.
    #[deny(unused_variables)]
    pub(crate) async fn build_agent(
        self: &Arc<Self>,
        definition: AgentDefinition,
        system_prompt_label: impl Into<String>,
    ) -> Result<Agent, AgentBuildError> {
        let (agent, _build_elapsed) = self
            .build_agent_inner(definition, system_prompt_label.into(), None, None)
            .await?;
        Ok(agent)
    }
    /// `persisted_skill_names`: restored into the `SkillManager` before `seed()` to prevent duplicate system-reminder injection on resume.
    /// `preloaded_skills`: parent-discovered skills passed to `AgentBuilder::with_preloaded_skills()` to bypass filesystem discovery in subagents.
    /// Returns the built agent and the pure construction time (entry to `SB_BUILDER_DONE`, before the batched resource seed).
    pub(crate) async fn build_agent_with_initial_overrides(
        self: &Arc<Self>,
        definition: AgentDefinition,
        system_prompt_label: impl Into<String>,
        persisted_skill_names: Option<std::collections::HashSet<String>>,
        preloaded_skills: Option<Vec<distill_tools::implementations::skills::types::SkillInfo>>,
    ) -> Result<(Agent, std::time::Duration), AgentBuildError> {
        self.build_agent_inner(
            definition,
            system_prompt_label.into(),
            persisted_skill_names,
            preloaded_skills,
        )
        .await
    }
    #[deny(unused_variables)]
    async fn build_agent_inner(
        self: &Arc<Self>,
        definition: AgentDefinition,
        system_prompt_label: String,
        persisted_skill_names: Option<std::collections::HashSet<String>>,
        preloaded_skills: Option<Vec<distill_tools::implementations::skills::types::SkillInfo>>,
    ) -> Result<(Agent, std::time::Duration), AgentBuildError> {
        let build_phase_start = std::time::Instant::now();
        let Self {
            working_directory,
            terminal_backend,
            fs_backend,
            tools_notification_handle,
            bridge_state_path,
            session_env,
            models_manager,
            compaction_policy,
            reminder_policy,
            memory_enabled,
            memory_global_path,
            memory_workspace_path,
            memory_backend,
            memory_v2_access,
            memory_v2_exposed,
            web_search_config,
            web_search_domains,
            backend_search,
            web_fetch_config,
            image_gen_config,
            video_gen_config,
            app_builder_deployer_config,
            media_gen_batch_limits: _,
            write_file_enabled,
            active_agent_messages_enabled,
            subagents_enabled,
            subagent_toggle,
            background_workflows_enabled,
            ask_user_question_enabled,
            persona_summaries,
            prompt_audience,
            role_instructions,
            persona_instructions,
            skills_config,
            compat,
            paths_config,
            context_window_tokens,
            prompt_working_directory,
            lsp,
            plugin_registry,
            api_key_provider,
            attribution_callback,
            tool_params_json,
            subagent_event_tx,
            subagent_coordinator_sender,
            agent_message_sender,
            monitor_event_buffer,
            user_question_tx,
            subagent_depth,
            subagents_max_depth,
            session_id_str,
            blocking_wait_depth,
            respect_gitignore,
            path_not_found_hints,
            mcp_state,
            managed_gateway_tool_client,
            is_non_interactive,
            owner_session_id,
            parent_scheduler_handle,
        } = self.as_ref();
        let _ = mcp_state;
        if (*prompt_audience == PromptAudience::Subagent || agent_message_sender.is_some())
            && subagent_coordinator_sender.is_some()
        {
            return Err(AgentBuildError::InvalidConfig(
                "a sender-scoped child cannot hold the subagent coordinator sender".to_owned(),
            ));
        }
        let active_agent_messages_enabled = *active_agent_messages_enabled
            && (*prompt_audience == PromptAudience::Primary
                || (agent_message_sender.is_some()
                    && definition.capability_mode.is_none_or(|mode| {
                        mode.allows_tool_kind(
                            distill_tools::types::tool::ToolKind::ActiveAgentMessage,
                        )
                    })));
        #[allow(unused_variables)]
        let is_cursor_template =
            crate::session::is_cursor_system_template(&definition.system_prompt);
        let mut definition = definition;
        if let Some(cfg_opts) = web_search_domains.clone() {
            definition
                .tool_overrides
                .get_or_insert_with(Default::default)
                .web_search = Some(cfg_opts);
        }
        let session_env = {
            let mut env = session_env.as_ref().clone();
            env.insert("GROK_SESSION_ID".to_string(), session_id_str.clone());
            Arc::new(env)
        };
        let mut builder = AgentBuilder::new(
            working_directory.clone(),
            terminal_backend.clone(),
            tools_notification_handle.clone(),
        )
        .from_definition(definition)
        .with_compaction_policy(compaction_policy.clone())
        .with_reminder_policy(reminder_policy.clone())
        .with_memory_enabled(*memory_enabled)
        .with_memory_paths(memory_global_path.clone(), memory_workspace_path.clone())
        .with_memory_v2_access(memory_v2_access.get(), *memory_v2_exposed)
        .with_is_non_interactive(*is_non_interactive)
        .with_system_prompt_label(system_prompt_label)
        .with_session_env(session_env.clone())
        .with_state_path(bridge_state_path.clone())
        .with_web_search_config(web_search_config.clone())
        .with_backend_search(*backend_search)
        .with_image_gen_config(image_gen_config.clone())
        .with_video_gen_config(video_gen_config.clone())
        .with_app_builder_deployer_config(app_builder_deployer_config.clone())
        .with_web_fetch_config(web_fetch_config.clone())
        .with_write_file_enabled(*write_file_enabled)
        .with_active_agent_messages_enabled(active_agent_messages_enabled)
        .with_fs(fs_backend.clone())
        .with_subagents_enabled(*subagents_enabled)
        .with_child_nested_subagents_allowed(1 < *subagents_max_depth)
        .with_subagent_toggle(subagent_toggle.clone())
        .with_background_workflows_enabled(*background_workflows_enabled)
        .with_task_model_slugs(
            models_manager
                .available()
                .keys()
                .map(|model_id| model_id.0.to_string())
                .collect::<Vec<_>>(),
        )
        .with_ask_user_question_enabled(*ask_user_question_enabled)
        .with_persona_summaries(persona_summaries.clone())
        .with_prompt_audience(*prompt_audience)
        .with_role_instructions(role_instructions.clone())
        .with_persona_instructions(persona_instructions.clone())
        .with_skills_config(skills_config.clone())
        .with_compat_config(*compat)
        .with_paths_config(paths_config.clone())
        .with_project_trusted(crate::agent::folder_trust::project_scope_allowed(
            working_directory,
        ))
        .with_context_window(*context_window_tokens)
        .with_mcp_max_output_bytes(
            crate::util::config::resolve_max_mcp_output_bytes_for_cwd(working_directory),
        );
        if let Some(owner_session_id) = owner_session_id.clone() {
            builder = builder.with_owner_session_id(owner_session_id);
        }
        if let Some(handle) = parent_scheduler_handle.clone() {
            builder = builder.with_parent_scheduler_handle(handle);
        }
        if let Some(memory_backend) = memory_backend.clone() {
            builder = builder.with_memory_backend(memory_backend);
        }
        if let Some(lsp) = lsp.clone() {
            builder = builder.with_lsp(lsp);
        }
        if let Some(plugin_registry) = plugin_registry.clone() {
            builder = builder.with_plugin_registry(plugin_registry);
        }
        if let Some(api_key_provider) = api_key_provider.clone() {
            builder = builder.with_api_key_provider(api_key_provider);
        }
        if let Some(attribution_callback) = attribution_callback.clone() {
            builder = builder.with_attribution_callback(attribution_callback);
        }
        if let Some(bash_params_json) = tool_params_json.bash.clone() {
            builder = builder.with_bash_params(bash_params_json);
        }
        if let Some(ask_user_question_params_json) = tool_params_json.ask_user_question.clone() {
            builder = builder.with_ask_user_question_params(ask_user_question_params_json);
        }
        if let Some(prompt_working_directory) = prompt_working_directory.clone() {
            builder = builder.with_prompt_working_directory(prompt_working_directory);
        }
        if let Some(names) = persisted_skill_names {
            builder = builder.with_persisted_announced_skill_names(names);
        }
        if let Some(skills) = preloaded_skills {
            builder = builder.with_preloaded_skills(skills);
        }
        let agent = builder.build().await?;
        crate::waterfall::mark(session_id_str, crate::waterfall::stage::SB_BUILDER_DONE);
        let agent_build_elapsed = build_phase_start.elapsed();
        let model_validator = models_manager.clone();
        agent
            .tool_bridge()
            .update_resources_with(|resources| {
                resources
                    .insert(
                        TaskModelValidator::new(move |requested| {
                            model_validator.task_model_error(requested)
                        }),
                    );
                if let Some(event_tx) = subagent_event_tx.clone() {
                    use distill_tools::implementations::distill::task::backend::{
                        ChannelBackend, SubagentBackendResource,
                    };
                    use distill_tools::implementations::distill::task::types::{
                        AgentMessageSenderResource, MaxSubagentDepth, SessionIdResource,
                        SubagentDepthCounter, SubagentEventSender,
                    };
                    resources
                        .insert(
                            SubagentBackendResource(
                                Arc::new(
                                    subagent_coordinator_sender
                                        .as_ref()
                                        .map_or_else(
                                            || ChannelBackend::for_session(
                                                event_tx.clone(),
                                                session_id_str.clone(),
                                            ),
                                            |sender| {
                                                ChannelBackend::for_coordinator_session(
                                                        sender.clone(),
                                                        session_id_str.clone(),
                                                    )
                                                    .with_root_targets()
                                            },
                                        ),
                                ),
                            ),
                        );
                    resources.insert(SubagentDepthCounter(*subagent_depth));
                    resources.insert(MaxSubagentDepth(*subagents_max_depth));
                    resources.insert(SessionIdResource(session_id_str.clone()));
                    resources.insert(SubagentEventSender(event_tx));
                    if active_agent_messages_enabled
                        && let Some(sender) = agent_message_sender.clone()
                    {
                        resources.insert(AgentMessageSenderResource(sender));
                    }
                    resources
                        .insert(
                            crate::tools::tool_context::subagent_foreground_wait(
                                Arc::clone(blocking_wait_depth),
                            ),
                        );
                    if let Some(buffer) = monitor_event_buffer.clone() {
                        resources.insert(buffer);
                    }
                }
                resources
                    .insert(
                        distill_tools::types::resources::RespectGitignore(
                            *respect_gitignore,
                        ),
                    );
                resources
                    .insert(
                        distill_tools::types::resources::PathNotFoundHints(
                            *path_not_found_hints,
                        ),
                    );
                if let Some(client) = managed_gateway_tool_client.clone() {
                    resources.insert(client);
                }
                {
                    use distill_tools::implementations::distill::ask_user_question::UserQuestionSender;
                    resources.insert(UserQuestionSender(user_question_tx.clone()));
                }
            })
            .await;
        Ok((agent, agent_build_elapsed))
    }
}
/// Every field is set to a minimal default suitable for test `SessionActor` literals and focused `build_agent` tests.
#[cfg(test)]
pub(crate) fn test_rebuild_spec_default() -> Arc<AgentRebuildSpec> {
    let (uq_tx, _uq_rx) = tokio::sync::mpsc::unbounded_channel();
    Arc::new(AgentRebuildSpec {
        working_directory: std::env::temp_dir(),
        terminal_backend: Arc::new(
            distill_tools::computer::local::LocalTerminalBackend::new_local(
                distill_tools::computer::local::SearchShadowConfig::default(),
            ),
        ),
        fs_backend: Arc::new(distill_tools::computer::local::LocalFs),
        tools_notification_handle: ToolNotificationHandle::noop(),
        bridge_state_path: std::env::temp_dir().join("test_tool_state.json"),
        session_env: Arc::new(HashMap::new()),
        models_manager: crate::agent::remote_config::ModelsManager::default(),
        compaction_policy: CompactionPolicy::default(),
        reminder_policy: ReminderPolicy::default(),
        memory_enabled: false,
        memory_global_path: None,
        memory_workspace_path: None,
        memory_backend: None,
        memory_v2_access: MemoryV2AccessSlot::new(None),
        memory_v2_exposed: false,
        web_search_config: WebSearchConfig::default(),
        web_search_domains: None,
        backend_search: false,
        web_fetch_config: WebFetchConfig::Disabled,
        image_gen_config: ImageGenConfig::default(),
        video_gen_config: VideoGenConfig::default(),
        app_builder_deployer_config: AppBuilderDeployerConfig::default(),
        media_gen_batch_limits: distill_tools::media_gen_limits::MediaGenBatchLimits::default(),
        write_file_enabled: true,
        active_agent_messages_enabled: false,
        subagents_enabled: false,
        subagent_toggle: HashMap::new(),
        background_workflows_enabled: false,
        ask_user_question_enabled: true,
        persona_summaries: vec![],
        prompt_audience: PromptAudience::Primary,
        role_instructions: None,
        persona_instructions: None,
        skills_config: SkillsConfig::default(),
        compat: CompatConfig::default(),
        paths_config: Default::default(),
        context_window_tokens: 256_000,
        prompt_working_directory: None,
        lsp: None,
        plugin_registry: None,
        api_key_provider: None,
        attribution_callback: None,
        tool_params_json: ResolvedToolParamsJson::default(),
        subagent_event_tx: None,
        subagent_coordinator_sender: None,
        agent_message_sender: None,
        monitor_event_buffer: None,
        user_question_tx: uq_tx,
        subagent_depth: 0,
        subagents_max_depth: distill_tools::implementations::distill::task::MAX_SUBAGENT_DEPTH,
        session_id_str: "test-session".to_string(),
        blocking_wait_depth: Arc::new(crate::tools::tool_context::BlockingWaitState::new()),
        respect_gitignore: false,
        path_not_found_hints: false,
        mcp_state: Arc::new(tokio::sync::Mutex::new(
            crate::session::mcp_servers::McpState::new(vec![]),
        )),
        managed_gateway_tool_client: None,
        is_non_interactive: false,
        owner_session_id: Some("test-session".to_string()),
        parent_scheduler_handle: None,
    })
}
#[cfg(test)]
mod legacy_tests {
    use super::*;
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    fn model_entry(internal_id: &str) -> ModelEntry {
        ModelEntry::fallback(internal_id, &EndpointsConfig::default())
    }
    fn task_description(agent: &Agent) -> String {
        let toolset = agent.tool_bridge().toolset();
        let task_name = toolset
            .tool_name_for_kind(distill_tools::types::tool::ToolKind::Task)
            .expect("Distill Task tool should be present");
        toolset
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.function.name == task_name)
            .and_then(|definition| definition.function.description)
            .expect("Distill Task description should be present")
    }
    /// The `[toolset.web_search]` policy is authoritative on the backend-hosted path.
    /// Agent frontmatter is model-writable (`.grok/agents/*.md`), so a configured blocklist must survive a frontmatter allowlist.
    #[tokio::test(flavor = "current_thread")]
    async fn config_web_search_domains_beat_agent_frontmatter() {
        use distill_sampling_types::{HostedTool, ToolOverrides, WebSearchOptions};
        let frontmatter = WebSearchOptions {
            allowed_domains: Some(vec!["attacker.example".into()]),
            excluded_domains: None,
        };
        let configured = WebSearchOptions {
            allowed_domains: None,
            excluded_domains: Some(vec!["reddit.com".into()]),
        };
        let definition = || {
            let mut d = AgentDefinition::default_distill();
            d.tool_overrides = Some(ToolOverrides {
                x_search: None,
                web_search: Some(frontmatter.clone()),
            });
            d
        };
        let spec_with = |domains: Option<WebSearchOptions>| {
            let mut spec = test_rebuild_spec_default();
            let spec_mut =
                Arc::get_mut(&mut spec).expect("test rebuild spec should be uniquely owned");
            spec_mut.web_search_domains = domains;
            spec_mut.backend_search = true;
            spec_mut.web_search_config = WebSearchConfig::Enabled {
                api_key: "test-key".to_string(),
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4".to_string(),
                extra_headers: Default::default(),
                alpha_test_key: None,
                allowed_domains: None,
                excluded_domains: None,
            };
            spec
        };
        tokio::task::LocalSet::new()
            .run_until(async {
                let agent = spec_with(Some(configured.clone()))
                    .build_agent(definition(), distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL)
                    .await
                    .expect("agent build should succeed");
                assert_eq!(
                    agent
                        .definition()
                        .tool_overrides
                        .as_ref()
                        .and_then(|o| o.web_search.clone()),
                    Some(configured.clone()),
                    "config must replace the frontmatter allowlist"
                );
                assert!(
                    agent.hosted_tools().contains(&HostedTool::WebSearch {
                        options: Some(configured.clone()),
                    }),
                    "the hosted tool carries the configured policy: {:?}",
                    agent.hosted_tools()
                );
                let agent = spec_with(None)
                    .build_agent(definition(), distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL)
                    .await
                    .expect("agent build should succeed");
                assert_eq!(
                    agent
                        .definition()
                        .tool_overrides
                        .as_ref()
                        .and_then(|o| o.web_search.clone()),
                    Some(frontmatter.clone())
                );
            })
            .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rebuild_projects_fresh_public_model_keys_into_task_description() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut spec = test_rebuild_spec_default();
                Arc::get_mut(&mut spec)
                    .expect("test rebuild spec should be uniquely owned")
                    .subagents_enabled = true;
                let models_manager = spec.models_manager.clone();
                models_manager
                    .insert_test_entry("zeta-public", model_entry("internal-zeta"));
                models_manager
                    .insert_test_entry("alpha-public", model_entry("internal-alpha"));
                let mut hidden = model_entry("internal-hidden");
                hidden.info.hidden = true;
                models_manager.insert_test_entry("private-hidden-model", hidden);
                let mut unselectable = model_entry("internal-unselectable");
                unselectable.info.user_selectable = false;
                models_manager
                    .insert_test_entry("private-unselectable-model", unselectable);
                let first = spec
                    .build_agent(
                        AgentDefinition::default_distill(),
                        distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    )
                    .await
                    .expect("first agent build should succeed");
                let first_description = task_description(&first);
                assert!(
                    first_description.contains(
                        "If the user explicitly asks for the model of a subagent/task, you may ONLY use model slugs from this list:\n\
                         - alpha-public\n\
                         - zeta-public"
                    )
                );
                assert!(!first_description.contains("private-hidden-model"));
                assert!(!first_description.contains("private-unselectable-model"));
                assert!(!first_description.contains("internal-alpha"));
                let validator = first
                    .tool_bridge()
                    .toolset()
                    .get_resource_cloned::<TaskModelValidator>()
                    .await
                    .expect("Task model validator should be registered");
                assert!(validator.error_for("alpha-public").is_none());
                assert!(validator.error_for("private-hidden-model").is_some());
                models_manager
                    .insert_test_entry("beta-public", model_entry("internal-beta"));
                assert!(validator.error_for("beta-public").is_none());
                let rebuilt = spec
                    .build_agent(
                        AgentDefinition::default_distill(),
                        distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    )
                    .await
                    .expect("rebuilt agent should succeed");
                let rebuilt_description = task_description(&rebuilt);
                assert!(
                    rebuilt_description.contains(
                        "If the user explicitly asks for the model of a subagent/task, you may ONLY use model slugs from this list:\n\
                         - alpha-public\n\
                         - beta-public\n\
                         - zeta-public"
                    )
                );
            })
            .await;
    }
}
#[cfg(test)]
#[path = "agent_rebuild_tests.rs"]
mod tests;
