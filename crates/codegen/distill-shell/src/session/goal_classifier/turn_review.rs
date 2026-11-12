// Modified for Distill by Samuel Fajreldines, 2026.
//! UltraCode v2: an independent review of one turn's patch at turn end, with a
//! status that fails closed.
//!
//! Inert unless the effective config sets `[modes.ultracode] contract = "v2"`.
//! The review reuses the goal verification stage with the turn lens and the
//! turn's own patch (see [`super::turn_snapshot`]). Any outcome in which no
//! reviewer judged the patch is UNVERIFIED, never a pass.

use super::turn_snapshot::{TurnPatch, TurnSnapshot};
use super::{
    GoalClassifierOutcome, GoalClassifierSpawner, HarnessCheck, VerificationStageInputs,
    VerificationStageResult, run_verification_stage,
};
use crate::session::acp_session::turn_facts::{CheckOutcome, TestEvidence};
use crate::session::events::Event;
use crate::session::goal_role_tools::RoleToolNames;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Rework rounds a refuted turn gets before it ends UNVERIFIED.
pub(crate) const MAX_REWORK_ROUNDS: u32 = 2;
/// Reviewers on the one panel a repeated refutation gets.
pub(crate) const PANEL_SKEPTICS: u32 = 3;
/// Wall-clock a turn may spend between its first review and its last.
pub(crate) const GATE_BUDGET: Duration = Duration::from_secs(15 * 60);
/// Budget for the snapshot before the turn's first tool call: the same
/// second the goal baseline capture gets, since the user is waiting.
pub(crate) const SNAPSHOT_BUDGET: Duration = super::GIT_BASELINE_CAPTURE_TIMEOUT;

/// Whether `root`, an effective config, sets `[modes.ultracode] contract = "v2"`.
pub(crate) fn contract_v2_in(root: Option<&toml::Value>) -> bool {
    root.and_then(|root| root.get("modes")?.get("ultracode")?.get("contract")?.as_str())
        .is_some_and(|contract| contract.trim() == "v2")
}

#[cfg(test)]
thread_local! {
    static CONTRACT_V2_FOR_TESTS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Overrides the configured contract on this thread; `None` reads the config.
#[cfg(test)]
pub(crate) fn set_contract_v2_for_tests(on: Option<bool>) {
    CONTRACT_V2_FOR_TESTS.with(|cell| cell.set(on));
}

/// The contract in the effective config, read once per process (config does
/// not change mid-run); a config that fails to load leaves it off.
pub(crate) fn contract_v2_configured() -> bool {
    #[cfg(test)]
    if let Some(on) = CONTRACT_V2_FOR_TESTS.with(std::cell::Cell::get) {
        return on;
    }
    static SETTING: OnceLock<bool> = OnceLock::new();
    *SETTING.get_or_init(|| contract_v2_in(crate::config::load_effective_config().ok().as_ref()))
}

/// Documentation and plain text: a turn that changed only these is not
/// reviewed.
pub(crate) fn is_prose_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('/').collect();
    let file = parts.pop().unwrap_or_default();
    parts.contains(&"docs")
        || [".md", ".mdx", ".txt", ".rst"]
            .iter()
            .any(|extension| file.ends_with(extension))
}

pub(crate) fn has_non_prose(files: &[String]) -> bool {
    files.iter().any(|file| !is_prose_path(file))
}

/// How a reviewed turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnStatus {
    /// A reviewer did not refute the patch and the latest check is a current pass (or none ran).
    Reviewed,
    Unverified(String),
}

impl TurnStatus {
    /// The last line of the turn's final message.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Reviewed => "Verification: REVIEWED".to_owned(),
            Self::Unverified(reason) => format!("Verification: UNVERIFIED ({reason})"),
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Reviewed => "reviewed",
            Self::Unverified(_) => "unverified",
        }
    }
}

