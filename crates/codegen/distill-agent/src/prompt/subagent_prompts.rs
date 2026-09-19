// Modified for Distill by Samuel Fajreldines, 2026.
//! System prompts for built-in subagent profiles.
//!
//! ## Tool name resolution
//!
//! All tool names in these prompts use the `${{ tools.by_kind.* }}` template syntax from the `TemplateRenderer`.
//! `PromptContext::render()` hands the prompt to `ToolBridge::render_prompt()`, where MiniJinja resolves each variable to the session's tool names.
//!
//! This means:
//! - Tool names are NEVER hardcoded; they adapt to name overrides and alternate tool namespaces
//! - If a tool kind is absent from the renderer's context, MiniJinja resolves it to an empty string.
//!   Templates can also use `${%- if tools.by_kind.X %}` conditionals to hide entire sections.
//!
//! Tool-kind mapping (common names → ToolKind):
//!   Read       → `${{ tools.by_kind.read }}`
//!   Write/Edit → `${{ tools.by_kind.edit }}`
//!   Glob       → `${{ tools.by_kind.list }}`
//!   Grep       → `${{ tools.by_kind.search }}`
//!   Bash       → `${{ tools.by_kind.execute }}`
//!   WebSearch  → `${{ tools.by_kind.web_search }}`

pub use distill_tool_types::{EXPLORE_PROMPT, GENERAL_PURPOSE_PROMPT, PLAN_PROMPT};
