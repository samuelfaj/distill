// Modified for Distill by Samuel Fajreldines, 2026.
use distill_telemetry::enums::PermissionMode;
use distill_telemetry::events::{
    self, PermissionClassifierSource, PermissionClassifierVerdict, PermissionDecisionPayload,
    PermissionDecisionReason, PermissionOutcome, PermissionPromptOutcome,
    PermissionPromptOutcomeDetail, PermissionSecurityFinding,
};
use distill_workspace::permission::{
    AUTO_DENY_CONSECUTIVE_LIMIT, AUTO_DENY_TOTAL_LIMIT, Decision, PermissionEvent,
};

/// Content-free analytics fields projected from the manager's authoritative [`PermissionEvent`] onto `PermissionDecisionPayload`.
/// Every field is `None` when the manager returned no event.
/// The shell then omits manager-only analytics rather than fabricating them from its pre-await snapshot.
#[derive(Default)]
pub(crate) struct ManagerPermissionAnalytics {
    pub manager_prompt_attempted: Option<bool>,
    pub prompt_outcome: Option<PermissionPromptOutcome>,
    pub prompt_outcome_detail: Option<PermissionPromptOutcomeDetail>,
    pub remember_tool_approvals: Option<bool>,
    pub decision_reason: Option<PermissionDecisionReason>,
    pub classifier_source: Option<PermissionClassifierSource>,
    pub classifier_verdict: Option<PermissionClassifierVerdict>,
    pub security_findings: Option<Vec<PermissionSecurityFinding>>,
    pub classifier_latency_ms: Option<u64>,
    pub auto_denials_consecutive: Option<u32>,
    pub auto_denials_total: Option<u32>,
}

/// Map a manager string through a closed telemetry enum.
/// Unknown values are dropped with a fixed local diagnostic category and never exported.
fn try_enum<T>(field: &'static str, raw: &str) -> Option<T>
where
    for<'a> T: TryFrom<&'a str>,
{
    match T::try_from(raw) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::debug!(
                target: "permission_analytics",
                field,
                "dropping unknown manager permission string (omitted, never exported)"
            );
            None
        }
    }
}

/// `Some([])` is preserved only when the source vector was truly empty (classifier route ran, empty assessment).
/// On the FIRST unknown token the ENTIRE field is omitted (`None`) with a fixed diagnostic.
/// An older shell thus never exports a misleadingly "clean" or partial subset of a newer manager's findings (truthful missingness for eval curation).
fn convert_findings(tokens: &[String]) -> Option<Vec<PermissionSecurityFinding>> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        match PermissionSecurityFinding::try_from(token.as_str()) {
            Ok(finding) => out.push(finding),
            Err(_) => {
                tracing::debug!(
                    target: "permission_analytics",
                    field = "security_findings",
                    "unknown finding token; omitting the entire findings field (truthful missingness)"
                );
                return None;
            }
        }
    }
    Some(out)
}

/// Pure projection of the manager event onto the additive content-free analytics fields.
/// No I/O, no shell state; unknown strings are omitted.
pub(crate) fn manager_permission_analytics(
    event: Option<&PermissionEvent>,
) -> ManagerPermissionAnalytics {
    let Some(ev) = event else {
        return ManagerPermissionAnalytics::default();
    };
    ManagerPermissionAnalytics {
        manager_prompt_attempted: Some(ev.user_prompted),
        prompt_outcome: ev
            .prompt_outcome
            .as_deref()
            .and_then(|s| try_enum("prompt_outcome", s)),
        prompt_outcome_detail: ev
            .prompt_outcome
            .as_deref()
            .and_then(|s| try_enum("prompt_outcome_detail", s)),
        remember_tool_approvals: ev.remember_tool_approvals,
        decision_reason: ev
            .decision_reason
            .as_deref()
            .and_then(|s| try_enum("decision_reason", s)),
        classifier_source: ev
            .classifier_source
            .as_deref()
            .and_then(|s| try_enum("classifier_source", s)),
        classifier_verdict: ev
            .classifier_verdict
            .as_deref()
            .and_then(|s| try_enum("classifier_verdict", s)),
        // None means never classified; Some([]) means the classifier route ran with an empty assessment
        // Any unknown token omits the whole field (see convert_findings)
        security_findings: ev.security_findings.as_deref().and_then(convert_findings),
        classifier_latency_ms: ev.classifier_latency_ms,
        // Clamp to the manager's own budget (defense-in-depth) using the owner's constants, so a budget change cannot silently desync this clamp
        auto_denials_consecutive: ev
            .auto_denials_consecutive
            .map(|n| n.min(AUTO_DENY_CONSECUTIVE_LIMIT)),
        auto_denials_total: ev.auto_denials_total.map(|n| n.min(AUTO_DENY_TOTAL_LIMIT)),
    }
}