/// Why the latest recorded check keeps a passing review from REVIEWED: it
/// failed, its outcome is unknown, or it ran before the working tree last
/// changed (a missing tree marker counts as before). `None` when it is a
/// current pass or no check was recorded.
pub(crate) fn latest_check_blocker(
    checks: &[TestEvidence],
    current_tree: Option<&str>,
) -> Option<String> {
    let latest = checks.iter().max_by_key(|check| check.finished_at)?;
    let mut command: String = latest.command.replace('\n', " ").chars().take(80).collect();
    if latest.command.chars().count() > 80 {
        command.push('…');
    }
    match latest.outcome {
        CheckOutcome::Failed => Some(format!("latest check `{command}` failed")),
        CheckOutcome::Unknown => Some(format!("latest check `{command}` has an unknown outcome")),
        CheckOutcome::Passed => match (latest.worktree.as_deref(), current_tree) {
            (Some(then), Some(now)) if then == now => None,
            _ => Some(format!("latest check `{command}` ran before the last change")),
        },
    }
}

/// What one review run said about the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnReviewVerdict {
    NotRefuted,
    Refuted { gaps: String, fingerprint: String },
    /// Every refuter says the gap needs the user, not another round.
    Blocked { gaps: String },
    /// No reviewer judged the patch.
    Unavailable { reason: String },
}

impl TurnReviewVerdict {
    fn label(&self) -> &'static str {
        match self {
            Self::NotRefuted => "not_refuted",
            Self::Refuted { .. } => "refuted",
            Self::Blocked { .. } => "blocked",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

fn verdict_of(result: VerificationStageResult) -> TurnReviewVerdict {
    match result.outcome {
        GoalClassifierOutcome::Achieved { .. } => TurnReviewVerdict::NotRefuted,
        GoalClassifierOutcome::NotAchieved { .. } if result.refuted_by_failures_only => {
            TurnReviewVerdict::Unavailable {
                reason: "the reviewer returned no usable verdict".to_owned(),
            }
        }
        GoalClassifierOutcome::NotAchieved {
            gaps_summary,
            gap_fingerprint,
            ..
        } => TurnReviewVerdict::Refuted {
            gaps: gaps_summary,
            fingerprint: gap_fingerprint,
        },
        GoalClassifierOutcome::Blocked { pause_summary, .. } => TurnReviewVerdict::Blocked {
            gaps: pause_summary,
        },
        GoalClassifierOutcome::FailOpenAchieved { reason, .. } => TurnReviewVerdict::Unavailable {
            reason: format!("the review could not run: {}", reason.as_const_str()),
        },
    }
}

/// One review run of a turn's patch.
pub(crate) struct TurnReviewInputs<'a> {
    /// The user's request, the contract the patch is judged against.
    pub request: &'a str,
    pub final_response: &'a str,
    pub patch: &'a TurnPatch,
    pub workspace_root: &'a Path,
    pub verifier_id: &'a str,
    /// 1-based review run within the turn; names this run's files.
    pub review: u32,
    pub model_id: &'a str,
    pub scratch_dir: &'a Path,
    pub scratch_dir_ready: bool,
    pub skeptic_count: u32,
    pub prior_gaps: Option<&'a str>,
    pub tool_names: &'a [RoleToolNames],
    pub inherit_tool_names: &'a RoleToolNames,
    pub harness_checks: &'a [HarnessCheck],
}

/// Runs the verification stage over the turn's patch with the turn lens. The
/// stage's goal events are dropped; one turn-scoped event reports the run.
pub(crate) async fn run_turn_review(
    spawner: Arc<dyn GoalClassifierSpawner>,
    inputs: TurnReviewInputs<'_>,
    emit_event: &dyn Fn(Event),
) -> TurnReviewVerdict {
    let started = Instant::now();
    let result = run_verification_stage(
        spawner,
        VerificationStageInputs {
            objective: inputs.request,
            final_response: inputs.final_response,
            baseline_commit: None,
            workspace_root: inputs.workspace_root,
            verifier_id: inputs.verifier_id,
            attempt: inputs.review,
            model_id: inputs.model_id,
            goal_created_at: 0,
            plan_file: None,
            plan_baseline_file: None,
            implementer_scratch_dir: inputs.scratch_dir,
            scratch_dir_ready: inputs.scratch_dir_ready,
            skeptic_count: inputs.skeptic_count,
            max_runs: MAX_REWORK_ROUNDS + 2,
            prior_skeptic0_session_id: None,
            prior_gaps: inputs.prior_gaps,
            tool_names: inputs.tool_names,
            inherit_tool_names: inputs.inherit_tool_names,
            harness_checks: inputs.harness_checks,
            turn_changes: Some(&inputs.patch.changes),
        },
        &|_| {},
    )
    .await;
    let verdict = verdict_of(result);
    emit_event(Event::UltracodeTurnReview {
        review: inputs.review,
        skeptics: inputs.skeptic_count,
        verdict: verdict.label(),
        latency_ms: started.elapsed().as_millis() as u64,
    });
    verdict
}

/// What the turn does after a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NextStep {
    /// Append the gaps and keep working.
    Rework { gaps: String },
    /// Ask the three-reviewer panel about the same patch.
    Panel,
    /// End the turn with this status.
    Finish {
        status: TurnStatus,
        gaps: Option<String>,
    },
}

