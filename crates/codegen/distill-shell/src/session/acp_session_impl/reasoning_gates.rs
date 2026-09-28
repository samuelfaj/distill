// Modified for Distill by Samuel Fajreldines, 2026.
//! When the main model consults the reasoning model.
//!
//! The main model runs every execution step. Jev selects the initial plan; a
//! selected plan requires a delivery review before approval. Other consults
//! remain selective, based on the facts
//! this module keeps about the request:
//!
//! * **plan**: once per request, whether the reasoning model plans it from the
//!   request or after a read-only planner inspects the workspace.
//! * **step**: each round with work the reasoning model has not weighed yet,
//!   whether the main model is stuck and needs advice before the next round.
//!   Repeated failures, loops, undone edits and rounds since the last advice
//!   are facts Jev reads. A persistent, diagnosed stall also triggers one
//!   bounded execution handoff.
//! * **review**: before delivery, required after a plan and otherwise selective.
//!
//! An edit the change review (C4) flags is Jev's decision already and is
//! reviewed as it comes. Routine consults have no fixed budget; the persistent
//! stall handoff is limited to one attempt per request.
//!
//! The consults of one request share a [`ReasoningThread`]: each one resends
//! the thread unchanged and appends only the work since the last reply, so the
//! reasoning model sees every piece of work once and its provider can serve
//! the repeated prefix from the prompt cache.

use distill_sampling_types::ConversationItem;
use distill_tool_types::{TaskOutputOutput, TaskOutputResult};
use distill_tools::types::output::{ApplyPatchOutput, SearchReplaceOutput, ToolOutput};
use distill_workspace::jev::JevAnswerSet;

/// Bytes of diff handed to a delivery review; the rest is listed by file.
pub(crate) const REVIEW_DIFF_BYTES: usize = 24_000;
/// Characters of a call kept as its identity.
const SIGNATURE_CHARS: usize = 200;
/// Tool results remembered per request.
const MAX_EVENTS: usize = 400;
/// Bytes of one change's diff kept when it is recorded.
const CHANGE_DIFF_BYTES: usize = 8_000;
/// Characters of a check's output kept as evidence.
const TEST_EXCERPT_CHARS: usize = 2_000;
const MAX_CHECKS: usize = 20;
/// Characters of a work item's first line in its one-line summary.
const SUMMARY_CHARS: usize = 160;
/// Characters of a tool call's arguments naming the result it produced.
const SOURCE_ARGS_CHARS: usize = 120;
/// Source of a work item the main model wrote itself.
const MAIN_MODEL_SOURCE: &str = "main model";
/// Latest calls a step decision sees one by one.
const RECENT_CALLS: usize = 12;
/// Changed files a delivery decision sees by name.
const LISTED_FILES: usize = 20;
/// Characters of the final message a delivery decision sees.
const FINAL_MESSAGE_CHARS: usize = 600;

/// One finished tool call, as the step facts read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolEvent {
    pub(crate) call_id: String,
    pub(crate) signature: String,
    pub(crate) failed: bool,
    /// Waiting on background work repeats the same call by design.
    pub(crate) polling: bool,
}

/// One executed edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeRecord {
    pub(crate) path: String,
    pub(crate) diff: String,
    pub(crate) added: u64,
    pub(crate) removed: u64,
    /// `(old, new)` of a search/replace edit, to spot one that undoes another.
    pub(crate) replaced: Option<(String, String)>,
}

/// The last build, test or lint the main model ran.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TestEvidence {
    pub(crate) command: String,
    pub(crate) command_hash: String,
    output_hash: String,
    pub(crate) cwd: String,
    pub(crate) failed: bool,
    pub(crate) excerpt: String,
}

/// Where the once-per-request plan gate stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PlanGate {
    /// Not judged yet: the first round of the request asks Jev.
    #[default]
    Undecided,
    /// A plan is wanted after workspace evidence is available.
    AfterEvidence,
    /// Planned, or judged unnecessary.
    Done,
}

/// The selected request's handoff phase. Planning and review are enforced by
/// the harness; the plan decision itself still belongs to Jev's `PlanGate`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FlowPhase {
    #[default]
    Worker,
    Planning,
    Executing,
    Correcting,
    Approved,
    Unavailable,
}

impl FlowPhase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Planning => "planning",
            Self::Executing => "executing",
            Self::Correcting => "correcting",
            Self::Approved => "approved",
            Self::Unavailable => "unavailable",
        }
    }
}

const MAX_FLOW_REVISIONS: u8 = 3;

/// The question Jev answers for this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundQuestion {
    Plan,
    Step,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StallSignal {
    FailedCall(String),
    UndoneEdit(String),
    RepeatedCall(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallAction {
    Diagnose,
    Handoff,
}

/// Why the reasoning model was consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsultKind {
    Plan,
    Recover,
    EditReview,
    Review,
}

impl ConsultKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Recover => "recover",
            Self::EditReview => "edit review",
            Self::Review => "review",
        }
    }
}

/// A call and how often it came up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CallCount {
    pub(crate) call: String,
    pub(crate) times: usize,
}

/// What the main model did since the reasoning model last advised it (or
/// since the request began): the evidence a step decision weighs.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct StepFacts {
    pub(crate) rounds_since_advice: u32,
    pub(crate) tool_calls: usize,
    pub(crate) failed_calls: usize,
    /// Failed calls in a row at the end.
    pub(crate) trailing_failures: usize,
    /// The call that failed most often, when one failed more than once.
    pub(crate) repeated_failure: Option<CallCount>,
    /// The call made most often, when one ran more than once; waiting on
    /// background work repeats a call by design and is left out.
    pub(crate) repeated_call: Option<CallCount>,
    /// Files where an edit undid an earlier one.
    pub(crate) undone_edits: Vec<String>,
    /// The latest calls, oldest first.
    pub(crate) recent_calls: Vec<String>,
}

impl StepFacts {
    /// The facts in one sentence, for the decision log and the reasoning model.
    pub(crate) fn describe(&self) -> String {
        let mut parts = vec![format!(
            "{} rounds and {} tool calls since the last advice, {} failed",
            self.rounds_since_advice, self.tool_calls, self.failed_calls
        )];
        if self.trailing_failures > 1 {
            parts.push(format!("the last {} in a row", self.trailing_failures));
        }
        if let Some(failure) = &self.repeated_failure {
            parts.push(format!("`{}` failed {} times", failure.call, failure.times));
        }
        if let Some(repeat) = &self.repeated_call {
            parts.push(format!("`{}` ran {} times", repeat.call, repeat.times));
        }
        for path in &self.undone_edits {
            parts.push(format!("an edit to `{path}` was undone"));
        }
        parts.join("; ")
    }
}

/// The first line of a delivery review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewVerdict {
    Approve,
    Revise,
    Unclear,
}

/// Everything the gates know about the current request. Lives in the turn
/// ledger, so it starts empty with every user prompt.
#[derive(Debug, Default)]
pub(crate) struct ReasoningGates {
    rounds: u32,
    events: Vec<ToolEvent>,
    events_discarded: usize,
    failed_events_discarded: usize,
    changes: Vec<ChangeRecord>,
    last_test: Option<TestEvidence>,
    checks: Vec<TestEvidence>,
    plan: PlanGate,
    flow_phase: FlowPhase,
    flow_revisions: u8,
    /// Revision verdicts of reviews Jev chose (no required flow) this turn.
    optional_revisions: u8,
    reviewed_artifact: Option<String>,
    approved_review: Option<(String, String, String)>,
    plan_unavailable: bool,
    planner_advice: Option<String>,
    complexity: Option<f64>,
    last_consult_round: Option<u32>,
    events_at_last_consult: usize,
    changes_at_last_consult: usize,
    consults: Vec<ConsultKind>,
    diagnosed_stall: Option<StallSignal>,
    stall_escalation_finished: bool,
    /// Tool results and changes the last review attempted to inspect.
    review_attempted_upto: Option<(usize, usize)>,
    verdicts: Vec<&'static str>,
    /// Answers a battery shared with another decision gave for this round.
    round_answers: Option<(RoundQuestion, JevAnswerSet)>,
    thread: ReasoningThread,
    /// The session directory's commit when this turn began (`Some(None)`
    /// outside Git), so a review can see work committed during the turn.
    turn_baseline: Option<Option<String>>,
}

