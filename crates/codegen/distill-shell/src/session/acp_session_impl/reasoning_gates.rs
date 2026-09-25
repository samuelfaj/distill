// Modified for Distill by Samuel Fajreldines, 2026.
//! When the main model consults the reasoning model.
//!
//! The main model runs every step. Jev decides every consult, from the facts
//! this module keeps about the request:
//!
//! * **plan**: once per request, whether the reasoning model plans it, now or
//!   once the main model has looked at the workspace.
//! * **step**: each round with work the reasoning model has not weighed yet,
//!   whether the main model is stuck and needs advice before the next round.
//!   Repeated failures, loops, undone edits and rounds since the last advice
//!   are facts Jev reads, not triggers.
//! * **review**: before delivery, whether the reasoning model reviews the work.
//!
//! An edit the change review (C4) flags is Jev's decision already and is
//! reviewed as it comes. Nothing here counts toward a fixed budget: the only
//! rules left are facts (nothing new since the last consult or review means
//! there is nothing to decide).
//!
//! The consults of one request share a [`ReasoningThread`]: each one resends
//! the thread unchanged and appends only the work since the last reply, so the
//! reasoning model sees every piece of work once and its provider can serve
//! the repeated prefix from the prompt cache.

use distill_sampling_types::ConversationItem;
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
    pub(crate) failed: bool,
    pub(crate) excerpt: String,
}

/// Where the once-per-request plan gate stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PlanGate {
    /// Not judged yet: the first round of the request asks Jev.
    #[default]
    Undecided,
    /// A plan is wanted once the main model has tool results to plan on.
    AfterEvidence,
    /// Planned, or judged unnecessary.
    Done,
}

/// The question Jev answers for this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundQuestion {
    Plan,
    Step,
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
    changes: Vec<ChangeRecord>,
    last_test: Option<TestEvidence>,
    checks: Vec<TestEvidence>,
    plan: PlanGate,
    plan_unavailable: bool,
    planner_advice: Option<String>,
    complexity: Option<f64>,
    last_consult_round: Option<u32>,
    events_at_last_consult: usize,
    changes_at_last_consult: usize,
    consults: Vec<ConsultKind>,
    /// Tool results and changes the last review attempted to inspect.
    review_attempted_upto: Option<(usize, usize)>,
    verdicts: Vec<&'static str>,
    /// Answers a battery shared with another decision gave for this round.
    round_answers: Option<(RoundQuestion, JevAnswerSet)>,
    thread: ReasoningThread,
}

impl ReasoningGates {
    /// One main-model call of this request.
    pub(crate) fn note_round(&mut self) {
        self.rounds = self.rounds.saturating_add(1);
    }

    /// Records the facts one finished tool call adds to the request.
    pub(crate) fn note_tool_result(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
        output: &ToolOutput,
    ) {
        if self.events.len() < MAX_EVENTS {
            self.events.push(ToolEvent {
                signature: call_signature(tool, args),
                failed: output.is_error(),
                polling: matches!(
                    output,
                    ToolOutput::TaskOutput(_)
                        | ToolOutput::KillTask(_)
                        | ToolOutput::Monitor(_)
                        | ToolOutput::Todo(_)
                        | ToolOutput::SchedulerList(_)
                ),
            });
        }
        self.changes.extend(change_records(output));
        if let ToolOutput::Bash(bash) = output
            && looks_like_check_command(&bash.command)
        {
            let check = TestEvidence {
                command: bash.command.chars().take(SIGNATURE_CHARS).collect(),
                failed: bash.exit_code != 0 || bash.timed_out || bash.signal.is_some(),
                excerpt: tail_chars(&bash.output_for_prompt, TEST_EXCERPT_CHARS),
            };
            self.last_test = Some(check.clone());
            if self.checks.len() == MAX_CHECKS {
                self.checks.remove(0);
            }
            self.checks.push(check);
        }
    }