/// The review's state for one prompt turn: the snapshot, the budget counters
/// and the status. Replaced when another prompt's first tool call takes a
/// snapshot, so nothing carries over between turns.
#[derive(Debug, Default)]
pub(crate) struct TurnReviewState {
    prompt_id: String,
    snapshot: Option<TurnSnapshot>,
    verifier_id: String,
    reviews: u32,
    rework_rounds: u32,
    panel_used: bool,
    gate_started: Option<Instant>,
    last_fingerprint: Option<String>,
    last_gaps: Option<String>,
    status: Option<TurnStatus>,
}

impl TurnReviewState {
    pub(crate) fn begin(prompt_id: &str, snapshot: TurnSnapshot) -> Self {
        Self {
            prompt_id: prompt_id.to_owned(),
            snapshot: Some(snapshot),
            verifier_id: format!("turn-{}", uuid::Uuid::new_v4().simple()),
            ..Default::default()
        }
    }

    /// Whether `prompt_id`'s snapshot was already attempted.
    pub(crate) fn started_for(&self, prompt_id: &str) -> bool {
        self.snapshot.is_some() && self.prompt_id == prompt_id
    }

    /// The snapshot of `prompt_id`, while its review is still open.
    pub(crate) fn open_snapshot(&self, prompt_id: &str) -> Option<&TurnSnapshot> {
        (self.prompt_id == prompt_id && self.status.is_none())
            .then_some(self.snapshot.as_ref())
            .flatten()
    }

    /// The status `prompt_id` ended with; `None` when the turn was not reviewed.
    pub(crate) fn status_for(&self, prompt_id: &str) -> Option<&TurnStatus> {
        (self.prompt_id == prompt_id)
            .then_some(self.status.as_ref())
            .flatten()
    }

    pub(crate) fn verifier_id(&self) -> &str {
        &self.verifier_id
    }

    pub(crate) fn prior_gaps(&self) -> Option<&str> {
        self.last_gaps.as_deref()
    }

    pub(crate) fn reviews(&self) -> u32 {
        self.reviews
    }

    pub(crate) fn rework_rounds(&self) -> u32 {
        self.rework_rounds
    }

    pub(crate) fn panel_used(&self) -> bool {
        self.panel_used
    }

    pub(crate) fn gate_elapsed(&self) -> Duration {
        self.gate_started.map_or(Duration::ZERO, |started| started.elapsed())
    }

    fn finish(&mut self, status: TurnStatus, gaps: Option<String>) -> NextStep {
        self.status = Some(status.clone());
        NextStep::Finish { status, gaps }
    }

    /// Ends the turn without a review, e.g. outside a repository.
    pub(crate) fn finish_unreviewed(&mut self, reason: &str) -> NextStep {
        self.finish(TurnStatus::Unverified(reason.to_owned()), None)
    }

    fn finish_passed(&mut self, check_blocker: Option<String>) -> NextStep {
        let status = match check_blocker {
            None => TurnStatus::Reviewed,
            Some(blocker) => TurnStatus::Unverified(blocker),
        };
        self.finish(status, None)
    }

