// Modified for Distill by Samuel Fajreldines, 2026.
//! B8 (Auto execution mode): with the session in Auto, one Jev decision picks
//! the effective mode (Normal, UltraFast, UltraCode, UltraSave) when a request
//! starts, and each new action may move it up or down when switching pays off.
//! Every pick reaches clients as `x.ai/session/execution_mode/changed`.

use distill_tool_types::ExecutionMode;
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

/// The request-start mode: Jev's pick, unless its probability is under `floor`.
fn request_start_mode(
    picked: ExecutionMode,
    probability: f64,
    current: ExecutionMode,
    floor: f64,
) -> ExecutionMode {
    if probability < floor { current } else { picked }
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

pub(super) fn mode_question()
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
        let Some(questions) = mode_question() else {
            return;
        };
        let state = self.request_start_state(request).await;
        let Some(answers) = crate::jev::ask_item(JevLever::B8ExecutionMode, state, questions).await
        else {
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
        let mode = request_start_mode(picked, probability, current, REQUEST_START_PROBABILITY_FLOOR);
        if mode != picked {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "keep",
                &format!("{} (pick below the request-start floor)", mode_id(current)),
                confidence,
                Some(&answers),
            );
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
    /// no switch quota: useful later changes must remain possible.
    pub(super) async fn jev_auto_review_mode(&self, request: &str, rounds: usize) -> bool {
        if !self.auto_mode_active() {
            return false;
        }
        if rounds <= self.jev_ledger.borrow().auto_mode.round_at_decision {
            return false;
        }
        self.jev_ledger.borrow_mut().auto_mode.round_at_decision = rounds;
        let Some(questions) = mode_question() else {
            return false;
        };
        let current = self.execution_mode.load();
        let state = self.auto_mode_state(request, Some(current), rounds).await;
        let Some(answers) = crate::jev::ask_item(JevLever::B8ExecutionMode, state, questions).await
        else {
            return false;
        };
        let confidence = answers.confidence(MODE_QUESTION);
        let Some((mode, probability)) = picked_mode(&answers) else {
            return false;
        };
        if !self.auto_mode_active() {
            return false;
        }
        use distill_workspace::jev::catalog::thresholds::{self, Threshold};
        let floor = thresholds::thresholds_for(&answers.model).mode_review_switch;
        let calibration =
            thresholds::calibration_word(&answers.model, &[Threshold::ModeReviewSwitch]);
        if mode == current || probability < floor {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "keep",
                &format!("{}; {calibration}", mode_id(current)),
                confidence,
                Some(&answers),
            );
            return false;
        }
        if let Some(hold) = self.review_hold_reason(mode, rounds) {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "keep",
                &format!("{} ({hold})", mode_id(current)),
                confidence,
                Some(&answers),
            );
            return false;
        }
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            mode_id(mode),
            &format!("switch from {}; {calibration}", mode_id(current)),
            confidence,
            Some(&answers),
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

    /// Auto picks at request start, may switch later on evidence, and a fixed
    /// user choice is never overridden.
    #[tokio::test(flavor = "current_thread")]
    async fn auto_picks_switches_on_evidence_and_never_overrides_a_fixed_mode() {
        tokio::task::LocalSet::new()
            .run_until(async {
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

    /// With the flag off (the default) a large prompt switches as freely as
    /// before, including straight back to the mode just left.
    #[tokio::test(flavor = "current_thread")]
    async fn without_review_hygiene_a_large_prompt_still_switches_freely() {
        tokio::task::LocalSet::new()
            .run_until(async {
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
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
                    assert!(!review(&actor, "ultracode", 2).await, "already there");
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
}
