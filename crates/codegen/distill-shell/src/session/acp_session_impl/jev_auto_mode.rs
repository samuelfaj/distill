// Modified for Distill by Samuel Fajreldines, 2026.
//! B8 (Auto execution mode): with the session in Auto, one Jev decision picks
//! the effective mode (Normal, UltraFast, UltraCode, UltraSave) when a request
//! starts, and each new action may move it up or down when switching pays off.
//! With `b8_up_only` a review only moves it up inside a request; a leaner
//! mode waits for the next request's pick. Every pick reaches clients as
//! `x.ai/session/execution_mode/changed`.

use std::sync::{Arc, Mutex};

use distill_tool_types::ExecutionMode;
use distill_workspace::jev::JevAnswerSet;
use distill_workspace::jev::flags::JevLever;

use super::turn_facts::CheckOutcome;
use super::*;

pub(super) const MODE_QUESTION: &str = "execution_mode";
pub(super) const MODE_CHANGED_METHOD: &str = "x.ai/session/execution_mode/changed";

/// Request start: below this probability on Jev's pick, the current mode
/// stays. 0.0 applies every pick. It must be fitted from recorded decision
/// outcomes before it is raised; a guessed value would move picks blindly.
const REQUEST_START_PROBABILITY_FLOOR: f64 = 0.0;
/// With `b8_review_hygiene`, completed tool rounds a mid-request switch holds.
const SWITCH_DWELL_ROUNDS: usize = 3;
const REQUEST_CHARS: usize = 4_000;

/// Auto's per-request bookkeeping.
#[derive(Debug, Default, Clone)]
pub(crate) struct AutoModeTurn {
    round_at_decision: usize,
    /// Completed tool rounds when the review last switched this request.
    switched_at_round: Option<usize>,
    /// Modes the review left this request, with the failure marks seen then.
    left: Vec<(ExecutionMode, FailureMarks)>,
    /// A review asked without waiting for it (UltraFast); a new request drops
    /// it, which aborts the ask.
    detached: Option<DetachedReview>,
}

/// A detached review's answer: `None` until the ask finishes, then Jev's
/// answer or `None` when it gave none.
type DetachedSlot = Arc<Mutex<Option<Option<JevAnswerSet>>>>;

#[derive(Clone)]
struct DetachedReview {
    answer: DetachedSlot,
    _task: Arc<crate::util::AbortOnDrop>,
}

impl std::fmt::Debug for DetachedReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DetachedReview")
    }
}

enum DetachedAnswer {
    Absent,
    Pending,
    Arrived(Option<JevAnswerSet>),
}

/// Failed tool calls and failed recorded checks so far in the request; both
/// only grow, so a change means a new failure.
type FailureMarks = (u64, usize);

fn failure_marks(facts: &super::turn_facts::TurnFacts) -> FailureMarks {
    (facts.failed_tool_count(), checks_with(facts, CheckOutcome::Failed))
}

fn checks_with(facts: &super::turn_facts::TurnFacts, outcome: CheckOutcome) -> usize {
    facts
        .recorded_checks()
        .iter()
        .filter(|check| check.outcome == outcome)
        .count()
}

/// How a drained request ended, from the facts the harness recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PreviousTurn {
    failed_tool_calls: u64,
    passed_checks: usize,
    failed_checks: usize,
    unknown_checks: usize,
}

impl PreviousTurn {
    pub(crate) fn of(facts: &super::turn_facts::TurnFacts) -> Self {
        Self {
            failed_tool_calls: facts.failed_tool_count(),
            passed_checks: checks_with(facts, CheckOutcome::Passed),
            failed_checks: checks_with(facts, CheckOutcome::Failed),
            unknown_checks: checks_with(facts, CheckOutcome::Unknown),
        }
    }
}

/// The request-start mode: Jev's pick, unless its probability is under
/// `floor`; then `fallback`.
fn request_start_mode(
    picked: ExecutionMode,
    probability: f64,
    fallback: ExecutionMode,
    floor: f64,
) -> ExecutionMode {
    if probability < floor {
        fallback
    } else {
        picked
    }
}

/// The mode a request start falls to below the floor: the mode in force,
/// except that the request-level question sends a session's first request,
/// which has no mode it chose, to Normal.
fn request_start_fallback(
    criteria_v2: bool,
    first_request: bool,
    current: ExecutionMode,
) -> ExecutionMode {
    if criteria_v2 && first_request {
        ExecutionMode::Normal
    } else {
        current
    }
}

/// Whether the mode question is the request-level one (`b8_criteria_v2`).
fn criteria_v2() -> bool {
    crate::jev::lever_active(JevLever::B8CriteriaV2)
}

/// Whether UltraSave changes anything that can run in this Auto session.
/// Under Auto only the runtime row follows the mode, so with no cheaper model
/// and no tier routing UltraSave would differ from Normal only by its
/// directive when its runtime row, tier routing aside, equals Normal's.
fn ultrasave_option_runs(
    no_cheaper_model: bool,
    tier_routing_available: bool,
    ultrasave: distill_tool_types::RuntimePolicy,
    normal: distill_tool_types::RuntimePolicy,
) -> bool {
    if !no_cheaper_model || tier_routing_available {
        return true;
    }
    let without_routing = |mut row: distill_tool_types::RuntimePolicy| {
        row.tier_routing = distill_tool_types::TierRouting::Off;
        row
    };
    without_routing(ultrasave) != without_routing(normal)
}

/// `[modes.ultrasave] model` in the effective config, read once per process
/// (config does not change mid-run).
fn ultrasave_has_no_cheaper_model() -> bool {
    static NO_CHEAPER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_CHEAPER.get_or_init(|| {
        crate::agent::config::ultrasave_has_no_cheaper_model(
            crate::config::load_effective_config().ok().as_ref(),
        )
    })
}

const MODES: [ExecutionMode; 4] = [
    ExecutionMode::Ultrasave,
    ExecutionMode::Ultrafast,
    ExecutionMode::Normal,
    ExecutionMode::Ultracode,
];

fn mode_id(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Normal => "normal",
        ExecutionMode::Ultrafast => "ultrafast",
        ExecutionMode::Ultracode => "ultracode",
        ExecutionMode::Ultrasave => "ultrasave",
    }
}

fn mode_criterion(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Ultrasave => {
            "UltraSave: minimize total money cost with direct, concise work and the cheapest capable model. Best for clear, bounded coding or mechanical work: a known edit, a small implementation with a complete specification, or a nonurgent batch. A long specification or many edge cases alone do not make a known, bounded task difficult. Correctness and required verification are mandatory."
        }
        ExecutionMode::Ultrafast => {
            "UltraFast: minimize time to a verified result for short, bounded actions such as a routine tool read, executing a known check, or delivering an already verified result. Favor it when the user is waiting for a simple interactive action, for urgent work, or when independent parts can usefully run in parallel. A request to take a screenshot of Google Chrome and show it should run directly and immediately. Do not delegate a single quick action. Correctness and required verification are mandatory."
        }
        ExecutionMode::Normal => {
            "Normal: balanced execution on the main model with full tools when the next action has no clear advantage from economy, speed or deeper reasoning."
        }
        ExecutionMode::Ultracode => {
            "UltraCode: extra care and deeper reasoning for an explicit request for care, refactoring that must preserve behavior and invariants, substantial or uncertain logic, subtle diagnosis, or risky design and correctness judgments. These can require UltraCode even in one file. Routine work from a complete, understood specification does not need this mode merely because it involves code or tests. Delegation is optional and only useful for independent parts."
        }
    }
}