/// Permission-mode label for the `session.permission_mode_changed` span.
pub(crate) fn permission_mode_label(is_yolo: bool) -> &'static str {
    if is_yolo {
        "bypassPermissions"
    } else {
        "default"
    }
}

/// Telemetry `source` label for a permission [`Decision`] on the `tool.decision` span.
/// `Decision::Allow`/`Ask` carry no provenance, so a config/policy allow is indistinguishable from a user click.
/// Report neutral `allowed` rather than guessing `user_temporary`.
pub(crate) fn permission_decision_source(decision: &Decision, is_yolo: bool) -> &'static str {
    match decision {
        Decision::PolicyDeny(_) => "config",
        Decision::Reject(_) => "user_reject",
        Decision::Cancelled => "user_abort",
        Decision::FollowupMessage(_) => "user_followup",
        Decision::Allow | Decision::Ask if is_yolo => "config",
        Decision::Allow | Decision::Ask => "allowed",
    }
}

/// Provenance for the analytics `source` field.
/// When the manager returned no event, a `Reject` is a channel failure (`manager_unavailable`), NOT a human choice.
/// Provenance is then omitted rather than mislabeled `user_reject`; with an event present, [`permission_decision_source`] labels as usual.
pub(crate) fn resolved_decision_source(
    manager_event_present: bool,
    decision: &Decision,
    is_yolo: bool,
) -> Option<String> {
    if !manager_event_present && matches!(decision, Decision::Reject(_)) {
        None
    } else {
        Some(permission_decision_source(decision, is_yolo).to_owned())
    }
}

/// One manager-authoritative telemetry snapshot for a resolved decision.
/// Both the `tool.decision` span and the product `PermissionDecisionPayload` are fed from this, so they can never disagree on mode, wait, or source.
pub(crate) struct ResolvedDecisionTelemetry {
    pub permission_mode: PermissionMode,
    pub wait_ms: u64,
    /// Provenance for the product payload and the span.
    /// `None` (omitted) for an event-less synthetic manager failure, so it is never mislabeled `user_reject`.
    pub source: Option<String>,
}

/// The shell's pre-await snapshot is used only when the manager returned no event.
/// Yolo derives from the frozen event mode, not the post-await handle state.
/// A mode or always-approve change around an open prompt thus cannot retroactively rewrite the source.
pub(crate) fn resolved_decision_telemetry(
    manager_event: Option<&PermissionEvent>,
    decision: &Decision,
    shell_permission_mode: PermissionMode,
    shell_wait_ms: u64,
    shell_is_yolo: bool,
) -> ResolvedDecisionTelemetry {
    let event_mode = manager_event.and_then(|e| e.permission_mode.as_deref());
    let permission_mode = event_mode
        .map(crate::util::config::parse_permission_mode_canonical)
        .unwrap_or(shell_permission_mode);
    let is_yolo = event_mode.map_or(shell_is_yolo, |m| m == "always-approve");
    let wait_ms = manager_event
        .and_then(|e| e.wait_ms)
        .unwrap_or(shell_wait_ms);
    let source = resolved_decision_source(manager_event.is_some(), decision, is_yolo);
    ResolvedDecisionTelemetry {
        permission_mode,
        wait_ms,
        source,
    }
}

