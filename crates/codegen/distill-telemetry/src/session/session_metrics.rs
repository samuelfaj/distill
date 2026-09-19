// Modified for Distill by Samuel Fajreldines, 2026.
//! Session lifecycle event structs.
//!
//! Fires in both `Enabled` and `SessionMetrics` telemetry modes via `log_session_event`.

use serde::Serialize;

/// The ACP method the client called.
/// It stays separate from the warm/cold mechanism (`SessionStarted::restored_from_disk`) so intent and mechanism can be queried independently.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartKind {
    New,
    Load,
    Resume,
}

#[derive(Serialize)]
pub struct SessionStarted {
    pub session_id: String,
    pub kind: SessionStartKind,
    pub setup_duration_ms: u64,
    /// Whether setup rebuilt the session from disk (cold) rather than reconnecting to a resident actor (warm). Mirrors `SessionLoad`.
    pub restored_from_disk: bool,
}

impl SessionStarted {
    pub fn new(
        session_id: String,
        kind: SessionStartKind,
        setup_duration: std::time::Duration,
        restored_from_disk: bool,
    ) -> Self {
        Self {
            session_id,
            kind,
            setup_duration_ms: setup_duration.as_millis() as u64,
            restored_from_disk,
        }
    }
}

/// Itemized context occupancy once session setup (including MCP init) has finished.
/// Category token fields are counted with the model's tokenizer via `POST /v1/tokenize-text`.
/// `used_tokens` / `message_tokens` stay the chat-state occupancy already shown in `/context`.
#[derive(Serialize)]
pub struct SessionContextSnapshot {
    pub session_id: String,
    pub model_id: String,
    pub context_window: u64,
    pub used_tokens: u64,
    pub usage_pct: u8,
    pub free_tokens: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_tokens: u64,
    pub tool_definitions_count: u64,
    pub message_tokens: u64,
    pub skills_tokens: u64,
    pub skills_count: u64,
    pub mcp_tokens: u64,
    pub mcp_server_count: u64,
    pub agents_md_tokens: u64,
    pub agents_md_file_count: u64,
    pub workflows_tokens: u64,
    pub workflows_count: u64,
}

#[derive(Serialize)]
pub struct Turn {
    pub session_id: String,
    pub turn_number: u64,
}

#[derive(Serialize)]
pub struct TurnCompletedLifecycle {
    pub session_id: String,
    pub turn_number: u64,
}

/// Server-side doom-loop detection observed this turn. Aggregated detector metadata only, never generation content or token IDs.
#[derive(Serialize)]
pub struct DoomLoopDetected {
    pub session_id: String,
    pub turn_number: u64,
    pub trigger_count: u32,
    pub detector_kinds: Vec<String>,
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tightest_tail_threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exact_sequence_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exact_repeat_count: Option<u32>,
    pub recovery_attempts: u32,
    pub model: String,
}

/// Doom-loop recovery acted this turn.
/// Poisoned attempts were resampled and/or a response was accepted with confident signals after the budget was spent.
/// Trigger labels only, never generation content.
#[derive(Serialize)]
pub struct DoomLoopRecovery {
    pub session_id: String,
    pub turn_number: u64,
    /// Resamples this turn (doomed attempts discarded).
    pub attempts: u32,
    /// Whether the final response kept confident signals (budget spent).
    pub accepted_after_budget: bool,
    /// Tightest raw trigger label observed this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_trigger: Option<String>,
    /// Model that produced the doomed attempts.
    pub model: String,
}

#[derive(Serialize)]
pub struct TraceUploadAttempted {
    pub session_id: String,
    pub turn_number: u64,
    pub upload_method: String,
}

#[derive(Serialize)]
pub struct TraceUploadSucceeded {
    pub session_id: String,
    pub turn_number: u64,
    pub upload_method: String,
    pub fully_uploaded: bool,
}

#[derive(Serialize)]
pub struct TraceUploadSkipped {
    pub session_id: String,
    pub turn_number: u64,
    pub reason: String,
}

#[derive(Serialize)]
pub struct TraceUploadFailed {
    pub session_id: String,
    pub turn_number: u64,
    pub upload_method: String,
    pub error_category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

/// Why trace uploads are enabled or disabled for a given prompt.
/// Recorded on the `agent.prompt` span as `upload_reason` for analytics queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum TraceUploadReason {
    /// ZDR (zero data retention) team: all uploads disabled.
    ZdrTeam,
    /// `[telemetry] trace_upload = false` in config.
    FeatureOff,
    /// No grok.com auth or deployment key.
    NoCredentials,
    /// Direct-to-bucket S3 upload.
    DirectS3,
    /// Proxy mode via grok.com auth.
    Proxy,
    /// Direct GCS with service account key.
    DirectGcs,
    /// Session handle not found (edge case).
    SessionNotFound,
}

impl TraceUploadReason {
    pub fn from_upload_method(method: &Option<distill_file_utils::UploadMethod>) -> Self {
        match method {
            Some(distill_file_utils::UploadMethod::Proxy { .. }) => Self::Proxy,
            Some(distill_file_utils::UploadMethod::S3 { .. }) => Self::DirectS3,
            Some(distill_file_utils::UploadMethod::Direct { .. }) => Self::DirectGcs,
            None => Self::NoCredentials,
        }
    }
}

#[cfg(test)]
mod tests {
    use distill_file_utils::UploadMethod;

    use super::TraceUploadReason;

