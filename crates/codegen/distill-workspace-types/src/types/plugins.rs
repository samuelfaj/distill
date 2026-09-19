// Modified for Distill by Samuel Fajreldines, 2026.
//! Discovery shapes for plugins and hooks.
//! They appear in `OpsChunk::Plugins`, `OpsChunk::Plugin`, `WorkspaceEvent::PluginsChanged`, and `WorkspaceEvent::HooksChanged`.
//!
//! TODO: align with the canonical types in
//! `distill-hooks-plugins-types` and `distill-plugin-marketplace`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Stable identifier.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Plugin version (semver).
    #[serde(default)]
    pub version: String,
    /// Filesystem path to the plugin (as a string).
    #[serde(default)]
    pub path: String,
    /// Source: `"global"`, `"workspace"`, `"marketplace"`, ...
    #[serde(default)]
    pub source: String,
    /// Whether the plugin is currently enabled.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInfo {
    /// Stable identifier (e.g. `"pre-tool-call"`).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Hook event the script attaches to.
    /// TODO: free-form string until aligned with `HookEvent`; typos currently pass.
    #[serde(default)]
    pub event: String,
    /// Originating plugin id, if the hook came from a plugin.
    #[serde(default)]
    pub plugin_id: Option<String>,
    /// Whether this hook is currently enabled.
    #[serde(default)]
    pub enabled: bool,
}
