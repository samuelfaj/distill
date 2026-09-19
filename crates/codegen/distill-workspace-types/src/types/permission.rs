// Modified for Distill by Samuel Fajreldines, 2026.
//! Permission flow shapes used by [`ToolChunk::NeedPermission`](crate::chunks::ToolChunk::NeedPermission).
//! They also appear in [`ToolResponse::Permission`](crate::chunks::ToolResponse::Permission).
//!
//! These feed the bidirectional tool-stream flow that consumes these types.
//!
//! TODO(workspace): align with the canonical permission types in
//! `distill-shell` once the wire surface is firm.

use serde::{Deserialize, Serialize};

/// A pending permission request emitted to the sampler via [`ToolChunk::NeedPermission`](crate::chunks::ToolChunk::NeedPermission).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Tool the permission is for (e.g. `"run_terminal_cmd"`).
    pub tool_name: String,
    /// Free-form summary the UI shows to the user.
    #[serde(default)]
    pub summary: String,
    /// JSON-serialized arguments the tool would receive.
    #[serde(default)]
    pub input_json: String,
    /// Whether allowing this is destructive (mutates state).
    #[serde(default)]
    pub destructive: bool,
}

/// User decision delivered via [`ToolResponse::Permission`](crate::chunks::ToolResponse::Permission) on the tool's bidi sender.
/// Adjacent tagging is the only form uniform across struct, newtype, and unit variants, and avoids the nested-`decision` hazard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this single invocation.
    AllowOnce,
    /// Allow and remember the decision for the rest of the session.
    AllowSession,
    /// Allow and persist the decision to the project's permission policy.
    AllowProject,
    /// Deny this invocation.
    Deny {
        /// Optional human-readable reason.
        #[serde(default)]
        reason: String,
    },
}