impl ReasoningGates {
    /// One main-model call of this request.
    pub(crate) fn note_round(&mut self) {
        self.rounds = self.rounds.saturating_add(1);
    }

    /// Records the facts one finished tool call adds to the request.
    pub(crate) fn note_tool_result(
        &mut self,
        call_id: &str,
        tool: &str,
        args: &serde_json::Value,
        output: &ToolOutput,
    ) {
        if self.events.len() == MAX_EVENTS {
            self.failed_events_discarded += usize::from(self.events.remove(0).failed);
            self.events_discarded += 1;
        }
        self.events.push(ToolEvent {
            call_id: call_id.to_owned(),
            signature: call_signature(tool, args),
            failed: output.is_error()
                || matches!(output, ToolOutput::Bash(bash) if bash.timed_out || bash.signal.is_some()),
            polling: matches!(
                output,
                ToolOutput::TaskOutput(_)
                    | ToolOutput::KillTask(_)
                    | ToolOutput::Monitor(_)
                    | ToolOutput::Todo(_)
                    | ToolOutput::SchedulerList(_)
            ),
        });
        self.changes.extend(change_records(output));
        match output {
            ToolOutput::Bash(bash) if looks_like_check_command(&bash.command) => {
                let failed = bash.exit_code != 0 || bash.timed_out || bash.signal.is_some();
                self.note_check(
                    &bash.command,
                    bash.current_dir.clone(),
                    failed,
                    &bash.output_for_prompt,
                );
            }
            // A check run in the background reports its outcome when its task
            // output is read after it finishes.
            ToolOutput::TaskOutput(TaskOutputOutput::Result(task)) => self.note_task_check(task),
            ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => {
                for task in &multi.results {
                    self.note_task_check(task);
                }
            }
            _ => {}
        }
    }

    fn note_task_check(&mut self, task: &TaskOutputResult) {
        if task.is_terminal() && looks_like_check_command(&task.command) {
            let failed = task.status != "completed" || task.exit_code != Some(0);
            self.note_check(&task.command, String::new(), failed, &task.output);
        }
    }

    fn note_check(&mut self, command: &str, cwd: String, failed: bool, output: &str) {
        let check = TestEvidence {
            command: command.chars().take(SIGNATURE_CHARS).collect(),
            command_hash: blake3::hash(command.as_bytes()).to_hex().to_string(),
            output_hash: check_output_hash(output, failed),
            cwd,
            failed,
            excerpt: tail_chars(output, TEST_EXCERPT_CHARS),
        };
        self.last_test = Some(check.clone());
        if let Some(previous) = self.checks.iter_mut().find(|previous| {
            previous.command_hash == check.command_hash
                && previous.cwd == check.cwd
                && previous.failed == check.failed
                && (!check.failed || previous.output_hash == check.output_hash)
        }) {
            *previous = check;
        } else {
            if self.checks.len() == MAX_CHECKS {
                self.checks.remove(0);
            }
            self.checks.push(check);
        }
    }

    pub(crate) fn plan(&self) -> PlanGate {
        self.plan
    }

    fn event_count(&self) -> usize {
        self.events_discarded + self.events.len()
    }

    fn events_since_consult(&self) -> &[ToolEvent] {
        self.events
            .get(
                self.events_at_last_consult
                    .saturating_sub(self.events_discarded)..,
            )
            .unwrap_or_default()
    }

    pub(crate) fn tool_failed(&self, call_id: &str) -> Option<bool> {
        self.events
            .iter()
            .rev()
            .find(|event| !call_id.is_empty() && event.call_id == call_id)
            .map(|event| event.failed)
    }

    pub(crate) fn flow_phase(&self) -> FlowPhase {
        self.flow_phase
    }

    pub(crate) fn require_plan(&mut self) {
        self.flow_phase = FlowPhase::Planning;
    }

    pub(crate) fn flow_requires_review(&self) -> bool {
        matches!(
            self.flow_phase,
            FlowPhase::Executing | FlowPhase::Correcting | FlowPhase::Approved
        )
    }

    pub(crate) fn has_new_review_artifact(&self, artifact: &str) -> bool {
        self.reviewed_artifact.as_deref() != Some(artifact)
    }

    pub(crate) fn note_review_artifact(&mut self, artifact: String) {
        self.reviewed_artifact = Some(artifact);
    }

    pub(crate) fn approved_review(&self, artifact: &str) -> Option<(&str, &str)> {
        self.approved_review
            .as_ref()
            .filter(|(key, _, _)| key == artifact)
            .map(|(_, report, verdict)| (report.as_str(), verdict.as_str()))
    }

    pub(crate) fn approve_review(&mut self, artifact: String, report: String, verdict: String) {
        self.approved_review = Some((artifact, report, verdict));
    }

    pub(crate) fn review_check_identity(&self) -> String {
        let mut checks = std::collections::BTreeSet::new();
        for check in &self.checks {
            // Repeated pass timing is immaterial, but changed counts, skipped
            // tests, warnings, and failures invalidate the earlier approval.
            checks.insert(
                serde_json::json!({
                    "command": check.command_hash, "cwd": check.cwd, "failed": check.failed,
                    "result": check.output_hash,
                })
                .to_string(),
            );
        }
        checks.into_iter().collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn stop_unresolved_review(&mut self) {
        self.flow_phase = FlowPhase::Unavailable;
    }

    pub(crate) fn set_plan(&mut self, plan: PlanGate) {
        self.plan = plan;
    }

    pub(crate) fn note_plan_unavailable(&mut self) {
        self.plan = PlanGate::Done;
        self.plan_unavailable = true;
        if self.flow_phase == FlowPhase::Planning {
            self.flow_phase = FlowPhase::Unavailable;
        }
    }

    pub(crate) fn note_planner_advice(&mut self, advice: String) {
        self.planner_advice = Some(advice);
    }

    pub(crate) fn planner_advice(&self) -> Option<&str> {
        self.planner_advice.as_deref()
    }

    /// Whether the main model already has tool results to plan on.
    pub(crate) fn has_evidence(&self) -> bool {
        !self.events.is_empty()
    }

    pub(crate) fn has_edits(&self) -> bool {
        !self.changes.is_empty()
    }

    /// How complex Jev judged the request when it decided the plan.
    pub(crate) fn note_assessment(&mut self, complexity: Option<f64>) {
        self.complexity = complexity;
    }

    /// The reasoning model's side of this request.
    pub(crate) fn thread(&mut self) -> &mut ReasoningThread {
        &mut self.thread
    }

    /// The question Jev answers this round: the plan while the request is
    /// undecided, then whether the main model needs advice, as long as it did
    /// something the reasoning model has not weighed. A plan waiting for the
    /// first evidence has nothing to ask.
    pub(crate) fn round_question(&self) -> Option<RoundQuestion> {
        match self.plan {
            PlanGate::Undecided => Some(RoundQuestion::Plan),
            PlanGate::AfterEvidence => None,
            PlanGate::Done => (self.event_count() > self.events_at_last_consult
                || self.changes.len() > self.changes_at_last_consult)
                .then_some(RoundQuestion::Step),
        }
    }

    /// Keeps the answers a shared battery gave for this round's question.
    pub(crate) fn set_round_answers(&mut self, question: RoundQuestion, answers: JevAnswerSet) {
        self.round_answers = Some((question, answers));
    }

    /// The answers kept for `question`, if any. The store is emptied either
    /// way, so answers never outlive the round they were given for.
    pub(crate) fn take_round_answers(&mut self, question: RoundQuestion) -> Option<JevAnswerSet> {
        self.round_answers
            .take()
            .filter(|(asked, _)| *asked == question)
            .map(|(_, answers)| answers)
    }

    pub(crate) fn clear_round_answers(&mut self) {
        self.round_answers = None;
    }

    /// What happened since the last consult, or since the request began.
    pub(crate) fn step_facts(&self) -> StepFacts {
        let events = self.events_since_consult();
        let most_frequent = |calls: &mut dyn Iterator<Item = &ToolEvent>| {
            let mut counts: Vec<(&str, usize)> = Vec::new();
            for event in calls {
                match counts.iter_mut().find(|(call, _)| *call == event.signature) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((&event.signature, 1)),
                }
            }
            counts
                .into_iter()
                .filter(|(_, times)| *times > 1)
                .max_by_key(|(_, times)| *times)
                .map(|(call, times)| CallCount {
                    call: call.to_owned(),
                    times,
                })
        };
        let changes = self
            .changes
            .get(self.changes_at_last_consult..)
            .unwrap_or_default();
        let mut undone_edits: Vec<String> = Vec::new();
        for (index, later) in changes.iter().enumerate() {
            let Some((later_old, later_new)) = &later.replaced else {
                continue;
            };
            let undoes = changes.get(..index).unwrap_or_default().iter().any(|earlier| {
                earlier.path == later.path
                    && earlier
                        .replaced
                        .as_ref()
                        .is_some_and(|(old, new)| old == later_new && new == later_old)
            });
            if undoes && !undone_edits.contains(&later.path) {
                undone_edits.push(later.path.clone());
            }
        }
        StepFacts {
            rounds_since_advice: self
                .rounds
                .saturating_sub(self.last_consult_round.unwrap_or(0)),
            tool_calls: events.len(),
            failed_calls: events.iter().filter(|event| event.failed).count(),
            trailing_failures: events.iter().rev().take_while(|event| event.failed).count(),
            repeated_failure: most_frequent(&mut events.iter().filter(|event| event.failed)),
            repeated_call: most_frequent(&mut events.iter().filter(|event| !event.polling)),
            undone_edits,
            recent_calls: events
                .iter()
                .skip(events.len().saturating_sub(RECENT_CALLS))
                .map(|event| {
                    if event.failed {
                        format!("{} (failed)", event.signature)
                    } else {
                        event.signature.clone()
                    }
                })
                .collect(),
        }
    }

