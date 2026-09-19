//! The lane decision: **one** battery that answers who should do a micro-action,
//! how, and at which effort.
//!
//! The app's invariant was "one request per decision point, never one call per
//! question"; the harness's per-item packs already follow it, and this pack is
//! the one the cheap lanes share. Asking the three questions together is what
//! keeps the family cheap: a lane decision costs one round trip, not three.
//!
//! The answer is advisory in the only direction that matters — a cheap lane may
//! run **only** when the decision says so with confidence, and "main" always
//! means the session model does the work with today's bytes. Anything unsure,
//! refused, errored or timed out is `None`, which every caller reads as "main".

use std::collections::BTreeMap;

use super::super::error::JevError;
use super::super::types::{Json, JevAnswerSet, Question, QuestionId};

/// Question ids of the battery.
pub const LANE_QUESTION: &str = "lane";
pub const FORM_QUESTION: &str = "form";
pub const EFFORT_QUESTION: &str = "effort";

/// Labels the decision chooses between.
pub const LANE_CHEAP: &str = "cheap";
pub const LANE_MAIN: &str = "main";
pub const FORM_DIRECT: &str = "direct";
pub const FORM_SUBAGENT: &str = "subagent";

/// Confidence a cheap choice needs before anything runs on the cheap worker.
///
/// Higher than the permission floor on purpose: a wrong cheap call costs the
/// reader a worse answer on work the main model could do, so the lane asks to be
/// sure rather than fast.
pub const CHEAP_CONFIDENCE_FLOOR: f64 = 0.75;

/// The effort levels the lane may choose, cheapest first. `none` is the honest
/// default for a closed task.
pub const EFFORT_CHOICES: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];

/// Where a micro-action runs, as the decision chose it.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneChoice {
    /// True when the cheap worker may run it.
    pub cheap: bool,
    /// `direct` (one request, no harness) or `subagent` (a cheap agent).
    pub direct: bool,
    /// The effort level the worker should use, when it is named.
    pub effort: Option<String>,
    /// Confidence the decision reported for the lane question.
    pub confidence: Option<f64>,
}

impl LaneChoice {
    /// The label a record carries: `cheap/direct/low`, `main`, …
    pub fn label(&self) -> String {
        if !self.cheap {
            return "main".to_owned();
        }
        let form = if self.direct { "direct" } else { "subagent" };
        match &self.effort {
            Some(effort) => format!("cheap/{form}/{effort}"),
            None => format!("cheap/{form}"),
        }
    }
}

/// Where the decision point sits, handed to the battery as state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneContext {
    /// What the micro-action is, in one line.
    pub action: String,
    /// How big the payload is, in bytes.
    pub payload_bytes: usize,
    /// What the payload classifies as, in one word.
    pub payload_class: String,
    /// What the turn asked for, one line.
    pub request: String,
    /// Model that would do the work if the lane says "main".
    pub main_model: String,
    /// Model that would do the work if the lane says "cheap".
    pub cheap_model: String,
}

/// The battery: three questions, one request.
pub fn lane_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = BTreeMap::new();
    questions.insert(
        LANE_QUESTION.to_owned(),
        Question::choice(
            "Which model should do this micro-action: the session's own model, or the cheap \
             worker? Choose `cheap` only when the task is closed and non-critical (summarising, \
             classifying, extracting, compressing a tool result) and a wrong answer would cost \
             little; choose `main` when it needs judgement, edits code, approves anything, or \
             when you are not sure.",
            [
                (LANE_CHEAP.to_owned(), Json::Null),
                (LANE_MAIN.to_owned(), Json::Null),
            ]
            .into_iter()
            .collect(),
        )?,
    );
    questions.insert(
        FORM_QUESTION.to_owned(),
        Question::choice(
            "If the cheap worker runs it, which form: `direct` (a single request, no harness, for \
             one closed task) or `subagent` (a cheap agent that may take several steps)? Prefer \
             `direct` unless the task needs more than one step or more than one file.",
            [
                (FORM_DIRECT.to_owned(), Json::Null),
                (FORM_SUBAGENT.to_owned(), Json::Null),
            ]
            .into_iter()
            .collect(),
        )?,
    );
    questions.insert(
        EFFORT_QUESTION.to_owned(),
        Question::choice(
            "How much thinking should that worker spend? `none` for a closed task that is a \
             lookup or a rewrite, higher only when the answer needs reasoning over the payload.",
            EFFORT_CHOICES
                .iter()
                .map(|level| ((*level).to_owned(), Json::Null))
                .collect(),
        )?,
    );
    Ok(questions)
}

