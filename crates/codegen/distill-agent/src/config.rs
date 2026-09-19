// Modified for Distill by Samuel Fajreldines, 2026.
//! Agent definition types, parsed from `.grok/agents/*.md` files.
use crate::error::AgentBuildError;
use crate::prompt::context::TemplateOverride;
use crate::prompt::user_message::UserMessageTemplate;
use distill_tools::implementations::codex;
use distill_tools::implementations::distill;
use distill_tools::implementations::distill_concise;
use distill_tools::implementations::memory;
use distill_tools::implementations::opencode;
use distill_tools::implementations::search_tool;
use distill_tools::implementations::use_tool;
use distill_tools::registry::types::{ToolConfig, ToolServerConfig};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use strum::{AsRefStr, Display, EnumIter, EnumString, IntoStaticStr};
/// Process-global registry of externally-provided toolset presets.
/// Public presets are enumerated; internal presets resolve only via [`toolset_for_preset`] and never appear in the manifest.
/// Register before the first preset resolution — earlier-resolved configs will not see later registrations.
pub type ToolsetPresetBuilder = fn() -> ToolServerConfig;
/// Whether a registered preset is enumerated publicly or resolved by name only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PresetVisibility {
    /// A product preset: enumerated by `preset_names()` / `all_toolset_presets()`.
    Public,
    /// A harness-internal preset: resolvable by name but never enumerated.
    Internal,
}
static TOOLSET_PRESETS: OnceLock<Mutex<HashMap<String, (ToolsetPresetBuilder, PresetVisibility)>>> =
    OnceLock::new();
fn toolset_preset_registry()
-> &'static Mutex<HashMap<String, (ToolsetPresetBuilder, PresetVisibility)>> {
    TOOLSET_PRESETS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Register an out-of-tree **public** (product) toolset preset by name. See [`TOOLSET_PRESETS`].
pub fn register_toolset_preset(name: &str, builder: ToolsetPresetBuilder) {
    toolset_preset_registry()
        .lock()
        .expect("toolset preset registry poisoned")
        .insert(name.to_string(), (builder, PresetVisibility::Public));
}
/// Register an out-of-tree **internal** toolset preset by name; the shell / orchestrator spawn path resolves it via [`toolset_for_preset`].
/// See [`TOOLSET_PRESETS`].
pub fn register_internal_toolset_preset(name: &str, builder: ToolsetPresetBuilder) {
    toolset_preset_registry()
        .lock()
        .expect("toolset preset registry poisoned")
        .insert(name.to_string(), (builder, PresetVisibility::Internal));
}
/// Look up an externally-registered toolset preset by (already-normalized) name.
/// Resolves BOTH public and internal presets.
fn registered_toolset_preset(name: &str) -> Option<ToolServerConfig> {
    toolset_preset_registry()
        .lock()
        .expect("toolset preset registry poisoned")
        .get(name)
        .map(|(f, _)| f())
}
/// Names of externally-registered **public** presets only (internal presets are intentionally excluded from enumeration).
fn registered_public_toolset_preset_names() -> Vec<String> {
    toolset_preset_registry()
        .lock()
        .expect("toolset preset registry poisoned")
        .iter()
        .filter(|(_, (_, visibility))| *visibility == PresetVisibility::Public)
        .map(|(name, _)| name.clone())
        .collect()
}
/// Orchestrator-specific prompt body appended to the standard Distill system prompt (`prompt.md`).
/// Instructs the GBL model to delegate coding and exploration work to subagents.
const ORCHESTRATOR_PROMPT_BODY: &str = "\
## Orchestrator Mode

You are a technical lead orchestrating a team of senior-engineer subagents. Your subagents \
are highly capable \u{2014} treat them as expert peers, not junior helpers. Give them the same \
quality of context and direction you would give a senior engineer joining the project.

Your job is to think, plan, coordinate, and review. Their job is to explore, implement, \
and execute. Use them aggressively and liberally \u{2014} spawn subagents early and often.

### Your direct responsibilities:
- High-level planning and architecture decisions
- Reading files for quick context (${{ tools.by_kind.read }}, ${{ tools.by_kind.search }}, ${{ tools.by_kind.list }})
- Running quick terminal commands for orientation (${{ tools.by_kind.execute }})
- Invoking skills and MCP tools (${{ tools.by_kind.skill }}, ${{ tools.by_kind.search_tool }}, ${{ tools.by_kind.use_tool }})
- Web research (${{ tools.by_kind.web_search }}, ${{ tools.by_kind.web_fetch }})
- Asking the user questions (${{ tools.by_kind.ask_user }})
- Managing task lists and tracking progress (${{ tools.by_kind.plan }})
- Reviewing subagent results and synthesizing responses for the user

### ALWAYS delegate to subagents:
- **ALL file modifications** \u{2014} creating, editing, deleting files (`general-purpose`)
- **ALL builds, tests, and verification** \u{2014} running test suites, linters, compilers (`general-purpose`)
- **Deep codebase exploration** \u{2014} searching across many files, understanding patterns (`explore`)
- **Multi-step implementation** \u{2014} any task involving more than reading (`general-purpose`)
- **Any research requiring thoroughness** \u{2014} don\u{2019}t do shallow searches yourself, spawn an `explore` subagent

### How to talk to subagents:
Write prompts the way you would brief a senior engineer:
- Explain WHAT you need done and WHY (the context behind the task)
- Share what you already know \u{2014} file paths, function names, architectural decisions
- Describe the end state, not step-by-step commands \u{2014} trust their judgment on HOW
- If you have opinions on approach, share them as guidance, not rigid instructions
- Include acceptance criteria: what does \"done\" look like?

### Parallelism:
- Break independent tasks into separate subagents and run them in parallel
- Use `explore` subagents to investigate multiple areas simultaneously
- Launch implementation subagents for independent files/modules at the same time
- Do NOT wait for one subagent before spawning others that don\u{2019}t depend on it

### Anti-patterns to avoid:
- Do NOT do shallow 1-2 file reads yourself when an `explore` agent would be more thorough
- Do NOT implement code changes yourself \u{2014} you have no file editing tools
- Do NOT give subagents overly prescriptive step-by-step instructions \u{2014} trust their expertise
- Do NOT summarize or re-explain what the user said \u{2014} get to work immediately";
/// Bash tool with clearer model-facing names: `run_terminal_cmd` becomes `run_terminal_command` and `is_background` becomes `background`.
fn bash_tool_config() -> ToolConfig {
    ToolConfig::from(&distill::BashTool)
        .with_name("run_terminal_command")
        .with_param_rename("is_background", "background")
}
/// Task/subagent tool with clearer model-facing names: `task` becomes `spawn_subagent` and `run_in_background` becomes `background`.
fn task_tool_config() -> ToolConfig {
    ToolConfig::from(&distill::TaskTool)
        .with_name("spawn_subagent")
        .with_param_rename("run_in_background", "background")
}
/// Task output tool renamed for clarity: `get_task_output` becomes `get_command_or_subagent_output`.
fn task_output_tool_config() -> ToolConfig {
    ToolConfig::from(&distill::TaskOutputTool).with_name("get_command_or_subagent_output")
}
/// `wait_tasks` becomes `wait_commands_or_subagents`.
fn wait_tasks_tool_config() -> ToolConfig {
    ToolConfig::from(&distill::WaitTasksTool).with_name("wait_commands_or_subagents")
}
/// `kill_task` becomes `kill_command_or_subagent`.
fn kill_task_tool_config() -> ToolConfig {
    ToolConfig::from(&distill::KillTaskTool).with_name("kill_command_or_subagent")
}
/// Complete workspace-executable toolset for hub registration.
/// Extends `default_distill_toolset()` with tools injected by `AgentBuilder` or available only in specific modes.
/// In proxy mode the workspace server executes all tools; the shell has zero local dispatch.
pub fn workspace_distill_toolset() -> ToolServerConfig {
    let mut tools = default_distill_toolset().tools;
    tools.push((&opencode::OpenCodeWriteTool).into());
    tools.push((&distill::EnterPlanModeTool).into());
    tools.push((&distill::ExitPlanModeTool).into());
    tools.push((&distill::AskUserQuestionTool).into());
    tools.push((&distill::WebSearchTool).into());
    tools.push((&distill::ImageGenTool).into());
    tools.push((&distill::ImageToVideoTool).into());
    tools.push((&distill::ReferenceToVideoTool).into());
    tools.push((&distill::WebFetchTool).into());
    tools.push((&memory::search_tool::MemorySearchImpl).into());
    tools.push((&memory::get_tool::MemoryGetImpl).into());
    tools.push((&distill::LspTool).into());
    ToolServerConfig {
        tools,
        behavior_preset: None,
    }
}
/// Fully qualified ids of the workspace tools that call the Grok API with the workspace server's own credential.
/// A server whose credential only serves the hub cannot run them, so it neither advertises nor serves them.
pub fn api_backed_tool_ids() -> Vec<String> {
    #[allow(unused_mut)]
    let mut ids = vec![
        ToolConfig::from(&distill::WebSearchTool).id,
        ToolConfig::from(&distill::ImageGenTool).id,
        ToolConfig::from(&distill::ImageToVideoTool).id,
        ToolConfig::from(&distill::ReferenceToVideoTool).id,
    ];
    ids
}
/// Toolset for the `grok-computer` (workspace/sandbox) preset.
fn grok_computer_toolset() -> ToolServerConfig {
    #[allow(unused_mut)]
    let mut tools = vec![
        bash_tool_config(),
        (&distill::ReadFileTool).into(),
        (&distill::SearchReplaceTool).into(),
        (&opencode::OpenCodeWriteTool).into(),
        (&distill::ListDirTool).into(),
        (&distill::GrepTool).into(),
        (&distill::KillTerminalCommandTool).into(),
        (&distill::GetTerminalCommandOutputTool).into(),
        (&distill::SchedulerCreateTool).into(),
        (&distill::SchedulerDeleteTool).into(),
        (&distill::SchedulerListTool).into(),
    ];
    ToolServerConfig {
        tools,
        behavior_preset: None,
    }
}
/// Every named toolset preset, as `(normalized_name, config)` pairs.
/// Single source of truth: [`toolset_for_preset`] resolves through this table, and coverage tests iterate it.
/// A new preset is covered the moment it becomes resolvable.
fn native_toolset_presets() -> Vec<(&'static str, ToolServerConfig)> {
    vec![
        ("distill", workspace_distill_toolset()),
        ("distill-concise", distill_concise_toolset()),
        ("distill-plan", distill_plan_toolset()),
        ("codex", codex_toolset()),
        ("explore", explore_toolset()),
        ("plan", plan_toolset()),
        ("grok-computer", grok_computer_toolset()),
    ]
}
/// Every named **public** toolset preset (native and externally registered public presets), as `(name, config)` pairs.
/// Harness-internal registered presets are intentionally excluded; resolve them by name via [`toolset_for_preset`].
fn all_toolset_presets() -> Vec<(String, ToolServerConfig)> {
    let mut out: Vec<(String, ToolServerConfig)> = native_toolset_presets()
        .into_iter()
        .map(|(name, cfg)| (name.to_string(), cfg))
        .collect();
    for name in registered_public_toolset_preset_names() {
        if !out.iter().any(|(n, _)| *n == name)
            && let Some(cfg) = registered_toolset_preset(&name)
        {
            out.push((name, cfg));
        }
    }
    out
}
/// Names of every named toolset preset (native first, then registered).
pub fn preset_names() -> Vec<String> {
    all_toolset_presets()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}
