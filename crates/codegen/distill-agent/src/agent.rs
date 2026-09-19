// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use distill_sampling_types::HostedTool;
use distill_tools::bridge::ToolBridge;
use distill_tools::types::definition::ToolDefinition;

use crate::compaction::CompactionPolicy;
use crate::config::{AgentDefinition, CompletionRequirement, PermissionMode};
use crate::prompt::context::{PromptContext, RenderedPrompt};
use crate::system_reminder::ReminderPolicy;

/// A fully built agent: an AgentDefinition plus its session context. Not portable.
/// Effectively immutable after construction. Tool-state mutations go through ToolBridge's internal locks.
pub struct Agent {
    /// The definition this agent was built from.
    definition: AgentDefinition,

    /// The context that produced the current system prompt.
    /// Stored for inspection, re-rendering, and serialization.
    prompt_context: PromptContext,

    /// The rendered system prompt (cached from prompt_context.render()).
    system_prompt: String,

    /// Owns the ToolBridge (registry, Resources, session context).
    tool_bridge: Arc<ToolBridge>,

    /// Session-level policies.
    reminder_policy: ReminderPolicy,
    compaction_policy: CompactionPolicy,

    /// Backend-hosted tools to include in API requests.
    /// These are sent as native Responses API types (e.g., `WebSearch`) and executed server-side by the agentic sampler.
    hosted_tools: Vec<HostedTool>,

    /// Build-time toggle for server-side search tools.
    /// ANDed at request time with the per-model `SessionActor::supports_backend_search`.
    backend_search_enabled: bool,
}

impl Agent {
    /// Normally called by `AgentBuilder::build()`.
    /// Exposed publicly for test helpers that need to construct an Agent with a pre-built ToolBridge.
    pub fn new(
        definition: AgentDefinition,
        prompt_context: PromptContext,
        system_prompt: String,
        tool_bridge: Arc<ToolBridge>,
        reminder_policy: ReminderPolicy,
        compaction_policy: CompactionPolicy,
        hosted_tools: Vec<HostedTool>,
        backend_search_enabled: bool,
    ) -> Self {
        Self {
            definition,
            prompt_context,
            system_prompt,
            tool_bridge,
            reminder_policy,
            compaction_policy,
            hosted_tools,
            backend_search_enabled,
        }
    }

    // ── From definition ──────────────────────────────────────────────

    /// Agent name (unique identifier).
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    pub fn description(&self) -> &str {
        &self.definition.description
    }

    pub fn definition(&self) -> &AgentDefinition {
        &self.definition
    }

    pub fn permission_mode(&self) -> &PermissionMode {
        &self.definition.permission_mode
    }

    pub fn completion_requirement(&self) -> Option<&CompletionRequirement> {
        self.definition.completion_requirement.as_ref()
    }

    // ── Session-level ────────────────────────────────────────────────

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Compact system prompt for post-compaction use.
    pub fn compact_system_prompt(&self) -> &str {
        crate::prompt::template::COMPACT_SYSTEM_PROMPT
    }

    pub fn tool_bridge(&self) -> &Arc<ToolBridge> {
        &self.tool_bridge
    }

    pub fn compaction_policy(&self) -> &CompactionPolicy {
        &self.compaction_policy
    }

    pub fn reminder_policy(&self) -> &ReminderPolicy {
        &self.reminder_policy
    }

    /// Cached AGENTS.md section (derived from prompt_context).
    pub fn agents_md_section(&self) -> Option<String> {
        self.prompt_context.format_agents_md_section()
    }

    /// Returns the AGENTS.md `<system-reminder>` block to prepend as a user message, respecting audience (compacted for subagents) and template.
    pub fn agents_md_user_reminder(&self) -> Option<String> {
        self.prompt_context.agents_md_user_reminder()
    }

    /// Returns the personas `<system-reminder>` block to prepend as a user message, respecting audience (suppressed for subagents) and template.
    pub fn personas_user_reminder(&self) -> Option<String> {
        self.prompt_context.personas_user_reminder()
    }