    pub(crate) fn plan(&self) -> PlanGate {
        self.plan
    }

    pub(crate) fn set_plan(&mut self, plan: PlanGate) {
        self.plan = plan;
    }

    pub(crate) fn note_plan_unavailable(&mut self) {
        self.plan = PlanGate::Done;
        self.plan_unavailable = true;
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
            PlanGate::Done => (self.events.len() > self.events_at_last_consult
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
        let events = self
            .events
            .get(self.events_at_last_consult..)
            .unwrap_or_default();
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

    /// What a step decision reads besides the round's own step.
    pub(crate) fn step_state(&self) -> serde_json::Value {
        serde_json::json!({
            "since_last_advice": self.step_facts(),
            "consults_this_request": self.consult_labels(),
            "rounds_this_request": self.rounds,
            "request_complexity": self.complexity,
        })
    }

    /// Books one consult: later decisions weigh only what came after it. Any
    /// advice before delivery also settles the plan: the reasoning model has
    /// already oriented the request.
    pub(crate) fn note_consult(&mut self, kind: ConsultKind) {
        self.consults.push(kind);
        self.last_consult_round = Some(self.rounds);
        self.events_at_last_consult = self.events.len();
        self.changes_at_last_consult = self.changes.len();
        match kind {
            ConsultKind::Plan | ConsultKind::Recover | ConsultKind::EditReview => {
                self.plan = PlanGate::Done;
                if kind == ConsultKind::Plan {
                    self.plan_unavailable = false;
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
            self.events.len() > events || self.changes.len() > changes
        })
    }

    /// A failed review must not count as advice, but the same delivery should
    /// not retry an unavailable endpoint indefinitely.
    pub(crate) fn note_review_attempt(&mut self) {
        self.review_attempted_upto = Some((self.events.len(), self.changes.len()));
    }

    pub(crate) fn note_review_unavailable(&mut self) {
        self.verdicts.push("unavailable");
    }

    pub(crate) fn note_verdict(&mut self, verdict: ReviewVerdict) {
        self.verdicts.push(match verdict {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::Revise => "revise",
            ReviewVerdict::Unclear => "unclear",
        });
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
            "tool_calls": self.events.len(),
            "failed_tool_calls": self.events.iter().filter(|event| event.failed).count(),
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
    pub(crate) fn review_changes(&self) -> String {
        let mut out = String::new();
        let mut omitted = Vec::new();
        for change in &self.changes {
            let block = format!(
                "--- {} (+{} -{})\n{}\n",
                change.path, change.added, change.removed, change.diff
            );
            if out.len().saturating_add(block.len()) <= REVIEW_DIFF_BYTES {
                out.push_str(&block);
            } else {
                omitted.push(format!("{} (+{} -{})", change.path, change.added, change.removed));
            }
        }
        if !omitted.is_empty() {
            out.push_str(&format!("[diff omitted for: {}]\n", omitted.join(", ")));
        }
        out
    }

    pub(crate) fn last_test(&self) -> Option<&TestEvidence> {
        self.last_test.as_ref()
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
        let failures = self.events.iter().filter(|event| event.failed).count();
        let (added, removed) = self.changes.iter().fold((0u64, 0u64), |(a, r), change| {
            (a.saturating_add(change.added), r.saturating_add(change.removed))
        });
        format!(
            "rounds={} tools={} failures={failures} changes={} (+{added} -{removed}) \
             complexity={} plan_unavailable={} consults=[{}] reviews=[{}]",
            self.rounds,
            self.events.len(),
            self.changes.len(),
            self.complexity
                .map_or_else(|| "unknown".to_owned(), |c| format!("{c:.2}")),
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
pub(crate) fn work_since_request(items: &[ConversationItem]) -> Vec<WorkEntry> {
    let start = items
        .iter()
        .rposition(distill_chat_state::compaction_utils::is_real_user_turn)
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
/// Anything else is unclear, and an unclear review lets the main model deliver.
pub(crate) fn review_verdict(text: &str) -> ReviewVerdict {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let first = first.trim_matches(|c: char| matches!(c, '*' | '#' | '`' | '_' | ' '));
    let Some(verdict) = first.strip_prefix("verdict:") else {
        return ReviewVerdict::Unclear;
    };
    let verdict = verdict.trim_matches(|c: char| matches!(c, '*' | '`' | '_' | ' ' | '.'));
    if verdict.starts_with("approve") {
        ReviewVerdict::Approve
    } else if verdict.starts_with("revise") {
        ReviewVerdict::Revise
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
    const RUNNERS: &[&str] = &[
        "cargo", "npm", "pnpm", "yarn", "bun", "go", "make", "mix", "dotnet", "deno", "swift",
        "flutter", "gradle", "./gradlew", "mvn", "uv", "poetry",
    ];
    const VERBS: &[&str] = &[
        "test", "tests", "check", "build", "lint", "typecheck", "vet", "clippy", "nextest",
    ];
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
        if !RUNNERS.contains(word) {
            return false;
        }
        // `npm run test`, `uv run pytest`: the verb may sit after `run`.
        words
            .get(index + 1..(index + 3).min(words.len()))
            .unwrap_or_default()
            .iter()
            .any(|next| VERBS.contains(next) || CHECK_TOOLS.contains(next))
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
        assert!(gates.has_new_work_since_review(), "a plain answer can be reviewed too");
        gates.changes = vec![change("a.rs", "x", "y"), change("a.rs", "y", "z"), change("b.rs", "p", "q")];
        gates.last_test = Some(TestEvidence {
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
        assert_eq!(gates.delivery_state("")["earlier_review_verdicts"], serde_json::json!(["unavailable"]));
        gates.note_tool_result(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &ToolOutput::Text("new evidence".to_owned().into()),
        );
        assert!(gates.has_new_work_since_review());
    }

    #[test]
    fn review_keeps_earlier_failed_checks_even_after_a_later_pass() {
        use distill_tools::types::output::BashOutput;

        let check = |command: &str, timed_out| ToolOutput::Bash(BashOutput {
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
        });
        let mut gates = ReasoningGates::default();
        for (command, timed_out) in [("cargo test parser", true), ("cargo test lexer", false)] {
            gates.note_tool_result(
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
    /// not see instead of silently dropping it.
    #[test]
    fn review_changes_lists_what_does_not_fit() {
        let gates = ReasoningGates {
            changes: vec![
                ChangeRecord {
                    // Fits alone (header + diff), but leaves no room for the next file.
                    diff: "x".repeat(REVIEW_DIFF_BYTES - 30),
                    ..change("big.rs", "a", "b")
                },
                change("small.rs", "a", "b"),
            ],
            ..Default::default()
        };
        let text = gates.review_changes();
        assert!(text.starts_with("--- big.rs (+1 -1)"), "{}", &text[..40]);
        assert!(text.contains("[diff omitted for: small.rs (+1 -1)]"));
        assert!(text.len() <= REVIEW_DIFF_BYTES + 100);
    }

    #[test]
    fn the_review_verdict_reads_the_first_line_only() {
        assert_eq!(review_verdict("VERDICT: approve\nLooks right."), ReviewVerdict::Approve);
        assert_eq!(review_verdict("\n**Verdict: revise**\n- fix x"), ReviewVerdict::Revise);
        assert_eq!(review_verdict("verdict: REVISE."), ReviewVerdict::Revise);
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
        let work = work_since_request(&items);
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
        ] {
            assert!(looks_like_check_command(command), "{command}");
        }
        for command in ["ls -la", "cat Cargo.toml", "git status", "test -f a.txt", "npm install"] {
            assert!(!looks_like_check_command(command), "{command}");
        }
    }
}