/// Resolve a named toolset preset to its [`ToolServerConfig`], or `None` if unknown.
pub fn toolset_for_preset(preset: &str) -> Option<ToolServerConfig> {
    let normalized = preset.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    native_toolset_presets()
        .into_iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, toolset)| toolset)
        .or_else(|| registered_toolset_preset(&normalized))
}
fn default_distill_toolset() -> ToolServerConfig {
    distill_core_toolset_with(true, true)
}
fn default_agent_toolset() -> ToolServerConfig {
    distill_core_toolset(true)
}
/// Same as the parent distill list, without `workflow`.
/// The usual `general-purpose` spawn path must not add that tool and then strip it.
fn general_purpose_toolset() -> ToolServerConfig {
    distill_core_toolset(false)
}
fn distill_core_toolset(include_workflow: bool) -> ToolServerConfig {
    distill_core_toolset_with(include_workflow, false)
}
fn distill_core_toolset_with(
    include_workflow: bool,
    include_send_feedback: bool,
) -> ToolServerConfig {
    let mut tools = vec![
        bash_tool_config(),
        (&distill::ReadFileTool).into(),
        (&distill::SearchReplaceTool).into(),
        (&distill::ListDirTool).into(),
        (&distill::GrepTool).into(),
        kill_task_tool_config(),
        (&distill::TodoWriteTool).into(),
        task_output_tool_config(),
        wait_tasks_tool_config(),
        task_tool_config(),
        (&distill::SchedulerCreateTool).into(),
        (&distill::SchedulerDeleteTool).into(),
        (&distill::SchedulerListTool).into(),
        (&distill::MonitorTool).into(),
        (&search_tool::SearchTool).into(),
        (&use_tool::UseTool).into(),
        (&distill::UpdateGoalTool).into(),
    ];
    if include_workflow {
        tools.push((&distill::WorkflowTool).into());
    }
    if include_send_feedback {
        tools.push((&distill::SendFeedbackTool).into());
    }
    ToolServerConfig {
        tools,
        behavior_preset: None,
    }
}
fn distill_concise_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            (&distill_concise::BashConciseTool).into(),
            (&distill_concise::ReadFileConciseTool).into(),
            (&distill_concise::SearchReplaceConciseTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            kill_task_tool_config(),
            (&distill::TodoWriteTool).into(),
            task_output_tool_config(),
            (&distill::SchedulerCreateTool).into(),
            (&distill::SchedulerDeleteTool).into(),
            (&distill::SchedulerListTool).into(),
            (&distill::MonitorTool).into(),
            (&distill::UpdateGoalTool).into(),
            (&distill::WorkflowTool).into(),
        ],
        behavior_preset: None,
    }
}
/// Hashline toolset: anchor-based read/edit/search and standard utilities.
/// `hashline_tools` should be the 3 hashline `ToolConfig` entries; they carry the scheme parameters as tool params.
pub fn distill_hashline_toolset(
    hashline_tools: Vec<distill_tools::registry::types::ToolConfig>,
) -> ToolServerConfig {
    let mut tools: Vec<distill_tools::registry::types::ToolConfig> = vec![bash_tool_config()];
    tools.extend(hashline_tools);
    tools.extend([
        (&distill::ListDirTool).into(),
        kill_task_tool_config(),
        (&distill::TodoWriteTool).into(),
        task_output_tool_config(),
        wait_tasks_tool_config(),
        task_tool_config(),
        (&distill::WebSearchTool).into(),
        (&distill::SchedulerCreateTool).into(),
        (&distill::SchedulerDeleteTool).into(),
        (&distill::SchedulerListTool).into(),
        (&distill::MonitorTool).into(),
        (&search_tool::SearchTool).into(),
        (&use_tool::UseTool).into(),
        (&distill::UpdateGoalTool).into(),
        (&distill::WorkflowTool).into(),
    ]);
    ToolServerConfig {
        tools,
        behavior_preset: None,
    }
}
fn codex_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            bash_tool_config(),
            (&codex::CodexReadFileTool).into(),
            (&codex::ApplyPatchTool).into(),
            (&codex::CodexListDirTool).into(),
            (&codex::CodexGrepFilesTool).into(),
            kill_task_tool_config(),
            (&distill::TodoWriteTool).into(),
            task_output_tool_config(),
            (&search_tool::SearchTool).into(),
            (&use_tool::UseTool).into(),
        ],
        behavior_preset: None,
    }
}
/// Read-only toolset for the explore subagent.
/// `run_terminal_command` is omitted so exploration cannot mutate the workspace — the toolset enforces read-only, not just the prompt.
/// With no `BashTool`, the background-task helpers are also omitted.
fn explore_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            (&distill::ReadFileTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
        ],
        behavior_preset: None,
    }
}
/// Plan-mode toolset: read-only inspection tools, no shell, no file-editing.
///
/// Enforces read-only at the toolset: the agent may inspect the repo and keep a todo list but cannot mutate the workspace.
fn plan_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            (&distill::ReadFileTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            // (&distill::SkillTool).into(),
            (&distill::TodoWriteTool).into(),
            // search_replace and run_terminal_command intentionally omitted (read-only)
        ],
        behavior_preset: None,
    }
}
/// Extends the default `distill` toolset with plan mode tools.
/// This allows the agent to enter a structured planning phase before writing code, with user-approved plans.
fn distill_plan_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            // Standard distill tools
            bash_tool_config(),
            (&distill::ReadFileTool).into(),
            (&distill::SearchReplaceTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            kill_task_tool_config(),
            (&distill::TodoWriteTool).into(),
            task_output_tool_config(),
            task_tool_config(),
            (&distill::SchedulerCreateTool).into(),
            (&distill::SchedulerDeleteTool).into(),
            (&distill::SchedulerListTool).into(),
            (&distill::MonitorTool).into(),
            (&search_tool::SearchTool).into(),
            (&use_tool::UseTool).into(),
            (&distill::UpdateGoalTool).into(),
            (&distill::WorkflowTool).into(),
            // Plan mode tools
            (&distill::EnterPlanModeTool).into(),
            (&distill::ExitPlanModeTool).into(),
            (&distill::AskUserQuestionTool).into(),
        ],
        behavior_preset: None,
    }
}
/// Orchestrator toolset: read/search/orchestration tools only. No terminal execution, no file editing.
/// The orchestrator delegates all execution and file modification to subagents.
fn orchestrator_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            // Research tools
            bash_tool_config(),
            (&distill::ReadFileTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            // Subagent orchestration
            task_tool_config(),
            task_output_tool_config(),
            wait_tasks_tool_config(),
            kill_task_tool_config(),
            // Skills and MCP
            (&search_tool::SearchTool).into(),
            (&use_tool::UseTool).into(),
            // Planning and user interaction
            (&distill::TodoWriteTool).into(),
            (&distill::EnterPlanModeTool).into(),
            (&distill::ExitPlanModeTool).into(),
            (&distill::AskUserQuestionTool).into(),
            (&distill::UpdateGoalTool).into(),
            (&distill::WorkflowTool).into(),
            // Scheduling and monitoring
            (&distill::SchedulerCreateTool).into(),
            (&distill::SchedulerDeleteTool).into(),
            (&distill::SchedulerListTool).into(),
            (&distill::MonitorTool).into(),
            // Web tools
            (&distill::WebSearchTool).into(),
            (&distill::WebFetchTool).into(),
            // Imagine
            (&distill::ImageGenTool).into(),
            (&distill::ImageToVideoTool).into(),
            (&distill::ReferenceToVideoTool).into(),
            // Memory
            (&memory::MemorySearchImpl).into(),
            (&memory::MemoryGetImpl).into(),
            // Intentionally excluded:
            // - SearchReplaceTool (no file editing; delegate to subagents)
            // - OpenCodeWriteTool (no file writing; delegate to subagents)
        ],
        behavior_preset: None,
    }
}
/// Same as `distill_plan_toolset` but excludes `TaskTool`, `TaskOutputTool`, and `KillTaskTool`.
/// Use this when the shell does not have subagent infrastructure wired up.
fn distill_plan_no_subagents_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            // Standard distill tools, minus TaskTool only
            // KillTaskTool and TaskOutputTool are kept because BashTool's background mode requires them
            bash_tool_config(),
            (&distill::ReadFileTool).into(),
            (&distill::SearchReplaceTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            kill_task_tool_config(),
            (&distill::TodoWriteTool).into(),
            task_output_tool_config(),
            (&distill::SchedulerCreateTool).into(),
            (&distill::SchedulerDeleteTool).into(),
            (&distill::SchedulerListTool).into(),
            (&distill::MonitorTool).into(),
            (&search_tool::SearchTool).into(),
            (&use_tool::UseTool).into(),
            (&distill::UpdateGoalTool).into(),
            (&distill::WorkflowTool).into(),
            // Plan mode tools
            (&distill::EnterPlanModeTool).into(),
            (&distill::ExitPlanModeTool).into(),
            (&distill::AskUserQuestionTool).into(),
        ],
        behavior_preset: None,
    }
}
/// Same as `default_distill_toolset` with the `AskUserQuestionTool` added, allowing the agent to ask structured questions without full plan mode.
fn distill_ask_user_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            bash_tool_config(),
            (&distill::ReadFileTool).into(),
            (&distill::SearchReplaceTool).into(),
            (&distill::ListDirTool).into(),
            (&distill::GrepTool).into(),
            kill_task_tool_config(),
            (&distill::TodoWriteTool).into(),
            task_output_tool_config(),
            wait_tasks_tool_config(),
            task_tool_config(),
            (&distill::SchedulerCreateTool).into(),
            (&distill::SchedulerDeleteTool).into(),
            (&distill::SchedulerListTool).into(),
            (&distill::MonitorTool).into(),
            (&search_tool::SearchTool).into(),
            (&use_tool::UseTool).into(),
            (&distill::UpdateGoalTool).into(),
            (&distill::WorkflowTool).into(),
            (&distill::AskUserQuestionTool).into(),
        ],
        behavior_preset: None,
    }
}
fn opencode_toolset() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            (&opencode::OpenCodeBashTool).into(),
            (&opencode::OpenCodeReadTool).into(),
            (&opencode::OpenCodeEditTool).into(),
            (&opencode::OpenCodeWriteTool).into(),
            (&opencode::OpenCodeGrepTool).into(),
            (&opencode::OpenCodeGlobTool).into(),
            (&opencode::OpenCodeTodoWriteTool).into(),
            (&opencode::OpenCodeSkillTool).into(),
            kill_task_tool_config(),
            task_output_tool_config(),
        ],
        behavior_preset: None,
    }
}
/// Model override for an agent definition. An `Override` ID is resolved against available models at subagent spawn.
/// `model: inherit` or omitted → `Inherit`; a concrete id → `Override`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelOverride {
    /// Use the parent session's model.
    #[default]
    Inherit,
    /// Use a specific model ID.
    Override(String),
}
impl std::fmt::Display for ModelOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inherit => f.write_str("inherit"),
            Self::Override(id) => f.write_str(id),
        }
    }
}
impl<'de> Deserialize<'de> for ModelOverride {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        match opt {
            None => Ok(Self::Inherit),
            Some(s) if s.is_empty() || s.eq_ignore_ascii_case("inherit") => Ok(Self::Inherit),
            Some(s) => Ok(Self::Override(s)),
        }
    }
}
impl serde::Serialize for ModelOverride {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Inherit => serializer.serialize_str("inherit"),
            Self::Override(id) => serializer.serialize_str(id),
        }
    }
}
const AGENT_TASK_KEYWORDS: &str = "Agent|Task";
/// Splits `"Agent(a, b), read_file"` into `["Agent(a, b)", "read_file"]`.
pub static AGENT_TASK_TOKENIZER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(&format!(r"(?i:{AGENT_TASK_KEYWORDS})\([^)]*\)|[^,]+"))
            .expect("valid regex")
    });