pub(crate) fn permission_outcome(decision: &Decision) -> PermissionOutcome {
    match decision {
        Decision::Allow | Decision::Ask => PermissionOutcome::Allow,
        Decision::Reject(_) | Decision::PolicyDeny(_) => PermissionOutcome::Deny,
        Decision::Cancelled => PermissionOutcome::Cancelled,
        Decision::FollowupMessage(_) => PermissionOutcome::Followup,
    }
}

/// The single production event-to-payload projection, used by `tool_calls.rs` and the cohort tests so the tested path is exactly the shipped one.
/// Mode, wait, and source are never re-derived, so the span and product rails cannot observe different shell state.
/// Content-free analytics come from [`manager_permission_analytics`].
pub(crate) fn canonical_permission_tool_name(
    access: &distill_workspace::permission::AccessKind,
) -> String {
    distill_workspace::permission::prompter_tool_name_for_access(access)
}

pub(crate) fn permission_decision_payload(
    tool_name: String,
    access_kind: events::AccessKind,
    decision: &Decision,
    subagent_session_id: Option<String>,
    manager_event: Option<&PermissionEvent>,
    resolved: ResolvedDecisionTelemetry,
) -> PermissionDecisionPayload {
    let analytics = manager_permission_analytics(manager_event);
    PermissionDecisionPayload {
        tool_name,
        access_kind,
        decision: permission_outcome(decision),
        wait_ms: resolved.wait_ms,
        permission_mode: resolved.permission_mode,
        source: resolved.source,
        subagent_session_id,
        subagent_type: None,
        manager_prompt_attempted: analytics.manager_prompt_attempted,
        prompt_outcome: analytics.prompt_outcome,
        prompt_outcome_detail: analytics.prompt_outcome_detail,
        remember_tool_approvals: analytics.remember_tool_approvals,
        decision_reason: analytics.decision_reason,
        classifier_source: analytics.classifier_source,
        classifier_verdict: analytics.classifier_verdict,
        security_findings: analytics.security_findings,
        classifier_latency_ms: analytics.classifier_latency_ms,
        auto_denials_consecutive: analytics.auto_denials_consecutive,
        auto_denials_total: analytics.auto_denials_total,
    }
}

#[cfg(test)]
mod permission_analytics_tests {
    use super::*;
    use chrono::Utc;
    use distill_telemetry::events::{
        self, AutoDenialKpi, PermissionClassifierSource, PermissionClassifierVerdict,
        PermissionDecisionPayload, PermissionDecisionReason, PermissionPromptOutcome,
        PermissionSecurityFinding,
    };
    use distill_workspace::permission::{ClassifierSecurityFinding, reasons};

    /// Build a manager `PermissionEvent` for the prompted auto denial-limit cohort.
    fn denial_limit_event(
        decision: &str,
        prompt_outcome: &str,
        decision_reason: &str,
        classifier_verdict: &str,
    ) -> PermissionEvent {
        PermissionEvent {
            tool_id: "tc1".into(),
            tool_name: "run_terminal_command".into(),
            access_kind: "bash".into(),
            access_detail: None,
            yolo_mode: false,
            auto_approved: false,
            user_prompted: true,
            decision: decision.into(),
            prompt_outcome: Some(prompt_outcome.into()),
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: None,
            subagent_type: None,
            subagent_description: None,
            permission_mode: Some("auto".into()),
            decision_reason: Some(decision_reason.into()),
            classifier_source: Some("llm".into()),
            classifier_latency_ms: Some(12),
            auto_denials_consecutive: Some(3),
            auto_denials_total: Some(3),
            wait_ms: Some(1000),
            queue_depth: Some(1),
            security_findings: Some(vec!["opaque_shell".into()]),
            classifier_verdict: Some(classifier_verdict.into()),
            remember_tool_approvals: Some(true),
        }
    }

    /// A `Decision` consistent with the event's decision string.
    /// The cohort test thus drives the *production* `permission_decision_payload` (the shipped projection), not a test-only copy.
    fn decision_for(ev: &PermissionEvent) -> Decision {
        match ev.decision.as_str() {
            "allow" | "ask" => Decision::Allow,
            "reject" => Decision::Reject("x".into()),
            "followup" => Decision::FollowupMessage("x".into()),
            _ => Decision::Cancelled,
        }
    }