    fn stall_signal(&self) -> Option<StallSignal> {
        let facts = self.step_facts();
        let events = self.events_since_consult();
        let last_call = events.iter().rev().find(|event| !event.polling);
        let changes = self
            .changes
            .get(self.changes_at_last_consult..)
            .unwrap_or_default();
        let undone_edit = changes.last().and_then(|last| {
            let (old, new) = last.replaced.as_ref()?;
            changes[..changes.len() - 1].iter().any(|earlier| {
                earlier.path == last.path
                    && earlier.replaced.as_ref().is_some_and(|(before, after)| {
                        before == new && after == old
                    })
            }).then(|| StallSignal::UndoneEdit(last.path.clone()))
        });
        let repeated_call = facts
            .repeated_call
            .as_ref()
            .filter(|call| {
                call.times >= 3
                    && call.times == facts.tool_calls
                    && events.iter().all(|event| !event.failed)
                    && self.changes.len() == self.changes_at_last_consult
            })
            .map(|call| StallSignal::RepeatedCall(call.call.clone()));
        facts
            .repeated_failure
            .filter(|call| {
                call.times >= 2
                    && last_call.is_some_and(|latest| {
                        latest.failed && latest.signature == call.call
                    })
            })
            .map(|call| StallSignal::FailedCall(call.call))
            .or(undone_edit)
            .or(repeated_call)
    }

    /// Require elapsed rounds and a repeated action or reversed edit.
    /// A diagnosis must see the same signal again before the one execution handoff.
    pub(crate) fn stall_action(&self) -> Option<StallAction> {
        if self.stall_escalation_finished {
            return None;
        }
        let signal = self.stall_signal()?;
        let rounds_since_advice = self.step_facts().rounds_since_advice;
        if self.diagnosed_stall.as_ref() == Some(&signal)
            && self.rounds >= 6
            && rounds_since_advice >= 3
        {
            Some(StallAction::Handoff)
        } else if rounds_since_advice
            >= match signal {
                StallSignal::RepeatedCall(_) => 8,
                _ => 6,
            }
        {
            Some(StallAction::Diagnose)
        } else {
            None
        }
    }

    pub(crate) fn finish_stall_escalation(&mut self) {
        self.stall_escalation_finished = true;
    }

    /// What a step decision reads besides the round's own step.
    pub(crate) fn step_state(&self) -> serde_json::Value {
        serde_json::json!({
            "since_last_advice": self.step_facts(),
            "consults_this_request": self.consult_labels(),
            "rounds_this_request": self.rounds,
            "request_complexity": self.complexity,
            "flow_phase": self.flow_phase.label(),
        })
    }

    /// Books one consult: later decisions weigh only what came after it. Any
    /// advice before delivery also settles the plan: the reasoning model has
    /// already oriented the request.
    pub(crate) fn note_consult(&mut self, kind: ConsultKind) {
        if kind == ConsultKind::Recover {
            self.diagnosed_stall = self.stall_signal();
        }
        self.consults.push(kind);
        self.last_consult_round = Some(self.rounds);
        self.events_at_last_consult = self.event_count();
        self.changes_at_last_consult = self.changes.len();
        match kind {
            ConsultKind::Plan | ConsultKind::Recover | ConsultKind::EditReview => {
                self.plan = PlanGate::Done;
                if kind == ConsultKind::Plan {
                    self.plan_unavailable = false;
                    if self.flow_phase == FlowPhase::Planning {
                        self.flow_phase = FlowPhase::Executing;
                    }
                }
            }
            ConsultKind::Review => {
                self.note_review_attempt();
            }
        }
    }

    /// The consults of this request, in order.
    pub(crate) fn consults(&self) -> &[ConsultKind] {
        &self.consults
    }

