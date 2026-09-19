// Modified for Distill by Samuel Fajreldines, 2026.
//! Configuration shapes referenced from session lifecycle requests and `OpsChunk::ProjectConfig` / `OpsChunk::Permissions`.
//!
//! TODO: align with the canonical project / permission /
//! agent-session config types in `distill-config` and friends.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Filesystem isolation strategy for a forked session.
/// `Default` is [`IsolationMode::None`] for the root session; a subagent that relies on it gets shared-tree access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// No isolation: subagent shares the parent's working tree.
    #[default]
    None,
    /// Run the subagent in a copy-on-write git worktree.
    Worktree,
    /// Run the subagent inside a sandbox/container.
    Sandbox,
}

/// Capability mode applied to a forked session.
/// `Default` is [`CapabilityMode::ReadWrite`] for the root session; a subagent that relies on it gets read and write access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    /// Full read+write capability (default for the root session).
    #[default]
    ReadWrite,
    /// Read-only: tools that mutate state are unavailable.
    ReadOnly,
    /// No tools at all.
    None,
}

/// Per-tool-server configuration knob.
/// TODO: align with the MCP/tool-server config in `distill-tools` once the wire surface is firm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolServerConfig {
    /// Tool server identifier.
    pub id: String,
    /// Whether this tool server is enabled for the session.
    #[serde(default)]
    pub enabled: bool,
    /// Optional command override (for dynamically launched servers).
    #[serde(default)]
    pub command: Option<String>,
    /// Free-form arguments (key/value).
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Configuration applied when forking a session via `SessionLifecycleRequest::Fork`.
/// `Default` is root-session (`None` + `ReadWrite`); name subagent fields explicitly rather than `..Default::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionConfig {
    /// Agent identifier (e.g. `"subagent-explore"`).
    pub agent_id: String,
    /// Filesystem isolation strategy.
    #[serde(default)]
    pub isolation: IsolationMode,
    /// Capability mode (read-only, read-write, none).
    #[serde(default)]
    pub capability_mode: CapabilityMode,
    /// Optional per-tool-server overrides.
    #[serde(default)]
    pub tool_config: Vec<ToolServerConfig>,
    /// Maximum recursion depth for subagent nesting; 0 means no further nesting.
    #[serde(default)]
    pub max_depth: u32,
    /// Working directory override (relative to workspace root).
    #[serde(default)]
    pub cwd_override: Option<String>,
    /// Extra environment variables to set for the subagent.
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
}

/// Project configuration returned by `OpsChunk::ProjectConfig`.
///
/// TODO: align with `distill_config::ProjectConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Free-form key/value config (placeholder).
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Whether the project is trusted (allows hooks/plugins to run).
    #[serde(default)]
    pub trusted: bool,
}

/// Permission policy returned by `OpsChunk::Permissions`.
/// TODO: align with the canonical permission policy type (currently free-form JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    /// Tool patterns that are unconditionally allowed (no prompt).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tool patterns that are unconditionally denied.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tool patterns that always prompt for permission.
    #[serde(default)]
    pub ask: Vec<String>,
}
