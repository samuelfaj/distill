//! Shared decision records and offline permission-battery evaluation helpers.
//!
//! The permission scores retained here support evaluation experiments only;
//! they do not authorize tool execution. Production permission decisions belong
//! exclusively to [`crate::permission`]. Optimization call sites use the shared
//! [`DecisionSink`] to record their decisions and usage.

use std::sync::{Arc, Mutex};

use crate::jev::questions::{
    PermissionThresholds, Q_DELETES_DATA, Q_ESCAPES_WORKSPACE, Q_NETWORK_EGRESS,
    Q_PRIVILEGE_ESCALATION, Q_RISK_CLASS, Q_SEVERITY, Q_STATE_INJECTION, Q_UNTRUSTED_EXECUTION,
    RISK_ROUTINE_BUILD,
};
use crate::jev::types::JevAnswerSet;

/// What the caller should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevDecision {
    /// Routine, in-workspace, confident: the harness may auto-approve.
    Allow { reason: String },
    /// Confident danger: refuse the action.
    Block { reason: String },
    /// Genuinely uncertain or out of Jev's competence: use the existing path.
    Escalate { reason: String },
}

impl JevDecision {
    /// Wire-ish label for telemetry and logs.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allow { .. } => "allow",
            Self::Block { .. } => "block",
            Self::Escalate { .. } => "escalate",
        }
    }

    /// The human-readable reason carried by the decision.
    pub fn reason(&self) -> &str {
        match self {
            Self::Allow { reason } | Self::Block { reason } | Self::Escalate { reason } => reason,
        }
    }

    /// True when the decision defers to the incumbent path.
    pub const fn is_escalation(&self) -> bool {
        matches!(self, Self::Escalate { .. })
    }
}

/// One recorded decision. Field set is the telemetry contract of this module.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    /// "permission_classifier" | "p1_tool_family" | … (see [`crate::jev::flags::JevLever`]).
    pub lever: String,
    /// Question ids asked in the single speculative request.
    pub questions: Vec<String>,
    /// Decision label: allow | block | escalate.
    pub decision: String,
    /// Why, in one reviewer-friendly sentence.
    pub reason: String,
    /// The confidence the decision hinged on, when the answer carried one.
    pub confidence: Option<f64>,
    /// Model id reported by the API (versioned, so thresholds can be pinned).
    pub model: String,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub request_id: Option<String>,
    /// Convenience copy of [`JevDecision::is_escalation`].
    pub escalated: bool,
}

/// Where decisions are reported. Implemented by the harness telemetry; the
/// default implementation logs a single structured line per decision.
pub trait DecisionSink: Send + Sync {
    fn record(&self, record: &DecisionRecord);
}

/// Logs one line per decision — never the state, the questions' text, or the key.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingSink;

impl DecisionSink for TracingSink {
    fn record(&self, record: &DecisionRecord) {
        tracing::info!(
            target: "jev.decision",
            lever = record.lever.as_str(),
            decision = record.decision.as_str(),
            escalated = record.escalated,
            confidence = record.confidence.unwrap_or(f64::NAN),
            model = record.model.as_str(),
            latency_ms = record.latency_ms,
            input_tokens = record.input_tokens,
            output_tokens = record.output_tokens,
            questions = record.questions.len(),
            request_id = record.request_id.as_deref().unwrap_or(""),
            reason = record.reason.as_str(),
            "jev decision"
        );
    }
}

/// Keeps every record in memory: used by tests and by the token-accounting
/// harness that reports how much Jev spent per lever.
#[derive(Debug, Default, Clone)]
pub struct CapturingSink {
    records: Arc<Mutex<Vec<DecisionRecord>>>,
}

impl CapturingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every record seen so far.
    pub fn records(&self) -> Vec<DecisionRecord> {
        self.records.lock().expect("capture lock").clone()
    }

    /// Total Jev tokens (input + output) spent across captured decisions.
    pub fn total_tokens(&self) -> u64 {
        self.records()
            .iter()
            .map(|r| r.input_tokens + r.output_tokens)
            .sum()
    }
}

