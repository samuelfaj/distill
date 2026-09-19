// Modified for Distill by Samuel Fajreldines, 2026.
//! Canonical external-settings tool name ↔ Grok tool correspondence: one table
//! replacing two that drifted apart.
//!
//! Two consumers read it independently. The hook matcher (`distill-hooks`) needs the
//! Grok tool **names** an external settings term maps to (and the reverse, for regex
//! matchers); the agent builder (`distill-agent`) needs the [`ToolKind`] a `tools:`
//! allowlist entry resolves to. A row may carry a kind without names (`PowerShell`
//! shares `Execute`, with no distinct tool) or names without a kind (e.g.
//! `Agent`/`ExitPlanMode`/`Cron*` are matchable but not allowlist-resolvable).
//!
//! The `grok` names are test-checked against the live registry.

use super::tool::ToolKind;
use ToolKind::*;

/// One Claude tool's correspondence to Grok, read via the accessor functions below.
struct ClaudeTool {
    claude: &'static str,
    /// Grok [`ToolKind`] for allowlist resolution; `None` for names that are matchable
    /// (spawn/plan-mode directives) but must not resolve an allowlist.
    kind: Option<ToolKind>,
    /// Grok tool names this Claude tool maps to (empty when there is no direct
    /// Grok tool — the entry then only contributes a `kind`).
    grok: &'static [&'static str],
}

/// Row that resolves an allowlist (carries a [`ToolKind`]) — the common case.
const fn k(claude: &'static str, kind: ToolKind, grok: &'static [&'static str]) -> ClaudeTool {
    ClaudeTool {
        claude,
        kind: Some(kind),
        grok,
    }
}

/// Row that is matchable but not allowlist-resolvable (`kind: None`).
const fn match_only(claude: &'static str, grok: &'static [&'static str]) -> ClaudeTool {
    ClaudeTool {
        claude,
        kind: None,
        grok,
    }
}

#[rustfmt::skip]
const CLAUDE_TOOLS: &[ClaudeTool] = &[
    k("Read",            Read,                 &["read_file", "hashline_read"]),
    k("Write",           Write,                &["write", "search_replace", "hashline_edit"]), // search_replace kept for back-compat
    k("Edit",            Edit,                 &["search_replace", "hashline_edit"]),
    k("MultiEdit",       Edit,                 &["search_replace", "hashline_edit"]), // legacy, superseded by Edit
    k("NotebookEdit",    Edit,                 &["search_replace", "hashline_edit"]),
    k("Bash",            Execute,              &["run_terminal_command"]),
    k("PowerShell",      Execute,              &[]),
    k("Grep",            Search,               &["grep", "hashline_grep"]),
    k("Glob",            List,                 &["list_dir"]),
    k("LS",              List,                 &[]),                                  // legacy name for Glob
    k("LSP",             Lsp,                  &["lsp"]),
    k("WebSearch",       WebSearch,            &["web_search"]),
    k("WebFetch",        WebFetch,             &["web_fetch"]),
    k("DeployApp",       DeployApp,            &[]),
    k("TodoWrite",       Plan,                 &["todo_write"]),
    k("AskUserQuestion", AskUser,              &["ask_user_question"]),
    k("TaskOutput",      BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("BashOutput",      BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("BashOutputTool",  BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("AgentOutputTool", BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("TaskStop",        KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    k("KillShell",       KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    k("KillBash",        KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    k("Skill",           Read,                 &["skill"]),                           // matcher: opencode's `skill` tool; allowlist Read (distill reads SKILL.md)
    k("ToolSearch",      SearchTool,           &["search_tool"]),
    match_only("Agent",         &["spawn_subagent"]),                                 // canonical; Task is the legacy alias
    match_only("Task",          &["spawn_subagent"]),
    match_only("EnterPlanMode", &["enter_plan_mode"]),                                // kind=None: enter/exit must stay paired
    match_only("ExitPlanMode",  &["exit_plan_mode"]),
    match_only("CronCreate",    &["scheduler_create"]),
    match_only("CronDelete",    &["scheduler_delete"]),
    match_only("CronList",      &["scheduler_list"]),
    match_only("ListMcpResourcesTool", &["ListMcpResources"]),                        // cursor preset
];

/// The Grok [`ToolKind`] a Claude allowlist entry resolves to, if any.
pub fn kind_for(claude: &str) -> Option<ToolKind> {
    CLAUDE_TOOLS
        .iter()
        .find(|t| t.claude == claude)
        .and_then(|t| t.kind)
}

/// The Grok tool names a Claude matcher term fires on.
pub fn grok_names_for(claude: &str) -> impl Iterator<Item = &'static str> {
    CLAUDE_TOOLS
        .iter()
        .find(|t| t.claude == claude)
        .map(|t| t.grok)
        .unwrap_or(&[])
        .iter()
        .copied()
}

/// The Claude names that map to `grok_name` (reverse lookup, for regex matchers).
pub fn claude_names_for(grok_name: &str) -> impl Iterator<Item = &'static str> + '_ {
    CLAUDE_TOOLS
        .iter()
        .filter(move |t| t.grok.contains(&grok_name))
        .map(|t| t.claude)
}

/// Every distinct Grok name the table references, for the `distill-agent` drift-check
/// test that asserts each is a real client tool name.
pub fn grok_names() -> impl Iterator<Item = &'static str> {
    let mut seen = std::collections::HashSet::new();
    CLAUDE_TOOLS
        .iter()
        .flat_map(|t| t.grok.iter().copied())
        .filter(move |name| seen.insert(*name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_are_unique() {
        // The drift this registry exists to prevent: two rows for one Claude name.
        let mut seen = std::collections::HashSet::new();
        for t in CLAUDE_TOOLS {
            assert!(seen.insert(t.claude), "duplicate Claude name: {}", t.claude);
        }
    }

    #[test]
    fn every_row_contributes() {
        // A row with neither a kind nor a Grok name is dead weight (and signals a typo).
        for t in CLAUDE_TOOLS {
            assert!(
                t.kind.is_some() || !t.grok.is_empty(),
                "dead row: {}",
                t.claude
            );
        }
    }
}