/// The state one lane decision reasons over: bounded, named fields, no payload
/// text and no file contents.
pub fn lane_state(context: &LaneContext) -> Json {
    serde_json::json!({
        "micro_action": context.action,
        "payload": {"bytes": context.payload_bytes, "class": context.payload_class},
        "turn_request": context.request,
        "models": {"main": context.main_model, "cheap": context.cheap_model},
    })
}

/// Reads the battery's answer into a decision the caller can act on.
///
/// `None` means "the session model does it": a missing or unreadable lane answer,
/// a `main` verdict, a confidence below the floor, or a form/effort the question
/// never offered.
pub fn compose_lane(
    answers: &JevAnswerSet,
    cheap_model_available: bool,
    confidence_floor: f64,
) -> Option<LaneChoice> {
    if !cheap_model_available {
        return None;
    }
    let lane = answers.choice(LANE_QUESTION)?;
    if lane != LANE_CHEAP {
        return None;
    }
    let confidence = answers.confidence(LANE_QUESTION);
    if confidence.is_some_and(|value| value < confidence_floor) {
        return None;
    }
    // A cheap choice with no probability behind it is not a cheap choice: the
    // caller asked for a decision, and a label alone is not one.
    let cheap_probability = answers.probability(LANE_QUESTION, LANE_CHEAP);
    if cheap_probability.is_none() && confidence.is_none() {
        return None;
    }
    let direct = match answers.choice(FORM_QUESTION) {
        Some(FORM_DIRECT) => true,
        Some(FORM_SUBAGENT) => false,
        // No form answer: the cheap lane is still allowed, in its lightest form.
        _ => true,
    };
    let effort = answers
        .choice(EFFORT_QUESTION)
        .filter(|level| EFFORT_CHOICES.contains(level))
        .map(str::to_owned);
    Some(LaneChoice {
        cheap: true,
        direct,
        effort,
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::types::{Answer, Usage};

    fn answers(lane: &str, form: Option<&str>, effort: Option<&str>, confidence: Option<f64>, cheap_p: Option<f64>) -> JevAnswerSet {
        let mut map = BTreeMap::new();
        map.insert(
            LANE_QUESTION.to_owned(),
            Answer::Choice {
                choice: lane.to_owned(),
                probabilities: cheap_p
                    .map(|p| {
                        [
                            (LANE_CHEAP.to_owned(), p),
                            (LANE_MAIN.to_owned(), 1.0 - p),
                        ]
                        .into_iter()
                        .collect()
                    })
                    .unwrap_or_default(),
                confidence,
            },
        );
        if let Some(form) = form {
            map.insert(
                FORM_QUESTION.to_owned(),
                Answer::Choice {
                    choice: form.to_owned(),
                    probabilities: BTreeMap::new(),
                    confidence: Some(0.9),
                },
            );
        }
        if let Some(effort) = effort {
            map.insert(
                EFFORT_QUESTION.to_owned(),
                Answer::Choice {
                    choice: effort.to_owned(),
                    probabilities: BTreeMap::new(),
                    confidence: Some(0.9),
                },
            );
        }
        JevAnswerSet {
            model: "typesafe/jev-1.13".to_owned(),
            answers: map,
            usage: Usage::default(),
            request_id: None,
            latency_ms: 0,
        }
    }

    #[test]
    fn the_battery_is_one_request_with_three_questions() {
        let questions = lane_questions().expect("the battery builds");
        assert_eq!(questions.len(), 3, "one request, three questions");
        for id in [LANE_QUESTION, FORM_QUESTION, EFFORT_QUESTION] {
            assert_eq!(questions.get(id).expect("present").kind(), "choice");
        }
    }

    #[test]
    fn a_confident_cheap_verdict_is_the_only_thing_that_runs_cheap() {
        let confident = answers(LANE_CHEAP, Some(FORM_DIRECT), Some("none"), Some(0.9), Some(0.9));
        let choice = compose_lane(&confident, true, CHEAP_CONFIDENCE_FLOOR).expect("cheap");
        assert!(choice.cheap && choice.direct);
        assert_eq!(choice.effort.as_deref(), Some("none"));
        assert_eq!(choice.label(), "cheap/direct/none");

        // `main` is not a cheap lane, whatever else the battery said.
        let main = answers(LANE_MAIN, Some(FORM_DIRECT), Some("none"), Some(0.99), Some(0.01));
        assert!(compose_lane(&main, true, CHEAP_CONFIDENCE_FLOOR).is_none());

        // A hesitant cheap verdict falls back to the main model.
        let hesitant = answers(LANE_CHEAP, Some(FORM_DIRECT), Some("none"), Some(0.5), Some(0.55));
        assert!(compose_lane(&hesitant, true, CHEAP_CONFIDENCE_FLOOR).is_none());

        // No cheap worker configured ⇒ no cheap lane, whatever the answer says.
        assert!(compose_lane(&confident, false, CHEAP_CONFIDENCE_FLOOR).is_none());

        // A label with no probability and no confidence is not a decision.
        let naked = answers(LANE_CHEAP, None, None, None, None);
        assert!(compose_lane(&naked, true, CHEAP_CONFIDENCE_FLOOR).is_none());
    }

    #[test]
    fn the_form_and_the_effort_follow_the_answer_and_stay_in_range() {
        let subagent = answers(LANE_CHEAP, Some(FORM_SUBAGENT), Some("high"), Some(0.8), Some(0.8));
        let choice = compose_lane(&subagent, true, CHEAP_CONFIDENCE_FLOOR).expect("cheap");
        assert!(!choice.direct);
        assert_eq!(choice.label(), "cheap/subagent/high");

        // An effort the question never offered is dropped, not invented.
        let odd = answers(LANE_CHEAP, Some(FORM_DIRECT), Some("turbo"), Some(0.8), Some(0.8));
        let choice = compose_lane(&odd, true, CHEAP_CONFIDENCE_FLOOR).expect("cheap");
        assert_eq!(choice.effort, None);
        assert_eq!(choice.label(), "cheap/direct");

        // No form answer: the lightest form, which is the directive's default.
        let formless = answers(LANE_CHEAP, None, Some("low"), Some(0.9), Some(0.9));
        let choice = compose_lane(&formless, true, CHEAP_CONFIDENCE_FLOOR).expect("cheap");
        assert!(choice.direct);
        assert_eq!(choice.label(), "cheap/direct/low");
    }

    #[test]
    fn the_state_names_the_micro_action_without_shipping_the_payload() {
        let context = LaneContext {
            action: "summarise a 240 KB build log".to_owned(),
            payload_bytes: 240_000,
            payload_class: "build_log".to_owned(),
            request: "fix the failing build".to_owned(),
            main_model: "DeepSeek V4.1 Flash".to_owned(),
            cheap_model: "qwen/qwen3.7-flash".to_owned(),
        };
        let state = lane_state(&context);
        assert_eq!(state["payload"]["bytes"], 240_000);
        assert_eq!(state["payload"]["class"], "build_log");
        assert_eq!(state["models"]["cheap"], "qwen/qwen3.7-flash");
        let rendered = state.to_string();
        assert!(!rendered.contains("error["), "no payload text in the state");
    }
}
