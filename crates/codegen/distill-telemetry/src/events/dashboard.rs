//! Dashboard and block-viewer product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct DashboardOpened {
    pub agents: usize,
    pub subagents: usize,
    pub leader_mode: bool,
}

/// Intent-only telemetry for the bindings that can own Ctrl+L. Allowlist: `interject_prompt` and `open_extensions`.
/// Absence of other actions is not “unused.”. Expand the allowlist deliberately; this is not full-registry coverage.
/// `key` is a platform-stable encoding (`Ctrl+L`, not locale-specific `Cmd`/`Opt` or mixed case).
#[derive(Serialize)]
pub struct ShortcutUsed {
    /// Stable chord encoding (`Ctrl+L`, `Ctrl+Enter`, …).
    pub key: String,
    /// Allowlisted action id (`interject_prompt`, `open_extensions`).
    pub action: String,
    /// Surface label (`prompt_focused`, `agent_screen`, `queue`, …).
    pub context: String,
}

#[derive(Serialize)]
pub struct DashboardClosed {
    pub agents: usize,
}

#[derive(Serialize)]
pub struct DashboardAgentAttached {
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct DashboardAgentLaunched {
    pub source: &'static str,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum BlockViewerKind {
    Markdown,
    Execute,
    Edit,
    BgTask,
    WebFetch,
    WebSearch,
    IntegrationSearch,
    UseTool,
    Read,
    Grep,
    PlainText,
}

#[derive(Serialize)]
pub struct BlockViewerOpened {
    pub kind: BlockViewerKind,
}

#[derive(Serialize)]
pub struct BlockViewerQuoted {
    pub kind: BlockViewerKind,
}