impl DecisionSink for CapturingSink {
    fn record(&self, record: &DecisionRecord) {
        self.records
            .lock()
            .expect("capture lock")
            .push(record.clone());
    }
}

/// Builds the record for one composed decision (pure; the caller emits it).
pub fn record_for(
    lever: &str,
    questions: &[&str],
    decision: &JevDecision,
    confidence: Option<f64>,
    answers: &JevAnswerSet,
) -> DecisionRecord {
    DecisionRecord {
        lever: lever.to_owned(),
        questions: questions.iter().map(|q| (*q).to_owned()).collect(),
        decision: decision.as_str().to_owned(),
        reason: decision.reason().to_owned(),
        confidence,
        model: answers.model.clone(),
        latency_ms: answers.latency_ms,
        input_tokens: answers.usage.input(),
        output_tokens: answers.usage.output(),
        request_id: answers.request_id.clone(),
        escalated: decision.is_escalation(),
    }
}

/// A record for a deferral: no Jev answer set exists because the call was never
/// made, failed, or timed out. Keeps the seam observable when it adds nothing.
pub fn record_escalation(lever: &str, reason: &str) -> DecisionRecord {
    DecisionRecord {
        lever: lever.to_owned(),
        questions: Vec::new(),
        decision: JevDecision::Escalate {
            reason: reason.to_owned(),
        }
        .as_str()
        .to_owned(),
        reason: reason.to_owned(),
        confidence: None,
        model: "n/a".to_owned(),
        latency_ms: 0,
        input_tokens: 0,
        output_tokens: 0,
        request_id: None,
        escalated: true,
    }
}

/// Outcome of composing the permission battery.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOutcome {
    pub decision: JevDecision,
    /// Confidence of the risk-class answer, when it arrived.
    pub confidence: Option<f64>,
}