    /// The structured prompt context for inspection and re-rendering.
    pub fn prompt_context(&self) -> &PromptContext {
        &self.prompt_context
    }

    /// Audience this agent's prompt was rendered for (Primary or Subagent).
    /// Read by the runtime TodoGate with [`crate::AgentDefinition::carries_task_completion_discipline`]
    /// to decide whether the active prompt actually carries the rules the reminder invokes.
    pub fn prompt_audience(&self) -> crate::prompt::context::PromptAudience {
        self.prompt_context.audience
    }

    /// Tool definitions for the sampling API.
    pub async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_bridge.tool_definitions().await
    }

    /// Backend-hosted tools that should be included in API requests.
    /// These are sent as native types (e.g., `rs::Tool::WebSearch`) and executed server-side by the agentic sampler.
    pub fn hosted_tools(&self) -> &[HostedTool] {
        &self.hosted_tools
    }

    /// Build-time toggle for server-side search tools.
    /// Callers should AND this with the per-model `supports_backend_search` flag to decide whether to ship `hosted_tools` on a request.
    /// Do not use `hosted_tools().is_empty()` as a proxy; the list also depends on web-search config.
    pub fn backend_search_enabled(&self) -> bool {
        self.backend_search_enabled
    }

    /// Built-in tool definitions only (excludes MCP tools).
    pub async fn tool_definitions_builtins_only(&self) -> Vec<ToolDefinition> {
        self.tool_bridge.tool_definitions_builtins_only().await
    }

    /// Whether auto-compact should trigger given current token usage.
    ///
    /// `context_window` comes from the session's SamplingConfig (model-provided).
    pub fn should_auto_compact(
        &self,
        total_tokens: u64,
        context_window: std::num::NonZeroU64,
    ) -> bool {
        let cw = context_window.get();
        distill_token_estimation::exceeds_threshold(
            total_tokens,
            cw,
            self.compaction_policy.auto_compact_threshold_percent as u8,
        )
    }

    /// Update completion and retry policies from a new definition.
    /// Does not rebuild the tool registry or re-render prompts. Used for mid-session mode switching.
    pub async fn update_policies_from_definition(&self, _def: &AgentDefinition) {
        // TODO: completion requirements and retry configs are now part of ToolServerConfig and handled at registry finalization time
        // Mid-session policy updates are not yet supported in the new architecture.
    }

    /// Re-render the system prompt from current ToolBridge state (tool name overrides, disabled tools).
    /// Called by hosts after mid-session tool-override updates.
    pub async fn finalize_prompt(&mut self) {
        self.prompt_context.build_timestamp_utc = chrono::Utc::now().to_rfc3339();

        self.system_prompt = self
            .prompt_context
            .render(&self.tool_bridge)
            .await
            .unwrap_or_default();
    }

    /// The pair comes only from [`PromptContext::render_paired`], so the installed context always matches its prompt.
    pub fn set_rendered_prompt(&mut self, rendered: RenderedPrompt) {
        (self.prompt_context, self.system_prompt) = rendered.into_parts();
    }

    /// Re-render the system prompt for a different definition, reusing the existing ToolBridge.
    /// Used for mid-session mode switching.
    pub async fn render_prompt_for_definition(&self, definition: &AgentDefinition) -> String {
        let mut ctx = self.prompt_context.clone();
        ctx.prompt_mode = definition.prompt_mode.clone();
        ctx.prompt_body = definition.prompt_body.clone();
        ctx.system_prompt = definition.system_prompt.clone();
        ctx.include_browser_verification = definition.include_browser_verification();
        ctx.build_timestamp_utc = chrono::Utc::now().to_rfc3339();

        if !definition.agents_md {
            ctx.agents_md_files.clear();
        }

        ctx.render(&self.tool_bridge).await.unwrap_or_default()
    }
}