    fn consult_labels(&self) -> Vec<&'static str> {
        self.consults.iter().map(|kind| kind.label()).collect()
    }

    /// Whether a delivery has anything a review has not seen: always before
    /// the first review; after one, only new tool results or changes. A second
    /// review of the same work would repeat the first.
    pub(crate) fn has_new_work_since_review(&self) -> bool {
        self.review_attempted_upto.is_none_or(|(events, changes)| {
            self.event_count() > events || self.changes.len() > changes
        })
    }

    /// A failed review must not count as advice, but the same delivery should
    /// not retry an unavailable endpoint indefinitely.
    pub(crate) fn note_review_attempt(&mut self) {
        self.review_attempted_upto = Some((self.event_count(), self.changes.len()));
    }

    pub(crate) fn note_review_unavailable(&mut self) {
        self.verdicts.push("unavailable");
        if self.flow_requires_review() {
            self.flow_phase = FlowPhase::Unavailable;
        }
    }

    pub(crate) fn note_verdict(&mut self, verdict: ReviewVerdict) {
        self.verdicts.push(match verdict {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::Revise => "revise",
            ReviewVerdict::Unclear => "unclear",
        });
        if self.flow_requires_review() {
            if verdict == ReviewVerdict::Approve {
                self.flow_revisions = 0;
            }
            if verdict == ReviewVerdict::Revise {
                self.flow_revisions = self.flow_revisions.saturating_add(1);
            }
            self.flow_phase = match verdict {
                ReviewVerdict::Approve => FlowPhase::Approved,
                ReviewVerdict::Revise if self.flow_revisions < MAX_FLOW_REVISIONS => {
                    FlowPhase::Correcting
                }
                ReviewVerdict::Revise | ReviewVerdict::Unclear => FlowPhase::Unavailable,
            };
        } else if verdict == ReviewVerdict::Revise {
            // Reviews Jev chose had no limit: fourteen revisions in a row.
            self.optional_revisions = self.optional_revisions.saturating_add(1);
            if self.optional_revisions >= MAX_FLOW_REVISIONS {
                self.flow_phase = FlowPhase::Unavailable;
            }
        }
    }

    /// What a delivery decision reads.
    pub(crate) fn delivery_state(&self, final_message: &str) -> serde_json::Value {
        let (added, removed) = self.changes.iter().fold((0u64, 0u64), |(a, r), change| {
            (a.saturating_add(change.added), r.saturating_add(change.removed))
        });
        let mut files: Vec<&str> = Vec::new();
        for change in &self.changes {
            if !files.contains(&change.path.as_str()) {
                files.push(&change.path);
            }
        }
        serde_json::json!({
            "changed_files": files.len(),
            "files": files.iter().take(LISTED_FILES).collect::<Vec<_>>(),
            "lines_added": added,
            "lines_removed": removed,
            "last_check": self.last_test.as_ref().map(|check| serde_json::json!({
                "command": check.command,
                "failed": check.failed,
            })),
            "checks": self.checks.iter().map(|check| serde_json::json!({
                "command": check.command,
                "failed": check.failed,
            })).collect::<Vec<_>>(),
            "last_tool_failed": self.events.last().is_some_and(|event| event.failed),
            "tool_calls": self.event_count(),
            "failed_tool_calls": self.failed_events_discarded + self.events.iter().filter(|event| event.failed).count(),
            "rounds": self.rounds,
            "request_complexity": self.complexity,
            "plan_unavailable": self.plan_unavailable,
            "consults": self.consult_labels(),
            "earlier_review_verdicts": self.verdicts,
            "final_message_start": final_message.chars().take(FINAL_MESSAGE_CHARS).collect::<String>(),
        })
    }

    /// The request's changes for a reviewer: diffs up to the byte budget, then
    /// the remaining files by name and size.
    /// Recorded edits within [`REVIEW_DIFF_BYTES`], oldest first. When they do
    /// not all fit, the newest win: they hold the fixes a re-review checks.
    pub(crate) fn review_changes(&self) -> String {
        let mut used = 0usize;
        let mut kept = Vec::new();
        let mut omitted = Vec::new();
        for change in self.changes.iter().rev() {
            let block = format!(
                "--- {} (+{} -{})\n{}\n",
                change.path, change.added, change.removed, change.diff
            );
            if used.saturating_add(block.len()) <= REVIEW_DIFF_BYTES {
                used += block.len();
                kept.push(block);
            } else {
                omitted.push(format!(
                    "{} (+{} -{})",
                    change.path, change.added, change.removed
                ));
            }
        }
        let mut out: String = kept.into_iter().rev().collect();
        if !omitted.is_empty() {
            omitted.reverse();
            out.push_str(&format!("[diff omitted for: {}]\n", omitted.join(", ")));
        }
        out
    }

    pub(crate) fn last_test(&self) -> Option<&TestEvidence> {
        self.last_test.as_ref()
    }

    /// The build, test and lint outcomes this request recorded.
    pub(crate) fn recorded_checks(&self) -> &[TestEvidence] {
        &self.checks
    }

    pub(crate) fn needs_turn_baseline(&self) -> bool {
        self.turn_baseline.is_none()
    }

    pub(crate) fn set_turn_baseline(&mut self, head: Option<String>) {
        self.turn_baseline.get_or_insert(head);
    }

    pub(crate) fn turn_baseline(&self) -> Option<&str> {
        self.turn_baseline.as_ref().and_then(Option::as_deref)
    }

    pub(crate) fn review_checks(&self) -> String {
        if self.checks.is_empty() {
            return "No build, test or lint check was recorded.".to_owned();
        }
        self.checks
            .iter()
            .map(|check| {
                format!(
                    "`{}` {}; end of output:\n{}",
                    check.command,
                    if check.failed { "failed" } else { "passed" },
                    check.excerpt,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// One line for the decision log at the end of the request.
    pub(crate) fn summary(&self) -> String {
        let failures =
            self.failed_events_discarded + self.events.iter().filter(|event| event.failed).count();
        let (added, removed) = self.changes.iter().fold((0u64, 0u64), |(a, r), change| {
            (
                a.saturating_add(change.added),
                r.saturating_add(change.removed),
            )
        });
        format!(
            "rounds={} tools={} failures={failures} changes={} (+{added} -{removed}) \
             complexity={} phase={} plan_unavailable={} consults=[{}] reviews=[{}]",
            self.rounds,
            self.event_count(),
            self.changes.len(),
            self.complexity
                .map_or_else(|| "unknown".to_owned(), |c| format!("{c:.2}")),
            self.flow_phase.label(),
            self.plan_unavailable,
            self.consult_labels().join(", "),
            self.verdicts.join(", ")
        )
    }
}

/// One thing the main model did or saw since the user's request: its own
/// message, or a tool result under the call that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkEntry {
    pub(crate) source: String,
    pub(crate) text: String,
}

impl WorkEntry {
    /// The item in one line, for when the reasoning model does not need it whole.
    pub(crate) fn summary(&self) -> String {
        let line = self
            .text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let line: String = line.chars().take(SUMMARY_CHARS).collect();
        format!("{} ({} bytes): {line}", self.source, self.text.len())
    }

    pub(crate) fn from_main_model(&self) -> bool {
        self.source == MAIN_MODEL_SOURCE
    }
}

/// The main model's work since the user's last request, oldest first.
/// `anchor` is the index of the item the request started at (a goal kickoff
/// is one); without it, the last real user turn.
pub(crate) fn work_since_request(
    items: &[ConversationItem],
    anchor: Option<usize>,
) -> Vec<WorkEntry> {
    let start = anchor
        .or_else(|| {
            items
                .iter()
                .rposition(distill_chat_state::compaction_utils::is_real_user_turn)
        })
        .map_or(0, |index| index + 1);
    let mut calls: std::collections::HashMap<&str, String> = Default::default();
    let mut work = Vec::new();
    for item in items.get(start..).unwrap_or_default() {
        match item {
            ConversationItem::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    let args: String = call.arguments.chars().take(SOURCE_ARGS_CHARS).collect();
                    calls.insert(call.id.as_ref(), format!("{} {args}", call.name));
                }
                let text = assistant.content.trim();
                if !text.is_empty() {
                    work.push(WorkEntry {
                        source: MAIN_MODEL_SOURCE.to_owned(),
                        text: text.to_owned(),
                    });
                }
            }
            ConversationItem::ToolResult(result) if !result.content.trim().is_empty() => {
                work.push(WorkEntry {
                    source: calls
                        .get(result.tool_call_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| "tool".to_owned()),
                    text: result.content.to_string(),
                });
            }
            _ => {}
        }
    }
    work
}

/// The reasoning model's side of one request: its instructions, then each
/// answered consult's message and the advice it gave, exactly as sent.
#[derive(Debug, Default)]
pub(crate) struct ReasoningThread {
    /// The prompt cache key every consult of this request shares.
    key: Option<String>,
    items: Vec<ConversationItem>,
    /// Work items the thread already carries.
    sent: usize,
}

impl ReasoningThread {
    pub(crate) fn cache_key(&mut self) -> String {
        self.key
            .get_or_insert_with(|| format!("jev-reasoning-{}", uuid::Uuid::new_v4()))
            .clone()
    }

    /// Starts a consult over `work` items: a thread whose instructions changed
    /// (another main model) starts over, and work that shrank (a compaction
    /// rewrote it) is sent again. Returns where the unsent work begins.
    pub(crate) fn begin(&mut self, system: &str, work: usize) -> usize {
        if self
            .items
            .first()
            .is_none_or(|first| first.text_content() != system)
        {
            self.items = vec![ConversationItem::system(system)];
            self.sent = 0;
        }
        if work < self.sent {
            self.sent = 0;
        }
        self.sent
    }

