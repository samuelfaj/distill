use serde::{Deserialize, Serialize};

pub const EVENT_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted {
        session_id: String,
        turn_number: u64,
        model_id: String,
        yolo_mode: bool,
        conversation_message_count: usize,
        session_relationship: SessionRelationship,
        schema_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        redirect_kind: Option<RedirectKind>,
    },
    PhaseChanged {
        phase: Phase,
    },
    FirstToken,
    LoopStarted {
        loop_index: u32,
    },
    ToolStarted {
        tool_name: String,
    },
    ToolCompleted {
        tool_name: String,
        duration_ms: u64,
        outcome: ToolOutcome,
        #[serde(skip_serializing_if = "String::is_empty")]
        tool_call_id: String,
        #[serde(skip_serializing_if = "ToolCompletedSource::is_shell")]
        source: ToolCompletedSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        rewriting_hook: Option<String>,
    },
    PermissionRequested {
        tool_name: String,
    },
    PermissionResolved {
        tool_name: String,
        decision: PermissionDecision,
        wait_ms: u64,
    },
    TurnEnded {
        outcome: TurnOutcomeLabel,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancellation_category: Option<CancellationCategory>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancellation_context: Option<serde_json::Value>,
    },
    Interjected {
        source: InterjectionSource,
        image_count: u32,
        redirect_kind: RedirectKind,
    },
    YoloToggled {
        enabled: bool,
    },
    GoalAutoPaused {
        reason: GoalPauseReasonTelemetry,
    },
    TodoGateFired {
        fires: u32,
        pending: usize,
        in_progress: usize,
        reason: &'static str,
    },
    TodoGateExhausted {
        pending: usize,
    },
    LazinessClassifierFired {
        model_id: String,
        category: &'static str,
        confidence: f32,
    },
    LazinessNudgeFired {
        model_id: String,
        category: &'static str,
        nudges_remaining: u32,
    },
    LazinessClassifierAborted {
        reason: &'static str,
    },
    GoalClassifierFired {
        attempt: u32,
        max_runs: u32,
        model_id: String,
    },
    GoalClassifierVerdict {
        verdict: GoalClassifierVerdictTelemetry,
        attempt: u32,
        latency_ms: u64,
    },
    GoalClassifierFailOpen {
        reason: &'static str,
        attempt: u32,
        latency_ms: u64,
    },
    GoalClassifierFailClosed {
        reason: &'static str,
        attempt: u32,
    },
    GoalClassifierCapReached {
        attempt: u32,
    },
    GoalClassifierMidTurnDeferred {
        pending_depth: u32,
    },
    GoalClassifierDroppedAfterCap {
        attempts_seen: u32,
    },
    GoalClassifierPendingQueueCleared {
        dropped: u32,
    },
    GoalPlannerFired {
        attempt: u32,
        max_runs: u32,
        model_id: String,
    },
    GoalPlannerCompleted {
        attempt: u32,
        latency_ms: u64,
    },
    GoalPlannerFailClosed {
        reason: &'static str,
        attempt: u32,
        latency_ms: u64,
    },
    GoalStrategistFired {
        attempt: u32,
        consecutive_failures: u32,
        every: u32,
        model_id: String,
    },
    GoalStrategistCompleted {
        attempt: u32,
        consecutive_failures: u32,
        latency_ms: u64,
    },
    GoalStrategistFailed {
        reason: &'static str,
        attempt: u32,
        consecutive_failures: u32,
        latency_ms: u64,
    },
    GoalStrategistContractRestoreFailed {
        reason: &'static str,
        attempt: u32,
    },
    GoalSummarizerFired {
        attempt: u32,
        model_id: String,
    },
    GoalSummarizerCompleted {
        attempt: u32,
        latency_ms: u64,
    },
    GoalSummarizerFailOpen {
        reason: &'static str,
        attempt: u32,
        latency_ms: u64,
    },

    GoalRoleModelResolved {
        role: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        skeptic_idx: Option<u32>,
        model_id: String,
        agent_type: String,
        source: &'static str,
    },
    GoalRoleModelFailOpen {
        role: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        skeptic_idx: Option<u32>,
        reason: &'static str,
    },

    GoalVerifierSkepticVerdict {
        attempt: u32,
        skeptic_idx: u32,
        refuted: bool,
        confidence: &'static str,
        latency_ms: u64,
    },
    GoalVerifierAggregateVerdict {
        attempt: u32,
        refuted_count: u32,
        total: u32,
        achieved: bool,
    },
    GoalPrematureStopDetected {
        pattern: &'static str,
    },

    McpConfigResolved {
        servers: Vec<McpConfigServer>,
        disabled: Vec<String>,
    },
    McpManagedConfigResult {
        server_count: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "mcp_oauth_discovery_timeout")]
    McpOAuthDiscoveryTimeout {
        server_name: String,
        url: String,
    },
    #[serde(rename = "mcp_oauth_probe_resolved")]
    McpOAuthProbeResolved {
        server_name: String,
        verdict: String,
    },
    McpServerStarting {
        server_name: String,
        transport: String,
        target: String,
        timeout_sec: u64,
    },
    McpServerConnected {
        server_name: String,
        transport: String,
        tool_count: u32,
        duration_ms: u64,
        tools: Vec<String>,
    },
    McpServerFailed {
        server_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        transport: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        error_type: McpErrorCategory,
        error_message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_sec: Option<u64>,
    },
    McpToolRegistrationFailed {
        server_name: String,
        tool_name: String,
        error: String,
    },
    McpInitCompleted {
        total_servers: u32,
        succeeded: u32,
        failed: u32,
        auth_required: u32,
        total_tools: u32,
        duration_ms: u64,
        is_reinit: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        failed_servers: Vec<String>,
    },
    McpInitCancelled {
        reason: String,
    },
    McpToolCallStarted {
        server_name: String,
        tool_name: String,
        call_id: String,
        timeout_sec: u64,
    },
    McpToolCallCompleted {
        server_name: String,
        tool_name: String,
        call_id: String,
        duration_ms: u64,
        success: bool,
        is_timeout: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        reconnect_attempted: bool,
        auth_retry_attempted: bool,
    },
    McpTransportError {
        server_name: String,
        tool_name: String,
        error: String,
    },
    McpTransportDecodeError {
        server_name: String,
        error: String,
        sample: String,
    },
    McpTransportReconnect {
        server_name: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    McpAuthRetry {
        server_name: String,
        trigger: String,
        success: bool,
    },
    McpHealthCheck {
        server_name: String,
        healthy: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_state: Option<String>,
    },
    McpServerToggled {
        server_name: String,
        enabled: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCompletedSource {
    #[default]
    Shell,
    Workspace,
}

impl ToolCompletedSource {
    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Shell)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterjectionSource {
    Direct,
    Queue,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectKind {
    Interjection,
    CancelThenSend,
    QueuedAfterCancel,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpErrorCategory {
    SpawnFailed,
    Timeout,
    HandshakeFailed,
    AuthRequired,
    ClientError,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpConfigServer {
    pub name: String,
    pub transport: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalClassifierVerdictTelemetry {
    Achieved,
    NotAchieved,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReasonTelemetry {
    User,
    BackOff,
    NoProgress,
    Verification,
    Infra,
    Planner,
}

#[derive(Debug, Clone, Copy, Serialize, strum::IntoStaticStr)]
#[cfg_attr(test, derive(strum::EnumIter))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ToolOutcome {
    Success,
    Error,
    PermissionRejected,
    PermissionCancelled,
    Followup,
    HookDenied,
    InvalidTool,
    Cancelled,
}

impl ToolOutcome {
    pub fn ran_successfully(self) -> bool {
        matches!(self, ToolOutcome::Success)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    WaitingForModel,
    StreamingText,
    StreamingReasoning,
    ToolExecution,
    PermissionPrompt,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRelationship {
    Primary,
    #[allow(dead_code)]
    Subagent,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcomeLabel {
    Completed,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Cancelled,
    Followup,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCategory {
    HookDenied,
    PermissionRejected,
    PermissionCancelled,
    MidTurnAbort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_outcome_wire_labels_are_pinned() {
        use strum::IntoEnumIterator;

        for variant in ToolOutcome::iter() {
            let label = match variant {
                ToolOutcome::Success => "success",
                ToolOutcome::Error => "error",
                ToolOutcome::PermissionRejected => "permission_rejected",
                ToolOutcome::PermissionCancelled => "permission_cancelled",
                ToolOutcome::Followup => "followup",
                ToolOutcome::HookDenied => "hook_denied",
                ToolOutcome::InvalidTool => "invalid_tool",
                ToolOutcome::Cancelled => "cancelled",
            };
            assert_eq!(<&'static str>::from(variant), label, "{variant:?}");
            assert_eq!(serde_json::to_value(variant).unwrap(), label, "{variant:?}");
        }
    }

    #[test]
    fn cancellation_category_round_trips_every_variant() {
        for variant in [
            CancellationCategory::HookDenied,
            CancellationCategory::PermissionRejected,
            CancellationCategory::PermissionCancelled,
            CancellationCategory::MidTurnAbort,
        ] {
            let value = serde_json::to_value(variant).unwrap();
            let decoded: CancellationCategory = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, variant, "{variant:?} must round-trip");
        }
    }

    #[test]
    fn cancellation_category_serializes_snake_case() {
        for (variant, expected) in [
            (CancellationCategory::HookDenied, "\"hook_denied\""),
            (
                CancellationCategory::PermissionRejected,
                "\"permission_rejected\"",
            ),
            (
                CancellationCategory::PermissionCancelled,
                "\"permission_cancelled\"",
            ),
            (CancellationCategory::MidTurnAbort, "\"mid_turn_abort\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "{variant:?} must serialize to {expected}");
        }
    }

    #[test]
    fn tool_completed_source_omits_shell_writes_workspace() {
        let shell = serde_json::to_value(Event::ToolCompleted {
            tool_name: "bash".into(),
            duration_ms: 10,
            outcome: ToolOutcome::Success,
            tool_call_id: "c1".into(),
            source: ToolCompletedSource::Shell,
            rewriting_hook: None,
        })
        .unwrap();
        assert!(shell.get("source").is_none());

        let workspace = serde_json::to_value(Event::ToolCompleted {
            tool_name: "bash".into(),
            duration_ms: 10,
            outcome: ToolOutcome::Success,
            tool_call_id: "c1".into(),
            source: ToolCompletedSource::Workspace,
            rewriting_hook: None,
        })
        .unwrap();
        assert_eq!(
            workspace.get("source"),
            Some(&serde_json::json!("workspace"))
        );
    }

    #[test]
    fn interjected_event_serializes_tag_source_and_count() {
        let ev = Event::Interjected {
            source: InterjectionSource::Direct,
            image_count: 2,
            redirect_kind: RedirectKind::Interjection,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type"), Some(&serde_json::json!("interjected")));
        assert_eq!(v.get("source"), Some(&serde_json::json!("direct")));
        assert_eq!(v.get("image_count"), Some(&serde_json::json!(2)));
        assert_eq!(
            v.get("redirect_kind"),
            Some(&serde_json::json!("interjection"))
        );

        let queue = serde_json::to_value(Event::Interjected {
            source: InterjectionSource::Queue,
            image_count: 0,
            redirect_kind: RedirectKind::Interjection,
        })
        .unwrap();
        assert_eq!(queue.get("source"), Some(&serde_json::json!("queue")));
        assert_eq!(queue.get("image_count"), Some(&serde_json::json!(0)));
        assert_eq!(
            queue.get("redirect_kind"),
            Some(&serde_json::json!("interjection"))
        );
    }

    #[test]
    fn redirect_kind_serializes_snake_case() {
        for (variant, expected) in [
            (RedirectKind::Interjection, "\"interjection\""),
            (RedirectKind::CancelThenSend, "\"cancel_then_send\""),
            (RedirectKind::QueuedAfterCancel, "\"queued_after_cancel\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "{variant:?} must serialize to {expected}");
        }
    }

    #[test]
    fn turn_started_redirect_kind_present_when_set_omitted_when_none() {
        let with_kind = serde_json::to_value(Event::TurnStarted {
            session_id: "s".into(),
            turn_number: 2,
            model_id: "grok-4".into(),
            yolo_mode: false,
            conversation_message_count: 3,
            session_relationship: SessionRelationship::Primary,
            schema_version: EVENT_SCHEMA_VERSION.into(),
            redirect_kind: Some(RedirectKind::QueuedAfterCancel),
        })
        .unwrap();
        assert_eq!(
            with_kind.get("type"),
            Some(&serde_json::json!("turn_started"))
        );
        assert_eq!(
            with_kind.get("redirect_kind"),
            Some(&serde_json::json!("queued_after_cancel"))
        );

        let normal = serde_json::to_value(Event::TurnStarted {
            session_id: "s".into(),
            turn_number: 1,
            model_id: "grok-4".into(),
            yolo_mode: false,
            conversation_message_count: 0,
            session_relationship: SessionRelationship::Primary,
            schema_version: EVENT_SCHEMA_VERSION.into(),
            redirect_kind: None,
        })
        .unwrap();
        assert!(
            normal.get("redirect_kind").is_none(),
            "redirect_kind must be omitted on a normal turn, got {normal}"
        );
    }

    #[test]
    fn goal_pause_reason_telemetry_serializes_snake_case() {
        for (variant, expected) in [
            (GoalPauseReasonTelemetry::User, "\"user\""),
            (GoalPauseReasonTelemetry::BackOff, "\"back_off\""),
            (GoalPauseReasonTelemetry::NoProgress, "\"no_progress\""),
            (GoalPauseReasonTelemetry::Verification, "\"verification\""),
            (GoalPauseReasonTelemetry::Infra, "\"infra\""),
            (GoalPauseReasonTelemetry::Planner, "\"planner\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "{variant:?} must serialize to {expected}");
        }
    }

    #[test]
    fn goal_strategist_fired_serializes_cadence_field() {
        let ev = Event::GoalStrategistFired {
            attempt: 2,
            consecutive_failures: 6,
            every: 3,
            model_id: "grok-4".to_string(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v.get("type"),
            Some(&serde_json::json!("goal_strategist_fired"))
        );
        assert_eq!(v.get("attempt"), Some(&serde_json::json!(2)));
        assert_eq!(v.get("consecutive_failures"), Some(&serde_json::json!(6)));
        assert_eq!(v.get("every"), Some(&serde_json::json!(3)));
        assert_eq!(v.get("model_id"), Some(&serde_json::json!("grok-4")));
    }

    #[test]
    fn goal_summarizer_events_serialize_tag_and_fields() {
        let fired = Event::GoalSummarizerFired {
            attempt: 2,
            model_id: "grok-4".to_string(),
        };
        let v = serde_json::to_value(&fired).unwrap();
        assert_eq!(
            v.get("type"),
            Some(&serde_json::json!("goal_summarizer_fired"))
        );
        assert_eq!(v.get("attempt"), Some(&serde_json::json!(2)));
        assert_eq!(v.get("model_id"), Some(&serde_json::json!("grok-4")));

        let completed = Event::GoalSummarizerCompleted {
            attempt: 2,
            latency_ms: 42,
        };
        let v = serde_json::to_value(&completed).unwrap();
        assert_eq!(
            v.get("type"),
            Some(&serde_json::json!("goal_summarizer_completed"))
        );
        assert_eq!(v.get("attempt"), Some(&serde_json::json!(2)));
        assert_eq!(v.get("latency_ms"), Some(&serde_json::json!(42)));

        let failed = Event::GoalSummarizerFailOpen {
            reason: "transport",
            attempt: 2,
            latency_ms: 7,
        };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            v.get("type"),
            Some(&serde_json::json!("goal_summarizer_fail_open"))
        );
        assert_eq!(v.get("reason"), Some(&serde_json::json!("transport")));
        assert_eq!(v.get("attempt"), Some(&serde_json::json!(2)));
        assert_eq!(v.get("latency_ms"), Some(&serde_json::json!(7)));
    }

    #[test]
    fn goal_role_model_resolved_serializes_tag_and_fields() {
        let ev = Event::GoalRoleModelResolved {
            role: "skeptic",
            skeptic_idx: Some(2),
            model_id: "grok-4".to_string(),
            agent_type: "general-purpose".to_string(),
            source: "remote",
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v.get("type"),
            Some(&serde_json::json!("goal_role_model_resolved"))
        );
        assert_eq!(v.get("role"), Some(&serde_json::json!("skeptic")));
        assert_eq!(v.get("skeptic_idx"), Some(&serde_json::json!(2)));
        assert_eq!(v.get("model_id"), Some(&serde_json::json!("grok-4")));
        assert_eq!(
            v.get("agent_type"),
            Some(&serde_json::json!("general-purpose"))
        );
        assert_eq!(v.get("source"), Some(&serde_json::json!("remote")));
    }

    #[test]
    fn goal_role_model_resolved_omits_skeptic_idx_when_none() {
        let ev = Event::GoalRoleModelResolved {
            role: "planner",
            skeptic_idx: None,
            model_id: "grok-4".to_string(),
            agent_type: "general-purpose".to_string(),
            source: "remote",
        };
        let obj = serde_json::to_value(&ev).unwrap();
        assert!(
            obj.get("skeptic_idx").is_none(),
            "skeptic_idx must be omitted when None, got {obj}"
        );
        assert_eq!(obj.get("role"), Some(&serde_json::json!("planner")));
    }

    #[test]
    fn goal_role_model_fail_open_omits_skeptic_idx_when_none() {
        let ev = Event::GoalRoleModelFailOpen {
            role: "strategist",
            skeptic_idx: None,
            reason: "model_unauthorized",
        };
        let obj = serde_json::to_value(&ev).unwrap();
        assert!(
            obj.get("skeptic_idx").is_none(),
            "skeptic_idx must be omitted when None, got {obj}"
        );
        assert_eq!(
            obj.get("type"),
            Some(&serde_json::json!("goal_role_model_fail_open"))
        );
        assert_eq!(obj.get("role"), Some(&serde_json::json!("strategist")));
        assert_eq!(
            obj.get("reason"),
            Some(&serde_json::json!("model_unauthorized"))
        );
    }
}
