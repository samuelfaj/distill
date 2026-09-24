// Modified for Distill by Samuel Fajreldines, 2026.
//! When the main model consults the reasoning model.
//!
//! The main model runs every step. The reasoning model is consulted only at a
//! few decision points, each with its own trigger:
//!
//! * **plan**: once per request. Jev judges whether the request needs a plan
//!   and how complex it is; a request that acts on the workspace is planned
//!   after the main model's first tool results, so the plan rests on evidence.
//! * **recover**: deterministic signs that the main model is stuck: the same
//!   call failing twice, three failures in a row, the same call issued three
//!   times, an edit undone, or a long request without advice.
//! * **review**: before the main model delivers a request that changed files,
//!   unless the change is trivial.
//!
//! A per-request budget and a cooldown bound the cost. Everything here is pure
//! over the facts the session records, so each rule is testable on its own.
//!
//! The consults of one request share a [`ReasoningThread`]: each one resends
//! the thread unchanged and appends only the work since the last reply, so the
//! reasoning model sees every piece of work once and its provider can serve
//! the repeated prefix from the prompt cache.

use distill_sampling_types::ConversationItem;
use distill_tools::types::output::{ApplyPatchOutput, SearchReplaceOutput, ToolOutput};

/// Recovery consults (struggle signals and flagged edits) per request.
pub(crate) const MAX_RECOVERIES: u32 = 2;
/// Main-model rounds between two recovery consults.
pub(crate) const RECOVERY_COOLDOWN_ROUNDS: u32 = 2;
/// Rounds without advice after which the request gets one checkpoint.
pub(crate) const LONG_REQUEST_ROUNDS: u32 = 20;
/// Changed lines up to which a delivery is trivial, when nothing failed after
/// the change and Jev did not judge the request complex.
pub(crate) const TRIVIAL_CHANGE_LINES: u64 = 6;
/// B1 complexity (0..=1) from which a request needs planning: "multi-file work
/// with investigation" and above.
pub(crate) const COMPLEX_REQUEST: f64 = 0.5;
/// Bytes of diff handed to a delivery review; the rest is listed by file.
pub(crate) const REVIEW_DIFF_BYTES: usize = 24_000;
/// Consecutive failed tool results that count as being stuck.
const CONSECUTIVE_FAILURES: usize = 3;
/// Times the same call may run before it counts as a loop.
const REPEATED_CALLS: usize = 3;
/// Characters of a call kept as its identity.
const SIGNATURE_CHARS: usize = 200;
/// Tool results remembered per request.
const MAX_EVENTS: usize = 400;
/// Bytes of one change's diff kept when it is recorded.
const CHANGE_DIFF_BYTES: usize = 8_000;
/// Characters of a check's output kept as evidence.
const TEST_EXCERPT_CHARS: usize = 2_000;
/// Characters of a work item's first line in its one-line summary.
const SUMMARY_CHARS: usize = 160;
/// Characters of a tool call's arguments naming the result it produced.
const SOURCE_ARGS_CHARS: usize = 120;
/// Source of a work item the main model wrote itself.
const MAIN_MODEL_SOURCE: &str = "main model";

/// One finished tool call, as the struggle signals read it.
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

/// What the plan gate does with Jev's answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanDecision {
    MainAlone,
    ConsultNow,
    AfterEvidence,
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

/// A deterministic sign that the main model is not managing on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Struggle {
    SameCallFailedTwice(String),
    ConsecutiveFailures(usize),
    RepeatedCall(String),
    EditReverted(String),
    LongRequest(u32),
}

impl Struggle {
    /// Short label for the decision log.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::SameCallFailedTwice(_) => "same-call-failed",
            Self::ConsecutiveFailures(_) => "consecutive-failures",
            Self::RepeatedCall(_) => "repeated-call",
            Self::EditReverted(_) => "edit-reverted",
            Self::LongRequest(_) => "long-request",
        }
    }

    /// What the reasoning model and the main model are told.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::SameCallFailedTwice(call) => {
                format!("the same call failed twice: `{call}`")
            }
            Self::ConsecutiveFailures(count) => format!("the last {count} tool calls failed"),
            Self::RepeatedCall(call) => {
                format!("the same call ran {REPEATED_CALLS} times: `{call}`")
            }
            Self::EditReverted(path) => format!("an edit to `{path}` was undone"),
            Self::LongRequest(rounds) => {
                format!("{rounds} rounds without a plan or advice")
            }
        }
    }
}