    /// Whether no consult has been answered yet, so the request goes along.
    pub(crate) fn is_fresh(&self) -> bool {
        self.items.len() <= 1
    }

    /// What one consult sends: the whole thread, then its own message.
    pub(crate) fn with(&self, message: &str) -> Vec<ConversationItem> {
        let mut items = self.items.clone();
        items.push(ConversationItem::user(message));
        items
    }

    /// Books an answered consult: its message and advice join the thread, and
    /// the work before `sent` counts as carried.
    pub(crate) fn record(&mut self, message: &str, advice: &str, sent: usize) {
        self.items.push(ConversationItem::user(message));
        self.items.push(ConversationItem::assistant(advice));
        self.sent = sent;
    }
}

/// The verdict on a delivery review's first line (`VERDICT: approve|revise`).
/// A revision without a finding is unclear and cannot start a correction loop.
pub(crate) fn review_verdict(text: &str) -> ReviewVerdict {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default().to_ascii_lowercase();
    let first = first.trim_matches(|c: char| matches!(c, '*' | '#' | '`' | '_' | ' '));
    let Some(verdict) = first.strip_prefix("verdict:") else {
        return ReviewVerdict::Unclear;
    };
    let verdict = verdict.trim_matches(|c: char| matches!(c, '*' | '`' | '_' | ' ' | '.'));
    if verdict.starts_with("approve") {
        ReviewVerdict::Approve
    } else if verdict.starts_with("revise") {
        if lines.next().is_some() {
            ReviewVerdict::Revise
        } else {
            ReviewVerdict::Unclear
        }
    } else {
        ReviewVerdict::Unclear
    }
}

/// A call's identity: the tool plus its command, or its whole arguments.
pub(crate) fn call_signature(tool: &str, args: &serde_json::Value) -> String {
    let body = ["command", "cmd", "script"]
        .iter()
        .find_map(|key| args.get(key).and_then(serde_json::Value::as_str))
        .map_or_else(
            || args.to_string(),
            |command| command.split_whitespace().collect::<Vec<_>>().join(" "),
        );
    format!("{tool} {body}").chars().take(SIGNATURE_CHARS).collect()
}

/// Whether a shell command checks the work: a test run, a build, a type check
/// or a lint.
pub(crate) fn looks_like_check_command(command: &str) -> bool {
    const CHECK_TOOLS: &[&str] = &[
        "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "ctest", "tox", "nox", "tsc",
        "eslint", "ruff", "mypy", "pyright", "clippy",
    ];
    // These check only through one subcommand: `playwright install` or
    // `cypress open` do not.
    const CHECK_SUBCOMMANDS: &[(&str, &str)] = &[
        ("playwright", "test"),
        ("cypress", "run"),
        ("detox", "test"),
        ("maestro", "test"),
    ];
    // `xcodebuild -list` or `-showBuildSettings` name no action.
    const XCODEBUILD_ACTIONS: &[&str] =
        &["build", "test", "build-for-testing", "test-without-building", "analyze"];
    const RUNNERS: &[&str] = &[
        "cargo", "npm", "pnpm", "yarn", "bun", "go", "make", "mix", "dotnet", "deno", "swift",
        "flutter", "gradle", "./gradlew", "mvn", "uv", "poetry",
    ];
    const VERBS: &[&str] = &[
        "test", "tests", "check", "build", "lint", "typecheck", "vet", "clippy", "nextest",
    ];
    // A package script such as `e2e`, `test:e2e` or `ios:build`.
    let is_check_script =
        |word: &str| word.split(':').any(|part| part == "e2e" || VERBS.contains(&part));
    let lowered = command.to_ascii_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .collect();
    words.iter().enumerate().any(|(index, word)| {
        let name = word.rsplit('/').next().unwrap_or(word);
        if CHECK_TOOLS.contains(&name) || name == "unittest" {
            return true;
        }
        if let Some((_, subcommand)) = CHECK_SUBCOMMANDS.iter().find(|(tool, _)| *tool == name) {
            return words.get(index + 1) == Some(subcommand);
        }
        if name == "xcodebuild" {
            return words
                .get(index + 1..)
                .unwrap_or_default()
                .iter()
                .any(|arg| XCODEBUILD_ACTIONS.contains(arg));
        }
        if !RUNNERS.contains(word) {
            return false;
        }
        // `npm run test`, `uv run pytest`: the verb may sit after `run`.
        words
            .get(index + 1..(index + 3).min(words.len()))
            .unwrap_or_default()
            .iter()
            .any(|next| is_check_script(next) || CHECK_TOOLS.contains(next))
    })
}

