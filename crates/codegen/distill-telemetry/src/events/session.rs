//! Session lifecycle product telemetry events.

use serde::Serialize;

use super::{MemoryRetrievalMode, PlanModeState};
use crate::enums::PermissionMode;

#[derive(Debug, Serialize)]
pub struct SessionHarness {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    pub model_id: String,
    pub agent_name: String,
    pub permission_mode: PermissionMode,
    pub mcp_server_names: Vec<String>,
    pub plugin_names: Vec<String>,
    pub skill_names: Vec<String>,
    pub lsp_server_names: Vec<String>,
    pub hook_names: Vec<String>,
    pub agents_md_dir_names: Vec<String>,
    pub memory_enabled: bool,
    pub memory_retrieval_mode: MemoryRetrievalMode,
    /// Whether the session cwd is inside a git repo (same value `SessionNew` carries); the external `session_start` event reads it.
    pub is_git_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
}

#[derive(Serialize)]
pub struct SessionLoad {
    pub session_id: String,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub plan_mode_state: PlanModeState,
    pub permission_mode: PermissionMode,
    pub model_id: String,
    pub restored_from_disk: bool,
}

#[derive(Serialize)]
pub struct SessionNew {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub is_git_repo: bool,
    pub permission_mode: PermissionMode,
}

#[derive(Serialize)]
pub struct RolloutSurvey {
    pub session_id: String,
    pub preferences: Vec<String>,
    pub has_feedback: bool,
}

#[derive(Serialize)]
pub struct SessionEnded {
    pub duration_secs: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub compaction_count: u64,
    pub model_id: String,
}

/// `total_ms` is the wall-clock of the whole teardown.
/// The other fields are individual steps, so `total_ms - sum(steps)` is time spent outside the measured steps.
/// Emitted once after feedback so late phases are not dropped: `SessionEnded` fires mid-teardown.
#[derive(Serialize, Default)]
pub struct SessionEndTimings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_save_ms: Option<u64>,
    /// Intentionally unpopulated; retained because downstream metrics consumers still read this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_consolidate_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks_dispatch_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks_stop_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflows_drain_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflows_persist_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_drain_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_tasks_save_ms: Option<u64>,
}

/// Connect outcome: the `agent_connect` product event, plus OTEL metrics.
#[derive(Serialize)]
pub struct AgentConnect {
    pub connect_target: crate::startup::AgentKind,
    pub outcome: crate::startup::StartupOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stuck_in: Option<String>,
    pub phases: String,
    pub phase_durations_ms: std::collections::BTreeMap<String, u64>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    pub embedded_fallback: bool,
    pub auth_mode: crate::startup::AuthMode,
}

/// A `session/new` that never reached `session created`. Maps to the create-timeout counter only.
#[derive(Serialize)]
pub struct SessionCreateFailed {
    pub outcome: crate::startup::StartupOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stuck_phase: Option<String>,
    pub elapsed_ms: u64,
}