    /// Starts the gate clock and counts a review run; `Some` ends the turn
    /// because the gate time is used up.
    pub(crate) fn before_review(&mut self) -> Option<NextStep> {
        let started = *self.gate_started.get_or_insert_with(Instant::now);
        if started.elapsed() >= GATE_BUDGET {
            let gaps = self.last_gaps.clone();
            return Some(self.finish(
                TurnStatus::Unverified("review time budget used".to_owned()),
                gaps,
            ));
        }
        self.reviews += 1;
        None
    }

    /// The next step after the sole reviewer's verdict.
    pub(crate) fn after_review(
        &mut self,
        verdict: TurnReviewVerdict,
        check_blocker: Option<String>,
    ) -> NextStep {
        match verdict {
            TurnReviewVerdict::NotRefuted => self.finish_passed(check_blocker),
            TurnReviewVerdict::Unavailable { reason } => {
                self.finish(TurnStatus::Unverified(reason), None)
            }
            TurnReviewVerdict::Blocked { gaps } => self.finish(
                TurnStatus::Unverified("the review found gaps that need your decision".to_owned()),
                Some(gaps),
            ),
            TurnReviewVerdict::Refuted { gaps, fingerprint } => {
                let repeated = self.last_fingerprint.as_deref() == Some(fingerprint.as_str());
                self.last_fingerprint = Some(fingerprint);
                self.last_gaps = Some(gaps.clone());
                if self.gate_elapsed() >= GATE_BUDGET {
                    self.finish(
                        TurnStatus::Unverified("review time budget used".to_owned()),
                        Some(gaps),
                    )
                } else if repeated && !self.panel_used {
                    NextStep::Panel
                } else if self.rework_rounds < MAX_REWORK_ROUNDS {
                    self.rework_rounds += 1;
                    NextStep::Rework { gaps }
                } else {
                    self.finish(
                        TurnStatus::Unverified(format!(
                            "still refuted after {MAX_REWORK_ROUNDS} rework rounds"
                        )),
                        Some(gaps),
                    )
                }
            }
        }
    }

    /// The final step after the panel's verdict: the panel is the last word.
    pub(crate) fn after_panel(
        &mut self,
        verdict: TurnReviewVerdict,
        check_blocker: Option<String>,
    ) -> NextStep {
        self.panel_used = true;
        match verdict {
            TurnReviewVerdict::NotRefuted => self.finish_passed(check_blocker),
            TurnReviewVerdict::Refuted { gaps, .. } | TurnReviewVerdict::Blocked { gaps } => self
                .finish(
                    TurnStatus::Unverified("refuted again by a three-reviewer panel".to_owned()),
                    Some(gaps),
                ),
            TurnReviewVerdict::Unavailable { reason } => {
                let gaps = self.last_gaps.clone();
                self.finish(TurnStatus::Unverified(reason), gaps)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn backdate_gate_for_tests(&mut self, by: Duration) {
        self.gate_started = Instant::now().checked_sub(by);
    }
}

/// The reminder that sends a refuted turn back to work.
pub(crate) fn rework_reminder(round: u32, gaps: &str) -> String {
    format!(
        "An independent review of this turn's changes refuted them (rework round {round} of {MAX_REWORK_ROUNDS}). \
         Fix these gaps, re-run the checks that cover the changed code, then give your final answer:\n{gaps}"
    )
}

/// Text appended to the turn's final message; the status is its last line.
pub(crate) fn final_text(status: &TurnStatus, gaps: Option<&str>) -> String {
    let mut out = String::from("\n\n");
    if let Some(gaps) = gaps.map(str::trim_end).filter(|gaps| !gaps.trim().is_empty()) {
        out.push_str("Open review gaps:\n");
        out.push_str(gaps);
        out.push_str(
            "\n\nTo keep working until these gaps are closed, continue this request as a /goal.\n",
        );
    }
    out.push_str(&status.line());
    out
}

#[cfg(test)]
#[path = "turn_review_tests.rs"]
mod tests;