    /// Project via the exact production call site: build the one resolved snapshot (as `tool_calls.rs` does), then feed it to the payload projection.
    /// Shell fallbacks are ignored because the manager event is present.
    fn payload(ev: &PermissionEvent) -> PermissionDecisionPayload {
        let decision = decision_for(ev);
        let resolved =
            resolved_decision_telemetry(Some(ev), &decision, PermissionMode::Ask, 0, false);
        permission_decision_payload(
            ev.tool_name.clone(),
            events::AccessKind::Bash,
            &decision,
            None,
            Some(ev),
            resolved,
        )
    }

    #[test]
    fn renamed_agent_message_keeps_canonical_product_and_external_identity() {
        let access = distill_workspace::permission::AccessKind::AgentMessage {
            subagent_id: "sub-1".into(),
        };
        let canonical = canonical_permission_tool_name(&access);
        assert_eq!(canonical, "send_subagent_message");
        assert_ne!(canonical, "relay_to_subagent");

        let event = PermissionEvent {
            tool_id: "tc-message".into(),
            tool_name: canonical.clone(),
            access_kind: "agent_message".into(),
            access_detail: Some("sub-1".into()),
            yolo_mode: false,
            auto_approved: false,
            user_prompted: true,
            decision: "allow".into(),
            prompt_outcome: Some("allow_once".into()),
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: None,
            subagent_type: None,
            subagent_description: None,
            permission_mode: Some("ask".into()),
            decision_reason: Some("needs_user".into()),
            classifier_source: None,
            classifier_latency_ms: None,
            auto_denials_consecutive: None,
            auto_denials_total: None,
            wait_ms: Some(1),
            queue_depth: Some(1),
            security_findings: None,
            classifier_verdict: None,
            remember_tool_approvals: Some(true),
        };
        let decision = Decision::Allow;
        let resolved =
            resolved_decision_telemetry(Some(&event), &decision, PermissionMode::Ask, 0, false);
        let payload = permission_decision_payload(
            canonical,
            events::AccessKind::AgentMessage,
            &decision,
            None,
            Some(&event),
            resolved,
        );
        assert_eq!(payload.tool_name, "send_subagent_message");
        assert_eq!(
            serde_json::to_value(payload.access_kind).unwrap(),
            serde_json::json!("agent_message")
        );
    }

    #[test]
    fn agent_message_payload_keeps_dedicated_identity() {
        let event = PermissionEvent {
            tool_id: "tc-message".into(),
            tool_name: "send_subagent_message".into(),
            access_kind: "agent_message".into(),
            access_detail: Some("sub-1".into()),
            yolo_mode: false,
            auto_approved: false,
            user_prompted: true,
            decision: "allow".into(),
            prompt_outcome: Some("allow_once".into()),
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: None,
            subagent_type: None,
            subagent_description: None,
            permission_mode: Some("ask".into()),
            decision_reason: Some("needs_user".into()),
            classifier_source: None,
            classifier_latency_ms: None,
            auto_denials_consecutive: None,
            auto_denials_total: None,
            wait_ms: Some(1),
            queue_depth: Some(1),
            security_findings: None,
            classifier_verdict: None,
            remember_tool_approvals: Some(true),
        };
        let decision = Decision::Allow;
        let resolved =
            resolved_decision_telemetry(Some(&event), &decision, PermissionMode::Ask, 0, false);
        let payload = permission_decision_payload(
            event.tool_name.clone(),
            events::AccessKind::AgentMessage,
            &decision,
            None,
            Some(&event),
            resolved,
        );

        assert_eq!(payload.tool_name, "send_subagent_message");
        assert_eq!(
            serde_json::to_value(payload.access_kind).unwrap(),
            serde_json::json!("agent_message")
        );
    }

    #[test]
    fn event_less_projection_omits_all_manager_fields() {
        let a = manager_permission_analytics(None);
        assert!(a.manager_prompt_attempted.is_none());
        assert!(a.prompt_outcome.is_none());
        assert!(a.prompt_outcome_detail.is_none());
        assert!(a.remember_tool_approvals.is_none());
        assert!(a.decision_reason.is_none());
        assert!(a.classifier_source.is_none());
        assert!(a.classifier_verdict.is_none());
        assert!(a.security_findings.is_none());
        assert!(a.classifier_latency_ms.is_none());
        assert!(a.auto_denials_consecutive.is_none());
        assert!(a.auto_denials_total.is_none());
    }