/// Matches `"Agent(a, b)"` and captures `"a, b"` in group 1; the group is `None` for bare `Agent`.
pub static AGENT_TASK_CLASSIFIER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(&format!(r"^(?i:{AGENT_TASK_KEYWORDS})(?:\(([^)]*)\))?$"))
            .expect("valid regex")
    });
/// Accepts `"a, b, c"` or `["a", "b"]`. Trims whitespace.
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct StringOrVec;
    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a comma-separated string or an array of strings")
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok(AGENT_TASK_TOKENIZER_RE
                .find_iter(s)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty())
                .collect())
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                v.push(item);
            }
            Ok(v)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }
    deserializer.deserialize_any(StringOrVec)
}
/// Accepts a positive u32 or null/absent. Rejects 0.
fn deserialize_nonzero_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<u32>::deserialize(deserializer)?;
    if let Some(0) = opt {
        return Err(serde::de::Error::custom("maxTurns must be greater than 0"));
    }
    Ok(opt)
}
/// Eliminates string matching in discovery and ensures built-in names are defined in exactly one place.
/// `subagent_variants()` returns only the 3 exposed to the LLM. The remaining 6 are resolvable by name but not advertised.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter, AsRefStr, IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum BuiltinAgentName {
    #[strum(to_string = "distill", serialize = "grok-build")]
    Distill,
    #[strum(to_string = "distill-concise", serialize = "grok-build-concise")]
    DistillConcise,
    #[strum(to_string = "distill-plan", serialize = "grok-build-plan")]
    DistillPlan,
    #[strum(
        to_string = "distill-plan-no-subagents",
        serialize = "grok-build-plan-no-subagents"
    )]
    DistillPlanNoSubagents,
    #[strum(to_string = "distill-ask-user", serialize = "grok-build-ask-user")]
    DistillAskUser,
    Codex,
    Opencode,
    GeneralPurpose,
    Explore,
    Plan,
    BrowserUse,
    #[strum(
        to_string = "distill-orchestrator",
        serialize = "grok-build-orchestrator"
    )]
    DistillOrchestrator,
}
/// Resolves via `BuiltinAgentName` and delegates to [`AgentDefinition::is_strict_harness`].
/// Unknown names return `false`: never enforce a harness we can't verify.
/// Callers that already hold an `AgentDefinition` should call that method directly so project-level shadowing is honored.
pub fn is_strict_harness_agent_type(name: &str) -> bool {
    use std::str::FromStr;
    BuiltinAgentName::from_str(name)
        .map(|b| b.definition().is_strict_harness())
        .unwrap_or(false)
}
impl BuiltinAgentName {
    pub fn definition(self) -> AgentDefinition {
        match self {
            Self::Distill => AgentDefinition::default_distill(),
            Self::DistillConcise => AgentDefinition::distill_concise(),
            Self::DistillPlan => AgentDefinition::distill_plan(),
            Self::DistillPlanNoSubagents => AgentDefinition::distill_plan_no_subagents(),
            Self::DistillAskUser => AgentDefinition::distill_ask_user(),
            Self::Codex => AgentDefinition::codex(),
            Self::Opencode => AgentDefinition::opencode(),
            Self::GeneralPurpose => AgentDefinition::general_purpose(),
            Self::Explore => AgentDefinition::explore(),
            Self::Plan => AgentDefinition::plan(),
            Self::BrowserUse => AgentDefinition::browser_use(),
            Self::DistillOrchestrator => AgentDefinition::distill_orchestrator(),
        }
    }
    /// Built-in agents available as subagents via the Task tool.
    pub fn subagent_variants() -> &'static [Self] {
        &[Self::GeneralPurpose, Self::Explore, Self::Plan]
    }
}
/// Portable agent identity, parsed from .grok/agents/*.md. Usable as a top-level agent or a subagent.
/// Does not contain session-level policies; those are provided by the AgentBuilder at build time.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    /// Plugin namespace for plugin-backed agents only.
    #[serde(skip)]
    pub plugin_name: Option<String>,
    /// Prevents external definitions from opting into built-in-only policy by name.
    #[serde(skip)]
    pub(crate) builtin_name: Option<BuiltinAgentName>,
    #[serde(default = "default_prompt_mode")]
    pub prompt_mode: PromptMode,
    #[serde(default = "default_agent_toolset")]
    pub tool_config: ToolServerConfig,
    /// Runtime capability mode that constrains which tool kinds the agent can use.
    /// Applied during subagent spawn in `handle_subagent_request` by filtering the definition's `tool_config` before session creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_mode: Option<distill_tool_types::SubagentCapabilityMode>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub skills: Vec<String>,
    /// When true (the default), the AgentBuilder discovers skills from CWD at build time and seeds mid-session skill discovery.
    /// When false, skill discovery is suppressed and the agent gets an empty skill list with no CWD-based runtime discovery.
    #[serde(default = "default_true")]
    pub discover_skills: bool,
    /// Whether to inherit the parent session's discovered skills when spawned as a subagent. Ignored for primary sessions.
    #[serde(default = "default_true")]
    pub inherit_skills: bool,
    #[serde(default = "default_true")]
    pub agents_md: bool,
    /// When true (the default), the AgentBuilder layers session-level optional tools on the declared `tool_config`.
    /// Set false for harnesses that need an exact, minimal toolset matching a trained schema.
    /// The agent's `tool_config` is then used verbatim with only the subagent strip applied.
    #[serde(default = "default_true")]
    pub inject_default_tools: bool,
    /// Tool allowlist. Empty means inherit all. Also carries `Agent(type)` directives.
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub tools: Vec<String>,
    /// Tool denylist. `Agent(type)` entries strip spawn permissions.
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
    #[serde(default, deserialize_with = "deserialize_nonzero_u32")]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub isolation: Option<IsolationMode>,
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_agent_color")]
    pub color: Option<AgentColor>,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerRef>,
    #[serde(default)]
    pub mcp_inheritance: McpInheritance,
    #[serde(default, deserialize_with = "deserialize_hooks_map")]
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub memory: Option<MemoryScope>,
    #[serde(default)]
    pub model: ModelOverride,
    #[serde(default)]
    pub completion_requirement: Option<CompletionRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_overrides: Option<distill_sampling_types::ToolOverrides>,
    /// Subagent types this agent can spawn (derived by builder from `tools`).
    /// `None` is unrestricted, `Some([t1])` restricts to those types, and `Some([])` blocks spawning.
    #[serde(skip)]
    pub allowed_subagent_types: Option<Vec<String>>,
    /// Session-operator tool restrictions (`--tools` / `--disallowed-tools`), distinct from the author's own filters.
    /// Applied as a final clamp over the fully-assembled toolset; compose with the agent's filters by intersection.
    /// `None` means no session restriction.
    #[serde(skip)]
    pub session_tools_allowlist: Option<Vec<String>>,
    #[serde(skip)]
    pub session_tools_denylist: Option<Vec<String>>,
    #[serde(skip)]
    pub prompt_body: Option<String>,
    #[serde(skip)]
    pub system_prompt: TemplateOverride,
    /// First-user-message template selector.
    /// `Default` (the default) lets the shell layer build the legacy `<user_info>` and `<git_status>` prefix.
    /// `Custom` uses a caller-supplied template string.
    #[serde(default)]
    pub user_message_template: UserMessageTemplate,
    /// Where this definition was loaded from; `None` for built-in definitions
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// Discovery scope (project vs user).
    #[serde(skip)]
    pub scope: AgentScope,
}
/// Declares that the agent must call a specific tool before the turn ends.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionRequirement {
    /// Canonical tool name that must be called.
    pub tool: String,
    /// Reminder text injected when the tool hasn't been called.
    pub reminder: String,
    /// Suggested recovery policy for the harness.
    #[serde(default)]
    pub recovery: Option<RecoveryPolicy>,
}
/// Suggested turn-level recovery policy.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPolicy {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}
/// Per-tool execution config.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecConfig {
    /// Retry config for this tool. None means no retry (execute once).
    #[serde(default)]
    pub retry: Option<ToolRetryConfig>,
}
/// Retry configuration for a single tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRetryConfig {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}
/// How the Markdown body interacts with the base prompt template.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    /// Body is appended to the base template (tool conventions, formatting rules, user_info).
    #[default]
    Extend,
    /// Body IS the complete system prompt.
    Full,
}
fn default_prompt_mode() -> PromptMode {
    PromptMode::Extend
}
/// Where the agent definition was discovered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentScope {
    /// .grok/agents/ (project-level, highest priority)
    Project,
    /// ~/.grok/agents/ (user-level)
    User,
    /// ~/.grok/bundled/agents/ (lowest-priority bundled cache)
    Bundled,
    /// Built-in agent (e.g., default_distill(), browser_use()).
    #[default]
    BuiltIn,
}
impl AgentScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Bundled => "bundled",
            Self::BuiltIn => "built-in",
        }
    }
}
impl std::fmt::Display for AgentScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
/// Controls which parent MCP servers a subagent inherits.
/// Custom `Deserialize` because `serde_yaml` 0.9 uses YAML tags for externally-tagged enums.
/// Frontmatter uses the mapping style that JSON also expects.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum McpInheritance {
    #[default]
    All,
    None,
    Named(Vec<String>),
    Except(Vec<String>),
}
impl<'de> Deserialize<'de> for McpInheritance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;
        struct McpInheritanceVisitor;
        impl<'de> de::Visitor<'de> for McpInheritanceVisitor {
            type Value = McpInheritance;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""all", "none", {"named": [...]}, or {"except": [...]}"#)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
                match s.to_ascii_lowercase().as_str() {
                    "all" => Ok(McpInheritance::All),
                    "none" => Ok(McpInheritance::None),
                    other => Err(de::Error::unknown_variant(other, &["all", "none"])),
                }
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::invalid_length(0, &"a single-key map"))?;
                let value: Vec<String> = map.next_value()?;
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "mcpInheritance map must have exactly one key",
                    ));
                }
                match key.as_str() {
                    "named" => Ok(McpInheritance::Named(value)),
                    "except" => Ok(McpInheritance::Except(value)),
                    other => Err(de::Error::unknown_variant(other, &["named", "except"])),
                }
            }
        }
        deserializer.deserialize_any(McpInheritanceVisitor)
    }
}
/// Only `BypassPermissions` is wired at spawn; others are forward-compat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize, strum::EnumCount)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    /// Background classifier reviews tool calls.
    Auto,
    /// Silently deny non-pre-approved tools.
    DontAsk,
    BypassPermissions,
    Plan,
}
impl PermissionMode {
    pub const VALID_VALUES: &[&str] = &[
        "default",
        "acceptEdits",
        "auto",
        "dontAsk",
        "bypassPermissions",
        "plan",
    ];
}
const _: () =
    assert!(PermissionMode::VALID_VALUES.len() == <PermissionMode as strum::EnumCount>::COUNT);
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    serde::Serialize,
    IntoStaticStr,
    strum::EnumCount,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    #[strum(serialize = "xhigh")]
    XHigh,
    Max,
}
impl Effort {
    pub const VALID_VALUES: &[&str] = &["low", "medium", "high", "xhigh", "max"];
}
const _: () = assert!(Effort::VALID_VALUES.len() == <Effort as strum::EnumCount>::COUNT);
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    serde::Serialize,
    IntoStaticStr,
    strum::EnumCount,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum IsolationMode {
    None,
    Worktree,
}
impl IsolationMode {
    pub const VALID_VALUES: &[&str] = &["none", "worktree"];
}
const _: () =
    assert!(IsolationMode::VALID_VALUES.len() == <IsolationMode as strum::EnumCount>::COUNT);
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    serde::Serialize,
    AsRefStr,
    EnumString,
    IntoStaticStr,
    strum::EnumCount,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum AgentColor {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}
