// Modified for Distill by Samuel Fajreldines, 2026.
use crate::agent::Agent;
use crate::compaction::CompactionPolicy;
use crate::config::{AGENT_TASK_CLASSIFIER_RE, short_tool_name, tool_id_eq, tool_id_matches};
use crate::config::{AgentDefinition, BuiltinAgentName, PermissionMode, PromptMode};
use crate::discovery::{SubagentEntry, SubagentSource};
use crate::error::AgentBuildError;
use crate::prompt::context::{PromptAudience, PromptContext};
use crate::system_reminder::ReminderPolicy;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::Instrument;
use distill_tools::bridge::ToolBridge;
use distill_tools::computer::types::{AsyncFileSystem, TerminalBackend};
use distill_tools::notification::ToolNotificationHandle;
use distill_tools::registry::types::SessionContext;
use distill_tools::types::tool::ToolKind;
/// The Grok [`ToolKind`] a vendor-compat `tools:` allowlist entry resolves to, so a plugin's upstream allowlist still binds.
/// Backed by the shared vendor-to-Grok tool registry in `distill-tools` (also used by the hook matcher).
fn claude_tool_kind(name: &str) -> Option<ToolKind> {
    distill_tools::types::kind_for(name)
}
/// Builds an [`Agent`] from an [`AgentDefinition`] (`from_definition`) or programmatic `with_*` calls, plus session context.
/// `Clone` lets a primary build preview its child types with the same session context.
#[derive(Clone)]
pub struct AgentBuilder {
    working_directory: PathBuf,
    /// Forked sessions: the real `working_directory` is an overlay/worktree path that must stay hidden from the model,
    /// so the system prompt shows this instead; tool execution keeps the real path.
    prompt_working_directory: Option<String>,
    terminal_backend: Arc<dyn TerminalBackend>,
    fs_backend: Arc<dyn AsyncFileSystem>,
    notification_handle: ToolNotificationHandle,
    owner_session_id: Option<String>,
    parent_scheduler_handle:
        Option<distill_tools::implementations::distill::scheduler::types::SchedulerHandle>,
    definition: Option<AgentDefinition>,
    /// Pre-rendered persona IO summaries for the task tool description.
    persona_summaries: Vec<String>,
    prompt_audience: crate::prompt::context::PromptAudience,
    role_instructions: Option<String>,
    persona_instructions: Option<String>,
    name: Option<String>,
    description: Option<String>,
    prompt_mode: PromptMode,
    tools: Option<Vec<String>>,
    disallowed_tools: Vec<String>,
    skill_names: Vec<String>,
    permission_mode: PermissionMode,
    agents_md: bool,
    custom_system_prompt: Option<String>,
    compaction_policy: CompactionPolicy,
    reminder_policy: ReminderPolicy,
    memory_enabled: bool,
    memory_v2_enabled: bool,
    memory_global_path: Option<String>,
    memory_workspace_path: Option<String>,
    memory_v2_access: Option<distill_tools::types::memory_v2::MemoryV2AccessResource>,
    is_non_interactive: bool,
    system_prompt_label: String,
    session_env: Option<Arc<HashMap<String, String>>>,
    state_path: Option<PathBuf>,
    memory_backend: Option<Arc<dyn distill_tools::types::memory_backend::MemoryBackend>>,
    web_search_config: distill_tools::implementations::web_search::WebSearchConfig,
    /// When true, web search and X search go to the agentic sampler as native server-side tools instead of registering as local Function tools.
    backend_search: bool,
    web_fetch_config: distill_tools::implementations::distill::web_fetch::WebFetchConfig,
    lsp: Option<std::sync::Arc<dyn distill_tools::implementations::lsp::LspBackend>>,
    image_gen_config: distill_tools::implementations::distill::image_gen::ImageGenConfig,
    video_gen_config: distill_tools::implementations::distill::video_gen::VideoGenConfig,
    app_builder_deployer_config:
        distill_tools::implementations::distill::app_builder::AppBuilderDeployerConfig,
    write_file_enabled: bool,
    active_agent_messages_enabled: bool,
    subagents_enabled: bool,
    /// Whether a depth-1 child may itself spawn; decides whether the child previews keep the Task tool.
    child_nested_subagents_allowed: bool,
    background_workflows_enabled: bool,
    ask_user_question_enabled: bool,
    subagent_toggle: HashMap<String, bool>,
    task_model_slugs: Vec<String>,
    skills_config: crate::prompt::skills::SkillsConfig,
    /// Which vendor (`.claude`/`.cursor`) dirs are scanned for skills / rules / AGENTS.md; the all-on default reproduces historical behavior.
    compat: distill_tools::types::compat::CompatConfig,
    /// `[paths]` table; its `extra_rule_dirs` are scanned for rules alongside the built-in home roots.
    paths_config: crate::prompt::paths::PathsConfig,
    bash_params_json: Option<serde_json::Map<String, serde_json::Value>>,
    ask_user_question_params_json: Option<serde_json::Map<String, serde_json::Value>>,
    plugin_registry: Option<std::sync::Arc<crate::plugins::PluginRegistry>>,
    context_window_tokens: Option<u64>,
    api_key_provider: Option<distill_tools::types::SharedApiKeyProvider>,
    attribution_callback: Option<distill_tools::SharedAttributionCallback>,
    /// Seeded into the toolset's `TruncationCfg` after finalize; the MCP truncation path consults it before the process-global
    /// cap. Set only when the repo-level `[mcp] max_output_bytes` tier wins (see `resolve_max_mcp_output_bytes_for_cwd`).
    mcp_max_output_bytes: Option<usize>,
    /// IDE-compat agent_type uses `"system_reminder"` instead of the default `"system-reminder"`.
    system_reminder_tag: &'static str,
    /// Restored into the SkillManager before `seed()`, which then skips the `BaselineChange` pending, so a resumed
    /// session does not re-inject a duplicate system-reminder.
    persisted_announced_skill_names: Option<std::collections::HashSet<String>>,
    /// Parent-inherited skills; `build()` uses them instead of running `list_skills_with_plugins()`.
    preloaded_skills: Option<Vec<distill_tools::implementations::skills::types::SkillInfo>>,
    /// Startup folder-trust verdict; hosts without this gate must opt in explicitly.
    project_trusted: bool,
}
fn ensure_plan_mode_tools(tool_config: &mut distill_tools::registry::types::ToolServerConfig) {
    use distill_tools::implementations::distill;
    let existing: std::collections::HashSet<&str> =
        tool_config.tools.iter().map(|tc| tc.id.as_str()).collect();
    let missing_enter = !existing.contains("Distill:enter_plan_mode");
    let missing_exit = !existing.contains("Distill:exit_plan_mode");
    let missing_ask = !existing.contains("Distill:ask_user_question");
    drop(existing);
    if missing_enter {
        tool_config
            .tools
            .push((&distill::EnterPlanModeTool).into());
    }
    if missing_exit {
        tool_config
            .tools
            .push((&distill::ExitPlanModeTool).into());
    }
    if missing_ask {
        tool_config
            .tools
            .push((&distill::AskUserQuestionTool).into());
    }
}
/// Single copy of the params-merge loop the per-tool param injections share.
fn merge_tool_params(
    tool_config: &mut distill_tools::registry::types::ToolServerConfig,
    ids: &[&str],
    map: &serde_json::Map<String, serde_json::Value>,
) {
    for tc in &mut tool_config.tools {
        if ids.contains(&tc.id.as_str()) {
            let params = tc.params.get_or_insert_with(serde_json::Map::new);
            for (k, v) in map {
                params.insert(k.clone(), v.clone());
            }
        }
    }
}
fn apply_workflow_tool_gates(
    tool_config: &mut distill_tools::registry::types::ToolServerConfig,
    background_workflows_enabled: bool,
) {
    use distill_tools::types::tool::ToolKind;
    if background_workflows_enabled {
        tool_config
            .tools
            .retain(|tool| tool.kind != Some(ToolKind::GoalUpdate));
    } else {
        tool_config
            .tools
            .retain(|tool| tool.kind != Some(ToolKind::Workflow));
    }
}
impl AgentBuilder {
    pub fn new(
        working_directory: PathBuf,
        terminal_backend: Arc<dyn TerminalBackend>,
        notification_handle: ToolNotificationHandle,
    ) -> Self {
        Self {
            working_directory,
            prompt_working_directory: None,
            terminal_backend,
            fs_backend: Arc::new(distill_tools::computer::local::LocalFs),
            notification_handle,
            owner_session_id: None,
            parent_scheduler_handle: None,
            definition: None,
            persona_summaries: Vec::new(),
            prompt_audience: crate::prompt::context::PromptAudience::Primary,
            role_instructions: None,
            persona_instructions: None,
            name: None,
            description: None,
            prompt_mode: PromptMode::Extend,
            tools: None,
            disallowed_tools: vec![],
            skill_names: vec![],
            permission_mode: PermissionMode::Default,
            agents_md: true,
            custom_system_prompt: None,
            compaction_policy: CompactionPolicy::default(),
            reminder_policy: ReminderPolicy::default(),
            memory_enabled: false,
            memory_v2_enabled: false,
            memory_global_path: None,
            memory_workspace_path: None,
            memory_v2_access: None,
            is_non_interactive: false,
            system_prompt_label: crate::prompt::context::DEFAULT_SYSTEM_PROMPT_LABEL.to_string(),
            session_env: None,
            state_path: None,
            memory_backend: None,
            web_search_config: Default::default(),
            backend_search: false,
            web_fetch_config: Default::default(),
            lsp: None,
            image_gen_config: Default::default(),
            video_gen_config: Default::default(),
            app_builder_deployer_config: Default::default(),
            write_file_enabled: true,
            active_agent_messages_enabled: false,
            subagents_enabled: false,
            child_nested_subagents_allowed: false,
            background_workflows_enabled: false,
            ask_user_question_enabled: true,
            subagent_toggle: HashMap::new(),
            task_model_slugs: Vec::new(),
            skills_config: Default::default(),
            compat: Default::default(),
            paths_config: Default::default(),
            bash_params_json: None,
            ask_user_question_params_json: None,
            plugin_registry: None,
            context_window_tokens: None,
            api_key_provider: None,
            attribution_callback: None,
            mcp_max_output_bytes: None,
            system_reminder_tag: distill_tools::reminders::DEFAULT_REMINDER_TAG,
            persisted_announced_skill_names: None,
            preloaded_skills: None,
            project_trusted: false,
        }
    }
    pub fn with_project_trusted(mut self, project_trusted: bool) -> Self {
        self.project_trusted = project_trusted;
        self
    }
    pub fn with_persisted_announced_skill_names(
        mut self,
        names: std::collections::HashSet<String>,
    ) -> Self {
        self.persisted_announced_skill_names = Some(names);
        self
    }
    pub fn with_preloaded_skills(
        mut self,
        skills: Vec<distill_tools::implementations::skills::types::SkillInfo>,
    ) -> Self {
        self.preloaded_skills = Some(skills);
        self
    }
    pub fn from_definition(mut self, def: AgentDefinition) -> Self {
        self.definition = Some(def);
        self
    }
    pub fn with_persona_summaries(mut self, summaries: Vec<String>) -> Self {
        self.persona_summaries = summaries;
        self
    }
    pub fn with_prompt_audience(
        mut self,
        audience: crate::prompt::context::PromptAudience,
    ) -> Self {
        self.prompt_audience = audience;
        self
    }
    pub fn with_role_instructions(mut self, instructions: Option<String>) -> Self {
        self.role_instructions = instructions;
        self
    }
    pub fn with_persona_instructions(mut self, instructions: Option<String>) -> Self {
        self.persona_instructions = instructions;
        self
    }
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
    pub fn with_prompt_mode(mut self, mode: PromptMode) -> Self {
        self.prompt_mode = mode;
        self
    }
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = Some(tools);
        self
    }
    pub fn with_disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.disallowed_tools = tools;
        self
    }
    pub fn with_skills(mut self, skill_names: Vec<String>) -> Self {
        self.skill_names = skill_names;
        self
    }
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }
    pub fn with_agents_md(mut self, enabled: bool) -> Self {
        self.agents_md = enabled;
        self
    }
    pub fn with_custom_system_prompt(mut self, prompt: String) -> Self {
        self.custom_system_prompt = Some(prompt);
        self
    }
    pub fn with_compaction_policy(mut self, policy: CompactionPolicy) -> Self {
        self.compaction_policy = policy;
        self
    }
    pub fn with_memory_enabled(mut self, enabled: bool) -> Self {
        self.memory_enabled = enabled;
        self
    }
    pub fn with_memory_paths(
        mut self,
        global_path: Option<String>,
        workspace_path: Option<String>,
    ) -> Self {
        self.memory_global_path = global_path;
        self.memory_workspace_path = workspace_path;
        self
    }
    pub fn with_memory_v2_access(
        mut self,
        access: Option<distill_tools::types::memory_v2::MemoryV2AccessResource>,
        exposed: bool,
    ) -> Self {
        if let Some(access) = access.as_ref() {
            let [global_root, workspace_root] = access.0.scope_roots();
            self.memory_global_path = Some(global_root.to_string_lossy().into_owned());
            self.memory_workspace_path = Some(workspace_root.to_string_lossy().into_owned());
        }
        self.memory_v2_enabled = exposed && access.is_some();
        self.memory_v2_access = access;
        self
    }
    /// Suppresses prompt sections that assume a human at the TUI prompt, and stamps the ask_user_question params so an
    /// unanswered questionnaire returns no-operator text instead of "user declined".
    pub fn with_is_non_interactive(mut self, value: bool) -> Self {
        self.is_non_interactive = value;
        self
    }
    pub fn with_system_prompt_label(mut self, label: impl Into<String>) -> Self {
        self.system_prompt_label = label.into();
        self
    }
    pub fn with_reminder_policy(mut self, policy: ReminderPolicy) -> Self {
        self.reminder_policy = policy;
        self
    }
    pub fn with_session_env(mut self, env: Arc<HashMap<String, String>>) -> Self {
        self.session_env = Some(env);
        self
    }
    pub fn with_mcp_max_output_bytes(mut self, bytes: Option<usize>) -> Self {
        self.mcp_max_output_bytes = bytes;
        self
    }
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = Some(path);
        self
    }
    /// Without a backend, `memory_search` / `memory_get` return "Memory is not enabled".
    pub fn with_memory_backend(
        mut self,
        backend: Arc<dyn distill_tools::types::memory_backend::MemoryBackend>,
    ) -> Self {
        self.memory_backend = Some(backend);
        self
    }
    /// ACP-backed when the client advertises `clientCapabilities.fs.readTextFile`/`writeTextFile`; `LocalFs` default.
    pub fn with_fs(mut self, fs: Arc<dyn AsyncFileSystem>) -> Self {
        self.fs_backend = fs;
        self
    }
    /// Set the session ID that owns processes spawned by this session's tools.
    pub fn with_owner_session_id(mut self, id: String) -> Self {
        self.owner_session_id = Some(id);
        self
    }
    /// Share the parent's scheduler handle so scheduled tasks survive subagent exit.
    pub fn with_parent_scheduler_handle(
        mut self,
        handle: distill_tools::implementations::distill::scheduler::types::SchedulerHandle,
    ) -> Self {
        self.parent_scheduler_handle = Some(handle);
        self
    }
    /// `Enabled` injects a `WebSearchClient` resource so `web_search` can call the Responses API; `Disabled` returns a graceful error.
    pub fn with_web_search_config(
        mut self,
        config: distill_tools::implementations::web_search::WebSearchConfig,
    ) -> Self {
        self.web_search_config = config;
        self
    }
    /// Per-model gating is applied at request time, not here.
    pub fn with_backend_search(mut self, enabled: bool) -> Self {
        self.backend_search = enabled;
        self
    }
    /// `Disabled` (default) does not register the tool; flagged via remote `web_fetch_enabled` and the `GROK_WEB_FETCH` env.
    pub fn with_web_fetch_config(
        mut self,
        config: distill_tools::implementations::distill::web_fetch::WebFetchConfig,
    ) -> Self {
        self.web_fetch_config = config;
        self
    }
    pub fn with_lsp(
        mut self,
        handle: std::sync::Arc<dyn distill_tools::implementations::lsp::LspBackend>,
    ) -> Self {
        self.lsp = Some(handle);
        self
    }
    pub fn with_image_gen_config(
        mut self,
        config: distill_tools::implementations::distill::image_gen::ImageGenConfig,
    ) -> Self {
        self.image_gen_config = config;
        self
    }
    pub fn with_video_gen_config(
        mut self,
        config: distill_tools::implementations::distill::video_gen::VideoGenConfig,
    ) -> Self {
        self.video_gen_config = config;
        self
    }
    pub fn with_app_builder_deployer_config(
        mut self,
        config: distill_tools::implementations::distill::app_builder::AppBuilderDeployerConfig,
    ) -> Self {
        self.app_builder_deployer_config = config;
        self
    }
    pub fn with_api_key_provider(
        mut self,
        provider: distill_tools::types::SharedApiKeyProvider,
    ) -> Self {
        self.api_key_provider = Some(provider);
        self
    }
    /// A 401 from `image_gen` / `video_gen` / `web_search` emits `auth_401_attribution` with a per-consumer tag. Pass the
    /// same `ShellAttribution` wired into the sampler so all 401s share one `AuthManager` and land in the same dataset.
    pub fn with_attribution_callback(
        mut self,
        callback: distill_tools::SharedAttributionCallback,
    ) -> Self {
        self.attribution_callback = Some(callback);
        self
    }
    /// Harnesses trained on a different tag variant call this so reminders match what their model was trained on.
    pub fn with_system_reminder_tag(mut self, tag: &'static str) -> Self {
        self.system_reminder_tag = tag;
        self
    }
    pub fn with_write_file_enabled(mut self, enabled: bool) -> Self {
        self.write_file_enabled = enabled;
        self
    }
    /// Gates the `send_subagent_message` tool (default: disabled).
    pub fn with_active_agent_messages_enabled(mut self, enabled: bool) -> Self {
        self.active_agent_messages_enabled = enabled;
        self
    }
    /// Disabled strips the `TaskTool` from the tool config, so the model cannot spawn child agent sessions.
    pub fn with_subagents_enabled(mut self, enabled: bool) -> Self {
        self.subagents_enabled = enabled;
        self
    }
    pub fn with_child_nested_subagents_allowed(mut self, allowed: bool) -> Self {
        self.child_nested_subagents_allowed = allowed;
        self
    }
    pub fn with_background_workflows_enabled(mut self, enabled: bool) -> Self {
        self.background_workflows_enabled = enabled;
        self
    }
    /// Advertised in the Distill Task description.
    pub fn with_task_model_slugs(mut self, slugs: Vec<String>) -> Self {
        self.task_model_slugs = slugs;
        self
    }
    /// Subagents never receive the tool; when disabled it is stripped after the `ensure_plan_mode_tools` injection.
    /// Gated by the shell-resolved feature (remote/config/env kill-switch) and the pager's `--no-ask-user`.
    pub fn with_ask_user_question_enabled(mut self, enabled: bool) -> Self {
        self.ask_user_question_enabled = enabled;
        self
    }
    /// `[subagents.toggle]`: omitted agents default to enabled; controls Task-description listing and spawn-time acceptance.
    pub fn with_subagent_toggle(mut self, toggle: HashMap<String, bool>) -> Self {
        self.subagent_toggle = toggle;
        self
    }
    /// Threaded into startup discovery and the dynamic-discovery seeds (`SkillManager` / `AgentsMdTracker`).
    pub fn with_compat_config(
        mut self,
        compat: distill_tools::types::compat::CompatConfig,
    ) -> Self {
        self.compat = compat;
        self
    }
    pub fn with_paths_config(mut self, config: crate::prompt::paths::PathsConfig) -> Self {
        self.paths_config = config;
        self
    }
    /// Without this, only auto-discovered skill dirs load and custom paths added via `x.ai/skills/add` would be ignored.
    pub fn with_skills_config(mut self, config: crate::prompt::skills::SkillsConfig) -> Self {
        self.skills_config = config;
        self
    }
    /// Inject `[toolset.bash]` overrides from config.toml into bash tool params.
    pub fn with_bash_params(mut self, params: serde_json::Map<String, serde_json::Value>) -> Self {
        self.bash_params_json = Some(params);
        self
    }
    /// Inject the shell-resolved `[toolset.ask_user_question]` params (timeout policy) into the ask_user_question tool.
    pub fn with_ask_user_question_params(
        mut self,
        params: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.ask_user_question_params_json = Some(params);
        self
    }
    pub fn with_plugin_registry(
        mut self,
        registry: std::sync::Arc<crate::plugins::PluginRegistry>,
    ) -> Self {
        self.plugin_registry = Some(registry);
        self
    }
    pub fn with_context_window(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }
    pub fn with_prompt_working_directory(mut self, cwd: String) -> Self {
        self.prompt_working_directory = Some(cwd);
        self
    }
    fn resolve_definition(&self) -> AgentDefinition {
        if let Some(ref def) = self.definition {
            return def.clone();
        }
        let mut def = AgentDefinition::default_distill();
        if let Some(ref name) = self.name {
            def.name = name.clone();
        }
        if let Some(ref desc) = self.description {
            def.description = desc.clone();
        }
        def.prompt_mode = self.prompt_mode.clone();
        def.permission_mode = self.permission_mode.clone();
        def.agents_md = self.agents_md;
        if let Some(ref prompt) = self.custom_system_prompt {
            def.prompt_body = Some(prompt.clone());
        }
        if !self.skill_names.is_empty() {
            def.skills = self.skill_names.clone();
        }
        if let Some(ref tools) = self.tools {
            def.tools = tools.clone();
        }
        def.disallowed_tools = self.disallowed_tools.clone();
        def
    }
    /// One child build per advertised built-in type, so the parent's task description lists the tools a child spawned from
    /// this session actually gets. A failed preview keeps the static fragment for that type.
    async fn preview_child_tools(&self, subagents: &[SubagentEntry]) -> ChildToolNames {
        let mut previews = ChildToolNames::new();
        for entry in subagents {
            let SubagentSource::Builtin(child) = &entry.source else {
                continue;
            };
            if !BuiltinAgentName::subagent_variants().contains(child) {
                continue;
            }
            match self.preview_child_tool_names(*child).await {
                Ok(preview) => {
                    previews.insert(*child, preview);
                }
                Err(e) => {
                    tracing::warn!(
                        child = %child,
                        error = %e,
                        "child toolset preview failed; advertising the static tool list"
                    )
                }
            }
        }
        previews
    }
    /// Builds `child` the way a spawn would, minus discovery and persistence, and reads the client names off its bridge.
    async fn preview_child_tool_names(
        &self,
        child: BuiltinAgentName,
    ) -> Result<ChildToolPreview, AgentBuildError> {
        use distill_tools::implementations::distill::task::types::{
            SubagentCapabilityModeExt, prune_orphaned_background_task_tools,
        };
        debug_assert!(self.prompt_audience == PromptAudience::Primary);
        let mut definition = child.definition();
        definition.agents_md = false;
        if !self.child_nested_subagents_allowed {
            definition
                .tool_config
                .tools
                .retain(|tool| tool.kind != Some(ToolKind::Task));
            prune_orphaned_background_task_tools(&mut definition.tool_config);
        }
        let mut preview = self.clone();
        preview.state_path = None;
        preview.preloaded_skills = Some(Vec::new());
        preview.persisted_announced_skill_names = None;
        let agent = Box::pin(
            preview
                .from_definition(definition)
                .with_prompt_audience(PromptAudience::Subagent)
                .with_subagents_enabled(true)
                .with_ask_user_question_enabled(false)
                .build(),
        )
        .await?;
        let bridge = agent.tool_bridge();
        let read_only_kinds = distill_tool_types::SubagentCapabilityMode::ReadOnly.allowed_tool_kinds();
        let mut names = Vec::new();
        let mut read_only = true;
        for tool in bridge.tool_definitions_builtins_only().await {
            let kind = bridge.tool_kind(&tool.function.name);
            read_only &= kind.is_some_and(|kind| read_only_kinds.contains(&kind));
            names.push(tool.function.name);
        }
        Ok(ChildToolPreview { names, read_only })
    }
    pub async fn build(mut self) -> Result<Agent, AgentBuildError> {
        let mut definition = self.resolve_definition();
        let working_dir_str = self.working_directory.to_str().unwrap_or(".").to_string();
        let skill_info = if let Some(preloaded) = self.preloaded_skills.take() {
            preloaded
        } else if definition.discover_skills {
            let skills_span = tracing::info_span!(
                "spawn.skills_discovery",
                skills_found = tracing::field::Empty
            );
            let discovered = crate::prompt::skills::list_skills_with_plugins(
                Some(&working_dir_str),
                &self.skills_config,
                self.plugin_registry.as_deref(),
                self.compat,
                self.project_trusted,
            )
            .instrument(skills_span.clone())
            .await;
            skills_span.record("skills_found", discovered.len() as i64);
            discovered
        } else {
            vec![]
        };
        let preloaded_skill_paths: std::collections::HashSet<String> = if !definition
            .skills
            .is_empty()
        {
            let preloaded =
                crate::prompt::skills::resolve_preloaded_skills(&definition.skills, &skill_info)
                    .await;
            let paths = preloaded.iter().map(|s| s.path.clone()).collect();
            if !preloaded.is_empty() {
                let injection = crate::prompt::skills::format_skills_for_injection(&preloaded);
                if !injection.is_empty() {
                    definition.prompt_body =
                        Some(injection + &definition.prompt_body.unwrap_or_default());
                }
            }
            paths
        } else {
            std::collections::HashSet::new()
        };
        let tool_bridge_builder = ToolBridge::get_builder();
        let state_path = self.state_path.clone().unwrap_or_default();
        let mut tool_config = definition.tool_config.clone();
        if !definition.inject_default_tools && tool_config.tools.is_empty() {
            return Err(AgentBuildError::InvalidConfig(format!(
                "agent '{}' declares a curated toolset (inject_default_tools = false) \
                 but its tool list is empty; if the toolset is a registry preset \
                 (e.g. the external harness), the provider crate's register() must run \
                 at process startup before any agent is built",
                definition.name
            )));
        }
        let is_parent_distill = matches!(
            definition.builtin_name,
            Some(
                BuiltinAgentName::Distill
                    | BuiltinAgentName::DistillPlan
                    | BuiltinAgentName::DistillPlanNoSubagents
                    | BuiltinAgentName::DistillAskUser
            )
        );
        if self.prompt_audience == PromptAudience::Primary
            && is_parent_distill
            && !tool_config
                .tools
                .iter()
                .any(|tool| tool.kind == Some(ToolKind::Feedback))
        {
            tool_config
                .tools
                .push((&distill_tools::implementations::distill::SendFeedbackTool).into());
        }
        if definition.inject_default_tools {
            if self.memory_backend.is_some() {
                use distill_tools::implementations::memory;
                tool_config
                    .tools
                    .push((&memory::search_tool::MemorySearchImpl).into());
                tool_config
                    .tools
                    .push((&memory::get_tool::MemoryGetImpl).into());
            }
            if self.web_search_config.is_enabled() {
                use distill_tools::implementations::distill;
                tool_config.tools.push((&distill::WebSearchTool).into());
            }
            if self.web_fetch_config.is_enabled() {
                use distill_tools::implementations::distill;
                tool_config.tools.push((&distill::WebFetchTool).into());
            }
            if self.lsp.is_some() {
                tool_config
                    .tools
                    .push((&distill_tools::implementations::distill::LspTool).into());
            }
            if self.image_gen_config.image_gen_enabled() {
                tool_config
                    .tools
                    .push((&distill_tools::implementations::distill::ImageGenTool).into());
            }
            if self.image_gen_config.image_edit_enabled() {
                tool_config
                    .tools
                    .push((&distill_tools::implementations::distill::ImageEditTool).into());
            }
            if self.video_gen_config.is_enabled() {
                tool_config
                    .tools
                    .push((&distill_tools::implementations::distill::ImageToVideoTool).into());
                tool_config.tools.push(
                    (&distill_tools::implementations::distill::ReferenceToVideoTool).into(),
                );
            }
            let has_write_tool = tool_config
                .tools
                .iter()
                .any(|tc| tc.id.ends_with(":write") || tc.id.ends_with(":Write"));
            let known_kinds = tool_bridge_builder.known_tool_kinds();
            let has_edit_tool = tool_config.tools.iter().any(|tc| {
                tc.kind.or_else(|| known_kinds.get(&tc.id).copied()) == Some(ToolKind::Edit)
            });
            if self.write_file_enabled && !has_write_tool && has_edit_tool {
                tool_config
                    .tools
                    .push((&distill_tools::implementations::opencode::OpenCodeWriteTool).into());
            }
            if self.prompt_audience == PromptAudience::Primary {
                ensure_plan_mode_tools(&mut tool_config);
            }
        }
        let active_agent_message = distill_tools::registry::types::ToolConfig::for_tool::<
            distill_tools::implementations::distill::SendSubagentMessageTool,
        >();
        let is_active_agent_message = |tool: &distill_tools::registry::types::ToolConfig| {
            tool.kind == Some(ToolKind::ActiveAgentMessage) || tool.id == active_agent_message.id
        };
        use distill_tools::implementations::distill::task::types::SubagentCapabilityModeExt;
        let within_capability_ceiling = self.prompt_audience == PromptAudience::Primary
            || definition
                .capability_mode
                .is_none_or(|mode| mode.allows_tool_kind(ToolKind::ActiveAgentMessage));
        let can_inject_active_agent_message = self.active_agent_messages_enabled
            && within_capability_ceiling
            && definition.inject_default_tools;
        if can_inject_active_agent_message {
            if !tool_config.tools.iter().any(is_active_agent_message) {
                tool_config.tools.push(active_agent_message);
            }
        } else if !self.active_agent_messages_enabled || !within_capability_ceiling {
            tool_config
                .tools
                .retain(|tool| !is_active_agent_message(tool));
        }
        if self.memory_backend.is_none() {
            let distill_ns = distill_tools::types::tool::ToolNamespace::Distill.to_string();
            let mem_search_id = format!(
                "{distill_ns}:{}",
                distill_tools::implementations::memory::MEMORY_SEARCH_TOOL_NAME
            );
            let mem_get_id = format!(
                "{distill_ns}:{}",
                distill_tools::implementations::memory::MEMORY_GET_TOOL_NAME
            );
            tool_config
                .tools
                .retain(|tc| tc.id != mem_search_id && tc.id != mem_get_id);
        }
        if self.prompt_audience == crate::prompt::context::PromptAudience::Subagent {
            let feedback_id = distill_tools::registry::types::ToolConfig::for_tool::<
                distill_tools::implementations::distill::SendFeedbackTool,
            >()
            .id;
            let feedback_name =
                distill_tools::implementations::distill::SEND_FEEDBACK_TOOL_NAME;
            tool_config.tools.retain(|tool| {
                !matches!(
                    tool.kind,
                    Some(
                        distill_tools::types::tool::ToolKind::AskUser
                            | distill_tools::types::tool::ToolKind::Feedback
                    )
                ) && tool.id != feedback_id
                    && tool.id != feedback_name
                    && tool.name_override.as_deref() != Some(feedback_name)
            });
        } else if !self.ask_user_question_enabled {
            let ask_user_id = format!(
                "{}:ask_user_question",
                distill_tools::types::tool::ToolNamespace::Distill,
            );
            tool_config.tools.retain(|tool| tool.id != ask_user_id);
        }
        apply_workflow_tool_gates(&mut tool_config, self.background_workflows_enabled);
        let task_tool_id = format!(
            "{}:{}",
            distill_tools::types::tool::ToolNamespace::Distill,
            "task"
        );
        let mut task_stripped = false;
        if !self.subagents_enabled {
            tool_config.tools.retain(|tc| tc.id != task_tool_id);
            task_stripped = true;
        } else {
            let subagents = crate::discovery::all_subagents_with_plugins(
                &self.working_directory,
                &self.subagent_toggle,
                self.plugin_registry.as_deref(),
            );
            if subagents.is_empty() {
                tool_config.tools.retain(|tc| tc.id != task_tool_id);
                task_stripped = true;
            } else if self.prompt_audience == crate::prompt::context::PromptAudience::Subagent {
                if let Some(task_tc) = tool_config
                    .tools
                    .iter_mut()
                    .find(|tc| tc.id == task_tool_id)
                {
                    task_tc.description_override = Some(CHILD_TASK_DESCRIPTION.to_string());
                }
            } else if let Some(task_tc) = tool_config
                .tools
                .iter_mut()
                .find(|tc| tc.id == task_tool_id)
            {
                let preview_span = tracing::info_span!(
                    "spawn.child_tool_preview",
                    previews = tracing::field::Empty
                );
                let child_tools = self
                    .preview_child_tools(&subagents)
                    .instrument(preview_span.clone())
                    .await;
                preview_span.record("previews", child_tools.len() as i64);
                task_tc.description_override = Some(build_task_description(
                    &subagents,
                    &self.task_model_slugs,
                    &child_tools,
                ));
            }
        }
        if task_stripped {
            use distill_tools::types::tool::ToolNamespace;
            let has_satisfier = |ns: ToolNamespace, id: &str, needs_bg: bool| {
                let fq = format!("{ns}:{id}");
                tool_config.tools.iter().any(|tc| {
                    tc.id == fq
                        && (!needs_bg
                            || tc
                                .params
                                .as_ref()
                                .and_then(|p| p.get("enabled_background"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true))
                })
            };
            if !has_satisfier(ToolNamespace::Distill, "run_terminal_cmd", true)
                && !has_satisfier(ToolNamespace::DistillConcise, "run_terminal_cmd", true)
                && !has_satisfier(ToolNamespace::OpenCode, "bash", false)
            {
                let lifecycle = [
                    "get_task_output",
                    "wait_tasks",
                    "kill_task",
                    "scheduler_create",
                    "scheduler_delete",
                    "scheduler_list",
                ];
                tool_config
                    .tools
                    .retain(|tc| !lifecycle.contains(&short_tool_name(&tc.id)));
            }
        }
        if let distill_tools::implementations::distill::web_fetch::WebFetchConfig::Enabled {
            ref params,
        } = self.web_fetch_config
            && let Ok(params_value) = serde_json::to_value(params)
            && let Some(obj) = params_value.as_object()
        {
            merge_tool_params(&mut tool_config, &["Distill:web_fetch"], obj);
        }
        if let Some(ref bash_params) = self.bash_params_json {
            merge_tool_params(
                &mut tool_config,
                &[
                    "Distill:run_terminal_cmd",
                    "DistillConcise:run_terminal_cmd",
                ],
                bash_params,
            );
        }
        if let Some(ref ask_params) = self.ask_user_question_params_json {
            merge_tool_params(
                &mut tool_config,
                &["Distill:ask_user_question"],
                ask_params,
            );
        }
        if self.is_non_interactive {
            let mut ni = serde_json::Map::new();
            ni.insert("non_interactive".into(), serde_json::Value::Bool(true));
            merge_tool_params(&mut tool_config, &["Distill:ask_user_question"], &ni);
        }
        if !definition.disallowed_tools.is_empty() {
            let before: std::collections::HashSet<String> =
                tool_config.tools.iter().map(|tc| tc.id.clone()).collect();
            tool_config
                .tools
                .retain(|tc| !tool_id_matches(&definition.disallowed_tools, &tc.id));
            let after: std::collections::HashSet<String> =
                tool_config.tools.iter().map(|tc| tc.id.clone()).collect();
            let removed: std::collections::HashSet<&String> = before.difference(&after).collect();
            for d in &definition.disallowed_tools {
                if AGENT_TASK_CLASSIFIER_RE.is_match(d) {
                    continue;
                }
                let matched = removed.iter().any(|&id| tool_id_eq(d, id));
                if !matched {
                    tracing::warn!(agent = %definition.name, tool = %d, "disallowedTools entry matched nothing");
                }
            }
        }
        if !definition.tools.is_empty() {
            let has_agent_entry = definition
                .tools
                .iter()
                .any(|t| AGENT_TASK_CLASSIFIER_RE.is_match(t));
            let task_deps = ["task", "get_task_output", "kill_task", "wait_tasks"];
            let registered_tool_ids = tool_bridge_builder.known_tool_ids();
            let present_kinds: std::collections::HashSet<ToolKind> =
                tool_config.tools.iter().filter_map(|tc| tc.kind).collect();
            let mut allow_kinds: std::collections::HashSet<ToolKind> =
                std::collections::HashSet::new();
            let mut unresolved: Vec<&str> = Vec::new();
            let mut recognized_but_unavailable: Vec<&str> = Vec::new();
            for t in &definition.tools {
                if AGENT_TASK_CLASSIFIER_RE.is_match(t) {
                    continue;
                }
                if t.starts_with("mcp__") {
                    continue;
                }
                if tool_config.tools.iter().any(|tc| tool_id_eq(t, &tc.id)) {
                    continue;
                }
                match claude_tool_kind(t) {
                    Some(kind) => {
                        if present_kinds.contains(&kind) {
                            allow_kinds.insert(kind);
                        } else {
                            recognized_but_unavailable.push(t);
                        }
                    }
                    None if registered_tool_ids.iter().any(|id| tool_id_eq(t, id)) => {
                        recognized_but_unavailable.push(t);
                    }
                    None => unresolved.push(t),
                }
            }
            if !recognized_but_unavailable.is_empty() {
                tracing::debug!(
                    agent = %definition.name,
                    recognized_but_unavailable = ?recognized_but_unavailable,
                    "tools allowlist named recognized tools that aren't enabled; ignoring them"
                );
            }
            if unresolved.is_empty() {
                tool_config.tools.retain(|tc| {
                    tool_id_matches(&definition.tools, &tc.id)
                        || tc.kind.is_some_and(|k| allow_kinds.contains(&k))
                        || (has_agent_entry && task_deps.contains(&short_tool_name(&tc.id)))
                        || matches!(tc.kind, Some(ToolKind::SearchTool | ToolKind::UseTool))
                });
                tracing::debug!(agent = %definition.name, allowed = ?definition.tools, "tools allowlist applied");
            } else {
                tracing::warn!(
                    agent = %definition.name,
                    unresolved = ?unresolved,
                    allowed = ?definition.tools,
                    "tools allowlist had unmappable entries; keeping full grok toolset"
                );
            }
        }
        tool_config
            .tools
            .retain(|tc| definition.session_tools_allowed(&tc.id));
        {
            let mut saw_directive = false;
            let types: Vec<String> = definition
                .tools
                .iter()
                .filter_map(|t| {
                    let caps = AGENT_TASK_CLASSIFIER_RE.captures(t)?;
                    saw_directive = true;
                    caps.get(1)
                })
                .flat_map(|m| m.as_str().split(','))
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            let types = {
                let mut seen = std::collections::HashSet::new();
                types
                    .into_iter()
                    .filter(|t| seen.insert(t.clone()))
                    .collect::<Vec<_>>()
            };
            definition.allowed_subagent_types = if !saw_directive && !definition.tools.is_empty() {
                Some(vec![])
            } else if types.is_empty() {
                None
            } else {
                Some(types)
            };
        }
        if !definition.disallowed_tools.is_empty() {
            let has_bare_deny = definition.disallowed_tools.iter().any(|d| {
                AGENT_TASK_CLASSIFIER_RE
                    .captures(d)
                    .is_some_and(|caps| caps.get(1).is_none_or(|m| m.as_str().trim().is_empty()))
            });
            if has_bare_deny {
                definition.allowed_subagent_types = Some(vec![]);
            } else {
                let denied_types: Vec<String> = definition
                    .disallowed_tools
                    .iter()
                    .filter_map(|d| AGENT_TASK_CLASSIFIER_RE.captures(d)?.get(1))
                    .flat_map(|m| m.as_str().split(','))
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !denied_types.is_empty()
                    && let Some(ref mut allowed) = definition.allowed_subagent_types
                {
                    allowed.retain(|t| !denied_types.iter().any(|d| d.eq_ignore_ascii_case(t)));
                }
            }
        }
        if definition.allowed_subagent_types.as_deref() == Some(&[]) {
            let task_deps = [
                "task",
                "get_task_output",
                "kill_task",
                "wait_tasks",
                "scheduler_create",
                "scheduler_delete",
                "scheduler_list",
            ];
            tool_config
                .tools
                .retain(|tc| !task_deps.contains(&short_tool_name(&tc.id)));
            for tc in &mut tool_config.tools {
                if short_tool_name(&tc.id) == "run_terminal_cmd" {
                    let params = tc.params.get_or_insert_with(Default::default);
                    params.insert("enabled_background".into(), false.into());
                    params.insert("auto_background_on_timeout".into(), false.into());
                }
            }
        }
        if self.prompt_audience == crate::prompt::context::PromptAudience::Subagent {
            tool_config.tools.retain(|tool| {
                !distill_tools::implementations::distill::is_workflow_tool(tool.kind, &tool.id)
            });
        }
        let use_backend_search = self.backend_search;
        let web_search_enabled = self.web_search_config.is_enabled();
        let tool_bridge = ToolBridge::finalize_builder(
            tool_bridge_builder,
            tool_config,
            SessionContext {
                backend: self.terminal_backend,
                fs: self.fs_backend,
                cwd: self.working_directory.clone(),
                session_folder: state_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(std::env::temp_dir),
                session_env: self.session_env.unwrap_or_default(),
                notification_handle: self.notification_handle.clone(),
                owner_session_id: self.owner_session_id.clone(),
                subagent: None,
                parent_scheduler_handle: self.parent_scheduler_handle.take(),
                skills: skill_info.clone(),
                state_path,
                memory_backend: self.memory_backend,
                web_search_config: self.web_search_config,
                web_fetch_config: self.web_fetch_config,
                lsp: self.lsp,
                image_gen_config: self.image_gen_config,
                video_gen_config: self.video_gen_config,
                app_builder_deployer_config: self.app_builder_deployer_config,
                api_key_provider: self.api_key_provider,
                auth_provider: None,
                attribution_callback: self.attribution_callback,
                system_reminder_tag: self.system_reminder_tag,
            },
        )
        .instrument(tracing::info_span!("spawn.tool_registry"))
        .await
        .map_err(|e| AgentBuildError::ToolError(e.to_string()))?;
        if let Some(access) = self.memory_v2_access.clone() {
            tool_bridge.update_resource(access).await;
        }
        if let Some(bytes) = self.mcp_max_output_bytes {
            tool_bridge.toolset().resources.lock().await.insert(
                distill_tools::types::resources::TruncationCfg(
                    distill_tools::types::context::TruncationConfig {
                        mcp_max_output_bytes: Some(bytes),
                        ..Default::default()
                    },
                ),
            );
        }
        if let Some(names) = self.persisted_announced_skill_names {
            tool_bridge.restore_announced_skill_names(names).await;
        }
        let mut agents_md_files = if definition.agents_md {
            let agents_md_span = tracing::info_span!(
                "spawn.agents_md_load",
                agents_md_files = tracing::field::Empty
            );
            let files = crate::prompt::agents_md::read_agents_config_with_paths(
                &working_dir_str,
                self.compat,
                &self.paths_config,
                self.project_trusted,
            )
            .instrument(agents_md_span.clone())
            .await;
            agents_md_span.record("agents_md_files", files.len() as i64);
            files
        } else {
            vec![]
        };
        {
            let initial_paths: Vec<PathBuf> = agents_md_files
                .iter()
                .map(|c| PathBuf::from(&c.file_path))
                .collect();
            let gitignore_span = tracing::info_span!("spawn.gitignore_compile").entered();
            let git_root = git2::Repository::discover(&self.working_directory)
                .ok()
                .and_then(|repo| repo.workdir().map(|p| p.to_path_buf()));
            let gitignore = crate::prompt::ignore::build_gitignore(git_root.as_deref());
            drop(gitignore_span);
            let canonical_cwd = dunce::canonicalize(&self.working_directory)
                .unwrap_or_else(|_| self.working_directory.clone());
            let canonical_root = git_root.as_ref().and_then(|r| dunce::canonicalize(r).ok());
            let chain: Vec<PathBuf> = if let Some(ref root) = canonical_root {
                let mut dirs = Vec::new();
                let mut current = Some(canonical_cwd.as_path());
                while let Some(dir) = current {
                    dirs.push(dir.to_path_buf());
                    if dir == root.as_path() {
                        break;
                    }
                    current = dir.parent();
                }
                dirs
            } else {
                vec![]
            };
            if let Some(gi) = gitignore.as_ref()
                && let Some(root) = git_root.as_ref()
            {
                tool_bridge
                    .seed_gitignore_filter(gi.clone(), root.clone())
                    .await;
            }
            tool_bridge
                .seed_agents_md(
                    initial_paths,
                    git_root.clone(),
                    chain,
                    gitignore,
                    self.compat,
                )
                .await;
            let listing_skills = if preloaded_skill_paths.is_empty() {
                skill_info.clone()
            } else {
                skill_info
                    .iter()
                    .filter(|s| !preloaded_skill_paths.contains(&s.path))
                    .cloned()
                    .collect()
            };
            let skill_budget_percent: Option<f64> = None;
            let skill_discovery_cwd = (definition.discover_skills && self.project_trusted)
                .then(|| self.working_directory.clone());
            tool_bridge
                .seed_skill_discovery(
                    skill_discovery_cwd,
                    git_root,
                    listing_skills,
                    self.prompt_working_directory.clone(),
                    self.context_window_tokens,
                    skill_budget_percent,
                    self.compat,
                )
                .await;
        }
        let now = chrono::Utc::now();
        if let Some(ref display_cwd) = self.prompt_working_directory {
            for file in &mut agents_md_files {
                file.file_path = file.file_path.replace(&working_dir_str, display_cwd);
            }
        }
        let display_working_dir = self
            .prompt_working_directory
            .unwrap_or_else(|| self.working_directory.to_string_lossy().into_owned());
        let prompt_context = PromptContext {
            version: 1,
            prompt_mode: definition.prompt_mode.clone(),
            audience: self.prompt_audience,
            prompt_body: definition.prompt_body.clone(),
            include_browser_verification: definition.include_browser_verification(),
            system_prompt: definition.system_prompt.clone(),
            agents_md_files,
            persona_summaries: self.persona_summaries,
            build_timestamp_utc: now.to_rfc3339(),
            memory_enabled: self.memory_enabled,
            memory_v2_enabled: self.memory_v2_enabled,
            memory_global_path: self.memory_global_path,
            memory_workspace_path: self.memory_workspace_path,
            role_instructions: self.role_instructions,
            persona_instructions: self.persona_instructions,
            os_name: Some(std::env::consts::OS.to_string()),
            shell_path: Some(resolve_shell_for_prompt()),
            working_directory: Some(display_working_dir),
            current_date: Some(
                now.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string(),
            ),
            is_non_interactive: self.is_non_interactive,
            system_prompt_label: self.system_prompt_label,
        };
        let system_prompt = prompt_context
            .render(&tool_bridge)
            .instrument(tracing::info_span!("spawn.prompt_render"))
            .await
            .unwrap_or_default();
        if let Some(rendered) = tool_bridge
            .render_prompt(&definition.description, &prompt_context.placeholders())
            .await
        {
            definition.description = rendered;
        }
        let mut hosted_tools = Vec::new();
        if use_backend_search {
            if web_search_enabled && definition.hosted_tool_allowed("web_search") {
                hosted_tools.push(distill_sampling_types::HostedTool::WebSearch { options: None });
            }
            if definition.hosted_tool_allowed("x_search") {
                hosted_tools.push(distill_sampling_types::HostedTool::XSearch { options: None });
            }
            distill_sampling_types::apply_tool_overrides(
                &mut hosted_tools,
                definition.tool_overrides.as_ref(),
            );
        }
        #[allow(clippy::arc_with_non_send_sync)]
        let tool_bridge = Arc::new(tool_bridge);
        Ok(Agent::new(
            definition,
            prompt_context,
            system_prompt,
            tool_bridge,
            self.reminder_policy,
            self.compaction_policy,
            hosted_tools,
            use_backend_search,
        ))
    }
}
/// CLI naming for the shared [`distill_tool_types::build_task_description`] builder.
const TASK_TOOL_NAMING: distill_tool_types::TaskToolNaming<'static> = distill_tool_types::TaskToolNaming {
    task_tool: "${{ tools.by_kind.task }}",
    subagent_type_param: "${{ params.task.subagent_type }}",
    run_in_background_param: "${{ params.task.run_in_background }}",
    resume_from_param: "${{ params.task.resume_from }}",
    background_retrieval_tool: "${{ tools.by_kind.background_task_action }}",
    isolation_param: "${{ params.task.isolation }}",
};
/// Child sessions get a concise description that discourages recursive delegation. Hardcodes the built-in type names;
/// if custom child-visible subagent types become common, generate this like the parent description.
const CHILD_TASK_DESCRIPTION: &str = "\
Launch a sub-agent to handle a specific sub-task. Use this only when \n\
the sub-task is clearly independent and would benefit from a separate \n\
context (e.g., a parallel search while you continue working).\n\
\n\
Prefer doing the work yourself unless delegation is clearly necessary.\n\
\n\
Usage: specify ${{ params.task.subagent_type }} (\"general-purpose\", \"explore\", or \"plan\"), \n\
a short ${{ params.task.description }}, and a detailed ${{ params.task.prompt }}.\n\
${{ params.task.run_in_background }}: Returns immediately with a subagent_id. Use the task output tool to retrieve results. This is set to true by default.";
/// Each kind maps to its `${{ tools.by_kind.* }}` placeholder for the CLI's `TemplateRenderer` to resolve at finalize time.
const SUBAGENT_TOOL_NAMING: distill_tool_types::SubagentToolNaming<'static> =
    distill_tool_types::SubagentToolNaming {
        execute: "${{ tools.by_kind.execute }}",
        read: "${{ tools.by_kind.read }}",
        edit: "${{ tools.by_kind.edit }}",
        list: "${{ tools.by_kind.list }}",
        search: "${{ tools.by_kind.search }}",
        web_search: "${{ tools.by_kind.web_search }}",
        plan: "${{ tools.by_kind.plan }}",
    };
