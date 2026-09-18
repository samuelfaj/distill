//! Per-request classification state and auto-denial limits used by the permission-manager actor.

use crate::permission::auto_mode::{
    BashSecurityAssessment, ClassifierSourceKind, ClassifierVerdict,
};

pub const AUTO_DENY_CONSECUTIVE_LIMIT: u32 = 3;
pub const AUTO_DENY_TOTAL_LIMIT: u32 = 20;

pub(super) const AUTO_DENY_GUIDANCE: &str = "Take a safer approach that stays within what the user asked \
     for; do not retry this exact action or attempt to work around the denial. If no safer \
     alternative exists, ask the user how to proceed.";

/// Refusal guidance for a YOLO/always-approve session: there is no prompt in
/// that mode, so the model is told to route around the refusal itself.
pub(super) const JEV_VETO_GUIDANCE: &str = "This session is in always-approve mode, so nothing will prompt the \
     user about it: pick a different approach that does not destroy or leave the workspace. Do \
     not retry this exact action.";

/// Auto-denial counters snapshotted for one decision.
/// They live in their own `Cell` so the finalizer reads the value meant for the event even after the running counters are reset later in the arm.
#[derive(Clone, Copy)]
pub(super) struct DenialCounters {
    pub(super) consecutive: u32,
    pub(super) total: u32,
}

/// Classifier provenance for one request, plus the manager-only `NotWired` that auto_mode's [`ClassifierSource`] cannot express.
/// It never reports `heuristic` for a request no classifier actually judged.
#[derive(Clone, Copy)]
pub(super) enum ClassificationSource {
    /// A classifier ran and produced this provenance (llm/heuristic/timeout/…).
    Classifier(crate::permission::auto_mode::ClassifierSource),
    /// Auto route entered but `set_classifier(None)` left no classifier installed.
    NotWired,
}

impl ClassificationSource {
    pub(super) const fn kind(self) -> ClassifierSourceKind {
        match self {
            Self::Classifier(source) => source.kind(),
            Self::NotWired => ClassifierSourceKind::NotWired,
        }
    }
}

/// The completed outcome of one classifier route.
/// `latency_ms` is `None` for the not-wired case (no classifier ran).
#[derive(Clone, Copy)]
pub(super) struct ClassificationOutcome {
    pub(super) verdict: ClassifierVerdict,
    pub(super) source: ClassificationSource,
    pub(super) latency_ms: Option<u64>,
}

/// The per-request classification state: route, frozen assessment, and typed outcome in one value, so impossible combinations are unrepresentable.
/// The permission event is projected from it once.
#[derive(Default)]
pub(super) enum RequestClassification {
    /// The request never entered the Auto classifier route (fast path skipped, non-Bash, or Auto disabled).
    /// Event `security_findings`/verdict/source stay `None`.
    #[default]
    NotClassified,
    /// Auto fast-path allow: no side query and no assessment.
    /// Event reports `classifier_source = fast_path` with no findings/verdict.
    FastPath,
    /// Entered the classifier route with this frozen assessment (the exact set handed to the classifier).
    /// `outcome` is `None` only when the side query was abandoned (requester gone mid-classify), so findings survive without verdict/source/latency.
    Classified {
        assessment: BashSecurityAssessment,
        outcome: Option<ClassificationOutcome>,
    },
}

impl RequestClassification {
    /// `FastPath` for the fast path, the outcome provenance for a completed route, `None` for not-classified or abandoned.
    pub(super) fn classifier_source(&self) -> Option<ClassifierSourceKind> {
        match self {
            Self::NotClassified => None,
            Self::FastPath => Some(ClassifierSourceKind::FastPath),
            Self::Classified { outcome, .. } => outcome.as_ref().map(|o| o.source.kind()),
        }
    }

    /// Typed verdict, only when a classifier produced one.
    pub(super) fn classifier_verdict(&self) -> Option<ClassifierVerdict> {
        match self {
            Self::Classified {
                outcome: Some(o), ..
            } => Some(o.verdict),
            _ => None,
        }
    }

    /// Classifier latency, only for a completed route that recorded one.
    pub(super) fn classifier_latency_ms(&self) -> Option<u64> {
        match self {
            Self::Classified {
                outcome: Some(o), ..
            } => o.latency_ms,
            _ => None,
        }
    }

    /// Ordered finding tokens when the classifier route was entered (`Some([])` for an empty attempted assessment); `None` when never classified.
    pub(super) fn security_findings_tokens(&self) -> Option<Vec<String>> {
        match self {
            Self::Classified { assessment, .. } => Some(assessment.tokens()),
            _ => None,
        }
    }
}

/// Matches the hyphenated `config.ui.permission_mode` values, not the telemetry enum's underscore serde used for product analytics.
pub(super) fn permission_mode_artifact_str(
    mode: xai_grok_telemetry::enums::PermissionMode,
) -> &'static str {
    use xai_grok_telemetry::enums::PermissionMode;
    match mode {
        PermissionMode::AlwaysApprove => "always-approve",
        PermissionMode::Auto => "auto",
        PermissionMode::Ask => "ask",
    }
}