/// The executed edits a tool result carries.
fn change_records(output: &ToolOutput) -> Vec<ChangeRecord> {
    use distill_tools::types::output::{line_diff, unified_diff};
    let counts = |old: &str, new: &str| {
        let (added, removed) = line_diff(old, new);
        (added.max(0).unsigned_abs(), removed.max(0).unsigned_abs())
    };
    match output {
        ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(edit)) => {
            let (added, removed) = counts(&edit.old_string, &edit.new_string);
            let diff = edit
                .patch
                .clone()
                .unwrap_or_else(|| unified_diff(&edit.old_string, &edit.new_string, 3));
            vec![ChangeRecord {
                path: edit.absolute_path.display().to_string(),
                diff: bounded(diff, CHANGE_DIFF_BYTES),
                added,
                removed,
                replaced: Some((edit.old_string.clone(), edit.new_string.clone())),
            }]
        }
        ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => files
            .iter()
            .map(|file| {
                let old = file.old_text.as_deref().unwrap_or_default();
                let (added, removed) = counts(old, &file.new_text);
                ChangeRecord {
                    path: file.path.display().to_string(),
                    diff: bounded(unified_diff(old, &file.new_text, 3), CHANGE_DIFF_BYTES),
                    added,
                    removed,
                    replaced: None,
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `text` cut to `limit` bytes at a character boundary, with a marker.
fn bounded(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… {} more bytes]", &text[..end], text.len() - end)
}

fn check_output_hash(output: &str, failed: bool) -> String {
    if failed {
        return blake3::hash(output.as_bytes()).to_hex().to_string();
    }
    let mut hash = blake3::Hasher::new();
    for line in output.lines() {
        // Cargo/pytest append execution time as "in 0.12s". Keep every
        // other byte, including evidence outside the displayed tail.
        let stable = line.rsplit_once(" in ").filter(|(_, duration)| {
            duration.strip_suffix('s').is_some_and(|seconds| {
                seconds.parse::<f64>().is_ok_and(|value| value.is_finite() && value >= 0.0)
            })
        }).map_or(line, |(prefix, _)| prefix);
        hash.update(stable.as_bytes());
        hash.update(b"\n");
    }
    hash.finalize().to_hex().to_string()
}

/// The last `limit` characters of `text`: a check's verdict is at its end.
fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(signature: &str, failed: bool) -> ToolEvent {
        ToolEvent {
            call_id: String::new(),
            signature: signature.to_owned(),
            failed,
            polling: false,
        }
    }

    fn change(path: &str, old: &str, new: &str) -> ChangeRecord {
        ChangeRecord {
            path: path.to_owned(),
            diff: format!("-{old}\n+{new}"),
            added: 1,
            removed: 1,
            replaced: Some((old.to_owned(), new.to_owned())),
        }
    }

    /// Jev is asked the plan first; after that, only when the main model did
    /// something the reasoning model has not weighed: with nothing new there is
    /// nothing to decide, and a plan waiting for evidence asks nothing.
    #[test]
    fn the_round_asks_the_plan_first_then_only_about_new_work() {
        let mut gates = ReasoningGates::default();
        assert_eq!(gates.round_question(), Some(RoundQuestion::Plan));
        gates.set_plan(PlanGate::AfterEvidence);
        assert_eq!(gates.round_question(), None);
        gates.set_plan(PlanGate::Done);
        assert_eq!(gates.round_question(), None, "no work yet");
        gates.events.push(event("read_file a.rs", false));
        assert_eq!(gates.round_question(), Some(RoundQuestion::Step));
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.round_question(), None, "the consult weighed it");
        gates.changes.push(change("a.rs", "x", "y"));
        assert_eq!(gates.round_question(), Some(RoundQuestion::Step));
    }

    #[test]
    fn long_requests_keep_new_failures_and_review_progress_after_eviction() {
        let mut gates = ReasoningGates::default();
        gates.set_plan(PlanGate::Done);
        for i in 0..MAX_EVENTS + 5 {
            gates.note_tool_result(
                &i.to_string(),
                "read_file",
                &serde_json::json!({"path": i}),
                &ToolOutput::Text("ok".into()),
            );
        }
        gates.note_consult(ConsultKind::Review);
        assert!(!gates.has_new_work_since_review());
        for _ in 0..6 {
            gates.note_round();
        }
        for id in ["failed-1", "failed-2"] {
            gates.note_tool_result(
                id,
                "read_file",
                &serde_json::json!({"path": "missing"}),
                &ToolOutput::ReadFile(distill_tools::types::output::ReadFileOutput::FileReadError(
                    "missing".into(),
                )),
            );
        }
        assert_eq!(gates.events.len(), MAX_EVENTS);
        assert_eq!(gates.step_facts().tool_calls, 2);
        assert_eq!(gates.step_facts().failed_calls, 2);
        assert_eq!(gates.tool_failed("failed-2"), Some(true));
        assert!(gates.has_new_work_since_review());
        assert_eq!(gates.round_question(), Some(RoundQuestion::Step));
        assert_eq!(gates.stall_action(), Some(StallAction::Diagnose));
    }

    #[test]
    fn code_review_identity_ignores_repeated_pass_timing_but_retains_scope_and_failures() {
        let mut gates = ReasoningGates::default();
        let mut check = TestEvidence {
            command: "cargo test parser".into(),
            command_hash: "parser command".into(),
            output_hash: check_output_hash("1 passed in 0.1s", false),
            cwd: "/repo".into(),
            failed: false,
            excerpt: "1 passed in 0.1s".into(),
        };
        gates.checks.push(check.clone());
        let original = gates.review_check_identity();
        check.excerpt = "1 passed in 0.2s".into();
        check.output_hash = check_output_hash(&check.excerpt, false);
        gates.checks.push(check.clone());
        assert_eq!(gates.review_check_identity(), original);
        let mut skipped = check.clone();
        skipped.output_hash = check_output_hash("0 passed, 1 skipped in 0.2s", false);
        gates.checks.push(skipped);
        assert_ne!(gates.review_check_identity(), original);
        gates.checks.pop();
        check.cwd = "/another-repo".into();
        gates.checks.push(check.clone());
        assert_ne!(gates.review_check_identity(), original);
        let scoped = gates.review_check_identity();
        check.failed = true;
        check.excerpt = "failed: parser".into();
        check.output_hash = check_output_hash(&check.excerpt, true);
        gates.checks.push(check);
        assert_ne!(gates.review_check_identity(), scoped);
    }

    /// The step decision gets the evidence of trouble as facts, counted since
    /// the last advice: repeated failures, loops (waiting on background work
    /// aside), undone edits and the rounds without advice.
    #[test]
    fn step_facts_report_failures_loops_and_undone_edits_since_the_last_advice() {
        let mut gates = ReasoningGates {
            events: vec![
                event("bash old failure", true),
                event("bash cargo test foo", true),
                event("read_file a.rs", false),
                event("bash cargo test foo", true),
                event("grep x", false),
                event("grep x", false),
                event("grep x", false),
                ToolEvent {
                    call_id: String::new(),
                    signature: "task_output t1".to_owned(),
                    failed: false,
                    polling: true,
                },
            ],
            changes: vec![change("src/a.rs", "old", "new"), change("src/a.rs", "new", "old")],
            ..Default::default()
        };
        gates.events_at_last_consult = 1;
        gates.last_consult_round = Some(2);
        for _ in 0..9 {
            gates.note_round();
        }
        let facts = gates.step_facts();
        assert_eq!(facts.rounds_since_advice, 7);
        assert_eq!((facts.tool_calls, facts.failed_calls), (7, 2), "the old failure was weighed");
        assert_eq!(facts.trailing_failures, 0);
        assert_eq!(
            facts.repeated_failure,
            Some(CallCount {
                call: "bash cargo test foo".to_owned(),
                times: 2
            })
        );
        assert_eq!(
            facts.repeated_call,
            Some(CallCount {
                call: "grep x".to_owned(),
                times: 3
            })
        );
        assert_eq!(facts.undone_edits, ["src/a.rs"]);
        assert_eq!(facts.recent_calls.len(), 7);
        assert_eq!(facts.recent_calls[0], "bash cargo test foo (failed)");
        assert_eq!(facts.recent_calls[1], "read_file a.rs");
        let said = facts.describe();
        assert!(
            said.contains("`bash cargo test foo` failed 2 times") && said.contains("`src/a.rs` was undone"),
            "{said}"
        );

        gates.note_consult(ConsultKind::Recover);
        let after = gates.step_facts();
        assert_eq!((after.tool_calls, after.rounds_since_advice), (0, 0));
        assert!(after.undone_edits.is_empty() && after.repeated_call.is_none());
    }

    #[test]
    fn a_persistent_stall_gets_one_diagnosis_then_one_handoff() {
        let mut gates = ReasoningGates::default();
        gates.set_plan(PlanGate::Done);
        for _ in 0..6 {
            gates.note_round();
        }
        gates.events.extend([
            event("cargo test parser", true),
            event("cargo test parser", true),
        ]);
        assert_eq!(gates.stall_action(), Some(StallAction::Diagnose));
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.stall_action(), None);

        for _ in 0..3 {
            gates.note_round();
        }
        gates.events.extend([
            event("cargo test parser", true),
            event("cargo test parser", true),
        ]);
        assert_eq!(gates.stall_action(), Some(StallAction::Handoff));
        gates.finish_stall_escalation();
        assert_eq!(gates.stall_action(), None);
    }

    #[test]
    fn polling_and_one_failure_are_safe_but_repeated_calls_and_undone_edits_trigger() {
        let mut gates = ReasoningGates::default();
        gates.set_plan(PlanGate::Done);
        for _ in 0..9 {
            gates.note_round();
        }
        gates.events.extend((0..9).map(|_| ToolEvent {
            call_id: String::new(),
            signature: "task_output job".to_owned(),
            failed: false,
            polling: true,
        }));
        gates.events.push(event("read_file parser.rs", false));
        assert_eq!(gates.stall_action(), None);

        gates.events.push(event("cargo test parser", true));
        assert_eq!(gates.stall_action(), None, "one failed check is not a loop");
        gates.events.push(event("cargo test parser", true));
        gates.events.push(event("read_file parser.rs", false));
        assert_eq!(gates.stall_action(), None, "a fresh investigation is progress");

        let mut repeated = ReasoningGates::default();
        repeated.set_plan(PlanGate::Done);
        for _ in 0..8 { repeated.note_round(); }
        repeated.events.extend((0..3).map(|_| event("read_file parser.rs", false)));
        assert_eq!(repeated.stall_action(), Some(StallAction::Diagnose));

        let mut undone = ReasoningGates::default();
        undone.set_plan(PlanGate::Done);
        for _ in 0..6 { undone.note_round(); }
        undone.changes.extend([
            change("parser.rs", "old", "new"),
            change("parser.rs", "new", "old"),
        ]);
        assert_eq!(undone.stall_action(), Some(StallAction::Diagnose));
        undone.changes.push(change("parser.rs", "old", "fixed"));
        assert_eq!(undone.stall_action(), None, "a later correction supersedes the undo");
    }

    /// Answers a shared battery gave are for one round and one question: a
    /// different question never reads them, and they never reach a later round.
    #[test]
    fn shared_answers_serve_only_their_own_round_question() {
        let answers = || JevAnswerSet {
            model: "test".to_owned(),
            answers: Default::default(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 0,
        };
        let mut gates = ReasoningGates::default();
        gates.set_round_answers(RoundQuestion::Step, answers());
        assert!(gates.take_round_answers(RoundQuestion::Plan).is_none());
        assert!(gates.take_round_answers(RoundQuestion::Step).is_none(), "the store emptied");
        gates.set_round_answers(RoundQuestion::Step, answers());
        assert!(gates.take_round_answers(RoundQuestion::Step).is_some());
        gates.set_round_answers(RoundQuestion::Plan, answers());
        gates.clear_round_answers();
        assert!(gates.take_round_answers(RoundQuestion::Plan).is_none());
    }

    /// Advice for a stuck main model already orients the request, so the plan
    /// must not be paid for again right after it.
    #[test]
    fn a_recovery_settles_the_plan() {
        let mut gates = ReasoningGates::default();
        gates.set_plan(PlanGate::AfterEvidence);
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.plan(), PlanGate::Done);
    }

    /// A delivery can be reviewed until a review saw it; after that only new
    /// work can be, or the same review would be paid for twice. The delivery
    /// decision reads the size, the files, the last check and earlier verdicts.
    #[test]
    fn a_delivery_is_reviewable_until_a_review_saw_it() {
        let mut gates = ReasoningGates::default();
        assert!(
            gates.has_new_work_since_review(),
            "a plain answer can be reviewed too"
        );
        gates.changes = vec![
            change("a.rs", "x", "y"),
            change("a.rs", "y", "z"),
            change("b.rs", "p", "q"),
        ];
        gates.last_test = Some(TestEvidence {
            command_hash: "cargo test".into(),
            output_hash: check_output_hash("1 failed", true),
            cwd: "/workspace".to_owned(),
            command: "cargo test".to_owned(),
            failed: true,
            excerpt: "1 failed".to_owned(),
        });
        let state = gates.delivery_state("Done: fixed the parser.");
        assert_eq!(state["changed_files"], 2);
        assert_eq!(state["files"], serde_json::json!(["a.rs", "b.rs"]));
        assert_eq!((state["lines_added"].as_u64(), state["lines_removed"].as_u64()), (Some(3), Some(3)));
        assert_eq!(state["last_check"]["failed"], true);
        assert_eq!(state["final_message_start"], "Done: fixed the parser.");

        gates.note_consult(ConsultKind::Review);
        gates.note_verdict(ReviewVerdict::Revise);
        assert!(!gates.has_new_work_since_review());
        assert_eq!(gates.delivery_state("")["earlier_review_verdicts"], serde_json::json!(["revise"]));
        gates.events.push(event("search_replace a.rs", false));
        assert!(gates.has_new_work_since_review());
    }

    #[test]
    fn failed_review_is_recorded_without_claiming_a_completed_consult() {
        let mut gates = ReasoningGates::default();
        gates.note_review_attempt();
        gates.note_review_unavailable();
        assert!(gates.consults().is_empty());
        assert!(!gates.has_new_work_since_review());
        assert_eq!(
            gates.delivery_state("")["earlier_review_verdicts"],
            serde_json::json!(["unavailable"])
        );
        gates.note_tool_result(
            "",
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &ToolOutput::Text("new evidence".to_owned().into()),
        );
        assert!(gates.has_new_work_since_review());
    }

    #[test]
    fn selected_flow_requires_a_plan_and_stops_after_three_revision_verdicts() {
        let mut gates = ReasoningGates::default();
        assert_eq!(gates.flow_phase(), FlowPhase::Worker);
        gates.require_plan();
        assert_eq!(gates.flow_phase(), FlowPhase::Planning);
        assert!(!gates.flow_requires_review());
        gates.note_consult(ConsultKind::Plan);
        assert_eq!(gates.flow_phase(), FlowPhase::Executing);
        assert!(gates.flow_requires_review());
        gates.note_review_artifact("first diff\nChecks: passed".to_owned());
        assert!(!gates.has_new_review_artifact("first diff\nChecks: passed"));
        assert!(gates.has_new_review_artifact("corrected diff\nChecks: passed"));
        for _ in 0..2 {
            gates.note_verdict(ReviewVerdict::Revise);
            assert_eq!(gates.flow_phase(), FlowPhase::Correcting);
        }
        gates.note_verdict(ReviewVerdict::Revise);
        assert_eq!(gates.flow_phase(), FlowPhase::Unavailable);
        assert!(!gates.flow_requires_review());
    }

    /// Reviews Jev chose had no limit, so one turn sent the Worker back
    /// fourteen times; they now stop at the three revisions a planned request
    /// allows.
    #[test]
    fn reviews_jev_chose_stop_after_three_revision_verdicts() {
        let mut gates = ReasoningGates::default();
        for _ in 0..2 {
            gates.note_verdict(ReviewVerdict::Revise);
            assert_eq!(gates.flow_phase(), FlowPhase::Worker);
        }
        gates.note_verdict(ReviewVerdict::Revise);
        assert_eq!(gates.flow_phase(), FlowPhase::Unavailable);
    }

    #[test]
    fn review_keeps_earlier_failed_checks_even_after_a_later_pass() {
        use distill_tools::types::output::BashOutput;

        let check = |command: &str, timed_out| {
            ToolOutput::Bash(BashOutput {
                output: Vec::new(),
                output_for_prompt: if timed_out { "timed out" } else { "passed" }.to_owned(),
                exit_code: 0,
                command: command.to_owned(),
                truncated: false,
                signal: None,
                timed_out,
                description: None,
                current_dir: "/tmp".to_owned(),
                output_file: String::new(),
                total_bytes: 0,
                output_delta: None,
                was_bare_echo: false,
            })
        };
        let mut gates = ReasoningGates::default();
        for (command, timed_out) in [("cargo test parser", true), ("cargo test lexer", false)] {
            gates.note_tool_result(
                "",
                "run_terminal_command",
                &serde_json::json!({"command": command}),
                &check(command, timed_out),
            );
        }
        let state = gates.delivery_state("");
        assert_eq!(state["checks"][0]["failed"], true);
        assert_eq!(state["last_check"]["failed"], false);
        let review = gates.review_checks();
        assert!(review.contains("`cargo test parser` failed"));
        assert!(review.contains("`cargo test lexer` passed"));
    }

    /// The reviewer sees whole diffs while they fit, then names what it could
    /// not see instead of silently dropping it. The newest edits win the room:
    /// a re-review that could not see the fix kept asking for it.
    #[test]
    fn review_changes_lists_what_does_not_fit() {
        let gates = ReasoningGates {
            changes: vec![
                change("small.rs", "a", "b"),
                ChangeRecord {
                    // Fits alone (header + diff), but leaves no room for the older file.
                    diff: "x".repeat(REVIEW_DIFF_BYTES - 30),
                    ..change("big.rs", "a", "b")
                },
            ],
            ..Default::default()
        };
        let text = gates.review_changes();
        assert!(text.starts_with("--- big.rs (+1 -1)"), "{}", &text[..40]);
        assert!(text.contains("[diff omitted for: small.rs (+1 -1)]"));
        assert!(text.len() <= REVIEW_DIFF_BYTES + 100);
        let newest_last = ReasoningGates {
            changes: vec![gates.changes[1].clone(), gates.changes[0].clone()],
            ..Default::default()
        };
        let text = newest_last.review_changes();
        assert!(
            text.starts_with("--- small.rs (+1 -1)"),
            "the newest edit is kept: {}",
            &text[..40]
        );
        assert!(text.contains("[diff omitted for: big.rs (+1 -1)]"));
    }

    #[test]
    fn the_review_verdict_reads_the_first_line_only() {
        assert_eq!(review_verdict("VERDICT: approve\nLooks right."), ReviewVerdict::Approve);
        assert_eq!(review_verdict("\n**Verdict: revise**\n- fix x"), ReviewVerdict::Revise);
        assert_eq!(review_verdict("verdict: REVISE."), ReviewVerdict::Unclear);
        assert_eq!(review_verdict("\nVERDICT: revise"), ReviewVerdict::Unclear);
        assert_eq!(
            review_verdict("Looks fine.\nVERDICT: revise"),
            ReviewVerdict::Unclear,
            "a verdict buried in prose is not a verdict"
        );
        assert_eq!(review_verdict(""), ReviewVerdict::Unclear);
    }

    #[test]
    fn a_call_signature_is_the_tool_and_its_command_or_arguments() {
        assert_eq!(
            call_signature("bash", &serde_json::json!({"command": "cargo   test  foo"})),
            "bash cargo test foo"
        );
        let read = call_signature("read_file", &serde_json::json!({"path": "a.rs", "offset": 10}));
        assert!(
            read.starts_with("read_file {")
                && read.contains(r#""path":"a.rs""#)
                && read.contains(r#""offset":10"#),
            "{read}"
        );
        let long = "x".repeat(500);
        assert_eq!(
            call_signature("bash", &serde_json::json!({ "command": long }))
                .chars()
                .count(),
            SIGNATURE_CHARS
        );
    }

    fn call(id: &str, name: &str, arguments: &str) -> distill_sampling_types::ToolCall {
        distill_sampling_types::ToolCall {
            id: id.into(),
            name: name.to_owned(),
            arguments: arguments.into(),
        }
    }

    /// The reasoning model reads the work of this request only, each result
    /// under the call that produced it, or it could not tell what a result
    /// answers; an earlier request's work is not its business.
    #[test]
    fn work_since_request_names_each_result_by_its_call() {
        let mut first = ConversationItem::assistant_tool_calls(vec![call(
            "c1",
            "read_file",
            r#"{"path":"parser.rs"}"#,
        )]);
        if let ConversationItem::Assistant(assistant) = &mut first {
            assistant.content = "Reading the parser.".into();
        }
        let items = vec![
            ConversationItem::user("Old request"),
            ConversationItem::assistant("old work"),
            ConversationItem::user("Fix the parser"),
            first,
            ConversationItem::tool_result("c1", "fn parse() {}"),
            ConversationItem::tool_result("unknown", "orphan output"),
            ConversationItem::system_reminder("advice"),
        ];
        let work = work_since_request(&items, None);
        assert_eq!(
            work,
            vec![
                WorkEntry {
                    source: MAIN_MODEL_SOURCE.to_owned(),
                    text: "Reading the parser.".to_owned(),
                },
                WorkEntry {
                    source: r#"read_file {"path":"parser.rs"}"#.to_owned(),
                    text: "fn parse() {}".to_owned(),
                },
                WorkEntry {
                    source: "tool".to_owned(),
                    text: "orphan output".to_owned(),
                },
            ]
        );
        assert_eq!(
            work[1].summary(),
            r#"read_file {"path":"parser.rs"} (13 bytes): fn parse() {}"#
        );
    }

    /// Each consult resends the thread unchanged and adds only new work, so
    /// the provider can reuse the cached prefix and nothing is paid for twice;
    /// a failed consult carries nothing, and a thread whose instructions
    /// changed starts over rather than mixing two main models.
    #[test]
    fn the_thread_resends_its_prefix_and_carries_only_new_work() {
        let mut thread = ReasoningThread::default();
        let key = thread.cache_key();
        assert_eq!(thread.begin("system A", 3), 0);
        assert!(thread.is_fresh());
        let first = thread.with("request + work 0..3 + Task: plan");
        assert_eq!(first.len(), 2);
        thread.record("request + work 0..3 + Task: plan", "the plan", 3);

        assert_eq!(thread.begin("system A", 5), 3, "only work 3..5 is new");
        assert!(!thread.is_fresh());
        let second = thread.with("work 3..5 + Task: recover");
        assert_eq!(second.len(), 4);
        assert_eq!(
            second[..2]
                .iter()
                .map(ConversationItem::text_content)
                .collect::<Vec<_>>(),
            ["system A", "request + work 0..3 + Task: plan"],
            "the prefix is sent exactly as before"
        );
        assert_eq!(second[2].text_content(), "the plan");
        // Not answered: nothing recorded, the same work is still unsent.
        assert_eq!(thread.begin("system A", 5), 3);

        assert_eq!(thread.begin("system A", 2), 0, "work that shrank is sent again");
        assert_eq!(thread.cache_key(), key, "one key for the whole request");
        assert_eq!(thread.begin("system B", 5), 0);
        assert!(thread.is_fresh(), "new instructions start a new thread");
    }

    /// Only commands that check the work count as test evidence.
    #[test]
    fn check_commands_are_tests_builds_type_checks_and_lints() {
        for command in [
            "cargo test -p distill-shell",
            "npm run test",
            "pnpm build",
            "uv run pytest -q",
            "python -m pytest",
            "npx tsc --noEmit",
            "go vet ./...",
            "cd api && make test",
            "python -m unittest",
            // Browser, device and native runs are the evidence reviewers kept
            // asking for; missing them made every delivery look unchecked.
            "npx playwright test e2e/web/onboarding.spec.ts",
            "pnpm exec playwright test",
            "npm run e2e",
            "npm run test:e2e",
            "yarn e2e",
            "npx detox test -c ios.sim.debug",
            "maestro test flows/onboarding.yaml",
            "npx cypress run",
            "xcodebuild -scheme App -destination 'platform=macOS' build",
            "xcodebuild test -scheme App",
        ] {
            assert!(looks_like_check_command(command), "{command}");
        }
        for command in [
            "ls -la",
            "cat Cargo.toml",
            "git status",
            "test -f a.txt",
            "npm install",
            "npx playwright install chromium",
            "npx cypress open",
            "npm run dev",
            "npx expo start --web",
            "xcodebuild -list",
        ] {
            assert!(!looks_like_check_command(command), "{command}");
        }
    }

    /// A suite started in the background is evidence once it finishes: its
    /// result arrives through the task output, not a shell result.
    #[test]
    fn a_finished_background_check_is_recorded_but_a_running_one_is_not() {
        let task = |status: &str, exit_code| {
            ToolOutput::TaskOutput(TaskOutputOutput::Result(TaskOutputResult {
                task_id: "bg-1".to_owned(),
                command: "npx playwright test e2e/web/zz-goal-proof.spec.ts".to_owned(),
                status: status.to_owned(),
                exit_code,
                output: "1 passed (14.1s)".to_owned(),
                ..Default::default()
            }))
        };
        let mut gates = ReasoningGates::default();
        gates.note_tool_result(
            "",
            "get_task_output",
            &serde_json::json!({}),
            &task("running", None),
        );
        assert_eq!(
            gates.review_checks(),
            "No build, test or lint check was recorded."
        );
        gates.note_tool_result(
            "",
            "get_task_output",
            &serde_json::json!({}),
            &task("completed", Some(0)),
        );
        assert!(
            gates
                .review_checks()
                .contains("`npx playwright test e2e/web/zz-goal-proof.spec.ts` passed"),
            "{}",
            gates.review_checks()
        );
        gates.note_tool_result(
            "",
            "get_task_output",
            &serde_json::json!({}),
            &task("completed", Some(1)),
        );
        assert!(gates.last_test().is_some_and(|check| check.failed));
    }
}