    #[test]
    fn doom_loop_detected_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(super::DoomLoopDetected::NAME, "doom_loop_detected");
        let event = serde_json::to_value(super::DoomLoopDetected {
            session_id: "s1".to_string(),
            turn_number: 7,
            trigger_count: 3,
            detector_kinds: vec![
                "tail_repetition".to_string(),
                "exact_repetition".to_string(),
            ],
            channels: vec!["thinking".to_string(), "response".to_string()],
            tightest_tail_threshold: Some(32),
            max_exact_sequence_tokens: Some(42),
            max_exact_repeat_count: Some(3),
            recovery_attempts: 1,
            model: "grok-4.6".to_string(),
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 7,
                "trigger_count": 3,
                "detector_kinds": ["tail_repetition", "exact_repetition"],
                "channels": ["thinking", "response"],
                "tightest_tail_threshold": 32,
                "max_exact_sequence_tokens": 42,
                "max_exact_repeat_count": 3,
                "recovery_attempts": 1,
                "model": "grok-4.6",
            })
        );
    }

    /// `session_context_snapshot` property keys are a Mixpanel dashboard contract; pin the shape so a rename cannot silently break queries.
    #[test]
    fn session_context_snapshot_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(
            super::SessionContextSnapshot::NAME,
            "session_context_snapshot"
        );
        let value = serde_json::to_value(super::SessionContextSnapshot {
            session_id: "s1".to_string(),
            model_id: "grok-4".to_string(),
            context_window: 1_000_000,
            used_tokens: 40_000,
            usage_pct: 4,
            free_tokens: 960_000,
            system_prompt_tokens: 8_000,
            tool_definitions_tokens: 5_000,
            tool_definitions_count: 12,
            message_tokens: 2_000,
            skills_tokens: 27_000,
            skills_count: 282,
            mcp_tokens: 1_200,
            mcp_server_count: 4,
            agents_md_tokens: 3_400,
            agents_md_file_count: 2,
            workflows_tokens: 800,
            workflows_count: 3,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "session_id": "s1",
                "model_id": "grok-4",
                "context_window": 1_000_000,
                "used_tokens": 40_000,
                "usage_pct": 4,
                "free_tokens": 960_000,
                "system_prompt_tokens": 8_000,
                "tool_definitions_tokens": 5_000,
                "tool_definitions_count": 12,
                "message_tokens": 2_000,
                "skills_tokens": 27_000,
                "skills_count": 282,
                "mcp_tokens": 1_200,
                "mcp_server_count": 4,
                "agents_md_tokens": 3_400,
                "agents_md_file_count": 2,
                "workflows_tokens": 800,
                "workflows_count": 3,
            })
        );
    }

    /// The `grok-shell-doom_loop_recovery` Mixpanel event's name and property keys are dashboard contracts; pin them.
    #[test]
    fn doom_loop_recovery_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(super::DoomLoopRecovery::NAME, "doom_loop_recovery");
        let with_trigger = serde_json::to_value(super::DoomLoopRecovery {
            session_id: "s1".to_string(),
            turn_number: 7,
            attempts: 2,
            accepted_after_budget: true,
            top_trigger: Some("tail_repetition:4@thinking".to_string()),
            model: "grok-4.5".to_string(),
        })
        .unwrap();
        assert_eq!(
            with_trigger,
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 7,
                "attempts": 2,
                "accepted_after_budget": true,
                "top_trigger": "tail_repetition:4@thinking",
                "model": "grok-4.5",
            })
        );
        let no_trigger = serde_json::to_value(super::DoomLoopRecovery {
            session_id: "s1".to_string(),
            turn_number: 7,
            attempts: 1,
            accepted_after_budget: false,
            top_trigger: None,
            model: "grok-4.5".to_string(),
        })
        .unwrap();
        assert!(no_trigger.get("top_trigger").is_none(), "None is omitted");
    }

    /// `as_str` values are recorded on the `agent.prompt` span as `upload_reason` and queried in analytics.
    /// They are a wire contract and must not drift.
    #[test]
    fn as_str_values_are_stable() {
        assert_eq!(TraceUploadReason::ZdrTeam.as_ref(), "zdr_team");
        assert_eq!(TraceUploadReason::FeatureOff.as_ref(), "feature_off");
        assert_eq!(TraceUploadReason::NoCredentials.as_ref(), "no_credentials");
        assert_eq!(TraceUploadReason::DirectS3.as_ref(), "direct_s3");
        assert_eq!(TraceUploadReason::Proxy.as_ref(), "proxy");
        assert_eq!(TraceUploadReason::DirectGcs.as_ref(), "direct_gcs");
        assert_eq!(
            TraceUploadReason::SessionNotFound.as_ref(),
            "session_not_found"
        );
    }

    /// Each `UploadMethod` maps to its corresponding reason; `None` (no credentials resolved) maps to `NoCredentials`.
    #[test]
    fn from_upload_method_maps_each_variant() {
        assert_eq!(
            TraceUploadReason::from_upload_method(&None),
            TraceUploadReason::NoCredentials
        );
        assert_eq!(
            TraceUploadReason::from_upload_method(&Some(UploadMethod::Direct {
                service_account_key: None,
            })),
            TraceUploadReason::DirectGcs
        );
        assert_eq!(
            TraceUploadReason::from_upload_method(&Some(UploadMethod::Proxy {
                proxy_base_url: String::new(),
                user_token: String::new(),
                deployment_key: None,
                alpha_test_key: None,
            })),
            TraceUploadReason::Proxy
        );
        assert_eq!(
            TraceUploadReason::from_upload_method(&Some(UploadMethod::S3 {
                bucket: String::new(),
                region: String::new(),
                credentials_file: None,
                credentials_content: None,
                endpoint_url: None,
            })),
            TraceUploadReason::DirectS3
        );
    }
}