/// One option of the request-level question: what the mode is for, what it
/// is not for, and example requests, each describing a request.
struct Criterion {
    is_for: &'static str,
    not_for: &'static str,
    examples: [&'static str; 3],
}

fn mode_criterion_v2(mode: ExecutionMode) -> Criterion {
    match mode {
        ExecutionMode::Ultrasave => Criterion {
            is_for: "Mechanical, fully specified work that is cheap to verify: the whole change is known before it starts and one quick check proves it.",
            not_for: "Requests that need investigation or design judgment, quick questions where only waiting time matters, or changes whose errors are costly.",
            examples: [
                "rename a function across the repository",
                "apply the same import fix to twenty files",
                "bump a dependency version and rebuild",
            ],
        },
        ExecutionMode::Ultrafast => Criterion {
            is_for: "A quick answer or a small, local fix where waiting is the main cost: few steps, little to read, and the result is wanted now.",
            not_for: "Long mechanical batches, multi-step features, or changes whose mistakes would be expensive to find later.",
            examples: [
                "what does this error message mean",
                "take a screenshot of the browser and show it",
                "fix the typo in the README title",
            ],
        },
        ExecutionMode::Normal => Criterion {
            is_for: "Ordinary multi-step software work: read the relevant code, change a few places, and run the checks that cover them.",
            not_for: "Single quick answers, purely mechanical batches, or changes where a hidden invariant or public contract makes an error expensive.",
            examples: [
                "add a command-line flag with a test",
                "fix a failing unit test",
                "implement a small endpoint from a description",
            ],
        },
        ExecutionMode::Ultracode => Criterion {
            is_for: "Changes where a wrong result is expensive: hidden invariants, data migrations, concurrency, or a public contract that other code relies on.",
            not_for: "Routine edits with a complete specification, quick answers, or work that is long but mechanical.",
            examples: [
                "change the stored session format with a migration",
                "fix a race between two background tasks",
                "refactor a public API without changing its behavior",
            ],
        },
    }
}

/// The live mode question with every option, as `[jev.ladder]` selects it.
pub(super) fn mode_question()
-> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
    if criteria_v2() {
        mode_question_v2(&MODES)
    } else {
        mode_question_v1()
    }
}

/// The request-level question over `options`.
fn mode_question_v2(
    options: &[ExecutionMode],
) -> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
    let criteria = options
        .iter()
        .map(|mode| {
            let criterion = mode_criterion_v2(*mode);
            (
                mode_id(*mode).to_owned(),
                serde_json::json!({
                    "for": criterion.is_for,
                    "not_for": criterion.not_for,
                    "examples": criterion.examples,
                }),
            )
        })
        .collect();
    let question = distill_workspace::jev::Question::choice(
        "Which execution mode fits the whole request in `request`? Each option gives what the mode is for, \
         what it is not for, and example requests: choose the option whose `for` matches the request and \
         whose `not_for` does not. `current_mode`, `previous_turn`, `progress` and `micro_action`, when present, \
         are evidence about the work so far, not a separate step to classify.",
        criteria,
    )
    .ok()?;
    Some([(MODE_QUESTION.to_owned(), question)].into())
}

/// The first wording: one sentence per mode, judged on the next action.
fn mode_question_v1()
-> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
    let criteria = MODES
        .iter()
        .map(|mode| {
            (
                mode_id(*mode).to_owned(),
                serde_json::Value::String(mode_criterion(*mode).to_owned()),
            )
        })
        .collect();
    let question = distill_workspace::jev::Question::choice(
        "Which mode best serves the next action or micro-action in `micro_action`? \
         `request` gives the overall objective; choose for this step. Use UltraSave for clear, bounded work whose implementation is understood; \
         UltraFast for short tool actions, known checks, delivery, immediate interactive actions, urgency or useful parallel work; UltraCode for explicit extra care, refactoring or genuinely substantial or uncertain reasoning; \
         otherwise Normal. Minimize total cost and time while delivering the correct, verified result, counting retries and rework. \
         When `current_mode` is set, switch only when the next action benefits enough to justify `switch_cost`; a routine step after difficult work can use a lighter mode. \
         Never delegate a quick single action merely because the mode permits subagents.",
        criteria,
    )
    .ok()?;
    Some([(MODE_QUESTION.to_owned(), question)].into())
}

fn picked_mode(answers: &distill_workspace::jev::JevAnswerSet) -> Option<(ExecutionMode, f64)> {
    let choice = answers.choice(MODE_QUESTION)?;
    let mode = MODES.into_iter().find(|mode| mode_id(*mode) == choice)?;
    let probability = answers
        .probability(MODE_QUESTION, choice)
        .or_else(|| answers.confidence(MODE_QUESTION))
        .unwrap_or(0.0);
    Some((mode, probability))
}

fn bounded(text: &str) -> String {
    if text.chars().count() <= REQUEST_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(REQUEST_CHARS).collect();
    out.push_str(" […]");
    out
}

/// Asks the mode question. With `b8_criteria_v2` and shadow packs on, the
/// first wording rides the same request on the same state and is only
/// recorded, as `shadow:mode_v1:<choice>`, so the two can be compared.
async fn ask_mode(
    state: serde_json::Value,
    questions: std::collections::BTreeMap<String, distill_workspace::jev::Question>,
) -> Option<JevAnswerSet> {
    if !criteria_v2() || !crate::jev_shadow::packs_on() {
        return crate::jev::ask_item(JevLever::B8ExecutionMode, state, questions).await;
    }
    let ([answers], first_wording) = crate::jev::ask_items_with_shadow(
        state,
        [(JevLever::B8ExecutionMode, Some(questions))],
        mode_question_v1().map(|questions| (JevLever::B8ExecutionMode, questions)),
    )
    .await;
    if let Some(first) = first_wording {
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            &format!(
                "shadow:mode_v1:{}",
                first.choice(MODE_QUESTION).unwrap_or("defer")
            ),
            "first mode criteria in shadow",
            first.confidence(MODE_QUESTION),
            Some(&first),
        );
    }
    answers
}

impl SessionActor {
    fn auto_mode_active(&self) -> bool {
        self.execution_mode.is_auto()
    }

    async fn auto_mode_state(
        &self,
        request: &str,
        current: Option<ExecutionMode>,
        rounds: usize,
    ) -> serde_json::Value {
        let conversation = self.chat_state_handle.get_conversation().await;
        let ledger = self.jev_ledger.borrow();
        let action = if current.is_some() {
            describe_micro_action(&conversation, &ledger.facts).as_json()
        } else {
            serde_json::json!({"step": "first_step", "what_it_must_do": bounded(request)})
        };
        let checks_with = |outcome: CheckOutcome| checks_with(&ledger.facts, outcome);
        serde_json::json!({
            "request": bounded(request),
            "micro_action": action,
            "current_mode": current.map(mode_id),
            "subagent_depth": self.tool_context.subagent_depth,
            "switch_cost": {
                "last_prompt_tokens": ledger.last_main_prompt_tokens(),
                "previous_dispatch": ledger.last_execution,
                "note": "Mode reminders append to history; they do not by themselves invalidate the existing prefix. Model or tool-list changes may lose cache; compare that possible cost with the next action's benefit.",
            },
            "progress": {
                "completed_tool_rounds": rounds,
                "failed_tool_calls": ledger.facts.failed_tool_count(),
                "checks_run": ledger.facts.recorded_checks().len(),
                "passed_checks": checks_with(CheckOutcome::Passed),
                "failed_checks": checks_with(CheckOutcome::Failed),
                // The exit code was a filter's and no runner verdict was seen.
                "unknown_checks": checks_with(CheckOutcome::Unknown),
            },
            "worker_model_configured": self.tier_worker_slug().is_some(),
            "tier_routing_available": self.tier_routing_available(),
        })
    }

    /// Whether UltraFast or UltraSave could route rounds to another tier in
    /// this session: `tier_routing_allowed` without its mode check, since the
    /// pick decides the mode.
    fn tier_routing_available(&self) -> bool {
        self.jev_effort_auto.load(std::sync::atomic::Ordering::Relaxed)
            && !self.child_model_routing_locked()
            && !self.child_jev_routing_locked()
            && !self.jev_ledger.borrow().tier_route_off
            && crate::jev::lever_active(JevLever::B2ExecutionTier)
    }

