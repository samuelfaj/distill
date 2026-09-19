use serde::Serialize;

use super::{AccessKind, PermissionOutcome};
use crate::enums::PermissionMode;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPromptOutcome {
    Allow,
    Reject,
    Cancel,
    Followup,
    Error,
}

impl PermissionPromptOutcome {
    pub const ALL: &'static [Self] = &[
        Self::Allow,
        Self::Reject,
        Self::Cancel,
        Self::Followup,
        Self::Error,
    ];
}

impl TryFrom<&str> for PermissionPromptOutcome {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        match s {
            "allow_once"
            | "allow_always"
            | "allow_always_bash"
            | "allow_always_bash_glob"
            | "allow_always_domain"
            | "allow_always_mcp_tool"
            | "allow_always_mcp_server"
            | "allow_edits_for_session" => Ok(Self::Allow),
            "reject_once"
            | "reject_always_bash"
            | "reject_always_mcp_tool"
            | "reject_always_domain" => Ok(Self::Reject),
            "cancelled" => Ok(Self::Cancel),
            "followup" => Ok(Self::Followup),
            "error" => Ok(Self::Error),
            _ => Err(()),
        }
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPromptOutcomeDetail {
    AllowOnce,
    AllowAlways,
    AllowEditsForSession,
    AllowAlwaysBash,
    AllowAlwaysBashGlob,
    AllowAlwaysDomain,
    AllowAlwaysMcpTool,
    AllowAlwaysMcpServer,
    RejectOnce,
    RejectAlwaysBash,
    RejectAlwaysMcpTool,
    RejectAlwaysDomain,
    Cancelled,
    Followup,
    Error,
}

impl PermissionPromptOutcomeDetail {
    pub const ALL: &'static [Self] = &[
        Self::AllowOnce,
        Self::AllowAlways,
        Self::AllowEditsForSession,
        Self::AllowAlwaysBash,
        Self::AllowAlwaysBashGlob,
        Self::AllowAlwaysDomain,
        Self::AllowAlwaysMcpTool,
        Self::AllowAlwaysMcpServer,
        Self::RejectOnce,
        Self::RejectAlwaysBash,
        Self::RejectAlwaysMcpTool,
        Self::RejectAlwaysDomain,
        Self::Cancelled,
        Self::Followup,
        Self::Error,
    ];
}

impl TryFrom<&str> for PermissionPromptOutcomeDetail {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "allow_once" => Self::AllowOnce,
            "allow_always" => Self::AllowAlways,
            "allow_edits_for_session" => Self::AllowEditsForSession,
            "allow_always_bash" => Self::AllowAlwaysBash,
            "allow_always_bash_glob" => Self::AllowAlwaysBashGlob,
            "allow_always_domain" => Self::AllowAlwaysDomain,
            "allow_always_mcp_tool" => Self::AllowAlwaysMcpTool,
            "allow_always_mcp_server" => Self::AllowAlwaysMcpServer,
            "reject_once" => Self::RejectOnce,
            "reject_always_bash" => Self::RejectAlwaysBash,
            "reject_always_mcp_tool" => Self::RejectAlwaysMcpTool,
            "reject_always_domain" => Self::RejectAlwaysDomain,
            "cancelled" => Self::Cancelled,
            "followup" => Self::Followup,
            "error" => Self::Error,
            _ => return Err(()),
        })
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionReason {
    Yolo,
    PolicyAllow,
    PolicyDeny,
    PolicyAsk,
    BashCommandGateAsk,
    ShellFileGateAsk,
    AutoFastPath,
    AutoClassifierAllow,
    AutoClassifierDeny,
    AutoClassifierTimeout,
    AutoClassifierUnavailable,
    AutoDenialLimit,
    SandboxAuto,
    PersistedGrant,
    SessionGrant,
    StaticAllowlist,
    SafeCommand,
    SessionDeny,
    PromptDeny,
    PromptAllow,
    NeedsUser,
    BashRequestFloor,
    OpaqueShell,
    HookAsk,
    RequesterGone,
}