/// Composes the permission battery into a decision.
///
/// `sensitive` is the caller's own risk flag (policy `Ask`, protected target,
/// security findings): it raises the bar for an allow to
/// [`PermissionThresholds::confidence_sensitive`] and never lowers it.
pub fn compose_permission(
    answers: &JevAnswerSet,
    thresholds: &PermissionThresholds,
    sensitive: bool,
) -> PermissionOutcome {
    let confidence = answers.confidence(Q_RISK_CLASS);

    // Missing pieces are an escalation, never an allow.
    let missing: Vec<&str> = [Q_RISK_CLASS, Q_ESCAPES_WORKSPACE, Q_SEVERITY]
        .into_iter()
        .filter(|id| answers.answers.get(*id).is_none())
        .collect();
    if !missing.is_empty() {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("incomplete answer set (missing {})", missing.join(", ")),
            },
            confidence,
        };
    }

    let escapes = answers.noul(Q_ESCAPES_WORKSPACE).unwrap_or(0.5);
    let deletes = answers.noul(Q_DELETES_DATA).unwrap_or(0.5);
    let privileges = answers.noul(Q_PRIVILEGE_ESCALATION).unwrap_or(0.5);
    let network = answers.noul(Q_NETWORK_EGRESS).unwrap_or(0.5);
    let untrusted = answers.noul(Q_UNTRUSTED_EXECUTION).unwrap_or(0.5);
    let injection = answers.noul(Q_STATE_INJECTION).unwrap_or(0.5);
    let severity = answers.score_normalized(Q_SEVERITY).unwrap_or(0.5);
    let risk = answers.choice(Q_RISK_CLASS).unwrap_or("");
    let top_probability = answers.top_probability(Q_RISK_CLASS);
    let destructive_probability = answers
        .probability(Q_RISK_CLASS, "destructive")
        .unwrap_or(0.0);

    // 1. Confident danger blocks, regardless of any other answer.
    if escapes >= thresholds.noul_deny_min {
        return PermissionOutcome {
            decision: JevDecision::Block {
                reason: format!("action escapes the workspace (p={escapes:.2})"),
            },
            confidence,
        };
    }
    if destructive_probability >= thresholds.destructive_probability_block {
        return PermissionOutcome {
            decision: JevDecision::Block {
                reason: format!("risk class leans destructive (p={destructive_probability:.2})"),
            },
            confidence,
        };
    }
    if severity >= thresholds.severity_block {
        return PermissionOutcome {
            decision: JevDecision::Block {
                reason: format!("severity {severity:.2} at or above the block line"),
            },
            confidence,
        };
    }

    // 2. Anything uncertain, or any hazard above its allow band, escalates.
    let in_band = |p: f64| p > thresholds.review_band_low && p < thresholds.review_band_high;
    for (label, probability) in [
        (Q_ESCAPES_WORKSPACE, escapes),
        (Q_DELETES_DATA, deletes),
        (Q_PRIVILEGE_ESCALATION, privileges),
        (Q_UNTRUSTED_EXECUTION, untrusted),
    ] {
        if in_band(probability) {
            return PermissionOutcome {
                decision: JevDecision::Escalate {
                    reason: format!("{label} is genuinely uncertain (p={probability:.2})"),
                },
                confidence,
            };
        }
    }
    if network >= thresholds.noul_allow_max {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("network egress p={network:.2} is not routine"),
            },
            confidence,
        };
    }
    if deletes >= thresholds.noul_allow_max {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("delete/overwrite p={deletes:.2} is not routine"),
            },
            confidence,
        };
    }
    if privileges >= thresholds.noul_allow_max {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("privilege escalation p={privileges:.2} is not routine"),
            },
            confidence,
        };
    }
    if untrusted >= thresholds.noul_allow_max {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("untrusted execution p={untrusted:.2} is not routine"),
            },
            confidence,
        };
    }
    // The injection screen is a *filter*, never a security boundary (item 103):
    // a raised screen escalates; a quiet screen does not authorise anything.
    if injection >= thresholds.injection_escalate {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!(
                    "state looks like it is steering the classifier (p={injection:.2})"
                ),
            },
            confidence,
        };
    }
    if severity >= thresholds.severity_escalate {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("severity {severity:.2} above the routine band"),
            },
            confidence,
        };
    }

    // 3. Only the routine, in-workspace class may be allowed, and only with a
    //    confident, unambiguous answer above the caller's bar.
    let bar = if sensitive {
        thresholds.confidence_sensitive
    } else {
        thresholds.confidence_floor
    };
    let confident = confidence.unwrap_or(0.0);
    if confidence.is_none() {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: "no confidence reported for the risk class".to_owned(),
            },
            confidence,
        };
    }
    if confident < bar {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("confidence {confident:.2} below the {bar:.2} bar"),
            },
            confidence,
        };
    }
    if let Some(top) = top_probability {
        if top < thresholds.top_probability_action {
            return PermissionOutcome {
                decision: JevDecision::Escalate {
                    reason: format!("top-label probability {top:.2} below the action line"),
                },
                confidence,
            };
        }
    }
    if risk != RISK_ROUTINE_BUILD {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("risk class `{risk}` is not routine"),
            },
            confidence,
        };
    }
    if escapes > thresholds.noul_allow_max {
        return PermissionOutcome {
            decision: JevDecision::Escalate {
                reason: format!("escape probability {escapes:.2} above the allow band"),
            },
            confidence,
        };
    }

    PermissionOutcome {
        decision: JevDecision::Allow {
            reason: format!(
                "routine in-workspace action (confidence {confident:.2}, escape p={escapes:.2})"
            ),
        },
        confidence,
    }
}