    /// The request-start state. Once a request has drained in this session,
    /// it also names the current mode and how that request ended, so a switch
    /// is weighed against its cost.
    async fn request_start_state(&self, request: &str) -> serde_json::Value {
        let mut state = self.auto_mode_state(request, None, 0).await;
        let previous = self.jev_ledger.borrow().previous_turn.clone();
        if let Some(previous) = previous
            && let Some(fields) = state.as_object_mut()
        {
            let current = mode_id(self.execution_mode.load());
            fields.insert("current_mode".to_owned(), current.into());
            fields.insert(
                "previous_turn".to_owned(),
                serde_json::json!({
                    "mode": current,
                    "failed_tool_calls": previous.failed_tool_calls,
                    "passed_checks": previous.passed_checks,
                    "failed_checks": previous.failed_checks,
                    "unknown_checks": previous.unknown_checks,
                }),
            );
        }
        state
    }

    /// The state of the request-level question: the request once, the mode in
    /// force, and the switch cost as numbers (the harness, not prose, keeps a
    /// switch from paying it twice). At request start it adds how the
    /// previous request ended, when there was one; in a review, the progress
    /// and the latest step.
    async fn auto_mode_state_v2(
        &self,
        request: &str,
        review_round: Option<usize>,
    ) -> serde_json::Value {
        let conversation = match review_round {
            Some(_) => Some(self.chat_state_handle.get_conversation().await),
            None => None,
        };
        let ledger = self.jev_ledger.borrow();
        let mut state = serde_json::Map::new();
        state.insert("request".to_owned(), bounded(request).into());
        state.insert(
            "current_mode".to_owned(),
            mode_id(self.execution_mode.load()).into(),
        );
        state.insert(
            "subagent_depth".to_owned(),
            self.tool_context.subagent_depth.into(),
        );
        state.insert(
            "switch_cost".to_owned(),
            serde_json::json!({
                "last_prompt_tokens": ledger.last_main_prompt_tokens(),
                "previous_dispatch": ledger.last_execution,
            }),
        );
        state.insert(
            "worker_model_configured".to_owned(),
            self.tier_worker_slug().is_some().into(),
        );
        state.insert(
            "tier_routing_available".to_owned(),
            self.tier_routing_available().into(),
        );
        match (review_round, conversation) {
            (Some(rounds), Some(conversation)) => {
                let checks_with = |outcome: CheckOutcome| checks_with(&ledger.facts, outcome);
                state.insert(
                    "progress".to_owned(),
                    serde_json::json!({
                        "completed_tool_rounds": rounds,
                        "failed_tool_calls": ledger.facts.failed_tool_count(),
                        "checks_run": ledger.facts.recorded_checks().len(),
                        "passed_checks": checks_with(CheckOutcome::Passed),
                        "failed_checks": checks_with(CheckOutcome::Failed),
                        "unknown_checks": checks_with(CheckOutcome::Unknown),
                    }),
                );
                state.insert(
                    "micro_action".to_owned(),
                    describe_micro_action(&conversation, &ledger.facts).as_json(),
                );
            }
            _ => {
                if let Some(previous) = &ledger.previous_turn {
                    state.insert(
                        "previous_turn".to_owned(),
                        serde_json::json!({
                            "failed_tool_calls": previous.failed_tool_calls,
                            "passed_checks": previous.passed_checks,
                            "failed_checks": previous.failed_checks,
                            "unknown_checks": previous.unknown_checks,
                        }),
                    );
                }
            }
        }
        serde_json::Value::Object(state)
    }

    /// The options the mode question offers: UltraSave only when something it
    /// changes can run here (see [`ultrasave_option_runs`]).
    fn mode_options(&self) -> Vec<ExecutionMode> {
        let rows = super::mode_policy::v2_modes();
        let ultrasave = ultrasave_option_runs(
            ultrasave_has_no_cheaper_model(),
            self.tier_routing_available(),
            rows.policy(ExecutionMode::Ultrasave).runtime,
            rows.policy(ExecutionMode::Normal).runtime,
        );
        MODES
            .into_iter()
            .filter(|mode| ultrasave || *mode != ExecutionMode::Ultrasave)
            .collect()
    }