impl PermissionDecisionReason {
    pub const ALL: &'static [Self] = &[
        Self::Yolo,
        Self::PolicyAllow,
        Self::PolicyDeny,
        Self::PolicyAsk,
        Self::BashCommandGateAsk,
        Self::ShellFileGateAsk,
        Self::AutoFastPath,
        Self::AutoClassifierAllow,
        Self::AutoClassifierDeny,
        Self::AutoClassifierTimeout,
        Self::AutoClassifierUnavailable,
        Self::AutoDenialLimit,
        Self::SandboxAuto,
        Self::PersistedGrant,
        Self::SessionGrant,
        Self::StaticAllowlist,
        Self::SafeCommand,
        Self::SessionDeny,
        Self::PromptDeny,
        Self::PromptAllow,
        Self::NeedsUser,
        Self::BashRequestFloor,
        Self::OpaqueShell,
        Self::HookAsk,
        Self::RequesterGone,
    ];
}

impl TryFrom<&str> for PermissionDecisionReason {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "yolo" => Self::Yolo,
            "policy_allow" => Self::PolicyAllow,
            "policy_deny" => Self::PolicyDeny,
            "policy_ask" => Self::PolicyAsk,
            "bash_command_gate_ask" => Self::BashCommandGateAsk,
            "shell_file_gate_ask" => Self::ShellFileGateAsk,
            "auto_fast_path" => Self::AutoFastPath,
            "auto_classifier_allow" => Self::AutoClassifierAllow,
            "auto_classifier_deny" => Self::AutoClassifierDeny,
            "auto_classifier_timeout" => Self::AutoClassifierTimeout,
            "auto_classifier_unavailable" => Self::AutoClassifierUnavailable,
            "auto_denial_limit" => Self::AutoDenialLimit,
            "sandbox_auto" => Self::SandboxAuto,
            "persisted_grant" => Self::PersistedGrant,
            "session_grant" => Self::SessionGrant,
            "static_allowlist" => Self::StaticAllowlist,
            "safe_command" => Self::SafeCommand,
            "session_deny" => Self::SessionDeny,
            "prompt_deny" => Self::PromptDeny,
            "prompt_allow" => Self::PromptAllow,
            "needs_user" => Self::NeedsUser,
            "bash_request_floor" => Self::BashRequestFloor,
            "opaque_shell" => Self::OpaqueShell,
            "hook_ask" => Self::HookAsk,
            "requester_gone" => Self::RequesterGone,
            _ => return Err(()),
        })
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionClassifierSource {
    Llm,
    Heuristic,
    Timeout,
    TransportError,
    FastPath,
    NotWired,
}

impl PermissionClassifierSource {
    pub const ALL: &'static [Self] = &[
        Self::Llm,
        Self::Heuristic,
        Self::Timeout,
        Self::TransportError,
        Self::FastPath,
        Self::NotWired,
    ];
}

impl TryFrom<&str> for PermissionClassifierSource {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "llm" => Self::Llm,
            "heuristic" => Self::Heuristic,
            "timeout" => Self::Timeout,
            "transport_error" => Self::TransportError,
            "fast_path" => Self::FastPath,
            "not_wired" => Self::NotWired,
            _ => return Err(()),
        })
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionClassifierVerdict {
    Allow,
    Block,
    Unavailable,
}

impl PermissionClassifierVerdict {
    pub const ALL: &'static [Self] = &[Self::Allow, Self::Block, Self::Unavailable];
}

impl TryFrom<&str> for PermissionClassifierVerdict {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "allow" => Self::Allow,
            "block" => Self::Block,
            "unavailable" => Self::Unavailable,
            _ => return Err(()),
        })
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSecurityFinding {
    FailClosedPolicy,
    UnparseableShell,
    OpaqueShell,
    ExecOrAmbientGit,
    EnvInjection,
    UnvettedEnv,
    FileWrite,
    DangerousCommand,
    SpecialExecSurface,
}

impl PermissionSecurityFinding {
    pub const ALL: &'static [Self] = &[
        Self::FailClosedPolicy,
        Self::UnparseableShell,
        Self::OpaqueShell,
        Self::ExecOrAmbientGit,
        Self::EnvInjection,
        Self::UnvettedEnv,
        Self::FileWrite,
        Self::DangerousCommand,
        Self::SpecialExecSurface,
    ];
}