impl AgentColor {
    pub const VALID_VALUES: &[&str] = &[
        "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
    ];
}
const _: () = assert!(AgentColor::VALID_VALUES.len() == <AgentColor as strum::EnumCount>::COUNT);
/// Never fails: `color` is decorative, but a rejected value fails the whole frontmatter parse and discovery skips the agent.
/// A typo'd color would silently make the agent unspawnable.
/// Unrecognized values drop to `None` with a warning rather than a stand-in color the author never wrote.
fn deserialize_agent_color<'de, D>(deserializer: D) -> Result<Option<AgentColor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use std::str::FromStr;
    let Some(value) = Option::<serde_yaml::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let parsed = value
        .as_str()
        .and_then(|name| AgentColor::from_str(name.trim()).ok());
    if parsed.is_none() {
        tracing::warn!(
            color = ?value,
            valid = ?AgentColor::VALID_VALUES,
            "unrecognized agent color, ignoring"
        );
    }
    Ok(parsed)
}
/// Distinct from `storage::MemoryScope` (global-vs-workspace write target).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    serde::Serialize,
    IntoStaticStr,
    strum::EnumCount,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum MemoryScope {
    /// `~/.grok/agent-memory/<name>/`
    User,
    /// `<project>/.grok/agent-memory/<name>/`
    Project,
    /// `<project>/.grok/agent-memory-local/<name>/`
    Local,
}
impl MemoryScope {
    pub const VALID_VALUES: &[&str] = &["user", "project", "local"];
}
const _: () = assert!(MemoryScope::VALID_VALUES.len() == <MemoryScope as strum::EnumCount>::COUNT);
#[derive(Debug)]
pub struct ResolvedMemoryDir {
    pub path: std::path::PathBuf,
    /// No workspace hash needed (already project-scoped).
    pub is_project_scoped: bool,
}
impl MemoryScope {
    pub fn resolve_dir(self, agent_name: &str, project_cwd: &std::path::Path) -> ResolvedMemoryDir {
        match self {
            Self::User => ResolvedMemoryDir {
                path: distill_config::distill_home()
                    .join("agent-memory")
                    .join(agent_name),
                is_project_scoped: false,
            },
            Self::Project => ResolvedMemoryDir {
                path: project_cwd.join(".grok/agent-memory").join(agent_name),
                is_project_scoped: true,
            },
            Self::Local => ResolvedMemoryDir {
                path: project_cwd
                    .join(".grok/agent-memory-local")
                    .join(agent_name),
                is_project_scoped: true,
            },
        }
    }
}
/// Hooks config is validated as an object at parse time; semantic parsing is deferred to spawn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HooksConfig(pub serde_json::Map<String, serde_json::Value>);
impl HooksConfig {
    pub fn as_value(&self) -> serde_json::Value {
        serde_json::Value::Object(self.0.clone())
    }
}
fn deserialize_hooks_map<'de, D>(deserializer: D) -> Result<Option<HooksConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<serde_json::Value>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(serde_json::Value::Object(map)) => Ok(Some(HooksConfig(map))),
        Some(_) => Err(serde::de::Error::custom("hooks must be an object")),
    }
}
/// MCP server reference, typed to catch config errors at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerRef {
    Named(String),
    /// Opaque JSON config resolved to `McpServer` at spawn time.
    Inline {
        name: String,
        config: serde_json::Value,
    },
}
impl<'de> Deserialize<'de> for McpServerRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        match val {
            serde_json::Value::String(s) => Ok(McpServerRef::Named(s)),
            serde_json::Value::Object(obj) if obj.len() == 1 => {
                let (name, config) = obj.into_iter().next().unwrap();
                if !config.is_object() {
                    return Err(serde::de::Error::custom(format!(
                        "mcpServers inline config for '{name}' must be an object"
                    )));
                }
                Ok(McpServerRef::Inline { name, config })
            }
            serde_json::Value::Object(obj) => {
                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                    Ok(McpServerRef::Inline {
                        name: name.to_string(),
                        config: serde_json::Value::Object(obj),
                    })
                } else {
                    Err(serde::de::Error::custom(
                        "mcpServers entry must be a string, a {name: config} map, \
                         or an object with a 'name' field",
                    ))
                }
            }
            _ => Err(serde::de::Error::custom(
                "mcpServers entry must be a string or object",
            )),
        }
    }
}
impl serde::Serialize for McpServerRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            McpServerRef::Named(s) => serializer.serialize_str(s),
            McpServerRef::Inline { name, config } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(name, config)?;
                map.end()
            }
        }
    }
}
/// Bash tool config overrides (agent-definition layer). Uses `camelCase` for YAML frontmatter.
/// The `AgentBuilder` maps these into the tools crate's `BashToolConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashConfig {
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: f64,
    #[serde(default = "default_output_byte_limit")]
    pub output_byte_limit: usize,
    #[serde(default)]
    pub cmd_prefix: Option<String>,
}
impl Default for BashConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout_secs(),
            output_byte_limit: default_output_byte_limit(),
            cmd_prefix: None,
        }
    }
}
fn default_timeout_secs() -> f64 {
    120.0
}
fn default_output_byte_limit() -> usize {
    200_000
}
fn default_true() -> bool {
    true
}
/// Strip a tool id's `Namespace:` prefix, yielding its short name.
pub(crate) fn short_tool_name(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}
/// Whether an allow/deny `entry` refers to tool `id` (by full id or short name).
pub(crate) fn tool_id_eq(entry: &str, id: &str) -> bool {
    entry == id || entry == short_tool_name(id)
}
/// Whether any `list` entry refers to tool `id`.
pub(crate) fn tool_id_matches(list: &[String], id: &str) -> bool {
    list.iter().any(|e| tool_id_eq(e, id))
}
impl AgentDefinition {
    /// Parse an agent definition from a Markdown file with YAML frontmatter.
    /// Frontmatter between `---` delimiters; the system prompt body follows.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, AgentBuildError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(AgentBuildError::IoError)?;
        let mut def = Self::parse(&content)?;
        def.source_path = Some(path.to_path_buf());
        def.plugin_name = None;
        def.scope = Self::scope_from_path(path);
        Ok(def)
    }
    /// Parse only YAML frontmatter from an agent file, leaving prompt_body unset.
    pub fn from_file_frontmatter_only(path: impl AsRef<Path>) -> Result<Self, AgentBuildError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(AgentBuildError::IoError)?;
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return Err(AgentBuildError::ParseError(
                "missing frontmatter delimiters".to_string(),
            ));
        }
        let after_opening = trimmed.get(3..).ok_or_else(|| {
            AgentBuildError::ParseError("missing frontmatter delimiters".to_string())
        })?;
        let closing_idx = after_opening.find("\n---").ok_or_else(|| {
            AgentBuildError::ParseError("missing closing frontmatter delimiter".to_string())
        })?;
        let yaml_content = after_opening.get(..closing_idx).ok_or_else(|| {
            AgentBuildError::ParseError("missing closing frontmatter delimiter".to_string())
        })?;
        let mut def: AgentDefinition = serde_yaml::from_str(yaml_content)
            .map_err(|e| AgentBuildError::ParseError(e.to_string()))?;
        def.prompt_body = None;
        def.system_prompt = TemplateOverride::None;
        def.source_path = Some(path.to_path_buf());
        def.plugin_name = None;
        def.scope = Self::scope_from_path(path);
        Ok(def)
    }
    /// Parse from string content (for testing and inline definitions).
    pub fn parse(content: &str) -> Result<Self, AgentBuildError> {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return Err(AgentBuildError::ParseError(
                "missing frontmatter delimiters".to_string(),
            ));
        }
        let after_opening = trimmed.get(3..).ok_or_else(|| {
            AgentBuildError::ParseError("missing frontmatter delimiters".to_string())
        })?;
        let closing_idx = after_opening.find("\n---").ok_or_else(|| {
            AgentBuildError::ParseError("missing closing frontmatter delimiter".to_string())
        })?;
        let yaml_content = after_opening.get(..closing_idx).ok_or_else(|| {
            AgentBuildError::ParseError("missing closing frontmatter delimiter".to_string())
        })?;
        let after_closing = after_opening.get(closing_idx + 4..).ok_or_else(|| {
            AgentBuildError::ParseError("missing closing frontmatter delimiter".to_string())
        })?;
        let body_start = after_closing.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = after_closing.get(body_start..).unwrap_or("").trim();
        let prompt_body = if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        };
        let mut def: AgentDefinition = serde_yaml::from_str(yaml_content)
            .map_err(|e| AgentBuildError::ParseError(e.to_string()))?;
        def.prompt_body = prompt_body;
        def.plugin_name = None;
        Ok(def)
    }
    fn scope_from_path(path: &Path) -> AgentScope {
        let path_str = path.to_string_lossy();
        let grok = distill_config::user_distill_home();
        let home = distill_dirs::home_dir();
        for (dir, scope) in crate::discovery::user_agent_dirs(home.as_deref(), grok.as_deref()) {
            if path.starts_with(&dir) {
                return scope;
            }
        }
        if path_str.contains(".grok/agents/") || path_str.contains(".grok\\agents\\") {
            return AgentScope::Project;
        }
        if path_str.contains(".grok/bundled/agents/")
            || path_str.contains(".grok\\bundled\\agents\\")
        {
            return AgentScope::Bundled;
        }
        AgentScope::BuiltIn
    }
}
impl AgentDefinition {
    /// Whether `id` passes the session-operator clamp: denylist wins, then an unset allowlist allows all.
    pub(crate) fn session_tools_allowed(&self, id: &str) -> bool {
        if self
            .session_tools_denylist
            .as_deref()
            .is_some_and(|d| tool_id_matches(d, id))
        {
            return false;
        }
        self.session_tools_allowlist
            .as_deref()
            .is_none_or(|a| tool_id_matches(a, id))
    }
    /// Whether a hosted/server-side tool `id` survives the agent's own `disallowed_tools`/`tools` and the session clamp.
    /// Hosted tools aren't in `tool_config`, so they're gated by name here.
    /// The gate is strict: no compat-name mapping or unresolved-entry fallback like the function path.
    pub(crate) fn hosted_tool_allowed(&self, id: &str) -> bool {
        if tool_id_matches(&self.disallowed_tools, id) {
            return false;
        }
        if !self.tools.is_empty() && !tool_id_matches(&self.tools, id) {
            return false;
        }
        self.session_tools_allowed(id)
    }
    /// True iff the active system prompt template for `audience` carries the `<task_completion_discipline>` block.
    /// Used by the TodoGate so it only fires when the prompt actually references those rules.
    /// The block was removed from every built-in template, so this returns `false`; kept so the call-site stays stable.
    pub fn carries_task_completion_discipline(
        &self,
        _audience: crate::prompt::context::PromptAudience,
    ) -> bool {
        false
    }
    pub fn include_browser_verification(&self) -> bool {
        matches!(
            self.builtin_name,
            Some(BuiltinAgentName::DistillPlan | BuiltinAgentName::DistillPlanNoSubagents)
        )
    }
    /// True iff this agent's wire format is non-interchangeable with the stock harness.
    /// A client-supplied `_meta.agentProfile` must not override it.
    /// Strict iff bespoke system prompt, bespoke user-message template, or curated toolset (`!inject_default_tools`).
    pub fn is_strict_harness(&self) -> bool {
        use crate::prompt::context::TemplateOverride;
        use crate::prompt::user_message::UserMessageTemplate;
        let prompt_is_custom = !matches!(self.system_prompt, TemplateOverride::None);
        let user_template_is_custom =
            !matches!(self.user_message_template, UserMessageTemplate::Default);
        let toolset_is_curated = !self.inject_default_tools;
        prompt_is_custom || user_template_is_custom || toolset_is_curated
    }
    /// Swap the definition's file tools for the equivalents in `file_tools` (hashline vs standard), slot by slot.
    /// Never grants a slot the definition doesn't already have (read-only toolsets stay read-only).
    pub fn override_file_tools(
        &mut self,
        file_tools: Vec<distill_tools::registry::types::ToolConfig>,
    ) {
        const FILE_TOOL_SLOTS: &[[&str; 2]] = &[
            ["Distill:read_file", "DistillHashline:hashline_read"],
            ["Distill:search_replace", "DistillHashline:hashline_edit"],
            ["Distill:grep", "DistillHashline:hashline_grep"],
        ];
        for tool in self.tool_config.tools.iter_mut() {
            let Some(slot) = FILE_TOOL_SLOTS
                .iter()
                .find(|slot| slot.contains(&tool.id.as_str()))
            else {
                continue;
            };
            if let Some(replacement) = file_tools.iter().find(|ft| slot.contains(&ft.id.as_str())) {
                *tool = replacement.clone();
            }
        }
    }
    /// Shared defaults for built-in constructors.
    fn base(name: BuiltinAgentName, description: &str) -> Self {
        Self {
            builtin_name: Some(name),
            ..Self::builtin_defaults(name.as_ref(), description)
        }
    }
    /// Shared defaults for out-of-tree built-in agent registrations.
    pub fn builtin_defaults(name: &str, description: &str) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_string(),
            plugin_name: None,
            builtin_name: None,
            prompt_mode: PromptMode::Extend,
            tool_config: default_agent_toolset(),
            capability_mode: None,
            permission_mode: PermissionMode::Default,
            skills: vec![],
            agents_md: true,
            discover_skills: true,
            inherit_skills: true,
            inject_default_tools: true,
            disallowed_tools: vec![],
            tools: vec![],
            effort: None,
            max_turns: None,
            isolation: None,
            background: None,
            color: None,
            initial_prompt: None,
            mcp_servers: vec![],
            mcp_inheritance: McpInheritance::All,
            hooks: None,
            memory: None,
            allowed_subagent_types: None,
            session_tools_allowlist: None,
            session_tools_denylist: None,
            model: ModelOverride::Inherit,
            completion_requirement: None,
            tool_overrides: None,
            prompt_body: None,
            system_prompt: TemplateOverride::None,
            source_path: None,
            user_message_template: UserMessageTemplate::Default,
            scope: AgentScope::BuiltIn,
        }
    }
    pub fn default_distill() -> Self {
        Self {
            tool_config: default_distill_toolset(),
            ..Self::base(
                BuiltinAgentName::Distill,
                "Distill agent for software engineering tasks.",
            )
        }
    }
    /// Concise output format for SFT/RL.
    pub fn distill_concise() -> Self {
        Self {
            tool_config: distill_concise_toolset(),
            agents_md: false,
            ..Self::base(
                BuiltinAgentName::DistillConcise,
                "Distill agent with concise output format.",
            )
        }
    }
    pub fn distill_plan() -> Self {
        Self {
            tool_config: distill_plan_toolset(),
            ..Self::base(
                BuiltinAgentName::DistillPlan,
                "Distill agent with plan mode support.",
            )
        }
    }
    pub fn distill_plan_no_subagents() -> Self {
        Self {
            tool_config: distill_plan_no_subagents_toolset(),
            ..Self::base(
                BuiltinAgentName::DistillPlanNoSubagents,
                "Distill agent with plan mode (no subagents).",
            )
        }
    }
    pub fn distill_ask_user() -> Self {
        Self {
            tool_config: distill_ask_user_toolset(),
            ..Self::base(
                BuiltinAgentName::DistillAskUser,
                "Distill agent with ask-user-question tool.",
            )
        }
    }
    pub fn codex() -> Self {
        Self {
            tool_config: codex_toolset(),
            system_prompt: TemplateOverride::Codex,
            ..Self::base(BuiltinAgentName::Codex, "Codex toolset and prompt")
        }
    }
    pub fn opencode() -> Self {
        Self {
            tool_config: opencode_toolset(),
            ..Self::base(
                BuiltinAgentName::Opencode,
                "OpenCode toolset — opencode-style tools and parameter conventions",
            )
        }
    }
    pub fn general_purpose() -> Self {
        use crate::prompt::subagent_prompts;
        Self {
            description: distill_tool_types::GENERAL_PURPOSE_SUBAGENT
                .description
                .to_string(),
            tool_config: general_purpose_toolset(),
            prompt_body: Some(subagent_prompts::GENERAL_PURPOSE_PROMPT.to_string()),
            ..Self::base(BuiltinAgentName::GeneralPurpose, "")
        }
    }
    pub fn explore() -> Self {
        use crate::prompt::subagent_prompts;
        Self {
            description: distill_tool_types::EXPLORE_SUBAGENT.description.to_string(),
            tool_config: explore_toolset(),
            permission_mode: PermissionMode::Plan,
            prompt_body: Some(subagent_prompts::EXPLORE_PROMPT.to_string()),
            inherit_skills: false,
            ..Self::base(BuiltinAgentName::Explore, "")
        }
    }
    pub fn plan() -> Self {
        use crate::prompt::subagent_prompts;
        Self {
            description: distill_tool_types::PLAN_SUBAGENT.description.to_string(),
            tool_config: plan_toolset(),
            permission_mode: PermissionMode::Plan,
            prompt_body: Some(subagent_prompts::PLAN_PROMPT.to_string()),
            inherit_skills: false,
            ..Self::base(BuiltinAgentName::Plan, "")
        }
    }
    pub fn browser_use() -> Self {
        Self {
            prompt_mode: PromptMode::Full,
            agents_md: false,
            prompt_body: Some(
                "You are a web browsing agent. You can navigate, interact with, and \
                 extract information from web pages. Use the available browsing tools \
                 to complete the user's request."
                    .to_string(),
            ),
            ..Self::base(
                BuiltinAgentName::BrowserUse,
                "Web browsing and interaction agent.",
            )
        }
    }
    /// GBL model with full Distill tools that delegates coding/exploration to subagents.
    /// Subagent overrides are applied in `handle_subagent_request`.
    pub fn distill_orchestrator() -> Self {
        Self {
            tool_config: orchestrator_toolset(),
            inject_default_tools: false,
            prompt_body: Some(ORCHESTRATOR_PROMPT_BODY.to_string()),
            ..Self::base(
                BuiltinAgentName::DistillOrchestrator,
                "Distill orchestrator that delegates coding to specialized subagents",
            )
        }
    }
    /// Deserialize an agent definition from a JSON value (e.g. ACP `_meta.agentProfile`).
    /// Unlike `parse()`, this accepts a flat JSON object; `promptBody` is an explicit string field.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, AgentBuildError> {
        let mut def: AgentDefinition = serde_json::from_value(value.clone())
            .map_err(|e| AgentBuildError::ParseError(e.to_string()))?;
        if let Some(body) = value.get("promptBody").and_then(|v| v.as_str()) {
            let trimmed = body.trim();
            if !trimmed.is_empty() {
                def.prompt_body = Some(trimmed.to_string());
            }
        }
        if !value.get("toolConfig").is_some_and(|v| v.is_object()) {
            def.tool_config = default_agent_toolset();
        }
        def.scope = AgentScope::BuiltIn;
        Ok(def)
    }
    /// Serialize to a JSON value suitable for `from_json` roundtrip.
    /// Handles `prompt_body` which is `#[serde(skip)]` on the struct.
    pub fn to_json_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("AgentDefinition is always serializable");
        if let Some(body) = &self.prompt_body
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert(
                "promptBody".to_owned(),
                serde_json::Value::String(body.clone()),
            );
        }
        value
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Pins the `spawn_subagent` rename to the shared predicate.
    #[test]
    fn task_tool_rename_matches_task_tool_id_predicate() {
        let name = task_tool_config()
            .name_override
            .expect("task tool is renamed");
        assert!(distill_tools::is_task_tool_id(&name));
    }
    /// Pins the `run_terminal_command` rename to the writing-phase taxonomy so a future rename can't silently degrade the spinner label.
    #[test]
    fn bash_tool_rename_matches_writing_tool_kind() {
        let name = bash_tool_config()
            .name_override
            .expect("bash tool is renamed");
        assert_eq!(
            distill_tools::tool_taxonomy::writing_tool_kind(&name),
            Some(distill_tools::types::tool::ToolKind::Execute)
        );
    }
    /// Native presets only.
    #[test]
    fn toolset_for_preset_resolves_known_names() {
        for name in [
            "distill",
            "distill",
            "distill-concise",
            "distill-plan",
            "codex",
            "explore",
            "plan",
            "grok-computer",
            "grok_computer",
        ] {
            assert!(
                toolset_for_preset(name).is_some(),
                "preset `{name}` should resolve"
            );
        }
        assert!(toolset_for_preset("does-not-exist").is_none());
    }
    #[test]
    fn presets_select_distinct_toolsets_by_size() {
        let gb = toolset_for_preset("distill").unwrap();
        let plan = toolset_for_preset("plan").unwrap();
        let explore = toolset_for_preset("explore").unwrap();
        assert!(explore.tools.len() < plan.tools.len());
        assert!(plan.tools.len() < gb.tools.len());
    }
    fn feedback_tool_id() -> String {
        ToolConfig::from(&distill::SendFeedbackTool).id
    }
    fn contains_feedback(config: &ToolServerConfig) -> bool {
        let id = feedback_tool_id();
        config.tools.iter().any(|tool| tool.id == id)
    }
    fn grok_computer_exclusive_ids() -> Vec<String> {
        #[allow(unused_mut)]
        let mut ids: Vec<String> = vec![
            ToolConfig::from(&distill::GetTerminalCommandOutputTool).id,
            ToolConfig::from(&distill::KillTerminalCommandTool).id,
        ];
        ids
    }
    #[test]
    fn send_feedback_exposure_is_distill_only() {
        use strum::IntoEnumIterator;
        let presets = all_toolset_presets();
        for (name, config) in &presets {
            let expected = name == "distill";
            let count = config
                .tools
                .iter()
                .filter(|tool| tool.id == feedback_tool_id())
                .count();
            assert_eq!(
                count,
                usize::from(expected),
                "preset `{name}` has the wrong send_feedback exposure"
            );
        }
        for builtin in BuiltinAgentName::iter() {
            let expected = match builtin {
                BuiltinAgentName::Distill => true,
                BuiltinAgentName::DistillConcise
                | BuiltinAgentName::DistillPlan
                | BuiltinAgentName::DistillPlanNoSubagents
                | BuiltinAgentName::DistillAskUser
                | BuiltinAgentName::Codex
                | BuiltinAgentName::Opencode
                | BuiltinAgentName::GeneralPurpose
                | BuiltinAgentName::Explore
                | BuiltinAgentName::Plan
                | BuiltinAgentName::BrowserUse
                | BuiltinAgentName::DistillOrchestrator => false,
            };
            assert_eq!(
                contains_feedback(&builtin.definition().tool_config),
                expected,
                "builtin `{builtin}` has the wrong send_feedback exposure"
            );
        }
        for (name, config) in [
            ("core", distill_core_toolset(true)),
            ("general-purpose", general_purpose_toolset()),
            ("hashline", distill_hashline_toolset(vec![])),
        ] {
            assert!(
                !contains_feedback(&config),
                "toolset `{name}` leaked send_feedback"
            );
        }
        let workspace = workspace_distill_toolset();
        assert_eq!(
            workspace
                .tools
                .iter()
                .filter(|tool| tool.id == feedback_tool_id())
                .count(),
            1
        );
    }
    #[test]
    fn defaulted_agent_definitions_omit_send_feedback() {
        let builtins = AgentDefinition::builtin_defaults("external", "External definition");
        assert!(!contains_feedback(&builtins.tool_config));
        assert!(!contains_feedback(
            &AgentDefinition::browser_use().tool_config
        ));
        let serde_default: AgentDefinition = serde_json::from_value(serde_json::json!({
            "name": "external",
            "description": "External definition"
        }))
        .unwrap();
        assert!(!contains_feedback(&serde_default.tool_config));
        let acp = AgentDefinition::from_json(&serde_json::json!({
            "name": "external",
            "description": "External definition"
        }))
        .unwrap();
        assert!(!contains_feedback(&acp.tool_config));
    }
    #[test]
    fn grok_computer_preset_is_curated_distill_subset() {
        let gc = toolset_for_preset("grok-computer").unwrap();
        let gb = toolset_for_preset("distill").unwrap();
        let gb_ids: std::collections::HashSet<&str> =
            gb.tools.iter().map(|t| t.id.as_str()).collect();
        let exclusive_ids = grok_computer_exclusive_ids();
        assert!(!gc.tools.is_empty());
        for t in &gc.tools {
            if exclusive_ids.contains(&t.id) {
                continue;
            }
            assert!(
                gb_ids.contains(t.id.as_str()),
                "grok-computer tool `{}` must also ship in the distill preset",
                t.id
            );
        }
        assert!(
            gc.tools.len() < gb.tools.len(),
            "grok-computer should be a curated subset of distill"
        );
    }
    #[test]
    fn grok_computer_uses_subagent_free_background_task_tools() {
        let gc = toolset_for_preset("grok-computer").unwrap();
        let ids: std::collections::HashSet<&str> = gc.tools.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.contains(
                ToolConfig::from(&distill::GetTerminalCommandOutputTool)
                    .id
                    .as_str()
            )
        );
        assert!(
            ids.contains(
                ToolConfig::from(&distill::KillTerminalCommandTool)
                    .id
                    .as_str()
            )
        );
        assert!(!ids.contains(ToolConfig::from(&distill::TaskOutputTool).id.as_str()));
        assert!(!ids.contains(ToolConfig::from(&distill::KillTaskTool).id.as_str()));
        assert!(!ids.contains(ToolConfig::from(&distill::TaskTool).id.as_str()));
        for t in &gc.tools {
            if t.id == ToolConfig::from(&distill::GetTerminalCommandOutputTool).id
                || t.id == ToolConfig::from(&distill::KillTerminalCommandTool).id
            {
                assert!(t.name_override.is_none(), "tool `{}` must not rename", t.id);
            }
        }
    }
    /// The grok-computer preset must ship a full-file write tool (legacy `write_file` parity), the same OpenCode `write` tool distill uses.
    /// Guards against `search_replace` being the only file-mutation path.
    /// With the empty-old_string overwrite guard enabled, that path has no single-tool full rewrite.
    #[test]
    fn grok_computer_preset_includes_write_tool() {
        let gc = toolset_for_preset("grok-computer").unwrap();
        let write_id = ToolConfig::from(&opencode::OpenCodeWriteTool).id;
        assert!(
            gc.tools.iter().any(|t| t.id == write_id),
            "grok-computer preset must include the `{write_id}` tool"
        );
    }
    #[test]
    fn grok_computer_preset_excludes_plan_and_lsp() {
        let gc = toolset_for_preset("grok-computer").unwrap();
        let gc_ids: std::collections::HashSet<&str> =
            gc.tools.iter().map(|t| t.id.as_str()).collect();
        for excluded in [
            ToolConfig::from(&distill::LspTool).id,
            ToolConfig::from(&distill::EnterPlanModeTool).id,
            ToolConfig::from(&distill::ExitPlanModeTool).id,
        ] {
            assert!(
                !gc_ids.contains(excluded.as_str()),
                "grok-computer preset must not advertise `{excluded}`"
            );
        }
        let full = workspace_distill_toolset();
        let full_ids: std::collections::HashSet<&str> =
            full.tools.iter().map(|t| t.id.as_str()).collect();
        for present in [
            ToolConfig::from(&distill::LspTool).id,
            ToolConfig::from(&distill::EnterPlanModeTool).id,
            ToolConfig::from(&distill::ExitPlanModeTool).id,
        ] {
            assert!(
                full_ids.contains(present.as_str()),
                "workspace_distill_toolset must ship `{present}`"
            );
        }
    }
    /// Exhaustive match, so adding a new `BuiltinAgentName` won't compile until classified.
    fn expected_strict_harness(name: BuiltinAgentName) -> bool {
        match name {
            BuiltinAgentName::Codex | BuiltinAgentName::DistillOrchestrator => true,
            BuiltinAgentName::Distill
            | BuiltinAgentName::DistillConcise
            | BuiltinAgentName::DistillPlan
            | BuiltinAgentName::DistillPlanNoSubagents
            | BuiltinAgentName::DistillAskUser
            | BuiltinAgentName::GeneralPurpose
            | BuiltinAgentName::Explore
            | BuiltinAgentName::Plan
            | BuiltinAgentName::Opencode
            | BuiltinAgentName::BrowserUse => false,
        }
    }
    /// Invariant: structural `is_strict_harness()` must match the hand-classified expectation for every built-in variant.
    #[test]
    fn is_strict_harness_matches_structural_classification_for_all_builtins() {
        use strum::IntoEnumIterator;
        for variant in BuiltinAgentName::iter() {
            let structural = variant.definition().is_strict_harness();
            let expected = expected_strict_harness(variant);
            assert_eq!(
                structural, expected,
                "BuiltinAgentName::{variant:?}: structural={structural} but \
                 expected={expected}. Update `expected_strict_harness` if the \
                 change is intentional.",
            );
        }
    }
    #[test]
    fn is_strict_harness_agent_type_classifies_by_name() {
        for strict in ["codex", "distill-orchestrator"] {
            assert!(
                is_strict_harness_agent_type(strict),
                "{strict} should be strict"
            );
        }
        for non_strict in [
            "distill",
            "distill-plan",
            "distill-concise",
            "distill-ask-user",
            "opencode",
            "browser-use",
            "custom-user-agent",
            "",
            "distill-totally-made-up",
        ] {
            assert!(
                !is_strict_harness_agent_type(non_strict),
                "{non_strict} should be non-strict",
            );
        }
    }
    #[test]
    fn test_parse_valid_full_definition() {
        let content = r#"---
name: test-agent
description: A test agent
promptMode: full
tools:
  - read_file
  - grep
permissionMode: plan
agentsMd: false
---

You are a test agent.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.name, "test-agent");
        assert_eq!(def.description, "A test agent");
        assert_eq!(def.prompt_mode, PromptMode::Full);
        assert_eq!(def.permission_mode, PermissionMode::Plan);
        assert!(!def.agents_md);
        assert_eq!(def.prompt_body.as_deref(), Some("You are a test agent."));
        assert_eq!(
            def.tools,
            vec!["read_file".to_string(), "grep".to_string()],
            "tools allowlist must be parsed from YAML frontmatter"
        );
    }
    #[test]
    fn test_parse_tools_and_disallowed_together() {
        let content = r#"---
name: mixed-tools
description: Both tools and disallowedTools
tools:
  - read_file
  - grep
  - search_replace
  - task
disallowedTools:
  - search_replace
---

Mixed agent.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.tools.len(), 4, "tools allowlist should have 4 entries");
        assert_eq!(
            def.disallowed_tools,
            vec!["search_replace".to_string()],
            "disallowedTools should have 1 entry"
        );
    }
    #[test]
    fn test_parse_tools_with_agent_parens() {
        let content = r#"---
name: coordinator
description: test
tools: Agent(worker, researcher), Read, Bash
---

Agent.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.tools,
            vec!["Agent(worker, researcher)", "Read", "Bash"],
            "Agent(a, b) must be kept as a single token"
        );
    }
    #[test]
    fn test_parse_tools_comma_separated() {
        let content = r#"---
name: comma-tools
description: Comma-separated tools
tools: read_file, grep, list_dir
disallowedTools: search_replace, write
---

Agent.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.tools,
            vec!["read_file", "grep", "list_dir"],
            "comma-separated tools must parse correctly"
        );
        assert_eq!(
            def.disallowed_tools,
            vec!["search_replace", "write"],
            "comma-separated disallowedTools must parse correctly"
        );
    }
    #[test]
    fn test_parse_tools_case_insensitive_agent() {
        let content = r#"---
name: ci-test
description: test
tools: agent(worker, researcher), Read
---

Agent.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.tools,
            vec!["agent(worker, researcher)", "Read"],
            "lowercase agent(a, b) must be kept as a single token"
        );
    }
    #[test]
    fn test_parse_max_turns_zero_rejected() {
        let content = "---\nname: test\ndescription: Test\nmaxTurns: 0\n---\n";
        let result = AgentDefinition::parse(content);
        assert!(
            result.is_err(),
            "maxTurns: 0 should be rejected at parse time"
        );
    }
    #[test]
    fn test_parse_model_empty_is_inherit() {
        let content = "---\nname: test\ndescription: Test\nmodel: \"\"\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.model, ModelOverride::Inherit);
    }
    #[test]
    fn test_parse_model_null_is_inherit() {
        let content = "---\nname: test\ndescription: Test\nmodel: ~\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.model, ModelOverride::Inherit);
    }
    #[test]
    fn test_parse_minimal_defaults_none_fields() {
        let content = "---\nname: minimal\ndescription: Test\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert!(def.effort.is_none());
        assert!(def.max_turns.is_none());
        assert!(def.isolation.is_none());
        assert!(def.background.is_none());
        assert!(def.color.is_none());
        assert!(def.initial_prompt.is_none());
        assert!(def.memory.is_none());
        assert!(def.hooks.is_none());
    }
    #[test]
    fn test_model_override_display_shows_id() {
        assert_eq!(ModelOverride::Inherit.to_string(), "inherit");
        assert_eq!(
            ModelOverride::Override("grok-3-fast".to_string()).to_string(),
            "grok-3-fast"
        );
    }
    #[test]
    fn test_parse_new_fields() {
        let content = r#"---
name: full-fields
description: All new fields
effort: high
maxTurns: 10
isolation: worktree
background: true
color: blue
initialPrompt: "hello world"
model: grok-3
---

Agent body.
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.effort, Some(Effort::High));
        assert_eq!(def.max_turns, Some(10));
        assert_eq!(def.isolation, Some(IsolationMode::Worktree));
        assert_eq!(def.background, Some(true));
        assert_eq!(def.color, Some(AgentColor::Blue));
        assert_eq!(def.initial_prompt.as_deref(), Some("hello world"));
        assert_eq!(def.model, ModelOverride::Override("grok-3".to_string()));
    }
    #[test]
    fn test_parse_minimal_definition() {
        let content = r#"---
name: minimal
description: Minimal agent
---
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.name, "minimal");
        assert_eq!(def.description, "Minimal agent");
        assert_eq!(def.prompt_mode, PromptMode::Extend);
        assert!(def.agents_md);
        assert!(def.prompt_body.is_none());
    }
    #[test]
    fn mcp_server_ref_parse_and_reject() {
        let v: McpServerRef = serde_json::from_value(serde_json::json!("slack")).unwrap();
        assert_eq!(v, McpServerRef::Named("slack".to_string()));
        let v: McpServerRef =
            serde_json::from_value(serde_json::json!({"s": {"type": "stdio"}})).unwrap();
        assert!(matches!(v, McpServerRef::Inline { ref name, .. } if name == "s"));
        let v: McpServerRef =
            serde_json::from_value(serde_json::json!({"name": "s", "type": "stdio"})).unwrap();
        assert!(matches!(v, McpServerRef::Inline { ref name, .. } if name == "s"));
        assert!(
            serde_json::from_value::<McpServerRef>(serde_json::json!({"type": "stdio"})).is_err()
        );
        assert!(serde_json::from_value::<McpServerRef>(serde_json::json!(42)).is_err());
        assert!(serde_json::from_value::<McpServerRef>(serde_json::json!({"s": "bad"})).is_err());
    }
    #[test]
    fn memory_scope_resolve_dir() {
        let cwd = std::path::Path::new("/project");
        let user = MemoryScope::User.resolve_dir("a", cwd);
        assert!(user.path.ends_with("agent-memory/a"));
        assert!(!user.is_project_scoped);
        let proj = MemoryScope::Project.resolve_dir("a", cwd);
        assert_eq!(
            proj.path,
            std::path::PathBuf::from("/project/.grok/agent-memory/a")
        );
        assert!(proj.is_project_scoped);
        let local = MemoryScope::Local.resolve_dir("a", cwd);
        assert_eq!(
            local.path,
            std::path::PathBuf::from("/project/.grok/agent-memory-local/a")
        );
        assert!(local.is_project_scoped);
    }
    #[test]
    fn all_new_enum_variants_parse() {
        for effort in Effort::VALID_VALUES {
            let c = format!("---\nname: t\ndescription: t\neffort: {effort}\n---\n");
            assert!(
                AgentDefinition::parse(&c).unwrap().effort.is_some(),
                "effort: {effort}"
            );
        }
        for iso in IsolationMode::VALID_VALUES {
            let c = format!("---\nname: t\ndescription: t\nisolation: {iso}\n---\n");
            assert!(
                AgentDefinition::parse(&c).unwrap().isolation.is_some(),
                "isolation: {iso}"
            );
        }
        for color in AgentColor::VALID_VALUES {
            let c = format!("---\nname: t\ndescription: t\ncolor: {color}\n---\n");
            let parsed = AgentDefinition::parse(&c).unwrap().color;
            assert_eq!(
                parsed.map(<&'static str>::from),
                Some(*color),
                "color: {color}"
            );
        }
        for memory in MemoryScope::VALID_VALUES {
            let c = format!("---\nname: t\ndescription: t\nmemory: {memory}\n---\n");
            assert!(
                AgentDefinition::parse(&c).unwrap().memory.is_some(),
                "memory: {memory}"
            );
        }
    }
    #[test]
    fn unparseable_color_is_dropped_instead_of_dropping_the_agent() {
        for (declared, expected) in [
            ("Purple", Some(AgentColor::Purple)),
            ("  CYAN  ", Some(AgentColor::Cyan)),
            ("teal", None),
            ("\"#ff0000\"", None),
            ("chartreuse", None),
            ("42", None),
            ("[red, blue]", None),
            ("!custom x", None),
            ("{1: 2}", None),
        ] {
            let c = format!("---\nname: t\ndescription: t\ncolor: {declared}\n---\n");
            let def = AgentDefinition::parse(&c)
                .unwrap_or_else(|e| panic!("color {declared} must not fail the parse: {e}"));
            assert_eq!(def.color, expected, "color: {declared}");
            assert_eq!(def.name, "t");
        }
    }
    #[test]
    fn absent_or_null_color_stays_none() {
        let def = AgentDefinition::parse("---\nname: t\ndescription: t\n---\n").unwrap();
        assert!(def.color.is_none());
        let def = AgentDefinition::parse("---\nname: t\ndescription: t\ncolor:\n---\n").unwrap();
        assert!(def.color.is_none());
    }
    #[test]
    fn test_parse_missing_name() {
        let content = r#"---
description: No name
---
"#;
        let result = AgentDefinition::parse(content);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentBuildError::ParseError(msg) => {
                assert!(msg.contains("name"), "Error should mention 'name': {}", msg);
            }
            e => panic!("Expected ParseError, got: {:?}", e),
        }
    }
    #[test]
    fn test_parse_missing_delimiters() {
        let content = "Just some text without frontmatter";
        let result = AgentDefinition::parse(content);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentBuildError::ParseError(msg) => {
                assert!(msg.contains("delimiter") || msg.contains("frontmatter"));
            }
            e => panic!("Expected ParseError, got: {:?}", e),
        }
    }
    #[test]
    fn test_parse_unknown_fields_ignored() {
        let content = r#"---
name: test
description: Test
hooks:
  PreToolUse:
    - matcher: Bash
      hooks:
        - type: command
          command: echo hi
memory: user
unknownField: value
---
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.name, "test");
    }
    #[test]
    fn test_parse_completion_requirement() {
        let content = r#"---
name: completion-test
description: Test completion requirement parsing
completionRequirement:
  tool: my_agent__complete_task
  reminder: You must call complete_task
  recovery:
    maxRetries: 5
    baseDelayMs: 5000
    maxDelayMs: 60000
---
"#;
        let def = AgentDefinition::parse(content).unwrap();
        let req = def.completion_requirement.unwrap();
        assert_eq!(req.tool, "my_agent__complete_task");
        assert_eq!(req.reminder, "You must call complete_task");
        let recovery = req.recovery.unwrap();
        assert_eq!(recovery.max_retries, 5);
        assert_eq!(recovery.base_delay_ms, 5000);
        assert_eq!(recovery.max_delay_ms, 60000);
    }
    #[test]
    fn test_builtin_browser_use() {
        let def = AgentDefinition::browser_use();
        assert_eq!(def.name, "browser-use");
        assert_eq!(def.prompt_mode, PromptMode::Full);
        assert!(!def.agents_md);
    }
    #[test]
    fn test_default_tool_config_has_distill_tools() {
        let content = r#"---
name: default-tools
description: Test default tool config
---
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert!(
            !def.tool_config.tools.is_empty(),
            "default tool_config should have distill tools"
        );
    }
    #[test]
    fn test_permission_mode_round_trips() {
        for v in PermissionMode::VALID_VALUES {
            let content = format!("---\nname: test\ndescription: Test\npermissionMode: {v}\n---\n");
            AgentDefinition::parse(&content)
                .unwrap_or_else(|e| panic!("PermissionMode '{v}' failed parse: {e}"));
        }
    }
    #[test]
    fn test_prompt_mode_round_trips() {
        for (yaml_val, expected) in [("extend", PromptMode::Extend), ("full", PromptMode::Full)] {
            let content =
                format!("---\nname: test\ndescription: Test\npromptMode: {yaml_val}\n---\n");
            let def = AgentDefinition::parse(&content).unwrap();
            assert_eq!(def.prompt_mode, expected, "Failed for: {yaml_val}");
        }
    }
    #[test]
    fn test_from_file_sets_scope_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test-agent.md");
        std::fs::write(
            &file_path,
            "---\nname: file-test\ndescription: From file\n---\n",
        )
        .unwrap();
        let def = AgentDefinition::from_file(&file_path).unwrap();
        assert_eq!(def.name, "file-test");
        assert_eq!(def.source_path, Some(file_path));
    }
    #[test]
    fn test_scope_from_path_detects_bundled_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp
            .path()
            .join("nested")
            .join(".grok")
            .join("bundled")
            .join("agents")
            .join("bundled-agent.md");
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(
            &bundled,
            "---\nname: bundled-agent\ndescription: Bundled agent\n---\n",
        )
        .unwrap();
        let def = AgentDefinition::from_file(&bundled).unwrap();
        assert_eq!(def.scope, AgentScope::Bundled);
        assert_eq!(AgentScope::Bundled.label(), "bundled");
        assert_eq!(AgentScope::BuiltIn.label(), "built-in");
    }
    #[test]
    fn test_from_json_minimal() {
        let json = serde_json::json!({
            "name": "acp-agent",
            "description": "An agent from ACP"
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(def.name, "acp-agent");
        assert_eq!(def.description, "An agent from ACP");
        assert_eq!(def.prompt_mode, PromptMode::Extend);
        assert!(def.agents_md);
        assert!(def.prompt_body.is_none());
        assert_eq!(def.scope, AgentScope::BuiltIn);
    }
    #[test]
    fn same_name_acp_definition_does_not_enable_browser_verification() {
        let def = AgentDefinition::from_json(&serde_json::json!({
            "name": "distill-plan",
            "description": "Custom plan agent"
        }))
        .unwrap();
        assert!(!def.include_browser_verification());
        assert!(AgentDefinition::distill_plan().include_browser_verification());
        assert!(AgentDefinition::distill_plan_no_subagents().include_browser_verification());
    }
    #[test]
    fn test_from_json_has_default_toolset_with_task_tool() {
        let json = serde_json::json!({
            "name": "distill",
            "description": "Multi-surface coding agent.",
            "promptMode": "extend",
            "permissionMode": "dontAsk",
            "agentsMd": true,
            "promptBody": "You are a coding assistant."
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        let task_tool_id = "Distill:task";
        assert!(
            def.tool_config.tools.iter().any(|tc| tc.id == task_tool_id),
            "from_json() without toolConfig should include TaskTool in default toolset, \
             got tool IDs: {:?}",
            def.tool_config
                .tools
                .iter()
                .map(|tc| &tc.id)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn test_from_json_with_prompt_body() {
        let json = serde_json::json!({
            "name": "custom-agent",
            "description": "Agent with prompt body",
            "promptBody": "You are a specialized coding assistant.\n\nFocus on Rust."
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(def.name, "custom-agent");
        assert_eq!(
            def.prompt_body.as_deref(),
            Some("You are a specialized coding assistant.\n\nFocus on Rust.")
        );
    }
    #[test]
    fn test_from_json_with_permission_mode() {
        let json = serde_json::json!({
            "name": "auto-accept-agent",
            "description": "Agent with dontAsk permission mode",
            "permissionMode": "dontAsk",
            "promptBody": "## Auto-accept Mode"
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(def.permission_mode, PermissionMode::DontAsk);
        assert_eq!(def.prompt_body.as_deref(), Some("## Auto-accept Mode"));
    }
    #[test]
    fn test_from_json_empty_prompt_body_is_none() {
        let json = serde_json::json!({
            "name": "test",
            "description": "Test",
            "promptBody": "   "
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert!(
            def.prompt_body.is_none(),
            "Whitespace-only promptBody should be None"
        );
    }
    #[test]
    fn test_from_json_missing_required_fields() {
        let json = serde_json::json!({
            "description": "Missing name"
        });
        let result = AgentDefinition::from_json(&json);
        assert!(result.is_err());
    }
    #[test]
    fn test_from_json_ignores_unknown_fields() {
        let json = serde_json::json!({
            "name": "test",
            "description": "Test",
            "unknownField": "value",
            "futureFeature": true
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(def.name, "test");
    }
    #[test]
    fn to_json_value_roundtrips_through_from_json() {
        let mut original = AgentDefinition::parse(
                "---\nname: test-agent\ndescription: A test\npermissionMode: dontAsk\n---\nYou are a helper.",
            )
            .unwrap();
        original.tools = vec!["read_file".to_string(), "grep".to_string()];
        original.disallowed_tools = vec!["web_search".to_string()];
        let json = original.to_json_value();
        let recovered = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(recovered.name, "test-agent");
        assert_eq!(recovered.description, "A test");
        assert_eq!(recovered.prompt_body.as_deref(), Some("You are a helper."));
        assert_eq!(recovered.permission_mode, PermissionMode::DontAsk);
        assert_eq!(recovered.tools, vec!["read_file", "grep"]);
        assert_eq!(recovered.disallowed_tools, vec!["web_search"]);
    }
    #[test]
    fn test_model_override_serde_inherit() {
        let yaml = "\"inherit\"";
        let m: ModelOverride = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m, ModelOverride::Inherit);
    }
    #[test]
    fn test_model_override_serde_inherit_case_insensitive() {
        let yaml = "\"Inherit\"";
        let m: ModelOverride = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m, ModelOverride::Inherit);
    }
    #[test]
    fn test_model_override_serde_explicit_model_id() {
        let yaml = "\"grok-3-fast\"";
        let m: ModelOverride = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m, ModelOverride::Override("grok-3-fast".to_string()));
    }
    #[test]
    fn test_model_override_serialize_inherit() {
        let m = ModelOverride::Inherit;
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "\"inherit\"");
    }
    #[test]
    fn test_model_override_serialize_override() {
        let m = ModelOverride::Override("grok-3-fast".to_string());
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "\"grok-3-fast\"");
    }
    #[test]
    fn test_model_override_in_frontmatter() {
        let content = "---\nname: test\ndescription: Test\nmodel: grok-3-fast\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.model,
            ModelOverride::Override("grok-3-fast".to_string())
        );
    }
    #[test]
    fn test_model_override_in_frontmatter_inherit() {
        let content = "---\nname: test\ndescription: Test\nmodel: inherit\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.model, ModelOverride::Inherit);
    }
    #[test]
    fn test_model_override_omitted_defaults_to_inherit() {
        let content = "---\nname: test\ndescription: Test\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.model, ModelOverride::Inherit);
    }
    #[test]
    fn test_model_override_in_json() {
        let json = serde_json::json!({
            "name": "test",
            "description": "Test",
            "model": "grok-code-fast-1"
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(
            def.model,
            ModelOverride::Override("grok-code-fast-1".to_string())
        );
    }
    #[test]
    fn test_builtin_agent_name_strum_round_trip() {
        use std::str::FromStr;
        for (s, expected) in [
            ("distill", BuiltinAgentName::Distill),
            ("distill-concise", BuiltinAgentName::DistillConcise),
            ("distill-ask-user", BuiltinAgentName::DistillAskUser),
            ("codex", BuiltinAgentName::Codex),
            ("opencode", BuiltinAgentName::Opencode),
            ("general-purpose", BuiltinAgentName::GeneralPurpose),
            ("explore", BuiltinAgentName::Explore),
            ("plan", BuiltinAgentName::Plan),
            ("browser-use", BuiltinAgentName::BrowserUse),
        ] {
            let parsed = BuiltinAgentName::from_str(s).unwrap();
            assert_eq!(parsed, expected, "from_str failed for: {s}");
            assert_eq!(parsed.as_ref(), s, "as_ref failed for: {s}");
        }
    }
    #[test]
    fn test_builtin_agent_name_unknown_returns_err() {
        use std::str::FromStr;
        assert!(BuiltinAgentName::from_str("nonexistent").is_err());
        assert!(BuiltinAgentName::from_str("not-a-builtin-agent").is_err());
    }
    #[test]
    fn test_builtin_agent_name_definition_names_match() {
        use strum::IntoEnumIterator;
        for variant in BuiltinAgentName::iter() {
            let def = variant.definition();
            assert_eq!(
                def.name,
                variant.as_ref(),
                "definition().name doesn't match as_ref() for {:?}",
                variant
            );
        }
    }
    #[test]
    fn test_builtin_agent_name_subagent_variants() {
        let variants = BuiltinAgentName::subagent_variants();
        assert_eq!(variants.len(), 3);
        assert!(variants.contains(&BuiltinAgentName::GeneralPurpose));
        assert!(variants.contains(&BuiltinAgentName::Explore));
        assert!(variants.contains(&BuiltinAgentName::Plan));
    }
    #[test]
    fn test_all_builtins_have_inherit_model() {
        use strum::IntoEnumIterator;
        for variant in BuiltinAgentName::iter() {
            let def = variant.definition();
            assert_eq!(
                def.model,
                ModelOverride::Inherit,
                "Built-in {:?} should default to Inherit",
                variant
            );
        }
    }
    #[test]
    fn mcp_inheritance_default_when_omitted() {
        let def = AgentDefinition::parse("---\nname: t\ndescription: t\n---\n").unwrap();
        assert_eq!(def.mcp_inheritance, McpInheritance::All);
    }
    #[test]
    fn mcp_inheritance_all_parses() {
        let def =
            AgentDefinition::parse("---\nname: t\ndescription: t\nmcpInheritance: all\n---\n")
                .unwrap();
        assert_eq!(def.mcp_inheritance, McpInheritance::All);
    }
    #[test]
    fn mcp_inheritance_none_parses() {
        let def =
            AgentDefinition::parse("---\nname: t\ndescription: t\nmcpInheritance: none\n---\n")
                .unwrap();
        assert_eq!(def.mcp_inheritance, McpInheritance::None);
    }
    #[test]
    fn mcp_inheritance_named_parses() {
        let content = "---\nname: t\ndescription: t\nmcpInheritance:\n  named:\n    - slack\n    - github\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.mcp_inheritance,
            McpInheritance::Named(vec!["slack".into(), "github".into()])
        );
    }
    #[test]
    fn mcp_inheritance_except_parses() {
        let content =
            "---\nname: t\ndescription: t\nmcpInheritance:\n  except:\n    - internal\n---\n";
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(
            def.mcp_inheritance,
            McpInheritance::Except(vec!["internal".into()])
        );
    }
    #[test]
    fn mcp_inheritance_round_trips_via_json() {
        let json = serde_json::json!({
            "name": "t",
            "description": "t",
            "mcpInheritance": {"named": ["a", "b"]}
        });
        let def = AgentDefinition::from_json(&json).unwrap();
        assert_eq!(
            def.mcp_inheritance,
            McpInheritance::Named(vec!["a".into(), "b".into()])
        );
        let serialized = def.to_json_value();
        let recovered = AgentDefinition::from_json(&serialized).unwrap();
        assert_eq!(recovered.mcp_inheritance, def.mcp_inheritance);
    }
    fn def_with_template(tpl: crate::prompt::context::TemplateOverride) -> AgentDefinition {
        let mut def = AgentDefinition::default_distill();
        def.system_prompt = tpl;
        def
    }
    #[test]
    fn carries_discipline_false_for_every_template_and_audience() {
        for tpl in [
            crate::prompt::context::TemplateOverride::None,
            crate::prompt::context::TemplateOverride::Codex,
            crate::prompt::context::TemplateOverride::Custom("fake".to_string()),
        ] {
            let def = def_with_template(tpl.clone());
            for audience in [
                crate::prompt::context::PromptAudience::Primary,
                crate::prompt::context::PromptAudience::Subagent,
            ] {
                assert!(
                    !def.carries_task_completion_discipline(audience),
                    "discipline block was removed; helper must return false \
                     (template: {tpl:?}, audience: {audience:?})"
                );
            }
        }
    }
}