    fn auto_mode_questions(
        &self,
        v2: bool,
    ) -> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
        if v2 {
            mode_question_v2(&self.mode_options())
        } else {
            mode_question_v1()
        }
    }

    /// With `b8_review_hygiene` on a prompt of 16k tokens or more, why the
    /// review must keep the current mode instead of switching to `mode`.
    /// Below that a cache miss is cheap, so switching stays free.
    fn review_hold_reason(&self, mode: ExecutionMode, rounds: usize) -> Option<&'static str> {
        // An escalation holds whatever the lever and the prompt size.
        if self.escalation_holds_auto_mode(mode) {
            return Some("escalated this request");
        }
        if !crate::jev::lever_active(JevLever::B8ReviewHygiene) {
            return None;
        }
        let ledger = self.jev_ledger.borrow();
        if ledger
            .last_main_prompt_tokens()
            .is_none_or(|tokens| tokens < super::jev_ledger::EFFORT_SWITCH_FREE_PROMPT_TOKENS)
        {
            return None;
        }
        let turn = &ledger.auto_mode;
        if turn
            .switched_at_round
            .is_some_and(|at| rounds < at + SWITCH_DWELL_ROUNDS)
        {
            return Some("dwell after a switch");
        }
        let marks = failure_marks(&ledger.facts);
        if turn
            .left
            .iter()
            .any(|(left, at_leave)| *left == mode && *at_leave == marks)
        {
            return Some("left this request, no new failure");
        }
        None
    }

    /// Notes a review switch away from `left` after `rounds` completed rounds.
    fn note_review_switch(&self, left: ExecutionMode, rounds: usize) {
        let mut ledger = self.jev_ledger.borrow_mut();
        let ledger = &mut *ledger;
        let marks = failure_marks(&ledger.facts);
        let turn = &mut ledger.auto_mode;
        turn.switched_at_round = Some(rounds);
        match turn.left.iter_mut().find(|(mode, _)| *mode == left) {
            Some(entry) => entry.1 = marks,
            None => turn.left.push((left, marks)),
        }
    }

    /// Applies Auto's pick and tells every attached client.
    fn publish_auto_mode(&self, mode: ExecutionMode, reason: &str) -> bool {
        if !self.execution_mode.pick_auto(mode) {
            return false;
        }
        if !self.startup_hints.is_subagent
            && let Err(error) = crate::extensions::session_ultracode::persist_mode(
                &self.session_info,
                distill_tool_types::ExecutionModeChoice::Auto,
                mode,
            )
        {
            tracing::warn!(%error, "auto execution mode could not be persisted");
        }
        let payload = serde_json::json!({
            "sessionId": self.session_info.id.0.as_ref(),
            "mode": mode,
            "choice": distill_tool_types::ExecutionModeChoice::Auto,
            "reason": reason,
        });
        if let Ok(params) = serde_json::value::to_raw_value(&payload) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    MODE_CHANGED_METHOD,
                    params.into(),
                ));
        }
        true
    }

    /// Request start: Jev picks the mode for this request. Without an answer
    /// the mode Auto last used stays.
    pub(super) async fn jev_auto_pick_mode(&self, request: &str) {
        if !self.auto_mode_active() {
            return;
        }
        self.jev_ledger.borrow_mut().auto_mode = AutoModeTurn::default();
        let v2 = criteria_v2();
        let Some(questions) = self.auto_mode_questions(v2) else {
            return;
        };
        let state = if v2 {
            self.auto_mode_state_v2(request, None).await
        } else {
            self.request_start_state(request).await
        };
        let Some(answers) = ask_mode(state, questions).await else {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "defer",
                "Jev gave no answer",
                None,
                None,
            );
            return;
        };
        let confidence = answers.confidence(MODE_QUESTION);
        let Some((picked, probability)) = picked_mode(&answers) else {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "defer",
                "no usable mode answer",
                confidence,
                Some(&answers),
            );
            return;
        };
        // The user may have fixed a mode while Jev was answering.
        if !self.auto_mode_active() {
            return;
        }
        let current = self.execution_mode.load();
        let first_request = self.jev_ledger.borrow().previous_turn.is_none();
        let fallback = request_start_fallback(v2, first_request, current);
        let mode = request_start_mode(
            picked,
            probability,
            fallback,
            REQUEST_START_PROBABILITY_FLOOR,
        );
        if mode != picked {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                if mode == current {
                    "keep"
                } else {
                    mode_id(mode)
                },
                &format!("{} (pick below the request-start floor)", mode_id(mode)),
                confidence,
                Some(&answers),
            );
            if mode != current && self.publish_auto_mode(mode, "below the request-start floor") {
                crate::jev::emit_mode_switched(
                    None,
                    current,
                    mode,
                    distill_session_events::ModeSwitchTrigger::RequestStart,
                );
            }
            return;
        }
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            mode_id(mode),
            "request start",
            confidence,
            Some(&answers),
        );
        if self.publish_auto_mode(mode, "picked for this request") {
            crate::jev::emit_mode_switched(
                None,
                current,
                mode,
                distill_session_events::ModeSwitchTrigger::RequestStart,
            );
        }
    }

    /// Reconsider after each completed tool round, including short actions.
    /// Retries of the same boundary do not pay for another decision. There is
    /// no switch quota: useful later changes must remain possible. With
    /// `b8_up_only` the mode only moves up, so UltraCode asks nothing, and an
    /// UltraFast whose row skips turn-start waits asks without waiting: the
    /// answer applies at the first review after it arrives.
    pub(super) async fn jev_auto_review_mode(&self, request: &str, rounds: usize) -> bool {
        if !self.auto_mode_active() {
            return false;
        }
        let up_only = crate::jev::lever_active(JevLever::B8UpOnly);
        match self.take_detached_answer() {
            DetachedAnswer::Absent => {}
            DetachedAnswer::Pending => return false,
            DetachedAnswer::Arrived(answers) => {
                return answers.is_some_and(|answers| self.apply_review(&answers, rounds, up_only));
            }
        }
        if rounds <= self.jev_ledger.borrow().auto_mode.round_at_decision {
            return false;
        }
        self.jev_ledger.borrow_mut().auto_mode.round_at_decision = rounds;
        let current = self.execution_mode.load();
        if up_only && current == ExecutionMode::Ultracode {
            return false;
        }
        let v2 = criteria_v2();
        let Some(questions) = self.auto_mode_questions(v2) else {
            return false;
        };
        let state = if v2 {
            self.auto_mode_state_v2(request, Some(rounds)).await
        } else {
            self.auto_mode_state(request, Some(current), rounds).await
        };
        if up_only && self.review_runs_detached() {
            self.start_detached_review(state, questions);
            return false;
        }
        let Some(answers) = ask_mode(state, questions).await else {
            return false;
        };
        self.apply_review(&answers, rounds, up_only)
    }

    /// A review's answer against the mode in force now. Without `up_only`
    /// any mode whose probability reaches the review floor may follow; with
    /// it, only a higher mode whose probability exceeds the current mode's by
    /// the climb margin, and a leaner pick waits for the next request.
    fn apply_review(&self, answers: &JevAnswerSet, rounds: usize, up_only: bool) -> bool {
        let confidence = answers.confidence(MODE_QUESTION);
        let Some((mode, probability)) = picked_mode(answers) else {
            return false;
        };
        if !self.auto_mode_active() {
            return false;
        }
        let current = self.execution_mode.load();
        use distill_workspace::jev::catalog::thresholds::{self, Threshold};
        let keep = |reason: String| {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "keep",
                &reason,
                confidence,
                Some(answers),
            );
        };
        let calibration = if up_only {
            let margin = thresholds::thresholds_for(&answers.model).mode_review_up_margin;
            let calibration =
                thresholds::calibration_word(&answers.model, &[Threshold::ModeReviewUpMargin]);
            if mode == current {
                keep(format!("{}; {calibration}", mode_id(current)));
                return false;
            }
            if super::escalation::mode_rank(mode) <= super::escalation::mode_rank(current) {
                keep(format!(
                    "{} (a leaner mode waits for the next request)",
                    mode_id(current)
                ));
                return false;
            }
            let current_probability = answers
                .probability(MODE_QUESTION, mode_id(current))
                .unwrap_or(0.0);
            if probability - current_probability < margin {
                keep(format!(
                    "{} (under the climb margin); {calibration}",
                    mode_id(current)
                ));
                return false;
            }
            calibration
        } else {
            let floor = thresholds::thresholds_for(&answers.model).mode_review_switch;
            let calibration =
                thresholds::calibration_word(&answers.model, &[Threshold::ModeReviewSwitch]);
            if mode == current || probability < floor {
                keep(format!("{}; {calibration}", mode_id(current)));
                return false;
            }
            calibration
        };
        if let Some(hold) = self.review_hold_reason(mode, rounds) {
            keep(format!("{} ({hold})", mode_id(current)));
            return false;
        }
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            mode_id(mode),
            &format!("switch from {}; {calibration}", mode_id(current)),
            confidence,
            Some(answers),
        );
        let reason = format!(
            "switched from {} after {rounds} tool rounds",
            current.label()
        );
        let switched = self.publish_auto_mode(mode, &reason);
        if switched {
            self.note_review_switch(current, rounds);
            crate::jev::emit_mode_switched(
                None,
                current,
                mode,
                distill_session_events::ModeSwitchTrigger::Review,
            );
        }
        switched
    }

    /// UltraFast on a row that skips turn-start waits does not wait for its
    /// review either: the next main request goes out at once.
    fn review_runs_detached(&self) -> bool {
        self.execution_mode.load() == ExecutionMode::Ultrafast
            && !self.mode_policy().runtime.jev.await_turn_start
    }

    /// Starts the review on its own task; its answer lands in the request's
    /// slot, and a new request drops the slot, aborting the ask.
    fn start_detached_review(
        &self,
        state: serde_json::Value,
        questions: std::collections::BTreeMap<String, distill_workspace::jev::Question>,
    ) {
        let answer = DetachedSlot::default();
        let slot = Arc::clone(&answer);
        let session_id = self.session_id_string();
        let recorder = crate::jev::active_usage_recorder();
        let task = tokio::task::spawn_local(async move {
            let answers = crate::jev::with_session_scope_and_recorder(
                session_id,
                recorder,
                ask_mode(state, questions),
            )
            .await;
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(answers);
        });
        self.jev_ledger.borrow_mut().auto_mode.detached = Some(DetachedReview {
            answer,
            _task: Arc::new(crate::util::AbortOnDrop(task)),
        });
    }

    /// The detached review's answer, taken once it arrived.
    fn take_detached_answer(&self) -> DetachedAnswer {
        let mut ledger = self.jev_ledger.borrow_mut();
        let Some(review) = ledger.auto_mode.detached.as_ref() else {
            return DetachedAnswer::Absent;
        };
        let arrived = review
            .answer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match arrived {
            None => DetachedAnswer::Pending,
            Some(answers) => {
                ledger.auto_mode.detached = None;
                DetachedAnswer::Arrived(answers)
            }
        }
    }

    /// The escalation ratchet's step under Auto: one mode up (UltraSave or
    /// UltraFast to Normal, Normal to UltraCode) without asking Jev. `None`
    /// outside Auto or at the top. The review keeps the new mode for the rest
    /// of the request (see `escalation_holds_auto_mode`).
    pub(super) fn escalate_auto_mode(&self) -> Option<ExecutionMode> {
        if !self.auto_mode_active() {
            return None;
        }
        let current = self.execution_mode.load();
        let next = match current {
            ExecutionMode::Ultrasave | ExecutionMode::Ultrafast => ExecutionMode::Normal,
            ExecutionMode::Normal => ExecutionMode::Ultracode,
            ExecutionMode::Ultracode => return None,
        };
        if !self.publish_auto_mode(next, "escalated after a harness signal") {
            return None;
        }
        crate::jev::emit_mode_switched(
            None,
            current,
            next,
            distill_session_events::ModeSwitchTrigger::Ratchet,
        );
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first Auto: one-sentence criteria and a review free to move either way.
    fn first_auto() -> distill_workspace::jev::JevFlags {
        distill_workspace::jev::JevFlags {
            b8_criteria_v2: false,
            b8_up_only: false,
            ..distill_workspace::jev::JevFlags::harness_default()
        }
    }

    fn answer(choice: &str, probability: f64) -> distill_workspace::jev::JevAnswerSet {
        distill_workspace::jev::JevAnswerSet {
            model: "test-jev".to_owned(),
            answers: [(
                MODE_QUESTION.to_owned(),
                distill_workspace::jev::Answer::Choice {
                    choice: choice.to_owned(),
                    probabilities: [(choice.to_owned(), probability)].into(),
                    confidence: Some(probability),
                },
            )]
            .into(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 0,
        }
    }

    #[test]
    fn the_question_offers_every_mode_and_unknown_answers_pick_nothing() {
        let questions = mode_question().expect("question");
        assert_eq!(questions.len(), 1);
        for mode in MODES {
            assert_eq!(
                picked_mode(&answer(mode_id(mode), 0.9)).map(|(m, _)| m),
                Some(mode)
            );
        }
        assert!(picked_mode(&answer("turbo", 0.9)).is_none());
    }

    /// The first Auto (`b8_criteria_v2` and `b8_up_only` off): it picks at
    /// request start, may switch either way later on evidence, and a fixed
    /// user choice is never overridden.
    #[tokio::test(flavor = "current_thread")]
    async fn auto_picks_switches_on_evidence_and_never_overrides_a_fixed_mode() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(first_auto());
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(ExecutionMode::Normal);
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_pick_mode("rename a variable").await;
                assert!(actor.execution_mode.is_auto());
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultrasave);

                // No completed action yet: nothing is asked.
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                actor.jev_auto_review_mode("rename a variable", 0).await;
                assert_eq!(crate::jev::test_decision_answers_remaining(), 1);

                // Even the first short action is reviewed; weak evidence keeps the mode.
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.4))]);
                actor.jev_auto_review_mode("rename a variable", 1).await;
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultrasave);

                // A confident answer switches.
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                actor.jev_auto_review_mode("rename a variable", 2).await;
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Normal);
                assert!(actor.execution_mode.is_auto());

                // More than two useful switches remain possible in the same request.
                for (round, mode) in [
                    (3, ExecutionMode::Ultracode),
                    (4, ExecutionMode::Ultrafast),
                    (5, ExecutionMode::Ultrasave),
                ] {
                    crate::jev::set_test_decision_answers([Some(answer(mode_id(mode), 0.9))]);
                    assert!(actor.jev_auto_review_mode("rename a variable", round).await);
                    assert_eq!(actor.execution_mode.load(), mode);
                }
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                assert!(!actor.jev_auto_review_mode("rename a variable", 5).await);
                assert_eq!(crate::jev::test_decision_answers_remaining(), 1);

                // A fixed choice belongs to the user.
                actor.execution_mode.store(ExecutionMode::Ultracode);
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.99))]);
                actor.jev_auto_pick_mode("rename a variable").await;
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultracode);
                assert_eq!(crate::jev::test_decision_answers_remaining(), 1);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    /// Without the current mode, Jev cannot weigh a request-start switch
    /// against the cache the session already holds. After a drained request
    /// the state names the current mode and how that request ended; the first
    /// request of a session has neither.
    #[tokio::test(flavor = "current_thread")]
    async fn the_request_start_state_carries_the_current_mode_and_last_outcome() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(ExecutionMode::Ultrasave);
                let first = actor.request_start_state("rename a variable").await;
                assert!(first["current_mode"].is_null(), "{first}");
                assert!(first.get("previous_turn").is_none(), "{first}");
                assert_eq!(first["micro_action"]["step"], "first_step");

                actor
                    .jev_ledger
                    .borrow_mut()
                    .facts
                    .note_tool_result("call-1", &failed_command());
                let _ = actor.jev_ledger.borrow_mut().take_rows();
                let next = actor.request_start_state("now add a test").await;
                assert_eq!(next["current_mode"], "ultrasave");
                assert_eq!(next["previous_turn"]["mode"], "ultrasave");
                assert_eq!(next["previous_turn"]["failed_tool_calls"], 1);
                assert_eq!(next["micro_action"]["step"], "first_step", "still a new request");
                assert!(next["worker_model_configured"].is_boolean(), "kept: {next}");
                // UltraSave is current here, so the mode-free gate must agree
                // with the gate the tier route itself applies.
                assert_eq!(
                    next["tier_routing_available"],
                    actor.tier_routing_allowed(),
                    "{next}"
                );
            })
            .await;
    }

    /// The floor keeps the current mode on a weak pick; at the default 0.0
    /// every pick applies, as before.
    #[tokio::test(flavor = "current_thread")]
    async fn a_request_start_pick_below_the_floor_keeps_the_current_mode() {
        use ExecutionMode::{Normal, Ultrasave};
        assert_eq!(request_start_mode(Normal, 0.4, Ultrasave, 0.5), Ultrasave);
        assert_eq!(request_start_mode(Normal, 0.5, Ultrasave, 0.5), Normal);
        assert_eq!(REQUEST_START_PROBABILITY_FLOOR, 0.0, "fit it from decision data first");
        tokio::task::LocalSet::new()
            .run_until(async {
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(Ultrasave);
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.05))]);
                actor.jev_auto_pick_mode("rename a variable").await;
                assert_eq!(actor.execution_mode.load(), Normal);
                crate::jev::clear_test_decision_answers();
            })
            .await;
    }

    fn failed_command() -> distill_tools::types::output::ToolOutput {
        distill_tools::types::output::ToolOutput::Bash(distill_tools::types::output::BashOutput {
            output: Vec::new(),
            output_for_prompt: "No such file or directory".to_owned(),
            exit_code: 1,
            command: "ls missing".to_owned(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_owned(),
            output_file: String::new(),
            total_bytes: 0,
            output_delta: None,
            was_bare_echo: false,
        })
    }

    async fn review(actor: &SessionActor, mode: &str, round: usize) -> bool {
        crate::jev::set_test_decision_answers([Some(answer(mode, 0.9))]);
        let switched = actor.jev_auto_review_mode("rename a variable", round).await;
        assert_eq!(crate::jev::test_decision_answers_remaining(), 0, "round {round} was reviewed");
        switched
    }

    fn report_prompt_tokens(actor: &SessionActor, tokens: u64) {
        let mut ledger = actor.jev_ledger.borrow_mut();
        ledger.note_round("main", None);
        ledger.add_usage(tokens, 1);
    }

    /// On a large prompt a switch can re-read it uncached, and going straight
    /// back to a mode the request just left pays that twice for nothing. With
    /// `b8_review_hygiene` a left mode comes back only after a new failure,
    /// and a switch holds for three rounds; every round is still reviewed,
    /// and a small prompt keeps switching freely.
    #[tokio::test(flavor = "current_thread")]
    async fn review_hygiene_blocks_returns_and_short_dwells_only_on_large_prompts() {
        use ExecutionMode::{Normal, Ultracode, Ultrasave};
        const LARGE: u64 = super::super::jev_ledger::EFFORT_SWITCH_FREE_PROMPT_TOKENS;
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
                    b8_review_hygiene: true,
                    b8_up_only: false,
                    ..distill_workspace::jev::JevFlags::harness_default()
                });
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(Normal);
                report_prompt_tokens(&actor, LARGE);

                assert!(review(&actor, "ultrasave", 1).await, "Normal is left at round 1");
                assert!(!review(&actor, "ultracode", 2).await, "inside the dwell");
                assert!(!review(&actor, "ultracode", 3).await, "inside the dwell");
                assert!(!review(&actor, "normal", 4).await, "no return without a new failure");
                assert_eq!(actor.execution_mode.load(), Ultrasave);
                assert!(review(&actor, "ultracode", 5).await, "a mode never left is free after the dwell");
                assert_eq!(actor.execution_mode.load(), Ultracode);

                assert!(!review(&actor, "ultrasave", 8).await, "UltraSave was left at round 5");
                actor
                    .jev_ledger
                    .borrow_mut()
                    .facts
                    .note_tool_result("call-1", &failed_command());
                assert!(review(&actor, "ultrasave", 9).await, "a new failure reopens a left mode");

                // A small prompt: a miss is cheap, so the rules step aside.
                report_prompt_tokens(&actor, LARGE - 1);
                assert!(review(&actor, "ultracode", 10).await, "return inside the dwell");
                assert_eq!(actor.execution_mode.load(), Ultracode);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    /// With the flag off (the default) and the free review (`b8_up_only` off)
    /// a large prompt switches as freely as before, including straight back
    /// to the mode just left.
    #[tokio::test(flavor = "current_thread")]
    async fn without_review_hygiene_a_large_prompt_still_switches_freely() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
                    b8_up_only: false,
                    ..distill_workspace::jev::JevFlags::harness_default()
                });
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(ExecutionMode::Normal);
                report_prompt_tokens(&actor, 200_000);
                assert!(review(&actor, "ultrasave", 1).await);
                assert!(review(&actor, "normal", 2).await);
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Normal);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_auto_uses_its_own_action_and_never_changes_the_parent() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let parent = distill_tool_types::AtomicExecutionMode::default();
                parent.store_auto(ExecutionMode::Ultracode);
                let mut actor = super::super::support::plain_actor().await;
                actor.startup_hints.is_subagent = true;
                actor.tool_context.subagent_depth = 1;
                actor.execution_mode = std::sync::Arc::new(parent.inherited());
                let request = "tire um screenshot do meu Google Chrome e me mostre";
                let state = actor.auto_mode_state(request, None, 0).await;
                assert_eq!(state["micro_action"]["what_it_must_do"], request);
                crate::jev::set_test_decision_answers([Some(answer("ultrafast", 0.95))]);
                actor.jev_auto_pick_mode(request).await;
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultrafast);
                assert_eq!(parent.load(), ExecutionMode::Ultracode);
                parent.store(ExecutionMode::Ultrasave);
                crate::jev::set_test_decision_answers([Some(answer("ultracode", 0.95))]);
                assert!(!actor.jev_auto_review_mode(request, 1).await);
                assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultrasave);
                assert_eq!(crate::jev::test_decision_answers_remaining(), 1);
                crate::jev::clear_test_decision_answers();
            })
            .await;
    }

    /// A mode abandoned within a few rounds can only be labelled if every
    /// real change is one `mode_switched` fact with what caused it; picking
    /// the mode already in force is no switch and writes none.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn every_real_mode_change_is_one_fact_with_its_trigger() {
        use ExecutionMode::{Normal, Ultracode};
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(Normal);
                let session_id = actor.session_info.id.0.to_string();
                let text = crate::jev::session_events_text_for_test(&session_id, async {
                    crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                    actor.jev_auto_pick_mode("rename a variable").await;
                    crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.9))]);
                    actor.jev_auto_pick_mode("rename a variable").await;
                    crate::jev::begin_model_round();
                    assert!(review(&actor, "ultracode", 1).await);
                    crate::jev::set_test_decision_answers([Some(answer("ultracode", 0.9))]);
                    assert!(
                        !actor.jev_auto_review_mode("rename a variable", 2).await,
                        "already there"
                    );
                })
                .await;
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
                assert_eq!(actor.execution_mode.load(), Ultracode);
                let switches: Vec<serde_json::Value> = text
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .filter(|event| event["type"] == "mode_switched")
                    .collect();
                assert_eq!(switches.len(), 2, "{text}");
                assert_eq!(switches[0]["from"], "normal");
                assert_eq!(switches[0]["to"], "ultrasave");
                assert_eq!(switches[0]["trigger"], "request_start");
                assert_eq!(switches[0]["round"], 0);
                assert_eq!(switches[1]["from"], "ultrasave");
                assert_eq!(switches[1]["to"], "ultracode");
                assert_eq!(switches[1]["trigger"], "review");
                assert_eq!(switches[1]["round"], 1);
            })
            .await;
    }

    /// Auto v2 with its two levers stated, whatever the defaults become.
    fn v2_auto() -> distill_workspace::jev::JevFlags {
        distill_workspace::jev::JevFlags {
            b8_criteria_v2: true,
            b8_up_only: true,
            b8_review_hygiene: false,
            ..distill_workspace::jev::JevFlags::harness_default()
        }
    }

    fn answer_with(choice: &str, probabilities: &[(&str, f64)]) -> JevAnswerSet {
        JevAnswerSet {
            answers: [(
                MODE_QUESTION.to_owned(),
                distill_workspace::jev::Answer::Choice {
                    choice: choice.to_owned(),
                    probabilities: probabilities
                        .iter()
                        .map(|(mode, probability)| ((*mode).to_owned(), *probability))
                        .collect(),
                    confidence: None,
                },
            )]
            .into(),
            ..answer(choice, 0.0)
        }
    }

    fn all_v2_rows() -> distill_tool_types::V2Modes {
        distill_tool_types::V2Modes::default()
            .with(ExecutionMode::Ultrasave)
            .with(ExecutionMode::Ultrafast)
            .with(ExecutionMode::Ultracode)
    }

    fn criteria_of(
        questions: &std::collections::BTreeMap<String, distill_workspace::jev::Question>,
    ) -> (
        &serde_json::Value,
        &std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        let distill_workspace::jev::Question::Choice {
            instructions,
            criteria,
        } = &questions[MODE_QUESTION]
        else {
            panic!("the mode question is a choice");
        };
        (instructions, criteria)
    }

    fn second_person(text: &str) -> bool {
        text.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "you" | "your" | "yours" | "yourself"
            )
        })
    }

    /// The options are alternatives for one request, so they share one shape
    /// and a similar weight (a longer description draws picks of its own),
    /// and none speaks to the main model, which never reads them.
    #[test]
    fn the_request_level_criteria_share_one_shape_and_weight_and_address_no_one() {
        let questions = mode_question_v2(&MODES).expect("question");
        let (instructions, criteria) = criteria_of(&questions);
        assert_eq!(criteria.len(), MODES.len());
        let mut lengths = Vec::new();
        for (mode, criterion) in criteria {
            let object = criterion.as_object().expect("an object per option");
            assert_eq!(
                object.keys().map(String::as_str).collect::<Vec<_>>(),
                ["for", "not_for", "examples"],
                "{mode}"
            );
            assert!(object["for"].as_str().is_some_and(|text| !text.is_empty()));
            assert!(
                object["not_for"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
            let examples = object["examples"].as_array().expect("examples");
            assert!((2..=3).contains(&examples.len()), "{mode}");
            let text = criterion.to_string();
            assert!(!second_person(&text), "{mode}: {text}");
            lengths.push(text.chars().count());
        }
        let shortest = *lengths.iter().min().unwrap() as f64;
        let longest = *lengths.iter().max().unwrap() as f64;
        assert!(longest <= shortest * 1.3, "lengths {lengths:?}");
        assert!(!second_person(&instructions.to_string()), "{instructions}");
    }

    /// The request is what the question judges, so it goes once: no step
    /// copy of it at request start, and no prose about the cost of switching
    /// (the harness enforces that cost). The state names the mode in force
    /// and, from the second request on, how the last one ended.
    #[tokio::test(flavor = "current_thread")]
    async fn the_request_level_state_carries_the_request_once_and_no_switch_note() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(v2_auto());
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(ExecutionMode::Ultrasave);
                let request = "rename parse_config to load_config everywhere";
                crate::jev::take_test_asked_bodies();
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_pick_mode(request).await;
                let bodies = crate::jev::take_test_asked_bodies();
                assert_eq!(bodies.len(), 1);
                assert_eq!(bodies[0].matches(request).count(), 1, "{}", bodies[0]);
                assert!(
                    !bodies[0].contains("Mode reminders append"),
                    "{}",
                    bodies[0]
                );
                let state =
                    serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["state"].clone();
                assert_eq!(state["request"], request);
                assert_eq!(state["current_mode"], "ultrasave");
                assert!(state.get("micro_action").is_none(), "{state}");
                assert!(
                    state["switch_cost"].is_object() && state["switch_cost"].get("note").is_none()
                );
                assert!(
                    state.get("previous_turn").is_none(),
                    "a first request: {state}"
                );

                actor
                    .jev_ledger
                    .borrow_mut()
                    .facts
                    .note_tool_result("call-1", &failed_command());
                let _ = actor.jev_ledger.borrow_mut().take_rows();
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_pick_mode("now add a test").await;
                let bodies = crate::jev::take_test_asked_bodies();
                let state =
                    serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["state"].clone();
                assert_eq!(state["previous_turn"]["failed_tool_calls"], 1, "{state}");
                for counts in ["passed_checks", "failed_checks", "unknown_checks"] {
                    assert!(state["previous_turn"][counts].is_number(), "{state}");
                }

                // A review sends the request once too, with the latest step.
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_review_mode(request, 1).await;
                let bodies = crate::jev::take_test_asked_bodies();
                assert_eq!(bodies[0].matches(request).count(), 1, "{}", bodies[0]);
                let state =
                    serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["state"].clone();
                assert!(state["micro_action"].is_object(), "{state}");
                assert_eq!(state["progress"]["completed_tool_rounds"], 1);
                assert!(state["switch_cost"].get("note").is_none());
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    /// `b8_criteria_v2 = false` sends exactly what the first Auto sent: the
    /// same question (pinned by its wording hash) on the same state, as a
    /// plain one-pack request, at request start and in a review.
    #[tokio::test(flavor = "current_thread")]
    async fn without_criteria_v2_the_first_question_and_state_are_sent_unchanged() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
                    b8_criteria_v2: false,
                    ..v2_auto()
                });
                let first = mode_question_v1().expect("question");
                assert_eq!(mode_question(), Some(first.clone()));
                assert_eq!(
                    distill_workspace::jev::catalog::thresholds::question_hash(&first),
                    FIRST_WORDING_HASH
                );
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(ExecutionMode::Ultrasave);
                let request = "rename a variable";
                crate::jev::take_test_asked_bodies();
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_pick_mode(request).await;
                let expected = serde_json::json!({
                    "state": actor.request_start_state(request).await,
                    "questions": &first,
                })
                .to_string();
                assert_eq!(crate::jev::take_test_asked_bodies(), [expected]);

                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.8))]);
                actor.jev_auto_review_mode(request, 1).await;
                let expected = serde_json::json!({
                    "state": actor.auto_mode_state(request, Some(ExecutionMode::Ultrasave), 1).await,
                    "questions": &first,
                })
                .to_string();
                assert_eq!(crate::jev::take_test_asked_bodies(), [expected]);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    /// The first wording's instruction hash; the v1 strings above must not change.
    const FIRST_WORDING_HASH: &str = "4ad958938e23";

    /// Below the floor a request start keeps the mode in force; a session's
    /// first request has no mode it chose, so it falls to Normal. The floor
    /// stays 0.0 until it is fitted, so only the rule is exercised here.
    #[test]
    fn below_the_floor_a_first_request_falls_to_normal_and_a_later_one_keeps_its_mode() {
        use ExecutionMode::{Normal, Ultracode, Ultrasave};
        assert_eq!(request_start_fallback(true, true, Ultrasave), Normal);
        assert_eq!(request_start_fallback(true, false, Ultrasave), Ultrasave);
        assert_eq!(
            request_start_fallback(false, true, Ultrasave),
            Ultrasave,
            "the first question keeps the mode in force"
        );
        let fallback = request_start_fallback(true, true, Ultrasave);
        assert_eq!(request_start_mode(Ultracode, 0.2, fallback, 0.5), Normal);
        assert_eq!(request_start_mode(Ultracode, 0.6, fallback, 0.5), Ultracode);
    }

    /// An option whose levers cannot run only adds a directive, so it is not
    /// offered. UltraSave stays while a cheaper model or tier routing runs,
    /// or while its row enforces rules of its own (both the v2 and the first
    /// rows do); tier routing that cannot run is no difference.
    #[tokio::test(flavor = "current_thread")]
    async fn ultrasave_is_offered_only_when_something_it_changes_can_run() {
        use ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
        for rows in [all_v2_rows(), distill_tool_types::V2Modes::default()] {
            let save = rows.policy(Ultrasave).runtime;
            let normal = rows.policy(Normal).runtime;
            assert!(ultrasave_option_runs(true, false, save, normal), "{rows:?}");
        }
        let normal = all_v2_rows().policy(Normal).runtime;
        assert!(
            ultrasave_option_runs(false, false, normal, normal),
            "a cheaper model runs"
        );
        assert!(
            ultrasave_option_runs(true, true, normal, normal),
            "tier routing runs"
        );
        assert!(
            !ultrasave_option_runs(true, false, normal, normal),
            "only a directive"
        );
        let routed_only = distill_tool_types::RuntimePolicy {
            tier_routing: distill_tool_types::TierRouting::Cheapest,
            ..normal
        };
        assert!(!ultrasave_option_runs(true, false, routed_only, normal));

        let offered = mode_question_v2(&[Ultrafast, Normal, Ultracode]).expect("question");
        let (_, criteria) = criteria_of(&offered);
        assert_eq!(
            criteria.keys().map(String::as_str).collect::<Vec<_>>(),
            ["normal", "ultracode", "ultrafast"]
        );
        tokio::task::LocalSet::new()
            .run_until(async {
                super::super::mode_policy::set_v2_modes_for_tests(Some(all_v2_rows()));
                let actor = super::super::support::plain_actor().await;
                assert_eq!(actor.mode_options(), MODES, "the v2 row keeps UltraSave");
                super::super::mode_policy::set_v2_modes_for_tests(None);
            })
            .await;
    }

    /// Inside a request the mode only climbs: a higher mode needs its
    /// probability to beat the current mode's by the climb margin, a leaner
    /// or sideways answer is recorded as kept, the ratchet still climbs, and
    /// the next request's pick may choose the leaner mode.
    #[tokio::test(flavor = "current_thread")]
    async fn up_only_climbs_on_a_margin_and_leaves_leaner_picks_to_the_next_request() {
        use ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
        tokio::task::LocalSet::new()
            .run_until(crate::jev::with_session_scope("auto-up-only", async {
                crate::jev::set_test_flags(v2_auto());
                // The first rows wait for the review, so every answer applies inline.
                super::super::mode_policy::set_v2_modes_for_tests(Some(
                    distill_tool_types::V2Modes::default(),
                ));
                let margin =
                    distill_workspace::jev::catalog::thresholds::thresholds_for("test-jev")
                        .mode_review_up_margin;
                assert_eq!(margin, 0.25);
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(Ultrafast);
                let request = "add a flag and its test";
                let review_with = |answers: JevAnswerSet, round: usize| {
                    let actor = &actor;
                    async move {
                        crate::jev::set_test_decision_answers([Some(answers)]);
                        actor.jev_auto_review_mode(request, round).await
                    }
                };

                let short = answer_with("normal", &[("normal", 0.5), ("ultrafast", 0.3)]);
                assert!(
                    !review_with(short, 1).await,
                    "0.2 over the current mode is not enough"
                );
                assert_eq!(actor.execution_mode.load(), Ultrafast);
                let enough = answer_with("normal", &[("normal", 0.55), ("ultrafast", 0.3)]);
                assert!(review_with(enough, 2).await);
                assert_eq!(actor.execution_mode.load(), Normal);
                assert!(
                    !review_with(answer("ultrasave", 0.99), 3).await,
                    "leaner waits"
                );
                assert_eq!(actor.execution_mode.load(), Normal);
                let labels: Vec<String> = crate::jev::recorded_decisions_for_test("auto-up-only")
                    .into_iter()
                    .map(|(_, label)| label)
                    .collect();
                assert_eq!(labels, ["keep", "normal", "keep"]);

                assert_eq!(
                    actor.escalate_auto_mode(),
                    Some(Ultracode),
                    "the ratchet climbs"
                );
                assert!(!review_with(answer("normal", 0.99), 4).await);
                assert_eq!(
                    crate::jev::test_decision_answers_remaining(),
                    1,
                    "nothing is above UltraCode, so nothing is asked"
                );

                let _ = actor.jev_ledger.borrow_mut().take_rows();
                crate::jev::set_test_decision_answers([Some(answer("ultrasave", 0.9))]);
                actor.jev_auto_pick_mode("rename a variable").await;
                assert_eq!(
                    actor.execution_mode.load(),
                    Ultrasave,
                    "a new request may pick lean"
                );
                assert!(
                    !review_with(answer("ultrafast", 0.99), 1).await,
                    "UltraSave to UltraFast is not a climb"
                );
                assert_eq!(actor.execution_mode.load(), Ultrasave);
                super::super::mode_policy::set_v2_modes_for_tests(None);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            }))
            .await;
    }

    /// UltraFast on its v2 row waits for nothing at turn start, and neither
    /// for its review: the review is asked on its own task and its answer
    /// applies at the first review after it arrived. A new request drops a
    /// review still in flight.
    #[tokio::test(flavor = "current_thread")]
    async fn an_ultrafast_review_does_not_hold_the_next_request() {
        use ExecutionMode::{Normal, Ultracode, Ultrafast};
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(v2_auto());
                super::super::mode_policy::set_v2_modes_for_tests(Some(all_v2_rows()));
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store_auto(Ultrafast);
                let request = "what does this error mean";
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                assert!(
                    !actor.jev_auto_review_mode(request, 1).await,
                    "nothing waits"
                );
                assert_eq!(
                    crate::jev::test_decision_answers_remaining(),
                    1,
                    "asked on its own task"
                );
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(crate::jev::test_decision_answers_remaining(), 0);
                assert_eq!(actor.execution_mode.load(), Ultrafast);
                assert!(
                    actor.jev_auto_review_mode(request, 2).await,
                    "the answer applies now"
                );
                assert_eq!(actor.execution_mode.load(), Normal);

                // Normal waits for its review as before.
                crate::jev::set_test_decision_answers([Some(answer("ultracode", 0.9))]);
                assert!(actor.jev_auto_review_mode(request, 3).await);
                assert_eq!(actor.execution_mode.load(), Ultracode);

                actor.execution_mode.store_auto(Ultrafast);
                crate::jev::set_test_decision_answers([Some(answer("normal", 0.9))]);
                assert!(!actor.jev_auto_review_mode(request, 4).await);
                assert!(actor.jev_ledger.borrow().auto_mode.detached.is_some());
                crate::jev::set_test_decision_answers([Some(answer("ultrafast", 0.9))]);
                actor.jev_auto_pick_mode("next request").await;
                assert!(actor.jev_ledger.borrow().auto_mode.detached.is_none());
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(actor.execution_mode.load(), Ultrafast);
                super::super::mode_policy::set_v2_modes_for_tests(None);
                crate::jev::clear_test_decision_answers();
                crate::jev::clear_test_flags();
            })
            .await;
    }

    /// With shadow packs on, the first wording rides the request-level
    /// question on the same request and is only recorded, so the two can be
    /// compared from recorded decisions; off, the request is the plain one.
    #[tokio::test(flavor = "current_thread")]
    async fn the_first_wording_rides_in_shadow_and_is_only_recorded() {
        tokio::task::LocalSet::new()
            .run_until(crate::jev::with_session_scope(
                "auto-mode-v1-shadow",
                async {
                    crate::jev::set_test_flags(v2_auto());
                    crate::jev_shadow::set_test_switches(Some(true), None);
                    let actor = super::super::support::plain_actor().await;
                    actor.execution_mode.store_auto(ExecutionMode::Normal);
                    let choice =
                        |choice: &str, probability: f64| distill_workspace::jev::Answer::Choice {
                            choice: choice.to_owned(),
                            probabilities: [(choice.to_owned(), probability)].into(),
                            confidence: Some(probability),
                        };
                    let both = JevAnswerSet {
                        answers: [
                            (format!("0:{MODE_QUESTION}"), choice("ultracode", 0.9)),
                            (format!("shadow:{MODE_QUESTION}"), choice("ultrasave", 0.6)),
                        ]
                        .into(),
                        ..answer("ultracode", 0.9)
                    };
                    crate::jev::take_test_asked_questions();
                    crate::jev::set_test_decision_answers([Some(both)]);
                    actor
                        .jev_auto_pick_mode("make the cache safe under concurrent writers")
                        .await;
                    assert_eq!(
                        crate::jev::take_test_asked_questions(),
                        vec![vec![
                            format!("0:{MODE_QUESTION}"),
                            format!("shadow:{MODE_QUESTION}")
                        ]]
                    );
                    assert_eq!(
                        actor.execution_mode.load(),
                        ExecutionMode::Ultracode,
                        "the live pick"
                    );
                    assert!(
                        crate::jev::recorded_decisions_for_test("auto-mode-v1-shadow").contains(&(
                            "b8_execution_mode".to_owned(),
                            "shadow:mode_v1:ultrasave".to_owned()
                        ))
                    );

                    crate::jev_shadow::set_test_switches(Some(false), None);
                    crate::jev::set_test_decision_answers([Some(answer("ultracode", 0.9))]);
                    actor.jev_auto_pick_mode("next request").await;
                    assert_eq!(
                        crate::jev::take_test_asked_questions(),
                        vec![vec![MODE_QUESTION.to_owned()]]
                    );
                    crate::jev_shadow::set_test_switches(None, None);
                    crate::jev::clear_test_decision_answers();
                    crate::jev::clear_test_flags();
                },
            ))
            .await;
    }

    /// A mid-turn switch whose differences the harness enforces (the tools
    /// array did not change and live history already ends on the new mode's
    /// directive) appends nothing; a switch that changes what the model must
    /// read states the mode, and so does every switch with `b8_up_only` off.
    #[tokio::test(flavor = "current_thread")]
    async fn a_switch_appends_the_directive_only_when_the_model_must_read_it() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(v2_auto());
                let actor = super::super::support::plain_actor().await;
                let directives = || async {
                    actor
                        .chat_state_handle
                        .get_conversation()
                        .await
                        .iter()
                        .map(ConversationItem::text_content)
                        .filter(|text| {
                            text.contains("supersedes all earlier execution mode reminders")
                        })
                        .collect::<Vec<_>>()
                };
                actor.execution_mode.store_auto(ExecutionMode::Normal);
                actor.inject_execution_mode_reminder_at_turn_start().await;
                assert_eq!(directives().await.len(), 1);

                actor.execution_mode.store_auto(ExecutionMode::Ultracode);
                actor.inject_execution_mode_reminder_on_switch(true).await;
                let stated = directives().await;
                assert_eq!(stated.len(), 2, "the text the model reads changed");
                assert_ne!(stated[0], stated[1]);

                actor.inject_execution_mode_reminder_on_switch(true).await;
                assert_eq!(directives().await.len(), 2, "already the live directive");
                actor.inject_execution_mode_reminder_on_switch(false).await;
                assert_eq!(
                    directives().await.len(),
                    3,
                    "a new tools array is read anew"
                );

                crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
                    b8_up_only: false,
                    ..v2_auto()
                });
                actor.inject_execution_mode_reminder_on_switch(true).await;
                assert_eq!(
                    directives().await.len(),
                    4,
                    "the first Auto always states it"
                );
                crate::jev::clear_test_flags();
            })
            .await;
    }
}
