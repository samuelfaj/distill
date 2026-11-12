// Modified for Distill by Samuel Fajreldines, 2026.
//! B8 (Auto execution mode): with the session in Auto, one Jev decision picks
//! the effective mode (Normal, UltraFast, UltraCode, UltraSave) when a request
//! starts, and each new action may move it up or down when switching pays off.
//! Every pick reaches clients as `x.ai/session/execution_mode/changed`.

use distill_tool_types::ExecutionMode;
use distill_workspace::jev::flags::JevLever;

use super::*;

pub(super) const MODE_QUESTION: &str = "execution_mode";
pub(super) const MODE_CHANGED_METHOD: &str = "x.ai/session/execution_mode/changed";

/// A mid-request switch needs this much of Jev's probability on the new mode.
const SWITCH_PROBABILITY: f64 = 0.6;
const REQUEST_CHARS: usize = 4_000;

/// Auto's per-request bookkeeping.
#[derive(Debug, Default, Clone)]
pub(crate) struct AutoModeTurn {
    round_at_decision: usize,
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

fn mode_question() -> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
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
        use super::turn_facts::CheckOutcome;
        let checks_with = |outcome: CheckOutcome| {
            ledger
                .facts
                .recorded_checks()
                .iter()
                .filter(|check| check.outcome == outcome)
                .count()
        };
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
        })
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
        let state = self.auto_mode_state(request, None, 0).await;
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
        let Some((mode, _)) = picked_mode(&answers) else {
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
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            mode_id(mode),
            "request start",
            confidence,
            Some(&answers),
        );
        self.publish_auto_mode(mode, "picked for this request");
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
        if mode == current || probability < SWITCH_PROBABILITY {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                "keep",
                mode_id(current),
                confidence,
                Some(&answers),
            );
            return false;
        }
        crate::jev::record_item(
            JevLever::B8ExecutionMode,
            mode_id(mode),
            &format!("switch from {}", mode_id(current)),
            confidence,
            Some(&answers),
        );
        let reason = format!(
            "switched from {} after {rounds} tool rounds",
            current.label()
        );
        self.publish_auto_mode(mode, &reason)
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
}
