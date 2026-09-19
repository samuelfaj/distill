//! Startup product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct StartupCompleted {
    pub total_ms: u64,
    pub outcome: crate::startup::StartupOutcome,
    pub phases: String,
    pub auth_mode: crate::startup::AuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefetch_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_load_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_replay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_git_scan_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_spawn_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_process_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_config_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_settings_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_manager_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_policy_auth_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_policy_config_sync_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_first_frame_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct StartupInteractive {
    pub interactive_ms: u64,
    pub auth_mode: crate::startup::AuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_to_first_frame_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct StartupSubTimers {
    pub timings: Vec<(String, u64)>,
    pub outcome: crate::startup::StartupOutcome,
    pub auth_mode: crate::startup::AuthMode,
}