/// Records a permission outcome through the sink and returns it unchanged.
pub fn emit_permission(
    sink: &dyn DecisionSink,
    outcome: &PermissionOutcome,
    answers: &JevAnswerSet,
) -> PermissionOutcome {
    let record = record_for(
        "permission_classifier",
        &[
            Q_RISK_CLASS,
            Q_ESCAPES_WORKSPACE,
            Q_DELETES_DATA,
            Q_PRIVILEGE_ESCALATION,
            Q_NETWORK_EGRESS,
            Q_UNTRUSTED_EXECUTION,
            Q_STATE_INJECTION,
            Q_SEVERITY,
        ],
        &outcome.decision,
        outcome.confidence,
        answers,
    );
    sink.record(&record);
    outcome.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::types::{Answer, JevAnswerSet, Usage};
    use std::collections::BTreeMap;

    fn answers_with(
        risk: &str,
        confidence: f64,
        probabilities: &[(&str, f64)],
        nouls: &[(&str, f64)],
        severity: f64,
        legend_levels: usize,
    ) -> JevAnswerSet {
        let mut answers: BTreeMap<String, Answer> = BTreeMap::new();
        answers.insert(
            Q_RISK_CLASS.to_owned(),
            Answer::Choice {
                choice: risk.to_owned(),
                probabilities: probabilities
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), *v))
                    .collect(),
                confidence: Some(confidence),
            },
        );
        for (id, value) in nouls {
            answers.insert((*id).to_owned(), Answer::Noul { noul: *value });
        }
        answers.insert(
            Q_SEVERITY.to_owned(),
            Answer::Score {
                score: severity,
                legend: (0..legend_levels)
                    .map(|i| (i.to_string(), serde_json::Value::Null))
                    .collect(),
                probabilities: BTreeMap::new(),
                confidence: Some(0.8),
            },
        );
        JevAnswerSet {
            model: "jev-1.13.0".to_owned(),
            answers,
            usage: Usage {
                input_tokens: Some(400),
                output_tokens: Some(60),
            },
            request_id: Some("req-1".to_owned()),
            latency_ms: 120,
        }
    }

    fn routine_answers(confidence: f64, escapes: f64) -> JevAnswerSet {
        answers_with(
            RISK_ROUTINE_BUILD,
            confidence,
            &[
                (RISK_ROUTINE_BUILD, 0.85),
                ("mutating_local", 0.10),
                ("destructive", 0.03),
            ],
            &[
                (Q_ESCAPES_WORKSPACE, escapes),
                (Q_DELETES_DATA, 0.05),
                (Q_PRIVILEGE_ESCALATION, 0.02),
                (Q_NETWORK_EGRESS, 0.02),
                (Q_UNTRUSTED_EXECUTION, 0.03),
                (Q_STATE_INJECTION, 0.05),
            ],
            0.4,
            3,
        )
    }

    #[test]
    fn routine_confident_in_workspace_action_allows() {
        let outcome = compose_permission(
            &routine_answers(0.92, 0.05),
            &PermissionThresholds::default(),
            false,
        );
        assert!(matches!(outcome.decision, JevDecision::Allow { .. }));
        assert_eq!(outcome.confidence, Some(0.92));
    }

    #[test]
    fn sensitive_context_raises_the_allow_bar() {
        let answers = routine_answers(0.70, 0.05);
        let normal = compose_permission(&answers, &PermissionThresholds::default(), false);
        assert!(matches!(normal.decision, JevDecision::Allow { .. }));
        let sensitive = compose_permission(&answers, &PermissionThresholds::default(), true);
        assert!(matches!(sensitive.decision, JevDecision::Escalate { .. }));
    }

    #[test]
    fn confident_escape_or_destructive_blocks() {
        let escapes = routine_answers(0.95, 0.95);
        assert!(matches!(
            compose_permission(&escapes, &PermissionThresholds::default(), false).decision,
            JevDecision::Block { .. }
        ));
        let destructive = answers_with(
            "destructive",
            0.9,
            &[("destructive", 0.9), ("mutating_local", 0.1)],
            &[
                (Q_ESCAPES_WORKSPACE, 0.1),
                (Q_DELETES_DATA, 0.9),
                (Q_PRIVILEGE_ESCALATION, 0.1),
                (Q_NETWORK_EGRESS, 0.1),
                (Q_UNTRUSTED_EXECUTION, 0.1),
                (Q_STATE_INJECTION, 0.1),
            ],
            0.9,
            3,
        );
        assert!(matches!(
            compose_permission(&destructive, &PermissionThresholds::default(), false).decision,
            JevDecision::Block { .. }
        ));
    }

    #[test]
    fn review_band_and_low_confidence_escalate() {
        let band = routine_answers(0.9, 0.5);
        assert!(matches!(
            compose_permission(&band, &PermissionThresholds::default(), false).decision,
            JevDecision::Escalate { .. }
        ));
        let shaky = routine_answers(0.55, 0.05);
        assert!(matches!(
            compose_permission(&shaky, &PermissionThresholds::default(), false).decision,
            JevDecision::Escalate { .. }
        ));
    }

    #[test]
    fn non_routine_class_escalates_even_when_confident() {
        let network = answers_with(
            "network",
            0.93,
            &[("network", 0.93), ("routine_build", 0.05)],
            &[
                (Q_ESCAPES_WORKSPACE, 0.05),
                (Q_DELETES_DATA, 0.05),
                (Q_PRIVILEGE_ESCALATION, 0.05),
                (Q_NETWORK_EGRESS, 0.9),
                (Q_UNTRUSTED_EXECUTION, 0.05),
                (Q_STATE_INJECTION, 0.05),
            ],
            0.2,
            3,
        );
        assert!(matches!(
            compose_permission(&network, &PermissionThresholds::default(), false).decision,
            JevDecision::Escalate { .. }
        ));
    }

    #[test]
    fn injection_screen_can_only_escalate() {
        let mut answers = routine_answers(0.95, 0.02);
        answers
            .answers
            .insert(Q_STATE_INJECTION.to_owned(), Answer::Noul { noul: 0.95 });
        assert!(matches!(
            compose_permission(&answers, &PermissionThresholds::default(), false).decision,
            JevDecision::Escalate { .. }
        ));
        // A quiet screen never authorises on its own: the routine path still
        // needs the risk/escape/severity evidence above.
        let quiet = routine_answers(0.95, 0.02);
        assert!(matches!(
            compose_permission(&quiet, &PermissionThresholds::default(), false).decision,
            JevDecision::Allow { .. }
        ));
    }

    #[test]
    fn incomplete_answer_sets_escalate() {
        let mut answers = routine_answers(0.95, 0.02);
        answers.answers.remove(Q_ESCAPES_WORKSPACE);
        let outcome = compose_permission(&answers, &PermissionThresholds::default(), false);
        assert!(matches!(outcome.decision, JevDecision::Escalate { .. }));
        assert!(outcome.decision.reason().contains(Q_ESCAPES_WORKSPACE));
    }

    #[test]
    fn every_decision_is_recorded_with_the_required_fields() {
        let sink = CapturingSink::new();
        let answers = routine_answers(0.9, 0.05);
        let outcome = compose_permission(&answers, &PermissionThresholds::default(), false);
        let returned = emit_permission(&sink, &outcome, &answers);
        assert_eq!(returned, outcome);
        let records = sink.records();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.lever, "permission_classifier");
        assert_eq!(record.questions.len(), 8);
        assert_eq!(record.decision, "allow");
        assert_eq!(record.model, "jev-1.13.0");
        assert_eq!(record.input_tokens, 400);
        assert_eq!(record.output_tokens, 60);
        assert_eq!(record.request_id.as_deref(), Some("req-1"));
        assert!(!record.escalated);
        assert_eq!(sink.total_tokens(), 460);
    }
}