    #[test]
    fn projection_preserves_none_vs_some_empty_findings() {
        let mut ev = denial_limit_event("allow", "allow_once", "auto_denial_limit", "block");
        // None (never classified) stays None.
        ev.security_findings = None;
        assert!(
            manager_permission_analytics(Some(&ev))
                .security_findings
                .is_none()
        );
        // Some([]) (classifier route ran, empty assessment) stays Some([]).
        ev.security_findings = Some(vec![]);
        assert_eq!(
            manager_permission_analytics(Some(&ev)).security_findings,
            Some(vec![])
        );
    }

    #[test]
    fn unknown_manager_strings_are_dropped_not_exported() {
        let mut ev = denial_limit_event("allow", "allow_once", "auto_denial_limit", "block");
        ev.decision_reason = Some("brand_new_reason".into());
        ev.classifier_verdict = Some("mystery".into());
        // Known and unknown tokens mixed: all-or-nothing omits the ENTIRE findings field (never a misleading partial or "clean" subset)
        ev.security_findings = Some(vec!["opaque_shell".into(), "made_up_token".into()]);
        let a = manager_permission_analytics(Some(&ev));
        assert!(a.decision_reason.is_none(), "unknown reason omitted");
        assert!(a.classifier_verdict.is_none(), "unknown verdict omitted");
        assert!(
            a.security_findings.is_none(),
            "any unknown finding token omits the whole field"
        );
        // All-known tokens still project fully
        ev.security_findings = Some(vec!["opaque_shell".into(), "file_write".into()]);
        assert_eq!(
            manager_permission_analytics(Some(&ev)).security_findings,
            Some(vec![
                PermissionSecurityFinding::OpaqueShell,
                PermissionSecurityFinding::FileWrite,
            ])
        );
    }

    #[test]
    fn not_wired_source_projects_typed_variant() {
        let mut ev = denial_limit_event(
            "reject",
            "reject_once",
            "auto_classifier_unavailable",
            "unavailable",
        );
        ev.classifier_source = Some("not_wired".into());
        ev.classifier_latency_ms = None;
        let a = manager_permission_analytics(Some(&ev));
        assert_eq!(
            a.classifier_source,
            Some(PermissionClassifierSource::NotWired)
        );
        assert_eq!(
            a.classifier_verdict,
            Some(PermissionClassifierVerdict::Unavailable)
        );
        assert!(
            a.classifier_latency_ms.is_none(),
            "no classifier ran → no latency"
        );
    }

    #[test]
    fn denial_counters_clamp_to_manager_budget() {
        let mut ev = denial_limit_event("allow", "allow_once", "auto_denial_limit", "block");
        ev.auto_denials_consecutive = Some(9999);
        ev.auto_denials_total = Some(9999);
        let a = manager_permission_analytics(Some(&ev));
        assert_eq!(
            a.auto_denials_consecutive,
            Some(AUTO_DENY_CONSECUTIVE_LIMIT)
        );
        assert_eq!(a.auto_denials_total, Some(AUTO_DENY_TOTAL_LIMIT));
    }