impl TryFrom<&str> for PermissionSecurityFinding {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "fail_closed_policy" => Self::FailClosedPolicy,
            "unparseable_shell" => Self::UnparseableShell,
            "opaque_shell" => Self::OpaqueShell,
            "exec_or_ambient_git" => Self::ExecOrAmbientGit,
            "env_injection" => Self::EnvInjection,
            "unvetted_env" => Self::UnvettedEnv,
            "file_write" => Self::FileWrite,
            "dangerous_command" => Self::DangerousCommand,
            "special_exec_surface" => Self::SpecialExecSurface,
            _ => return Err(()),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoDenialKpi {
    Alignment,
    Disagreement,
}

pub fn auto_denial_kpi(p: &PermissionDecisionPayload) -> Option<AutoDenialKpi> {
    if p.permission_mode != PermissionMode::Auto
        || p.decision_reason != Some(PermissionDecisionReason::AutoDenialLimit)
        || p.classifier_verdict != Some(PermissionClassifierVerdict::Block)
    {
        return None;
    }
    match p.prompt_outcome {
        Some(PermissionPromptOutcome::Allow) => Some(AutoDenialKpi::Disagreement),
        Some(PermissionPromptOutcome::Reject) => Some(AutoDenialKpi::Alignment),
        _ => None,
    }
}

#[derive(Serialize)]
pub struct PermissionPrompted {
    pub tool_name: String,
    pub access_kind: AccessKind,
    pub permission_mode: PermissionMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
}

#[derive(Serialize)]
pub struct PermissionDecisionPayload {
    pub tool_name: String,
    pub access_kind: AccessKind,
    pub decision: PermissionOutcome,
    pub wait_ms: u64,
    pub permission_mode: PermissionMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager_prompt_attempted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_outcome: Option<PermissionPromptOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_outcome_detail: Option<PermissionPromptOutcomeDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember_tool_approvals: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<PermissionDecisionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_source: Option<PermissionClassifierSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_verdict: Option<PermissionClassifierVerdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_findings: Option<Vec<PermissionSecurityFinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_denials_consecutive: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_denials_total: Option<u32>,
}

/// External-stream-only tool args for a permission decision. Never serialized
/// onto Mixpanel; deny never reaches [`super::ToolCallCompleted`].
#[derive(Debug, Clone, Default)]
pub struct ExternalToolInput {
    pub parameters: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
}

/// Product `permission_decision` event: Mixpanel sees only [`PermissionDecisionPayload`];
/// the sidecar is passed into the external mapper via [`PermissionDecisionRecord::tool_input`].
pub struct PermissionDecisionRecord {
    pub payload: PermissionDecisionPayload,
    pub tool_input: ExternalToolInput,
}

impl serde::Serialize for PermissionDecisionRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.payload.serialize(serializer)
    }
}

impl From<PermissionDecisionPayload> for PermissionDecisionRecord {
    fn from(payload: PermissionDecisionPayload) -> Self {
        Self {
            payload,
            tool_input: ExternalToolInput::default(),
        }
    }
}

#[cfg(test)]
mod permission_analytics_tests {
    use super::*;

    #[test]
    fn decision_reason_enum_round_trips_every_variant() {
        for &variant in PermissionDecisionReason::ALL {
            let wire = serde_json::to_value(variant).unwrap();
            let s = wire.as_str().expect("reason serializes to a string");
            assert_eq!(
                PermissionDecisionReason::try_from(s),
                Ok(variant),
                "reason {s} must round-trip"
            );
        }
        assert!(PermissionDecisionReason::try_from("not_a_reason").is_err());
    }

    #[test]
    fn prompt_outcome_normalizes_representative_outcomes() {
        use PermissionPromptOutcome as O;
        assert_eq!(
            PermissionPromptOutcome::try_from("allow_once"),
            Ok(O::Allow)
        );
        assert_eq!(
            PermissionPromptOutcome::try_from("reject_once"),
            Ok(O::Reject)
        );
        assert_eq!(
            PermissionPromptOutcome::try_from("reject_always_mcp_tool"),
            Ok(O::Reject)
        );
        assert_eq!(
            PermissionPromptOutcome::try_from("reject_always_domain"),
            Ok(O::Reject)
        );
        assert_eq!(
            PermissionPromptOutcome::try_from("cancelled"),
            Ok(O::Cancel)
        );
        assert_eq!(
            PermissionPromptOutcome::try_from("followup"),
            Ok(O::Followup)
        );
        assert_eq!(PermissionPromptOutcome::try_from("error"), Ok(O::Error));
        assert!(PermissionPromptOutcome::try_from("mystery").is_err());
    }

