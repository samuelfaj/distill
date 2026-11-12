use std::collections::BTreeMap;

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

    /// UltraCode v2: one independent review run over a turn's patch.
    UltracodeTurnReview {
        review: u32,
        skeptics: u32,
        verdict: &'static str,
        latency_ms: u64,
    },
    /// UltraCode v2: how a reviewed turn ended.
    UltracodeTurnStatus {
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        reviews: u32,
        rework_rounds: u32,
        panel_used: bool,
        gate_ms: u64,
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
    /// One recorded Jev decision. Content-free by contract: ids, numbers and
    /// the composed label only, never the state, question text or reason.
    JevDecision {
        /// Per session and process; shared with `JevNoAnswer`.
        seq: u64,
        lever: &'static str,
        /// 12 hex over the question ids and instruction texts; absent when
        /// the answers did not come through `ask_item`.
        #[serde(skip_serializing_if = "Option::is_none")]
        question_hash: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        answers: BTreeMap<String, JevAnswerSummary>,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        /// Absent when the provenance of the answers is unknown.
        #[serde(skip_serializing_if = "Option::is_none")]
        memo: Option<bool>,
        latency_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// One Jev ask that returned no answer.
    JevNoAnswer {
        seq: u64,
        lever: &'static str,
        kind: JevNoAnswerKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        question_hash: Option<String>,
        latency_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    // Outcome facts a decision is later joined with. Content-free like the
    // decision events: ids, numbers, booleans and 12-hex hashes only.
    /// A build, test or lint the harness recorded as a check.
    CheckRecorded {
        outcome: CheckRecordedOutcome,
        /// 12 hex over the full command.
        command_hash: String,
        /// Whether the check ran on the working tree as it is now; absent
        /// when no working-tree marker was taken.
        #[serde(skip_serializing_if = "Option::is_none")]
        on_current_tree: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// The turn-end final check appended its reminder.
    FinalCheckReminder {
        /// Reminders sent in this session, this one included.
        seq: u64,
        /// The labels recorded for C1 and C3, as `c1:<label>,c3:<label>`.
        label: String,
        /// 12 hex over the working-tree marker; absent when none was taken.
        #[serde(skip_serializing_if = "Option::is_none")]
        worktree_hash: Option<String>,
    },
    /// What the request that received a final-check reminder did after it.
    FinalCheckFollowup {
        reminder_seq: u64,
        /// Rounds with at least one tool result after the reminder.
        tool_rounds: u64,
        /// An edit tool ran after the reminder.
        edited: bool,
        /// A commit, push or PR command ran after the reminder.
        delivered: bool,
        /// The first reply to the reminder was only the confirmation token.
        fixed_token: bool,
        /// The working-tree marker differs from the reminder's; absent when
        /// either marker is missing.
        #[serde(skip_serializing_if = "Option::is_none")]
        worktree_changed: Option<bool>,
    },
    /// The effective execution mode changed.
    ModeSwitched {
        from: &'static str,
        to: &'static str,
        trigger: ModeSwitchTrigger,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// The next call's effort floor actually rose.
    EffortFloorRaised {
        level: String,
        trigger: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// The skill ranking surfaced a skill to the model.
    SkillSuggested {
        /// 12 hex over the skill name.
        name_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// The model read a catalog skill file or invoked a skill.
    SkillRead {
        name_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRecordedOutcome {
    Passed,
    Failed,
    Unknown,
}

/// What changed the effective execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeSwitchTrigger {
    /// Auto's pick when a request starts.
    RequestStart,
    /// Auto's review after a tool round.
    Review,
    /// The user fixed a mode.
    User,
    /// The escalation ratchet moved Auto one step up after a harness signal.
    Ratchet,
}

/// One Jev answer as the model returned it, without any text: the picked
/// option id and numbers. A distribution over more than eight options keeps
/// its five most likely entries and counts the rest in `omitted`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JevAnswerSummary {
    Choice {
        choice: String,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        probabilities: BTreeMap<String, f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        #[serde(skip_serializing_if = "is_zero")]
        omitted: usize,
    },
    Score {
        score: f64,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        probabilities: BTreeMap<String, f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        #[serde(skip_serializing_if = "is_zero")]
        omitted: usize,
    },
    Noul {
        noul: f64,
    },
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum JevNoAnswerKind {
    Error,
    Timeout,
    NoCredential,
    LeverOff,
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

    /// The analyzer reads these field names; a rename breaks it silently.
    #[test]
    fn jev_events_serialize_their_wire_shape() {
        let decision = serde_json::to_value(Event::JevDecision {
            seq: 3,
            lever: "b8_execution_mode",
            question_hash: Some("0123456789ab".into()),
            model_id: Some("jev-model".into()),
            answers: BTreeMap::from([
                (
                    "mode".to_owned(),
                    JevAnswerSummary::Choice {
                        choice: "normal".into(),
                        probabilities: BTreeMap::from([("normal".to_owned(), 0.75)]),
                        confidence: Some(0.5),
                        omitted: 0,
                    },
                ),
                ("risk".to_owned(), JevAnswerSummary::Noul { noul: 0.25 }),
            ]),
            label: "normal".into(),
            confidence: None,
            memo: Some(false),
            latency_ms: 40,
            round: Some(2),
        })
        .unwrap();
        assert_eq!(
            decision,
            serde_json::json!({
                "type": "jev_decision",
                "seq": 3,
                "lever": "b8_execution_mode",
                "question_hash": "0123456789ab",
                "model_id": "jev-model",
                "answers": {
                    "mode": {"type": "choice", "choice": "normal", "probabilities": {"normal": 0.75}, "confidence": 0.5},
                    "risk": {"type": "noul", "noul": 0.25}
                },
                "label": "normal",
                "memo": false,
                "latency_ms": 40,
                "round": 2
            })
        );

        let missing = serde_json::to_value(Event::JevNoAnswer {
            seq: 4,
            lever: "c4_diff_risk",
            kind: JevNoAnswerKind::NoCredential,
            question_hash: None,
            latency_ms: 0,
            round: None,
        })
        .unwrap();
        assert_eq!(
            missing,
            serde_json::json!({
                "type": "jev_no_answer",
                "seq": 4,
                "lever": "c4_diff_risk",
                "kind": "no_credential",
                "latency_ms": 0
            })
        );
    }

    /// The decisions report joins on these names; a rename breaks it silently.
    #[test]
    fn outcome_facts_serialize_their_wire_shape() {
        let shape = |event| serde_json::to_value(event).unwrap();
        assert_eq!(
            shape(Event::CheckRecorded {
                outcome: CheckRecordedOutcome::Failed,
                command_hash: "0123456789ab".into(),
                on_current_tree: Some(true),
                round: Some(3),
            }),
            serde_json::json!({"type": "check_recorded", "outcome": "failed",
                "command_hash": "0123456789ab", "on_current_tree": true, "round": 3})
        );
        assert_eq!(
            shape(Event::CheckRecorded {
                outcome: CheckRecordedOutcome::Unknown,
                command_hash: "0123456789ab".into(),
                on_current_tree: None,
                round: None,
            }),
            serde_json::json!({"type": "check_recorded", "outcome": "unknown",
                "command_hash": "0123456789ab"})
        );
        assert_eq!(
            shape(Event::FinalCheckReminder {
                seq: 1,
                label: "c1:incomplete,c3:complete".into(),
                worktree_hash: Some("ba9876543210".into()),
            }),
            serde_json::json!({"type": "final_check_reminder", "seq": 1,
                "label": "c1:incomplete,c3:complete", "worktree_hash": "ba9876543210"})
        );
        assert_eq!(
            shape(Event::FinalCheckFollowup {
                reminder_seq: 1,
                tool_rounds: 2,
                edited: true,
                delivered: false,
                fixed_token: false,
                worktree_changed: Some(true),
            }),
            serde_json::json!({"type": "final_check_followup", "reminder_seq": 1,
                "tool_rounds": 2, "edited": true, "delivered": false, "fixed_token": false,
                "worktree_changed": true})
        );
        assert_eq!(
            shape(Event::ModeSwitched {
                from: "normal",
                to: "ultrasave",
                trigger: ModeSwitchTrigger::RequestStart,
                round: Some(0),
            }),
            serde_json::json!({"type": "mode_switched", "from": "normal", "to": "ultrasave",
                "trigger": "request_start", "round": 0})
        );
        assert_eq!(
            shape(Event::EffortFloorRaised {
                level: "high".into(),
                trigger: "unspecified",
                round: Some(4),
            }),
            serde_json::json!({"type": "effort_floor_raised", "level": "high",
                "trigger": "unspecified", "round": 4})
        );
        assert_eq!(
            shape(Event::SkillSuggested {
                name_hash: "0123456789ab".into(),
                round: None,
            }),
            serde_json::json!({"type": "skill_suggested", "name_hash": "0123456789ab"})
        );
        assert_eq!(
            shape(Event::SkillRead {
                name_hash: "0123456789ab".into(),
                round: Some(2),
            }),
            serde_json::json!({"type": "skill_read", "name_hash": "0123456789ab", "round": 2})
        );
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