/// Whether a finished request gets a delivery review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewNeed {
    Skip(&'static str),
    Review,
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
    plan: PlanGate,
    complexity: Option<f64>,
    recoveries: u32,
    reviewed: bool,
    last_consult_round: Option<u32>,
    events_at_last_consult: usize,
    changes_at_last_consult: usize,
    consults: Vec<ConsultKind>,
    /// Gates already logged as blocked, so a lasting signal is logged once.
    blocked: std::collections::HashSet<String>,
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
            self.last_test = Some(TestEvidence {
                command: bash.command.chars().take(SIGNATURE_CHARS).collect(),
                failed: bash.exit_code != 0,
                excerpt: tail_chars(&bash.output_for_prompt, TEST_EXCERPT_CHARS),
            });
        }
    }

    pub(crate) fn plan(&self) -> PlanGate {
        self.plan
    }

    pub(crate) fn set_plan(&mut self, plan: PlanGate) {
        self.plan = plan;
    }

    /// Whether the main model already has tool results to plan on.
    pub(crate) fn has_evidence(&self) -> bool {
        !self.events.is_empty()
    }

    /// How complex Jev judged the request at the plan gate.
    pub(crate) fn note_assessment(&mut self, complexity: Option<f64>) {
        self.complexity = complexity;
    }

    /// The reasoning model's side of this request.
    pub(crate) fn thread(&mut self) -> &mut ReasoningThread {
        &mut self.thread
    }

    /// The first sign, since the last consult, that the main model is stuck.
    /// A consult resets the window, so one signal never triggers twice.
    pub(crate) fn struggle(&self) -> Option<Struggle> {
        let events = self
            .events
            .get(self.events_at_last_consult..)
            .unwrap_or_default();
        let mut failures: std::collections::HashMap<&str, usize> = Default::default();
        for event in events.iter().filter(|event| event.failed) {
            let count = failures.entry(event.signature.as_str()).or_default();
            *count += 1;
            if *count >= 2 {
                return Some(Struggle::SameCallFailedTwice(event.signature.clone()));
            }
        }
        let trailing = events.iter().rev().take_while(|event| event.failed).count();
        if trailing >= CONSECUTIVE_FAILURES {
            return Some(Struggle::ConsecutiveFailures(trailing));
        }
        let mut calls: std::collections::HashMap<&str, usize> = Default::default();
        for event in events.iter().filter(|event| !event.polling) {
            let count = calls.entry(event.signature.as_str()).or_default();
            *count += 1;
            if *count >= REPEATED_CALLS {
                return Some(Struggle::RepeatedCall(event.signature.clone()));
            }
        }
        let changes = self
            .changes
            .get(self.changes_at_last_consult..)
            .unwrap_or_default();
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
            if undoes {
                return Some(Struggle::EditReverted(later.path.clone()));
            }
        }
        let since = self
            .rounds
            .saturating_sub(self.last_consult_round.unwrap_or(0));
        (since >= LONG_REQUEST_ROUNDS).then_some(Struggle::LongRequest(since))
    }

    /// Whether another recovery consult fits the budget and the cooldown.
    pub(crate) fn may_recover(&self) -> Result<(), &'static str> {
        if self.recoveries >= MAX_RECOVERIES {
            return Err("budget");
        }
        if self
            .last_consult_round
            .is_some_and(|round| self.rounds.saturating_sub(round) < RECOVERY_COOLDOWN_ROUNDS)
        {
            return Err("cooldown");
        }
        Ok(())
    }

    /// Books one consult: it spends its budget and resets the struggle window.
    /// Any advice before delivery also settles the plan gate: the reasoning
    /// model has already oriented the request.
    pub(crate) fn note_consult(&mut self, kind: ConsultKind) {
        self.consults.push(kind);
        self.last_consult_round = Some(self.rounds);
        self.events_at_last_consult = self.events.len();
        self.changes_at_last_consult = self.changes.len();
        match kind {
            ConsultKind::Plan => self.plan = PlanGate::Done,
            ConsultKind::Recover | ConsultKind::EditReview => {
                self.recoveries = self.recoveries.saturating_add(1);
                self.plan = PlanGate::Done;
            }
            ConsultKind::Review => self.reviewed = true,
        }
    }

    /// `true` the first time `gate` is blocked in this request, so the log
    /// names a lasting signal once instead of on every round.
    pub(crate) fn first_block(&mut self, gate: String) -> bool {
        self.blocked.insert(gate)
    }

    /// The consults of this request, in order.
    pub(crate) fn consults(&self) -> &[ConsultKind] {
        &self.consults
    }

    /// Whether the finished request gets a delivery review.
    pub(crate) fn delivery_review_need(&self) -> ReviewNeed {
        if self.reviewed {
            return ReviewNeed::Skip("already-reviewed");
        }
        if self.changes.is_empty() {
            return ReviewNeed::Skip("no-changes");
        }
        let lines: u64 = self
            .changes
            .iter()
            .map(|change| change.added.saturating_add(change.removed))
            .sum();
        let failing = self.last_test.as_ref().is_some_and(|test| test.failed)
            || self.events.last().is_some_and(|event| event.failed);
        let complex = self
            .complexity
            .is_some_and(|complexity| complexity >= COMPLEX_REQUEST);
        if lines <= TRIVIAL_CHANGE_LINES && !failing && !complex {
            return ReviewNeed::Skip("trivial-change");
        }
        ReviewNeed::Review
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

    /// One line for the decision log at the end of the request.
    pub(crate) fn summary(&self) -> String {
        let failures = self.events.iter().filter(|event| event.failed).count();
        let (added, removed) = self.changes.iter().fold((0u64, 0u64), |(a, r), change| {
            (a.saturating_add(change.added), r.saturating_add(change.removed))
        });
        let consults: Vec<&str> = self.consults.iter().map(|kind| kind.label()).collect();
        format!(
            "rounds={} tools={} failures={failures} changes={} (+{added} -{removed}) \
             complexity={} consults=[{}]",
            self.rounds,
            self.events.len(),
            self.changes.len(),
            self.complexity
                .map_or_else(|| "unknown".to_owned(), |c| format!("{c:.2}")),
            consults.join(", ")
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

/// The plan gate's decision. A confident Jev answer decides whether the request
/// needs a plan; an unsure one falls back to how complex Jev judged it, and an
/// unknown complexity leaves the main model alone (the struggle and delivery
/// gates still watch it). A request that only asks for an answer is planned
/// now; one that works on the workspace is planned after the main model's
/// first tool results, so the plan rests on what it found.
pub(crate) fn plan_decision(
    consult: Option<bool>,
    complexity: Option<f64>,
    intent: Option<&str>,
) -> PlanDecision {
    let needs_plan = consult.unwrap_or_else(|| {
        complexity.is_some_and(|complexity| complexity >= COMPLEX_REQUEST)
    });
    if !needs_plan {
        PlanDecision::MainAlone
    } else if intent == Some("question") {
        PlanDecision::ConsultNow
    } else {
        PlanDecision::AfterEvidence
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

    fn gates_with(events: Vec<ToolEvent>) -> ReasoningGates {
        ReasoningGates {
            events,
            ..Default::default()
        }
    }

    /// Planning a request the main model can do alone wastes a reasoning call;
    /// not planning a complex one lets it act without a plan. A confident
    /// answer decides, an unsure one follows complexity, and nothing known
    /// means the main model works alone (the other gates still watch it).
    #[test]
    fn the_plan_gate_follows_confidence_then_complexity() {
        use PlanDecision::*;
        assert_eq!(plan_decision(Some(false), Some(1.0), Some("edit")), MainAlone);
        assert_eq!(plan_decision(Some(true), Some(0.0), Some("edit")), AfterEvidence);
        assert_eq!(plan_decision(None, Some(0.67), Some("edit")), AfterEvidence);
        assert_eq!(plan_decision(None, Some(0.33), Some("edit")), MainAlone);
        assert_eq!(plan_decision(None, None, None), MainAlone);
    }

    /// A question is answered without tools, so its plan cannot wait for
    /// evidence; work on the workspace is planned on what the main model found.
    #[test]
    fn answers_are_planned_now_and_workspace_work_after_evidence() {
        use PlanDecision::*;
        assert_eq!(plan_decision(Some(true), None, Some("question")), ConsultNow);
        assert_eq!(plan_decision(Some(true), None, Some("research")), AfterEvidence);
        assert_eq!(plan_decision(Some(true), None, Some("command")), AfterEvidence);
        assert_eq!(plan_decision(Some(true), None, None), AfterEvidence);
    }

    /// Each struggle signal is a concrete sign the main model is not managing:
    /// the same failure twice, a run of failures, a loop, or an edit undone.
    #[test]
    fn struggle_signals_fire_on_repeated_failure_loops_and_undone_edits() {
        let same = gates_with(vec![
            event("bash cargo test foo", true),
            event("read_file a.rs", false),
            event("bash cargo test foo", true),
        ]);
        assert_eq!(
            same.struggle(),
            Some(Struggle::SameCallFailedTwice("bash cargo test foo".to_owned()))
        );

        let run = gates_with(vec![
            event("bash a", true),
            event("bash b", true),
            event("bash c", true),
        ]);
        assert_eq!(run.struggle(), Some(Struggle::ConsecutiveFailures(3)));

        let looped = gates_with(vec![
            event("grep x", false),
            event("grep x", false),
            event("grep x", false),
        ]);
        assert_eq!(looped.struggle(), Some(Struggle::RepeatedCall("grep x".to_owned())));

        let reverted = ReasoningGates {
            changes: vec![change("src/a.rs", "old", "new"), change("src/a.rs", "new", "old")],
            ..Default::default()
        };
        assert_eq!(reverted.struggle(), Some(Struggle::EditReverted("src/a.rs".to_owned())));
    }

    /// Normal progress is not a struggle: one failure that was then fixed,
    /// polling a background task, and edits that move forward stay quiet.
    #[test]
    fn ordinary_progress_is_not_a_struggle() {
        let fixed = gates_with(vec![
            event("bash cargo test", true),
            event("search_replace a.rs", false),
            event("bash cargo test", false),
        ]);
        assert_eq!(fixed.struggle(), None);

        let polling = gates_with(
            (0..5)
                .map(|_| ToolEvent {
                    signature: "task_output t1".to_owned(),
                    failed: false,
                    polling: true,
                })
                .collect(),
        );
        assert_eq!(polling.struggle(), None);

        let forward = ReasoningGates {
            changes: vec![change("src/a.rs", "a", "b"), change("src/a.rs", "b", "c")],
            ..Default::default()
        };
        assert_eq!(forward.struggle(), None);
    }

    /// Advice for a stuck main model already orients the request, so the plan
    /// gate must not pay for a second consult right after it.
    #[test]
    fn a_recovery_settles_the_plan_gate_and_blocks_are_logged_once() {
        let mut gates = ReasoningGates::default();
        gates.set_plan(PlanGate::AfterEvidence);
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.plan(), PlanGate::Done);
        assert!(gates.first_block("recover:budget".to_owned()));
        assert!(!gates.first_block("recover:budget".to_owned()));
    }

    /// A long request gets one checkpoint, counted from the last advice.
    #[test]
    fn a_long_request_without_advice_gets_a_checkpoint() {
        let mut gates = ReasoningGates::default();
        for _ in 0..LONG_REQUEST_ROUNDS - 1 {
            gates.note_round();
        }
        assert_eq!(gates.struggle(), None);
        gates.note_round();
        assert_eq!(gates.struggle(), Some(Struggle::LongRequest(LONG_REQUEST_ROUNDS)));
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.struggle(), None, "the checkpoint resets the window");
    }

    /// A consult resets the window: the failures it was asked about must not
    /// trigger it again, and budget and cooldown bound the cost.
    #[test]
    fn consults_reset_the_window_and_respect_budget_and_cooldown() {
        let mut gates = gates_with(vec![event("bash x", true), event("bash x", true)]);
        assert!(gates.struggle().is_some());
        assert_eq!(gates.may_recover(), Ok(()));
        gates.note_consult(ConsultKind::Recover);
        assert_eq!(gates.struggle(), None);
        assert_eq!(gates.may_recover(), Err("cooldown"));
        gates.note_round();
        gates.note_round();
        assert_eq!(gates.may_recover(), Ok(()));
        gates.note_consult(ConsultKind::EditReview);
        gates.note_round();
        gates.note_round();
        assert_eq!(gates.may_recover(), Err("budget"));
        assert_eq!(
            gates.consults(),
            [ConsultKind::Recover, ConsultKind::EditReview]
        );
    }

    /// A delivery review is for work that changed something that matters: no
    /// change or a tiny, passing one on a simple request is delivered as is;
    /// a larger change, a failing check or a complex request is reviewed, once.
    #[test]
    fn delivery_review_skips_trivial_work_and_reviews_the_rest_once() {
        assert_eq!(
            ReasoningGates::default().delivery_review_need(),
            ReviewNeed::Skip("no-changes")
        );
        let tiny = ReasoningGates {
            changes: vec![change("a.rs", "x", "y")],
            ..Default::default()
        };
        assert_eq!(tiny.delivery_review_need(), ReviewNeed::Skip("trivial-change"));

        let failing = ReasoningGates {
            changes: vec![change("a.rs", "x", "y")],
            last_test: Some(TestEvidence {
                command: "cargo test".to_owned(),
                failed: true,
                excerpt: "1 failed".to_owned(),
            }),
            ..Default::default()
        };
        assert_eq!(failing.delivery_review_need(), ReviewNeed::Review);

        let mut complex = ReasoningGates {
            changes: vec![change("a.rs", "x", "y")],
            ..Default::default()
        };
        complex.note_assessment(Some(0.67));
        assert_eq!(complex.delivery_review_need(), ReviewNeed::Review);

        let mut large = ReasoningGates {
            changes: vec![ChangeRecord {
                added: 40,
                removed: 3,
                ..change("a.rs", "x", "y")
            }],
            ..Default::default()
        };
        assert_eq!(large.delivery_review_need(), ReviewNeed::Review);
        large.note_consult(ConsultKind::Review);
        assert_eq!(large.delivery_review_need(), ReviewNeed::Skip("already-reviewed"));
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