    #[test]
    fn prompt_outcome_detail_round_trips_every_variant() {
        for &variant in PermissionPromptOutcomeDetail::ALL {
            let wire = serde_json::to_value(variant).unwrap();
            let s = wire.as_str().expect("detail serializes to a string");
            assert_eq!(
                PermissionPromptOutcomeDetail::try_from(s),
                Ok(variant),
                "detail {s} must round-trip"
            );
        }
        assert!(PermissionPromptOutcomeDetail::try_from("mystery").is_err());
    }

    #[test]
    fn classifier_source_verdict_finding_enums_round_trip() {
        for &v in PermissionClassifierSource::ALL {
            let s = serde_json::to_value(v).unwrap();
            let s = s.as_str().unwrap();
            assert_eq!(PermissionClassifierSource::try_from(s), Ok(v), "{s}");
        }
        assert!(PermissionClassifierSource::try_from("nope").is_err());
        for &v in PermissionClassifierVerdict::ALL {
            let s = serde_json::to_value(v).unwrap();
            let s = s.as_str().unwrap();
            assert_eq!(PermissionClassifierVerdict::try_from(s), Ok(v), "{s}");
        }
        assert!(PermissionClassifierVerdict::try_from("nope").is_err());
        for &f in PermissionSecurityFinding::ALL {
            let s = serde_json::to_value(f).unwrap();
            let s = s.as_str().unwrap();
            assert_eq!(PermissionSecurityFinding::try_from(s), Ok(f), "{s}");
        }
        assert!(PermissionSecurityFinding::try_from("made_up").is_err());
    }

    fn kpi_payload(
        mode: PermissionMode,
        reason: Option<PermissionDecisionReason>,
        verdict: Option<PermissionClassifierVerdict>,
        outcome: Option<PermissionPromptOutcome>,
    ) -> PermissionDecisionPayload {
        PermissionDecisionPayload {
            tool_name: "run_terminal_cmd".into(),
            access_kind: AccessKind::Bash,
            decision: PermissionOutcome::Deny,
            wait_ms: 0,
            permission_mode: mode,
            source: None,
            subagent_session_id: None,
            subagent_type: None,
            manager_prompt_attempted: Some(true),
            prompt_outcome: outcome,
            prompt_outcome_detail: None,
            remember_tool_approvals: Some(true),
            decision_reason: reason,
            classifier_source: Some(PermissionClassifierSource::Llm),
            classifier_verdict: verdict,
            security_findings: Some(vec![PermissionSecurityFinding::DangerousCommand]),
            classifier_latency_ms: Some(10),
            auto_denials_consecutive: Some(3),
            auto_denials_total: Some(3),
        }
    }

    #[test]
    fn auto_denial_kpi_cohort_and_sides() {
        use AutoDenialKpi::*;
        use PermissionClassifierVerdict as V;
        use PermissionDecisionReason as R;
        use PermissionPromptOutcome as O;
        assert_eq!(
            auto_denial_kpi(&kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoDenialLimit),
                Some(V::Block),
                Some(O::Reject)
            )),
            Some(Alignment)
        );
        assert_eq!(
            auto_denial_kpi(&kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoDenialLimit),
                Some(V::Block),
                Some(O::Allow)
            )),
            Some(Disagreement)
        );
        for excluded in [
            kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoDenialLimit),
                Some(V::Block),
                Some(O::Cancel),
            ),
            kpi_payload(
                PermissionMode::Ask,
                Some(R::AutoDenialLimit),
                Some(V::Block),
                Some(O::Reject),
            ),
            kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoClassifierDeny),
                Some(V::Block),
                Some(O::Reject),
            ),
            kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoDenialLimit),
                Some(V::Allow),
                Some(O::Reject),
            ),
            kpi_payload(
                PermissionMode::Auto,
                Some(R::AutoDenialLimit),
                Some(V::Block),
                None,
            ),
        ] {
            assert_eq!(auto_denial_kpi(&excluded), None);
        }
    }
}