fn builtin_tools_fragment(name: BuiltinAgentName) -> String {
    let subagent = match name {
        BuiltinAgentName::GeneralPurpose => distill_tool_types::GENERAL_PURPOSE_SUBAGENT,
        BuiltinAgentName::Explore => distill_tool_types::EXPLORE_SUBAGENT,
        BuiltinAgentName::Plan => distill_tool_types::PLAN_SUBAGENT,
        BuiltinAgentName::CodeReviewer => distill_tool_types::EXPLORE_SUBAGENT,
        _ => return String::new(),
    };
    subagent.render_tools(&SUBAGENT_TOOL_NAMING)
}
/// Every client name a built-in child type's finalized bridge registers, in toolset order — exactly what that child is
/// sent on the wire.
pub(crate) struct ChildToolPreview {
    pub names: Vec<String>,
    pub read_only: bool,
}
pub(crate) type ChildToolNames = HashMap<BuiltinAgentName, ChildToolPreview>;
const TASK_MODEL_PARAM: &str = "${{ params.task.model }}";
fn task_model_guidance(model_slugs: &[String]) -> String {
    let mut model_slugs = model_slugs.to_vec();
    model_slugs.sort_unstable();
    model_slugs.dedup();
    if model_slugs.is_empty() {
        return format!(
            "\n\nNo explicit model slugs are currently available. \
             Omit `{TASK_MODEL_PARAM}` for a fresh bounded task to use the session model \
             when available; a code-reviewer may use the configured reasoning model unless pinned in \
             [subagents.models]. Explicit model pins remain \
             authoritative, and resumed or full-context forked children retain their existing \
             model/context semantics."
        );
    }
    let model_list = model_slugs
        .into_iter()
        .map(|slug| format!("- {slug}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\n\nIf the user explicitly asks for the model of a subagent/task, or difficult \
         diagnosis, architecture, failure recovery, or review needs the configured reasoning \
         model, you may ONLY use model slugs from this list:\n\
         {model_list}\n\n\
         Otherwise omit `{TASK_MODEL_PARAM}`. For a fresh bounded task this uses the session \
         model when available; a code-reviewer may use the configured reasoning model unless \
         pinned in [subagents.models]. Explicit model pins remain \
         authoritative, and resumed or full-context \
         forked children retain their existing model/context semantics."
    )
}
/// Defers to [`distill_tool_types::build_task_description`] so the CLI and the prod chat stack share one builder.
/// A built-in entry lists the tools its preview resolved, falling back to the static template when no preview exists;
/// user-defined entries carry `None` tools so their raw `description` is used verbatim.
pub(crate) fn build_task_description(
    subagents: &[SubagentEntry],
    model_slugs: &[String],
    child_tools: &ChildToolNames,
) -> String {
    let descriptors: Vec<distill_tool_types::SubagentDescriptor> = subagents
        .iter()
        .map(|entry| {
            let tools = match &entry.source {
                SubagentSource::Builtin(b) => Some(match child_tools.get(b) {
                    Some(preview) => distill_tool_types::render_tool_access_fragment(
                        &preview.names,
                        preview.read_only,
                    ),
                    None => builtin_tools_fragment(*b),
                }),
                SubagentSource::UserDefined { .. } => None,
            };
            distill_tool_types::SubagentDescriptor {
                name: entry.name.clone(),
                description: entry.description.clone(),
                tools,
            }
        })
        .collect();
    let mut description = distill_tool_types::build_task_description(&descriptors, &TASK_TOOL_NAMING);
    description.push_str(&task_model_guidance(model_slugs));
    if subagents.iter().any(|entry| entry.name == "code-reviewer") {
        description.push_str(
            "\n\nDelegate coherent, bounded implementation work with the goal, acceptance criteria \
             and relevant paths; return the result and verification, not the parent transcript. \
             After a substantive code checkpoint or before final handoff, use code-reviewer when \
             independent review could catch a material defect. Provide the diff, criteria and test \
             evidence. Skip trivial or unchanged checkpoints and do not repeat review of the same diff.",
        );
    }
    description
}
fn resolve_shell_for_prompt() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
    #[cfg(not(unix))]
    {
        distill_config::shell::detect_windows_shell()
            .name()
            .to_string()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentScope;
    use distill_tools::types::definition::ToolDefinition;
    use distill_tools::types::template_renderer::unresolved_template_markers;
    #[derive(Debug)]
    struct TestMemoryV2Access {
        roots: [PathBuf; 2],
    }
    impl distill_tools::types::memory_v2::MemoryV2Access for TestMemoryV2Access {
        fn validate_read(&self, _path: &std::path::Path) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn record_read(
            &self,
            _path: &std::path::Path,
            _contents: &[u8],
        ) -> std::result::Result<(), String> {
            Ok(())
        }
        fn preflight_write(
            &self,
            _path: &std::path::Path,
            _contents: &[u8],
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn write_file(
            &self,
            _path: &std::path::Path,
            _contents: &[u8],
        ) -> std::result::Result<distill_tools::types::memory_v2::MemoryV2Write, String> {
            Ok(distill_tools::types::memory_v2::MemoryV2Write::Outside)
        }
        fn scope_roots(&self) -> [PathBuf; 2] {
            self.roots.clone()
        }
    }
    #[tokio::test]
    async fn memory_v2_access_sets_prompt_roots_from_policy() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let access = distill_tools::types::memory_v2::MemoryV2AccessResource(Arc::new(
            TestMemoryV2Access {
                roots: [
                    PathBuf::from("/memory/global"),
                    PathBuf::from("/memory/workspace"),
                ],
            },
        ));
        let builder = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .with_memory_paths(Some("/stale/global".to_owned()), None)
        .with_memory_v2_access(Some(access), true);
        assert_eq!(
            builder.memory_global_path.as_deref(),
            Some("/memory/global")
        );
        assert_eq!(
            builder.memory_workspace_path.as_deref(),
            Some("/memory/workspace")
        );
        assert!(builder.memory_v2_enabled);
    }
    #[tokio::test]
    async fn unexposed_memory_v2_access_installs_policy_without_prompt_block() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let access = || {
            distill_tools::types::memory_v2::MemoryV2AccessResource(Arc::new(TestMemoryV2Access {
                roots: [
                    PathBuf::from("/memory/global"),
                    PathBuf::from("/memory/workspace"),
                ],
            }))
        };
        let builder = || {
            AgentBuilder::new(
                std::env::temp_dir(),
                Arc::new(LocalTerminalBackend::new()),
                ToolNotificationHandle::noop(),
            )
        };
        let hidden = builder().with_memory_v2_access(Some(access()), false);
        assert!(hidden.memory_v2_access.is_some());
        assert!(!hidden.memory_v2_enabled);
        assert_eq!(hidden.memory_global_path.as_deref(), Some("/memory/global"));
        let absent = builder().with_memory_v2_access(None, true);
        assert!(absent.memory_v2_access.is_none());
        assert!(!absent.memory_v2_enabled);
    }
    async fn messaging_tool_count(
        enabled: Option<bool>,
        predeclared: bool,
        ceiling: Option<distill_tool_types::SubagentCapabilityMode>,
    ) -> usize {
        messaging_tool_count_for(enabled, predeclared, ceiling, PromptAudience::Primary).await
    }
    async fn messaging_tool_count_for(
        enabled: Option<bool>,
        predeclared: bool,
        ceiling: Option<distill_tool_types::SubagentCapabilityMode>,
        audience: PromptAudience,
    ) -> usize {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::distill::task::types::SubagentCapabilityModeExt;
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition.capability_mode = ceiling;
        if let (Some(mode), PromptAudience::Subagent) = (ceiling, audience) {
            mode.filter_tool_config(&mut definition.tool_config);
        }
        if predeclared {
            definition.tool_config.tools.push(
                distill_tools::registry::types::ToolConfig::for_tool::<
                    distill_tools::implementations::distill::SendSubagentMessageTool,
                >(),
            );
        }
        let mut builder = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_prompt_audience(audience);
        if let Some(enabled) = enabled {
            builder = builder.with_active_agent_messages_enabled(enabled);
        }
        builder
            .build()
            .await
            .expect("agent should build")
            .tool_definitions()
            .await
            .iter()
            .filter(|definition| definition.function.name == "send_subagent_message")
            .count()
    }
    #[tokio::test]
    async fn active_agent_messages_default_and_false_are_absent() {
        assert_eq!(messaging_tool_count(None, false, None).await, 0);
        assert_eq!(messaging_tool_count(Some(false), false, None).await, 0);
        assert_eq!(messaging_tool_count(None, true, None).await, 0);
        assert_eq!(messaging_tool_count(Some(false), true, None).await, 0);
    }
    #[tokio::test]
    async fn active_agent_messages_true_is_present_exactly_once() {
        assert_eq!(messaging_tool_count(Some(true), false, None).await, 1);
    }
    #[tokio::test]
    async fn active_agent_messages_predeclared_is_not_duplicated() {
        assert_eq!(messaging_tool_count(Some(true), true, None).await, 1);
    }
    #[tokio::test]
    async fn active_agent_messages_stay_within_the_capability_ceiling() {
        use distill_tool_types::SubagentCapabilityMode;
        let child = PromptAudience::Subagent;
        let read_only = Some(SubagentCapabilityMode::ReadOnly);
        assert_eq!(
            messaging_tool_count_for(Some(true), false, read_only, child).await,
            0
        );
        assert_eq!(
            messaging_tool_count_for(Some(true), true, read_only, child).await,
            0
        );
        for ceiling in [
            SubagentCapabilityMode::ReadWrite,
            SubagentCapabilityMode::Execute,
            SubagentCapabilityMode::All,
        ] {
            assert_eq!(
                messaging_tool_count_for(Some(true), false, Some(ceiling), child).await,
                1,
                "{ceiling:?}"
            );
        }
        assert_eq!(messaging_tool_count(Some(true), false, read_only).await, 1);
    }
    #[tokio::test]
    async fn active_agent_messages_are_present_in_enabled_child_toolsets() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition
            .tool_config
            .tools
            .push(distill_tools::registry::types::ToolConfig::for_tool::<
                distill_tools::implementations::distill::SendSubagentMessageTool,
            >());
        let definitions = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_active_agent_messages_enabled(true)
        .with_prompt_audience(PromptAudience::Subagent)
        .build()
        .await
        .expect("child agent should build")
        .tool_definitions()
        .await;
        assert_eq!(
            1,
            definitions
                .iter()
                .filter(|definition| definition.function.name == "send_subagent_message")
                .count()
        );
    }
    #[tokio::test]
    async fn active_agent_messages_never_inject_into_curated_toolsets() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition.inject_default_tools = false;
        definition.tool_config.tools =
            vec![distill_tools::registry::types::ToolConfig::for_tool::<
                distill_tools::implementations::distill::SendSubagentMessageTool,
            >()];
        let build = |enabled| {
            AgentBuilder::new(
                std::env::temp_dir(),
                Arc::new(LocalTerminalBackend::new()),
                ToolNotificationHandle::noop(),
            )
            .from_definition(definition.clone())
            .with_active_agent_messages_enabled(enabled)
            .with_subagents_enabled(true)
            .with_background_workflows_enabled(true)
        };
        let disabled = build(false)
            .build()
            .await
            .expect("disabled curated agent should build")
            .tool_definitions()
            .await;
        let enabled = build(true)
            .build()
            .await
            .expect("active-message curated agent should build")
            .tool_definitions()
            .await;
        let enabled_names = enabled
            .iter()
            .map(|definition| definition.function.name.as_str())
            .collect::<Vec<_>>();
        let disabled_names = disabled
            .iter()
            .map(|definition| definition.function.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            1,
            enabled_names
                .iter()
                .filter(|name| **name == "send_subagent_message")
                .count(),
        );
        assert!(!disabled_names.contains(&"send_subagent_message"));
        let without_gate = |names: &[&str]| {
            names
                .iter()
                .filter(|name| **name != "send_subagent_message")
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(without_gate(&enabled_names), without_gate(&disabled_names));
    }
    fn entry(name: &str, desc: &str, source: SubagentSource) -> SubagentEntry {
        SubagentEntry {
            name: name.to_string(),
            description: desc.to_string(),
            source,
            shadows_builtin: None,
            config_source: distill_tools::types::config_source::ConfigSource::Builtin,
        }
    }
    #[test]
    fn build_task_description_builtin_includes_tools() {
        let subagents = vec![
            entry(
                "general-purpose",
                "General-purpose agent.",
                SubagentSource::Builtin(BuiltinAgentName::GeneralPurpose),
            ),
            entry(
                "explore",
                "Explore agent.",
                SubagentSource::Builtin(BuiltinAgentName::Explore),
            ),
        ];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(
            desc.contains(distill_tool_types::GENERAL_PURPOSE_SUBAGENT.tools_template),
            "should include general-purpose tool names"
        );
        assert!(
            desc.contains(distill_tool_types::EXPLORE_SUBAGENT.tools_template),
            "should include explore tool names"
        );
        assert!(
            desc.contains("- **general-purpose**: General-purpose agent."),
            "should include agent entry"
        );
    }
    #[test]
    fn build_task_description_user_entry_is_raw() {
        let subagents = vec![entry(
            "code-reviewer",
            "Reviews code for bugs and style issues.",
            SubagentSource::UserDefined {
                scope: AgentScope::Project,
            },
        )];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(desc.contains("- **code-reviewer**: Reviews code for bugs and style issues."));
        assert!(desc.contains("substantive code checkpoint"));
        assert!(
            !desc.contains(distill_tool_types::GENERAL_PURPOSE_SUBAGENT.tools_template),
            "user-defined entries should not get built-in tool fragments"
        );
    }
    #[test]
    fn build_task_description_user_template_syntax_verbatim() {
        let subagents = vec![entry(
            "my-agent",
            "Uses ${{ some.template }} syntax.",
            SubagentSource::UserDefined {
                scope: AgentScope::User,
            },
        )];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(
            desc.contains("${{ some.template }}"),
            "template-like syntax in user descriptions should be rendered verbatim"
        );
    }
    #[test]
    fn build_task_description_shadowed_builtin_uses_user_desc() {
        let subagents = vec![SubagentEntry {
            name: "explore".to_string(),
            description: "My custom explore agent.".to_string(),
            source: SubagentSource::UserDefined {
                scope: AgentScope::Project,
            },
            shadows_builtin: Some(BuiltinAgentName::Explore),
            config_source: distill_tools::types::config_source::ConfigSource::Project {
                path: std::path::PathBuf::new(),
            },
        }];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(
            desc.contains("- **explore**: My custom explore agent."),
            "shadowed built-in should use user description"
        );
        assert!(
            !desc.contains(distill_tool_types::EXPLORE_SUBAGENT.tools_template),
            "shadowed built-in should NOT include built-in tool fragment"
        );
    }
    fn explore_preview() -> ChildToolNames {
        ChildToolNames::from([(
            BuiltinAgentName::Explore,
            ChildToolPreview {
                names: ["read_file", "list_dir", "grep"].map(String::from).to_vec(),
                read_only: true,
            },
        )])
    }
    #[test]
    fn build_task_description_uses_template_variables() {
        let subagents = vec![entry(
            "explore",
            "Explore.",
            SubagentSource::Builtin(BuiltinAgentName::Explore),
        )];
        let desc = build_task_description(&subagents, &[], &explore_preview());
        assert!(
            desc.contains("${{ tools.by_kind.task }}"),
            "should use tools.by_kind.task template variable"
        );
        assert!(
            desc.contains("${{ params.task.subagent_type }}"),
            "should use params.task.subagent_type template variable"
        );
        assert!(
            desc.contains("${{ params.task.model }}"),
            "should use params.task.model template variable"
        );
    }
    #[test]
    fn build_task_description_prefers_previewed_tools_and_falls_back_per_type() {
        let subagents = vec![
            entry(
                "general-purpose",
                "General-purpose agent.",
                SubagentSource::Builtin(BuiltinAgentName::GeneralPurpose),
            ),
            entry(
                "explore",
                "Explore agent.",
                SubagentSource::Builtin(BuiltinAgentName::Explore),
            ),
        ];
        let desc = build_task_description(&subagents, &[], &explore_preview());
        assert!(
            desc.contains(
                "- **explore**: Explore agent. Read-only \u{2014} has access to: read_file, list_dir, and grep."
            ),
            "previewed type lists its resolved tools: {desc}"
        );
        assert!(
            desc.contains(distill_tool_types::GENERAL_PURPOSE_SUBAGENT.tools_template),
            "type without a preview keeps the static fragment: {desc}"
        );
    }
    #[test]
    fn build_task_description_lists_public_model_slugs() {
        let subagents = vec![entry(
            "explore",
            "Explore.",
            SubagentSource::Builtin(BuiltinAgentName::Explore),
        )];
        let desc = build_task_description(
            &subagents,
            &["zeta".to_string(), "alpha".to_string(), "alpha".to_string()],
            &ChildToolNames::new(),
        );
        assert!(desc.contains("- alpha\n- zeta"));
        assert!(desc.contains("${{ params.task.model }}"));
        assert!(desc.contains("session model"));
    }
    #[test]
    fn build_task_description_handles_empty_model_catalog() {
        let subagents = vec![entry(
            "explore",
            "Explore.",
            SubagentSource::Builtin(BuiltinAgentName::Explore),
        )];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(desc.contains("${{ params.task.model }}"));
        assert!(!desc.contains("- alpha"));
        assert!(desc.contains("session model"));
    }
    #[test]
    fn task_model_guidance_resolves_model_param_override() {
        use distill_tools::types::template_renderer::TemplateRenderer;
        use distill_tools::types::tool::ToolKind;
        let renderer = TemplateRenderer::new(
            Default::default(),
            std::collections::HashMap::from([(
                ToolKind::Task,
                std::collections::HashMap::from([("model".to_string(), "child_model".to_string())]),
            )]),
        );
        let rendered = renderer
            .render(&task_model_guidance(&["alpha".to_string()]))
            .expect("model guidance should render");
        assert!(rendered.contains("`child_model`"));
        assert!(!rendered.contains("params.task.model"));
    }
    #[test]
    fn child_task_description_is_concise() {
        assert!(CHILD_TASK_DESCRIPTION.contains("${{ params.task.subagent_type }}"));
        assert!(CHILD_TASK_DESCRIPTION.contains("${{ params.task.description }}"));
        assert!(CHILD_TASK_DESCRIPTION.contains("${{ params.task.prompt }}"));
        assert!(
            CHILD_TASK_DESCRIPTION.len() < 700,
            "child description should be compact, got {} chars",
            CHILD_TASK_DESCRIPTION.len()
        );
    }
    #[test]
    fn build_task_description_contains_resume_from_guidance() {
        let subagents = vec![entry(
            "general-purpose",
            "GP agent.",
            SubagentSource::Builtin(BuiltinAgentName::GeneralPurpose),
        )];
        let desc = build_task_description(&subagents, &[], &ChildToolNames::new());
        assert!(
            desc.contains("resume_from"),
            "should reference the resume_from parameter"
        );
        assert!(
            desc.contains("subagent_type"),
            "should reference the subagent_type parameter"
        );
    }
    #[tokio::test]
    async fn discovery_snapshot_records_gated_and_preloaded_skills() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        let tmp = tempfile::tempdir().unwrap();
        let write_skill = |dir: &str, content: &str| {
            let d = tmp.path().join(".grok/skills").join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), content).unwrap();
        };
        write_skill(
            "snapshot-plain-skill",
            "---\nname: snapshot-plain-skill\ndescription: plain\n---\nbody\n",
        );
        write_skill(
            "snapshot-gated-skill",
            "---\nname: snapshot-gated-skill\ndescription: gated\npaths: \"src/**\"\n---\nbody\n",
        );
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition.skills = vec!["snapshot-plain-skill".to_string()];
        let agent = AgentBuilder::new(
            tmp.path().to_path_buf(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_project_trusted(true)
        .build()
        .await
        .expect("agent should build with local skill fixtures");
        let snapshot = agent.tool_bridge().skill_discovery_snapshot_names().await;
        assert!(
            snapshot.contains(&"snapshot-plain-skill".to_string()),
            "preloaded skill missing from snapshot: {snapshot:?}"
        );
        assert!(
            snapshot.contains(&"snapshot-gated-skill".to_string()),
            "paths:-gated skill missing from snapshot: {snapshot:?}"
        );
        let listed: Vec<String> = agent
            .tool_bridge()
            .slash_skills()
            .await
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            !listed.contains(&"snapshot-gated-skill".to_string()),
            "paths:-gated skill must stay out of the listing baseline: {listed:?}"
        );
        assert!(
            !listed.contains(&"snapshot-plain-skill".to_string()),
            "preloaded skill must stay out of the listing baseline: {listed:?}"
        );
    }
    async fn build_pager_agent(
        profile: crate::config::AgentDefinition,
        subagents_enabled: bool,
        ask_user_question_enabled: bool,
    ) -> crate::agent::Agent {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(profile)
        .with_subagents_enabled(subagents_enabled)
        .with_ask_user_question_enabled(ask_user_question_enabled)
        .build()
        .await
        .expect("agent should build for every pager-reachable flag combination")
    }
    #[tokio::test]
    async fn pager_flag_combinations_satisfy_tool_invariants() {
        use crate::config::AgentDefinition;
        struct PagerFlagCase {
            label: &'static str,
            profile: fn() -> AgentDefinition,
            subagents: bool,
            ask_user: bool,
        }
        let cases: &[PagerFlagCase] = &[
            PagerFlagCase {
                label: "distill / subagents+ask_user",
                profile: AgentDefinition::default_distill,
                subagents: true,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill / subagents / no-ask-user",
                profile: AgentDefinition::default_distill,
                subagents: true,
                ask_user: false,
            },
            PagerFlagCase {
                label: "distill / no-subagents / ask_user",
                profile: AgentDefinition::default_distill,
                subagents: false,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill / no-subagents / no-ask-user",
                profile: AgentDefinition::default_distill,
                subagents: false,
                ask_user: false,
            },
            PagerFlagCase {
                label: "distill-ask-user / subagents",
                profile: AgentDefinition::distill_ask_user,
                subagents: true,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill-ask-user / no-subagents",
                profile: AgentDefinition::distill_ask_user,
                subagents: false,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill-plan",
                profile: AgentDefinition::distill_plan,
                subagents: true,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill-plan / no-ask-user",
                profile: AgentDefinition::distill_plan,
                subagents: true,
                ask_user: false,
            },
            PagerFlagCase {
                label: "distill-plan-no-subagents",
                profile: AgentDefinition::distill_plan_no_subagents,
                subagents: false,
                ask_user: true,
            },
            PagerFlagCase {
                label: "distill-plan-no-subagents / no-ask-user",
                profile: AgentDefinition::distill_plan_no_subagents,
                subagents: false,
                ask_user: false,
            },
        ];
        for case in cases {
            let PagerFlagCase {
                label,
                profile,
                subagents,
                ask_user,
            } = case;
            let agent = build_pager_agent(profile(), *subagents, *ask_user).await;
            let defs = agent.tool_definitions().await;
            let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
            let builtin_defs = agent.tool_definitions_builtins_only().await;
            let builtin_names: Vec<&str> = builtin_defs
                .iter()
                .map(|definition| definition.function.name.as_str())
                .collect();
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for n in &names {
                *counts.entry(*n).or_default() += 1;
            }
            let dupes: Vec<(&&str, &usize)> = counts.iter().filter(|(_, c)| **c > 1).collect();
            assert!(
                dupes.is_empty(),
                "[{label}] tool names must be unique, found duplicates: {dupes:?}; full list: {names:?}"
            );
            let has_ask_user = names.contains(&"ask_user_question");
            assert_eq!(
                has_ask_user, *ask_user,
                "[{label}] ask_user_question presence should match ask_user_question_enabled={ask_user}; got tools: {names:?}"
            );
            let has_task = names.contains(&"spawn_subagent");
            assert_eq!(
                has_task, *subagents,
                "[{label}] spawn_subagent presence should match subagents_enabled={subagents}; got tools: {names:?}"
            );
            if *subagents {
                let task = spawn_subagent_description(&defs);
                assert!(
                    task.contains("read_file"),
                    "[{label}] the per-type tool list must resolve to client tool names: {task}"
                );
            }
            assert_eq!(
                Vec::<(String, String)>::new(),
                unresolved_template_markers(&defs),
                "[{label}] rendered descriptions must not leak template markers"
            );
            assert!(
                names.contains(&"send_feedback"),
                "[{label}] parent distill sessions must advertise send_feedback; got tools: {names:?}"
            );
            assert!(
                builtin_names.contains(&"send_feedback"),
                "[{label}] parent distill built-in definitions must advertise send_feedback; got tools: {builtin_names:?}"
            );
            assert!(
                names.contains(&"enter_plan_mode"),
                "[{label}] enter_plan_mode must always be present (TUI plan-mode keybind needs it); got tools: {names:?}"
            );
            assert!(
                names.contains(&"exit_plan_mode"),
                "[{label}] exit_plan_mode must always be present (TUI plan-mode keybind needs it); got tools: {names:?}"
            );
            assert!(
                names.contains(&"write"),
                "[{label}] every pager profile has an edit tool, so write must be injected; got tools: {names:?}"
            );
            for def in &defs {
                let description = def.function.description.as_deref().unwrap_or_default();
                for slot in [", ,", ": ,", ", and ."] {
                    assert!(
                        !description.contains(slot),
                        "[{label}] {} renders an empty list slot {slot:?}: {description}",
                        def.function.name
                    );
                }
            }
        }
    }
    fn spawn_subagent_description(defs: &[ToolDefinition]) -> String {
        let names: Vec<&str> = defs.iter().map(|def| def.function.name.as_str()).collect();
        defs.iter()
            .find(|def| def.function.name == "spawn_subagent")
            .and_then(|def| def.function.description.clone())
            .unwrap_or_else(|| panic!("spawn_subagent must advertise a description: {names:?}"))
    }
    #[tokio::test]
    async fn task_description_renders_without_template_markers_subagent_audience() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(crate::config::AgentDefinition::default_distill())
        .with_prompt_audience(PromptAudience::Subagent)
        .with_subagents_enabled(true)
        .build()
        .await
        .expect("child agent should build");
        let defs = agent.tool_definitions().await;
        let task = spawn_subagent_description(&defs);
        let child_lead = CHILD_TASK_DESCRIPTION
            .split_inclusive('.')
            .next()
            .expect("child description has a sentence");
        assert!(
            task.starts_with(child_lead),
            "child sessions must get the concise task description: {task}"
        );
        assert!(
            !task.contains("has access to:"),
            "child sessions must not preview their own children: {task}"
        );
        assert_eq!(
            Vec::<(String, String)>::new(),
            unresolved_template_markers(&defs)
        );
    }
    /// The `- **<type>**: … <fragment>` line the parent's task description renders for one built-in child type.
    fn task_type_line(defs: &[ToolDefinition], subagent_type: &str) -> String {
        let task = spawn_subagent_description(defs);
        let prefix = format!("- **{subagent_type}**:");
        task.lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no {subagent_type} line in task description: {task}"))
            .to_owned()
    }
    /// Tool definitions of a default distill primary with subagents on, so its `spawn_subagent` description carries
    /// the previewed child lists; `configure` adds the session knobs under test.
    async fn previewing_primary(
        configure: impl FnOnce(AgentBuilder) -> AgentBuilder,
    ) -> Vec<ToolDefinition> {
        use distill_tools::computer::local::LocalTerminalBackend;
        configure(
            AgentBuilder::new(
                std::env::temp_dir(),
                Arc::new(LocalTerminalBackend::new()),
                ToolNotificationHandle::noop(),
            )
            .from_definition(crate::config::AgentDefinition::default_distill())
            .with_subagents_enabled(true),
        )
        .build()
        .await
        .expect("primary agent should build")
        .tool_definitions()
        .await
    }
    #[tokio::test]
    async fn default_knobs_task_description_lines_match_the_child_toolsets() {
        let defs = previewing_primary(std::convert::identity).await;
        let general_purpose = task_type_line(&defs, "general-purpose");
        assert!(
            general_purpose.ends_with(
                "Has access to: run_terminal_command, read_file, search_replace, list_dir, grep, \
                 kill_command_or_subagent, todo_write, get_command_or_subagent_output, \
                 wait_commands_or_subagents, scheduler_create, scheduler_delete, scheduler_list, \
                 monitor, search_tool, use_tool, update_goal, and write."
            ),
            "{general_purpose}"
        );
        let explore = task_type_line(&defs, "explore");
        assert!(
            explore.ends_with("Read-only \u{2014} has access to: read_file, list_dir, and grep."),
            "{explore}"
        );
        let plan = task_type_line(&defs, "plan");
        assert!(
            plan.ends_with(
                "Read-only \u{2014} has access to: read_file, list_dir, grep, and todo_write."
            ),
            "{plan}"
        );
    }
    #[tokio::test]
    async fn plan_fragment_lists_web_search_when_the_parent_enables_it() {
        use distill_tools::implementations::web_search::WebSearchConfig;
        let enabled = WebSearchConfig::Enabled {
            api_key: "test-key".into(),
            base_url: "https://api.x.ai/v1".into(),
            model: "test-web-search-model".into(),
            extra_headers: Default::default(),
            alpha_test_key: None,
            allowed_domains: None,
            excluded_domains: None,
        };
        let defs = previewing_primary(|builder| builder.with_web_search_config(enabled)).await;
        let plan = task_type_line(&defs, "plan");
        assert!(
            plan.ends_with(
                "Read-only \u{2014} has access to: read_file, list_dir, grep, todo_write, and web_search."
            ),
            "{plan}"
        );
    }
    /// The depth policy is the only thing between the child's toolset and the advertised list.
    #[tokio::test]
    async fn general_purpose_fragment_lists_spawn_subagent_when_children_may_nest() {
        let defs =
            previewing_primary(|builder| builder.with_child_nested_subagents_allowed(true)).await;
        let general_purpose = task_type_line(&defs, "general-purpose");
        assert!(
            general_purpose.contains("spawn_subagent"),
            "{general_purpose}"
        );
    }
    /// "Read-only" follows the read-only capability allowlist, so a child handed a generating tool loses the prefix.
    #[tokio::test]
    async fn explore_fragment_drops_read_only_prefix_when_image_generation_is_enabled() {
        use distill_tools::implementations::distill::image_gen::ImageGenConfig;
        let image_gen = ImageGenConfig::Enabled {
            api_key: "test-key".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: Default::default(),
            image_gen_enabled: true,
            image_edit_enabled: false,
            model_override: None,
            edit_model_override: None,
            tier_restricted: false,
        };
        let defs = previewing_primary(|builder| builder.with_image_gen_config(image_gen)).await;
        let explore = task_type_line(&defs, "explore");
        assert!(explore.contains("image_gen"), "{explore}");
        assert!(!explore.contains("Read-only"), "{explore}");
    }
    /// Write follows the edit tool and plan mode never reaches a child, whatever the built-in type's toolset.
    /// Children are built at the default depth, so nested subagents stay disabled as in production.
    #[tokio::test]
    async fn child_toolsets_inject_write_only_alongside_edit_and_never_plan_mode() {
        use distill_tools::computer::local::LocalTerminalBackend;
        let cases: [(&str, crate::config::AgentDefinition, &[&str], &[&str]); 3] = [
            (
                "explore",
                crate::config::AgentDefinition::explore(),
                &["read_file", "list_dir", "grep"],
                &["write", "enter_plan_mode", "exit_plan_mode"],
            ),
            (
                "plan",
                crate::config::AgentDefinition::plan(),
                &["read_file", "list_dir", "grep", "todo_write"],
                &["write", "enter_plan_mode", "exit_plan_mode"],
            ),
            (
                "general-purpose",
                crate::config::AgentDefinition::general_purpose(),
                &["write", "search_replace"],
                &["enter_plan_mode", "exit_plan_mode"],
            ),
        ];
        for (label, definition, present, absent) in cases {
            let names: Vec<String> = AgentBuilder::new(
                std::env::temp_dir(),
                Arc::new(LocalTerminalBackend::new()),
                ToolNotificationHandle::noop(),
            )
            .from_definition(definition)
            .with_prompt_audience(PromptAudience::Subagent)
            .build()
            .await
            .expect("child agent should build")
            .tool_definitions()
            .await
            .into_iter()
            .map(|definition| definition.function.name)
            .collect();
            for name in present {
                assert!(
                    names.iter().any(|n| n == name),
                    "[{label}] {name} must be present: {names:?}"
                );
            }
            for name in absent {
                assert!(
                    !names.iter().any(|n| n == name),
                    "[{label}] {name} must be absent: {names:?}"
                );
            }
        }
    }
    #[tokio::test]
    async fn subagent_audience_never_receives_parent_only_tools() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        use distill_tools::registry::types::ToolConfig;
        let feedback_id =
            ToolConfig::for_tool::<distill_tools::implementations::distill::SendFeedbackTool>()
                .id;
        let mut kindless_feedback_id = crate::config::AgentDefinition::default_distill();
        kindless_feedback_id.tool_config.tools = vec![ToolConfig::from_id(&feedback_id)];
        kindless_feedback_id.inject_default_tools = false;
        let mut kindless_feedback_name = crate::config::AgentDefinition::default_distill();
        kindless_feedback_name.tool_config.tools = vec![
            ToolConfig::from_id("custom:tool")
                .with_name(distill_tools::implementations::distill::SEND_FEEDBACK_TOOL_NAME),
        ];
        kindless_feedback_name.inject_default_tools = false;
        for definition in [
            crate::config::AgentDefinition::default_distill(),
            crate::config::AgentDefinition::distill_ask_user(),
            kindless_feedback_id,
            kindless_feedback_name,
        ] {
            let agent = AgentBuilder::new(
                std::env::temp_dir(),
                Arc::new(LocalTerminalBackend::new()),
                ToolNotificationHandle::noop(),
            )
            .from_definition(definition)
            .with_ask_user_question_enabled(true)
            .with_prompt_audience(crate::prompt::context::PromptAudience::Subagent)
            .build()
            .await
            .expect("subagent should build");
            let names: Vec<String> = agent
                .tool_definitions()
                .await
                .into_iter()
                .map(|definition| definition.function.name)
                .collect();
            for parent_only in ["ask_user_question", "send_feedback"] {
                assert!(
                    !names.iter().any(|name| name == parent_only),
                    "subagents must not receive {parent_only}: {names:?}"
                );
            }
        }
    }
    async fn workflow_tool_names(
        audience: crate::prompt::context::PromptAudience,
        definition: crate::config::AgentDefinition,
    ) -> Vec<String> {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_background_workflows_enabled(true)
        .with_prompt_audience(audience)
        .build()
        .await
        .expect("agent should build")
        .tool_definitions()
        .await
        .into_iter()
        .map(|definition| definition.function.name)
        .collect()
    }
    #[tokio::test]
    async fn top_level_session_still_receives_workflow() {
        let names = workflow_tool_names(
            crate::prompt::context::PromptAudience::Primary,
            crate::config::AgentDefinition::default_distill(),
        )
        .await;
        assert!(
            names.iter().any(|name| name == "workflow"),
            "top-level sessions must keep workflow when the feature is enabled: {names:?}"
        );
    }
    #[tokio::test]
    async fn ordinary_subagent_does_not_receive_workflow() {
        let names = workflow_tool_names(
            crate::prompt::context::PromptAudience::Subagent,
            crate::config::AgentDefinition::general_purpose(),
        )
        .await;
        assert!(
            !names.iter().any(|name| name == "workflow"),
            "ordinary subagents must not receive workflow: {names:?}"
        );
    }
    #[tokio::test]
    async fn workflow_spawned_agent_does_not_receive_workflow() {
        let names = workflow_tool_names(
            crate::prompt::context::PromptAudience::Subagent,
            crate::config::AgentDefinition::default_distill(),
        )
        .await;
        assert!(
            !names.iter().any(|name| name == "workflow"),
            "workflow-spawned agents must not receive workflow: {names:?}"
        );
    }
    #[tokio::test]
    async fn custom_child_toolset_cannot_reintroduce_workflow() {
        use distill_tools::implementations::distill::{ReadFileTool, WorkflowTool};
        let mut definition = crate::config::AgentDefinition::general_purpose();
        definition.inject_default_tools = false;
        definition.tool_config.tools = vec![
            (&ReadFileTool).into(),
            (&WorkflowTool).into(),
            distill_tools::registry::types::ToolConfig::from_id("Distill:workflow"),
        ];
        let names =
            workflow_tool_names(crate::prompt::context::PromptAudience::Subagent, definition).await;
        assert!(
            names.iter().any(|name| name == "read_file"),
            "unrelated custom tools must survive: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "workflow"),
            "a custom child toolset must not be able to reintroduce workflow: {names:?}"
        );
    }
    #[tokio::test]
    async fn child_workflow_strip_leaves_unrelated_tools() {
        let primary = workflow_tool_names(
            crate::prompt::context::PromptAudience::Primary,
            crate::config::AgentDefinition::default_distill(),
        )
        .await;
        let child = workflow_tool_names(
            crate::prompt::context::PromptAudience::Subagent,
            crate::config::AgentDefinition::default_distill(),
        )
        .await;
        assert!(
            primary.iter().any(|name| name == "workflow"),
            "premise: primary toolset includes workflow: {primary:?}"
        );
        let lost: Vec<&String> = primary
            .iter()
            .filter(|name| !child.contains(name))
            .collect();
        assert!(
            lost.iter().any(|name| *name == "workflow"),
            "child must lose workflow: lost={lost:?}"
        );
        assert!(
            lost.iter().all(|name| {
                matches!(
                    name.as_str(),
                    "workflow"
                        | "ask_user_question"
                        | "send_feedback"
                        | "enter_plan_mode"
                        | "exit_plan_mode"
                )
            }),
            "child strip must not drop unrelated tools: lost={lost:?}"
        );
        for name in &child {
            assert!(
                primary.contains(name),
                "child gained unexpected tool {name}; primary={primary:?} child={child:?}"
            );
        }
    }
    #[tokio::test]
    async fn curated_empty_toolset_fails_agent_build() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        let mut profile = crate::config::AgentDefinition::default_distill();
        profile.tool_config = Default::default();
        profile.inject_default_tools = false;
        let result = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(profile)
        .build()
        .await;
        match result {
            Ok(_) => panic!("empty curated toolset must be rejected at build time"),
            Err(err) => {
                assert!(
                    matches!(err, AgentBuildError::InvalidConfig(_)),
                    "expected InvalidConfig, got: {err:?}"
                )
            }
        }
    }
    #[tokio::test]
    async fn plan_mode_injected_ask_user_question_receives_params() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::distill::ask_user_question::AskUserQuestionParams;
        use distill_tools::notification::ToolNotificationHandle;
        use distill_tools::types::resources::Params;
        let profile = crate::config::AgentDefinition::default_distill();
        assert!(
            !profile
                .tool_config
                .tools
                .iter()
                .any(|tc| tc.id == "Distill:ask_user_question"),
            "test premise: the profile must not pre-declare ask_user_question"
        );
        let mut params = serde_json::Map::new();
        params.insert("timeout_enabled".into(), serde_json::Value::Bool(false));
        params.insert("timeout_secs".into(), serde_json::Value::from(5));
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(profile)
        .with_ask_user_question_params(params)
        .build()
        .await
        .expect("agent should build");
        let applied = agent
            .tool_bridge()
            .read_resource::<Params<AskUserQuestionParams>>()
            .await
            .expect("finalize must insert Params for the injected ask_user_question");
        assert_eq!(applied.0.timeout_enabled, Some(false));
        assert_eq!(applied.0.timeout_secs, Some(5));
        assert_eq!(applied.0.non_interactive, None);
    }
    #[tokio::test]
    async fn non_interactive_build_stamps_ask_user_question_params() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::distill::ask_user_question::AskUserQuestionParams;
        use distill_tools::notification::ToolNotificationHandle;
        use distill_tools::types::resources::Params;
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(crate::config::AgentDefinition::default_distill())
        .with_is_non_interactive(true)
        .build()
        .await
        .expect("agent should build");
        let applied = agent
            .tool_bridge()
            .read_resource::<Params<AskUserQuestionParams>>()
            .await
            .expect("finalize must insert Params for the injected ask_user_question");
        assert_eq!(applied.0.non_interactive, Some(true));
    }
    async fn build_with_tools(tools: Vec<String>, disallowed: Vec<String>) -> crate::agent::Agent {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        let mut def = crate::config::AgentDefinition::default_distill();
        def.tools = tools;
        def.disallowed_tools = disallowed;
        AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(def)
        .build()
        .await
        .unwrap()
    }
    async fn session_clamp_tool_names(
        own_tools: Vec<String>,
        session_allow: Vec<String>,
    ) -> Vec<String> {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        let mut def = crate::config::AgentDefinition::default_distill();
        def.tools = own_tools;
        def.session_tools_allowlist = Some(session_allow);
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(def)
        .build()
        .await
        .unwrap();
        agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect()
    }
    #[tokio::test]
    async fn session_clamp_intersects_own_allowlist() {
        let has = |v: &[String], t: &str| v.iter().any(|n| n == t);
        let names = session_clamp_tool_names(vec![], vec!["read_file".into(), "grep".into()]).await;
        assert!(has(&names, "read_file") && has(&names, "grep"), "{names:?}");
        assert!(!has(&names, "run_terminal_cmd"), "{names:?}");
        let names = session_clamp_tool_names(
            vec!["read_file".into(), "search_replace".into()],
            vec!["read_file".into(), "grep".into()],
        )
        .await;
        assert!(has(&names, "read_file"), "{names:?}");
        assert!(
            !has(&names, "search_replace"),
            "session denies it: {names:?}"
        );
        assert!(!has(&names, "grep"), "own allowlist denies it: {names:?}");
        let names = session_clamp_tool_names(
            vec!["search_replace".into()],
            vec!["read_file".into(), "grep".into()],
        )
        .await;
        assert!(
            !has(&names, "search_replace"),
            "session denies it: {names:?}"
        );
        assert!(
            !has(&names, "read_file"),
            "own allowlist denies it: {names:?}"
        );
        assert!(
            !has(&names, "run_terminal_cmd"),
            "must not inherit-all: {names:?}"
        );
    }
    #[tokio::test]
    async fn session_clamp_binds_when_own_allowlist_falls_back() {
        let names = session_clamp_tool_names(
            vec!["read_file".into(), "bogus_unresolved_xyz".into()],
            vec!["read_file".into()],
        )
        .await;
        assert!(names.iter().any(|n| n == "read_file"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "run_terminal_cmd"),
            "session clamp must bind despite the step-4 full-toolset fallback: {names:?}"
        );
    }
    #[test]
    fn session_tools_allowed_clamp() {
        let mut def = crate::config::AgentDefinition::general_purpose();
        assert!(def.session_tools_allowed("read_file"));
        def.session_tools_allowlist = Some(vec!["read_file".into()]);
        assert!(def.session_tools_allowed("Distill:read_file"));
        assert!(!def.session_tools_allowed("grep"));
        def.session_tools_denylist = Some(vec!["read_file".into()]);
        assert!(!def.session_tools_allowed("read_file"));
    }
    #[test]
    fn hosted_tool_gating() {
        let base = crate::config::AgentDefinition::general_purpose;
        assert!(base().hosted_tool_allowed("web_search"));
        assert!(base().hosted_tool_allowed("x_search"));
        let mut d = base();
        d.disallowed_tools = vec!["x_search".into()];
        assert!(!d.hosted_tool_allowed("x_search"));
        assert!(d.hosted_tool_allowed("web_search"));
        let mut d = base();
        d.tools = vec!["read_file".into()];
        assert!(!d.hosted_tool_allowed("web_search"));
        assert!(!d.hosted_tool_allowed("x_search"));
        let mut d = base();
        d.session_tools_allowlist = Some(vec!["read_file".into()]);
        assert!(!d.hosted_tool_allowed("web_search"));
    }
    const AGENT_TOOLS_BASE: &[&str] = &["read_file", "run_terminal_cmd"];
    #[tokio::test]
    async fn agent_type_restricted_to_listed_types() {
        let mut tools: Vec<String> = AGENT_TOOLS_BASE.iter().map(|s| s.to_string()).collect();
        tools.push("Agent(worker, researcher)".into());
        let agent = build_with_tools(tools, vec![]).await;
        assert_eq!(
            agent.definition().allowed_subagent_types,
            Some(vec!["worker".into(), "researcher".into()])
        );
        let names: Vec<_> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(names.contains(&"read_file".to_string()));
    }
    #[tokio::test]
    async fn bare_agent_allows_all_spawns() {
        let mut tools: Vec<String> = AGENT_TOOLS_BASE.iter().map(|s| s.to_string()).collect();
        tools.push("Agent".into());
        let agent = build_with_tools(tools, vec![]).await;
        assert_eq!(agent.definition().allowed_subagent_types, None);
    }
    #[tokio::test]
    async fn spawning_blocked_or_unrestricted() {
        let mut tools: Vec<String> = AGENT_TOOLS_BASE.iter().map(|s| s.to_string()).collect();
        tools.push("grep".into());
        let agent = build_with_tools(tools, vec![]).await;
        assert_eq!(agent.definition().allowed_subagent_types, Some(vec![]));
        let agent = build_with_tools(vec![], vec![]).await;
        assert_eq!(agent.definition().allowed_subagent_types, None);
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        let mut def = crate::config::AgentDefinition::default_distill();
        def.disallowed_tools = vec!["Agent".into()];
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(def)
        .build()
        .await
        .unwrap();
        assert_eq!(agent.definition().allowed_subagent_types, Some(vec![]));
    }
    #[tokio::test]
    async fn spawning_blocked_disables_all_background_bash_modes() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::distill::bash::BashParams;
        use distill_tools::notification::ToolNotificationHandle;
        use distill_tools::types::resources::Params;
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition.tools = vec!["run_terminal_cmd".into()];
        let bash_params = serde_json::json!({
            "max_timeout_secs": 36_000.0,
            "auto_background_on_timeout": true,
            "allow_background_operator": false,
        })
        .as_object()
        .unwrap()
        .clone();
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_bash_params(bash_params)
        .build()
        .await
        .expect("spawning-blocked agent should normalize background bash params");
        assert_eq!(agent.definition().allowed_subagent_types, Some(vec![]));
        let applied = agent
            .tool_bridge()
            .read_resource::<Params<BashParams>>()
            .await
            .expect("bash params should be registered");
        assert!(!applied.0.enabled_background);
        assert!(!applied.0.auto_background_on_timeout);
        assert_eq!(applied.0.max_timeout_secs, Some(36_000.0));
        assert!(!applied.0.allow_background_operator);
    }
    #[tokio::test]
    async fn disallowed_agent_type_strips_from_allowed() {
        let mut tools: Vec<String> = AGENT_TOOLS_BASE.iter().map(|s| s.to_string()).collect();
        tools.push("Agent(worker, researcher)".into());
        let agent = build_with_tools(tools, vec!["Agent(researcher)".into()]).await;
        assert_eq!(
            agent.definition().allowed_subagent_types,
            Some(vec!["worker".into()])
        );
    }
    #[tokio::test]
    async fn claude_tool_names_map_to_grok_equivalents() {
        let tools = vec!["Read".into(), "Bash".into(), "Grep".into()];
        let agent = build_with_tools(tools, vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"read_file".to_string()),
            "Read→read_file; got: {names:?}"
        );
        assert!(
            names.contains(&"run_terminal_command".to_string()),
            "Bash→run_terminal_command; got: {names:?}"
        );
        assert!(
            names.contains(&"grep".to_string()),
            "Grep→grep; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string()),
            "Edit must be excluded by the allowlist; got: {names:?}"
        );
    }
    #[test]
    fn shell_lsp_ask_and_task_tool_names_map() {
        assert_eq!(claude_tool_kind("PowerShell"), Some(ToolKind::Execute));
        assert_eq!(claude_tool_kind("LSP"), Some(ToolKind::Lsp));
        assert_eq!(claude_tool_kind("AskUserQuestion"), Some(ToolKind::AskUser));
        for name in ["TaskOutput", "BashOutputTool", "AgentOutputTool"] {
            assert_eq!(claude_tool_kind(name), Some(ToolKind::BackgroundTaskAction));
        }
        assert_eq!(claude_tool_kind("TaskStop"), Some(ToolKind::KillTaskAction));
        assert_eq!(claude_tool_kind("EnterPlanMode"), None);
        assert_eq!(claude_tool_kind("ExitPlanMode"), None);
    }
    #[tokio::test]
    async fn ask_user_question_allowlist_builds_without_plan_tools() {
        let tools = vec!["Read".into(), "Edit".into(), "AskUserQuestion".into()];
        let agent = build_with_tools(tools, vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        for kept in ["read_file", "search_replace", "ask_user_question"] {
            assert!(names.contains(&kept.to_string()), "got: {names:?}");
        }
        for dropped in ["enter_plan_mode", "exit_plan_mode", "run_terminal_command"] {
            assert!(!names.contains(&dropped.to_string()), "got: {names:?}");
        }
    }
    #[tokio::test]
    async fn unmappable_allowlist_falls_back_to_full_toolset() {
        let tools = vec!["Frobnicate".into(), "Wibble".into()];
        let agent = build_with_tools(tools, vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(names.contains(&"read_file".to_string()), "got: {names:?}");
        assert!(
            names.contains(&"search_replace".to_string()),
            "got: {names:?}"
        );
        assert!(
            names.contains(&"run_terminal_command".to_string()),
            "got: {names:?}"
        );
    }
    #[tokio::test]
    async fn plugin_style_agent_file_maps_claude_tools() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::notification::ToolNotificationHandle;
        const MD: &str = "---\n\
            name: test\n\
            description: test agent\n\
            tools: Read, Bash, Grep\n\
            ---\n\n\
            Test agent body.\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(&path, MD).unwrap();
        let def = crate::config::AgentDefinition::from_file_frontmatter_only(&path).unwrap();
        assert_eq!(
            def.tools,
            vec!["Read".to_string(), "Bash".to_string(), "Grep".to_string()],
        );
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(def)
        .build()
        .await
        .unwrap();
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"read_file".to_string()),
            "Read→read_file; got: {names:?}"
        );
        assert!(
            names.contains(&"run_terminal_command".to_string()),
            "Bash→run_terminal_command; got: {names:?}"
        );
        assert!(
            names.contains(&"grep".to_string()),
            "Grep→grep; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string()),
            "Edit must be excluded; got: {names:?}"
        );
    }
    #[tokio::test]
    async fn restrictive_allowlist_keeps_mcp_access() {
        let agent = build_with_tools(vec!["Read".into()], vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"read_file".to_string()),
            "Read→read_file; got: {names:?}"
        );
        assert!(
            names.contains(&"search_tool".to_string()) && names.contains(&"use_tool".to_string()),
            "search_tool/use_tool (MCP access) must not be stripped; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string()),
            "Edit must be excluded; got: {names:?}"
        );
    }
    #[tokio::test]
    async fn registered_but_absent_web_tools_do_not_fall_back() {
        let tools = vec![
            "read_file".into(),
            "grep".into(),
            "list_dir".into(),
            "web_search".into(),
            "web_fetch".into(),
        ];
        let agent = build_with_tools(tools, vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        for absent in ["web_search", "web_fetch"] {
            assert!(!names.contains(&absent.to_string()), "got: {names:?}");
        }
        for kept in ["read_file", "grep", "list_dir"] {
            assert!(names.contains(&kept.to_string()), "got: {names:?}");
        }
        for excluded in ["run_terminal_command", "search_replace"] {
            assert!(!names.contains(&excluded.to_string()), "got: {names:?}");
        }
    }
    #[tokio::test]
    async fn requested_enabled_web_tools_survive_allowlist() {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::distill::web_fetch::WebFetchConfig;
        use distill_tools::implementations::web_search::WebSearchConfig;
        use distill_tools::notification::ToolNotificationHandle;
        let mut definition = crate::config::AgentDefinition::default_distill();
        definition.tools = vec![
            "read_file".into(),
            "grep".into(),
            "list_dir".into(),
            "web_search".into(),
            "web_fetch".into(),
        ];
        let agent = AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(definition)
        .with_web_search_config(WebSearchConfig::Enabled {
            api_key: "test-key".into(),
            base_url: "https://api.x.ai/v1".into(),
            model: "test-web-search-model".into(),
            extra_headers: Default::default(),
            alpha_test_key: None,
            allowed_domains: None,
            excluded_domains: None,
        })
        .with_web_fetch_config(WebFetchConfig::Enabled {
            params: Default::default(),
        })
        .build()
        .await
        .expect("agent should build with requested web tools");
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        for kept in ["read_file", "grep", "list_dir", "web_search", "web_fetch"] {
            assert!(names.contains(&kept.to_string()), "got: {names:?}");
        }
        for excluded in ["run_terminal_command", "search_replace"] {
            assert!(!names.contains(&excluded.to_string()), "got: {names:?}");
        }
    }
    #[tokio::test]
    async fn skill_allowlist_maps_to_read() {
        let agent = build_with_tools(vec!["Skill".into()], vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"read_file".to_string()),
            "Skill→read_file; got: {names:?}"
        );
        assert!(
            names.contains(&"search_tool".to_string()) && names.contains(&"use_tool".to_string()),
            "MCP access must be kept; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string())
                && !names.contains(&"run_terminal_command".to_string()),
            "no full-toolset fallback — unlisted tools must be excluded; got: {names:?}"
        );
    }
    #[tokio::test]
    async fn read_and_skill_allowlist_keeps_single_read_file() {
        let agent = build_with_tools(vec!["Read".into(), "Skill".into()], vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        let read_file_count = names.iter().filter(|n| *n == "read_file").count();
        assert_eq!(
            read_file_count, 1,
            "read_file must be registered exactly once for tools: [Read, Skill]; got: {names:?}"
        );
    }
    #[tokio::test]
    async fn mcp_prefixed_allowlist_entry_keeps_mcp_access() {
        let tools = vec!["mcp__github__create_issue".into(), "Read".into()];
        let agent = build_with_tools(tools, vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"read_file".to_string()),
            "Read must be kept; got: {names:?}"
        );
        assert!(
            names.contains(&"search_tool".to_string()) && names.contains(&"use_tool".to_string()),
            "MCP access must be kept; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string())
                && !names.contains(&"run_terminal_command".to_string()),
            "no full-toolset fallback — unlisted tools must be excluded; got: {names:?}"
        );
    }
    #[tokio::test]
    async fn tool_search_allowlist_maps_to_search_tool() {
        let agent = build_with_tools(vec!["ToolSearch".into()], vec![]).await;
        let names: Vec<String> = agent
            .tool_definitions()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            names.contains(&"search_tool".to_string()),
            "ToolSearch→search_tool; got: {names:?}"
        );
        assert!(
            !names.contains(&"search_replace".to_string()),
            "no full-toolset fallback — Edit must be excluded; got: {names:?}"
        );
    }
    async fn build_with_web_search(
        web_search_enabled: bool,
        backend_search_enabled: bool,
        disallowed_tools: &[&str],
        tool_overrides: Option<distill_sampling_types::ToolOverrides>,
    ) -> crate::agent::Agent {
        use distill_tools::computer::local::LocalTerminalBackend;
        use distill_tools::implementations::web_search::WebSearchConfig;
        use distill_tools::notification::ToolNotificationHandle;
        let web_search_config = if web_search_enabled {
            WebSearchConfig::Enabled {
                api_key: "test-key".into(),
                base_url: "https://api.x.ai/v1".into(),
                model: "test-web-search-model".into(),
                extra_headers: Default::default(),
                alpha_test_key: None,
                allowed_domains: None,
                excluded_domains: None,
            }
        } else {
            WebSearchConfig::Disabled
        };
        let mut def = crate::config::AgentDefinition::default_distill();
        def.disallowed_tools = disallowed_tools.iter().map(|s| s.to_string()).collect();
        def.tool_overrides = tool_overrides;
        AgentBuilder::new(
            std::env::temp_dir(),
            Arc::new(LocalTerminalBackend::new()),
            ToolNotificationHandle::noop(),
        )
        .from_definition(def)
        .with_web_search_config(web_search_config)
        .with_backend_search(backend_search_enabled)
        .build()
        .await
        .expect("agent should build for backend-search test case")
    }
    #[tokio::test]
    async fn disallowed_web_search_strips_function_and_hosted_tools() {
        let agent = build_with_web_search(true, true, &["web_search"], None).await;
        let hosted = agent.hosted_tools();
        assert!(
            !hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::WebSearch { .. })),
            "hosted WebSearch must be removed when web_search is disallowed, got: {hosted:?}"
        );
        assert!(
            hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::XSearch { .. })),
            "XSearch must remain when only web_search is disallowed, got: {hosted:?}"
        );
        let has_web_search_fn = agent
            .tool_definitions()
            .await
            .iter()
            .any(|td| short_tool_name(&td.function.name) == "web_search");
        assert!(
            !has_web_search_fn,
            "function web_search tool must be removed when disallowed"
        );
    }
    #[tokio::test]
    async fn hosted_tools_populated_when_backend_search_and_web_search_enabled() {
        let agent = build_with_web_search(true, true, &[], None).await;
        assert!(agent.backend_search_enabled());
        let hosted = agent.hosted_tools();
        assert!(
            hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::WebSearch { .. })),
            "expected WebSearch hosted tool, got: {hosted:?}"
        );
        assert!(
            hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::XSearch { .. })),
            "expected XSearch hosted tool, got: {hosted:?}"
        );
    }
    #[tokio::test]
    async fn hosted_tools_only_xsearch_when_web_search_disabled() {
        let agent = build_with_web_search(false, true, &[], None).await;
        let hosted = agent.hosted_tools();
        assert!(
            !hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::WebSearch { .. })),
            "WebSearch must NOT appear when web_search is disabled, got: {hosted:?}"
        );
        assert!(
            hosted
                .iter()
                .any(|t| matches!(t, distill_sampling_types::HostedTool::XSearch { .. })),
            "expected XSearch hosted tool, got: {hosted:?}"
        );
    }
    #[tokio::test]
    async fn hosted_tools_empty_when_backend_search_disabled() {
        let agent = build_with_web_search(true, false, &[], None).await;
        assert!(!agent.backend_search_enabled());
        assert!(agent.hosted_tools().is_empty());
    }
    #[tokio::test]
    async fn hosted_tools_bake_definition_tool_overrides_into_options() {
        let x_search = distill_sampling_types::XSearchOptions {
            date_bound: Some(
                distill_sampling_types::SearchDateBound::new(None, Some("2024-03-15".into()))
                    .unwrap(),
            ),
        };
        let agent = build_with_web_search(
            true,
            true,
            &[],
            Some(distill_sampling_types::ToolOverrides {
                x_search: Some(x_search.clone()),
                web_search: None,
            }),
        )
        .await;
        assert!(
            agent
                .hosted_tools()
                .contains(&distill_sampling_types::HostedTool::XSearch {
                    options: Some(x_search),
                }),
            "definition tool_overrides must be applied to HostedTool options"
        );
    }
}