    /// Drift guard: the telemetry reason enum is a bijection with the manager's owned `reasons::ALL` vocabulary.
    /// Adding a reason on either side without the other fails this.
    #[test]
    fn decision_reason_enum_matches_manager_vocabulary() {
        use std::collections::BTreeSet;
        for r in reasons::ALL {
            assert!(
                PermissionDecisionReason::try_from(*r).is_ok(),
                "manager reason {r} is not mapped by PermissionDecisionReason"
            );
        }
        let manager: BTreeSet<&str> = reasons::ALL.iter().copied().collect();
        let enum_wire: BTreeSet<String> = PermissionDecisionReason::ALL
            .iter()
            .map(|r| {
                serde_json::to_value(r)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let enum_refs: BTreeSet<&str> = enum_wire.iter().map(String::as_str).collect();
        assert_eq!(
            manager, enum_refs,
            "manager reasons and PermissionDecisionReason must be identical sets"
        );
    }

    /// Drift guard for the finding vocabulary against the workspace owner.
    #[test]
    fn security_finding_enum_matches_manager_tokens() {
        use std::collections::BTreeSet;
        let manager: BTreeSet<&str> = ClassifierSecurityFinding::ALL
            .iter()
            .map(|f| f.token())
            .collect();
        let enum_wire: BTreeSet<String> = PermissionSecurityFinding::ALL
            .iter()
            .map(|f| {
                serde_json::to_value(f)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let enum_refs: BTreeSet<&str> = enum_wire.iter().map(String::as_str).collect();
        assert_eq!(
            manager, enum_refs,
            "manager finding tokens and PermissionSecurityFinding must be identical sets"
        );
    }

    /// Drift guard: every prompt-outcome wire in `PromptOutcomeKind::ALL` normalizes to a telemetry category.
    /// That list is what the manager itself emits via `PromptOutcome::kind().wire_str()`.
    /// A new owner kind (one `wire_enum!` list entry) that lacks a `PermissionPromptOutcome` mapping fails here rather than being silently omitted.
    #[test]
    fn prompt_outcome_covers_manager_vocabulary() {
        use distill_workspace::permission::PromptOutcomeKind;
        for kind in PromptOutcomeKind::ALL {
            let wire = kind.wire_str();
            assert!(
                PermissionPromptOutcome::try_from(wire).is_ok(),
                "manager prompt outcome {wire} is not mapped by PermissionPromptOutcome"
            );
        }
    }

    /// Drift guard: the outcome-detail enum is a bijection with the manager's `PromptOutcomeKind::ALL` wire vocabulary.
    /// A new "Always allow" prompt option thus cannot be silently dropped from adoption analytics.
    #[test]
    fn prompt_outcome_detail_matches_manager_vocabulary() {
        use distill_telemetry::events::PermissionPromptOutcomeDetail;
        use distill_workspace::permission::PromptOutcomeKind;
        use std::collections::BTreeSet;
        let manager: BTreeSet<&str> = PromptOutcomeKind::ALL
            .iter()
            .map(|k| k.wire_str())
            .collect();
        let enum_wire: BTreeSet<String> = PermissionPromptOutcomeDetail::ALL
            .iter()
            .map(|d| {
                serde_json::to_value(d)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let enum_refs: BTreeSet<&str> = enum_wire.iter().map(String::as_str).collect();
        assert_eq!(
            manager, enum_refs,
            "manager prompt-outcome wires and PermissionPromptOutcomeDetail must be identical sets"
        );
    }

    /// Drift guard: the classifier-source enum is a bijection with the workspace owner projection `ClassifierSourceKind::ALL`.
    /// That vocabulary covers classifier provenances plus `fast_path` and `not_wired`, generated from one list.
    /// A new owner kind not mirrored by the telemetry enum fails here.
    #[test]
    fn classifier_source_enum_matches_manager_vocabulary() {
        use distill_workspace::permission::ClassifierSourceKind;
        use std::collections::BTreeSet;
        let manager: BTreeSet<&str> = ClassifierSourceKind::ALL
            .iter()
            .map(|k| k.wire_str())
            .collect();
        let enum_wire: BTreeSet<String> = PermissionClassifierSource::ALL
            .iter()
            .map(|s| {
                serde_json::to_value(s)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let enum_refs: BTreeSet<&str> = enum_wire.iter().map(String::as_str).collect();
        assert_eq!(
            manager, enum_refs,
            "manager classifier sources and PermissionClassifierSource must be identical sets"
        );
    }

    /// Drift guard: the classifier-verdict enum is a bijection with the workspace owner's `ClassifierVerdict::ALL` (generated from one list).
    #[test]
    fn classifier_verdict_enum_matches_manager_vocabulary() {
        use distill_workspace::permission::ClassifierVerdict;
        use std::collections::BTreeSet;
        let manager: BTreeSet<&str> = ClassifierVerdict::ALL
            .iter()
            .map(|v| v.wire_str())
            .collect();
        let enum_wire: BTreeSet<String> = PermissionClassifierVerdict::ALL
            .iter()
            .map(|v| {
                serde_json::to_value(v)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let enum_refs: BTreeSet<&str> = enum_wire.iter().map(String::as_str).collect();
        assert_eq!(
            manager, enum_refs,
            "manager classifier verdicts and PermissionClassifierVerdict must be identical sets"
        );
    }

    #[test]
    fn resolved_snapshot_uses_frozen_event_mode_and_wait() {
        // Manager event froze always-approve; the shell's post-await snapshot says non-yolo Ask
        let mut ev = denial_limit_event("allow", "allow_once", "yolo", "unavailable");
        ev.permission_mode = Some("always-approve".into());
        ev.wait_ms = Some(4321);
        let r = resolved_decision_telemetry(
            Some(&ev),
            &Decision::Allow,
            PermissionMode::Ask,
            10,
            false,
        );
        assert_eq!(r.permission_mode, PermissionMode::AlwaysApprove);
        assert_eq!(r.wait_ms, 4321);
        assert_eq!(
            r.source.as_deref(),
            Some("config"),
            "frozen yolo → config, not allowed"
        );
        // Event-less synthetic Reject: omit source, use shell fallbacks.
        let r2 = resolved_decision_telemetry(
            None,
            &Decision::Reject("x".into()),
            PermissionMode::Auto,
            7,
            false,
        );
        assert!(r2.source.is_none(), "event-less Reject omits source");
        assert_eq!(r2.permission_mode, PermissionMode::Auto);
        assert_eq!(r2.wait_ms, 7);
    }

    #[test]
    fn resolved_source_omits_manager_unavailable_reject() {
        // Event-less Reject (manager channel failure): never `user_reject`.
        assert_eq!(
            resolved_decision_source(false, &Decision::Reject("boom".into()), false),
            None
        );
        // Event present: unchanged provenance.
        assert_eq!(
            resolved_decision_source(true, &Decision::Reject("boom".into()), false).as_deref(),
            Some("user_reject")
        );
        // Event-less Allow (AllowAll) keeps provenance.
        assert!(resolved_decision_source(false, &Decision::Allow, false).is_some());
    }

    /// Four real manager events run through the production projection and then the KPI predicate.
    #[test]
    fn four_event_cohort_smoke_denominator_and_rates() {
        let events = [
            denial_limit_event("reject", "reject_once", "auto_denial_limit", "block"),
            denial_limit_event("allow", "allow_once", "auto_denial_limit", "block"),
            denial_limit_event("cancelled", "cancelled", "auto_denial_limit", "block"),
            // Timeout escalation the user cancelled: excluded (verdict unavailable).
            denial_limit_event(
                "cancelled",
                "cancelled",
                "auto_classifier_timeout",
                "unavailable",
            ),
        ];
        let payloads: Vec<_> = events.iter().map(payload).collect();
        let kpis: Vec<_> = payloads
            .iter()
            .filter_map(events::auto_denial_kpi)
            .collect();
        assert_eq!(kpis.len(), 2, "denominator counts only human allow/reject");
        let disagreements = kpis
            .iter()
            .filter(|k| **k == AutoDenialKpi::Disagreement)
            .count();
        let alignments = kpis
            .iter()
            .filter(|k| **k == AutoDenialKpi::Alignment)
            .count();
        assert_eq!((alignments, disagreements), (1, 1), "50/50");

        // Content-free: no command, path, or free text on the projected payload JSON
        let [first, second, ..] = payloads.as_slice() else {
            panic!("expected at least two payloads, got {}", payloads.len());
        };
        let json = serde_json::to_string(first).unwrap();
        assert!(!json.contains("$X") && !json.contains("bash -c"));
        assert_eq!(
            events::PermissionDecisionReason::try_from("auto_denial_limit"),
            Ok(PermissionDecisionReason::AutoDenialLimit)
        );
        assert_eq!(first.prompt_outcome, Some(PermissionPromptOutcome::Reject));
        assert_eq!(
            second.classifier_verdict,
            Some(PermissionClassifierVerdict::Block)
        );
    }
}
