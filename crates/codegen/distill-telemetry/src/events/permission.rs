//! Shared permission field enums (structs live in `permission_analytics`).

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum AccessKind {
    Read,
    Edit,
    Bash,
    Grep,
    Mcp,
    Web,
    AgentMessage,
    Other,
}

#[derive(Serialize, Clone, Copy, strum::AsRefStr, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PermissionOutcome {
    Allow,
    Deny,
    Cancelled,
    Followup,
}
