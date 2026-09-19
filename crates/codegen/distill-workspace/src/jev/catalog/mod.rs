//! The decision catalogue: every structured decision the harness can hand to
//! Jev, grouped by area (`todo.md` is the human-facing copy of this list).
//!
//! Shape of a pack, always the same three pieces so the whole catalogue reads
//! the same way:
//! 1. `*_questions()` — the typed battery (data only, no I/O);
//! 2. a compose function that turns an answer set into a decision, using the
//!    shared helpers here (so floors and deferral rules cannot drift apart);
//! 3. constants for the thresholds, next to the pack that uses them.
//!
//! Two composition families cover almost every item:
//! * [`rank_by_noul`] — one `noul` per candidate: keep the ones above a floor,
//!   best first, and **defer** (keep everything, in the original order) when any
//!   answer is missing or of the wrong type;
//! * [`pick_one`] — a single `choice`: take it only when it clears a confidence
//!   floor, otherwise defer.
//!
//! Authority rules live here too: nothing in this module can widen what the
//! caller already allows, introduce a candidate the code did not produce, or
//! replace an existing safety check.

pub mod context;
pub mod lanes;
pub mod routing;
pub mod selection;
pub mod verify;

use crate::jev::types::{Answer, JevAnswerSet};

/// Prefix convention for per-candidate `noul` questions: `rank_<id>`.
pub const RANK_PREFIX: &str = "rank_";

/// B1 — confidence needed before an intent is used for routing.
pub const INTENT_MIN_CONFIDENCE: f64 = 0.60;
/// B2 — confidence needed before applying a cheaper model tier.
pub const MODEL_TIER_MIN_CONFIDENCE: f64 = 0.80;

/// The result of a ranking/recorte decision over candidates the code produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// Kept ids, best first. Empty means "the decision dropped everything",
    /// which only the packs that allow it (retention) may act on.
    pub keep: Vec<String>,
    /// True when the caller must keep its own ordering/selection untouched
    /// (missing answers, wrong types, or nothing above the floor).
    pub deferred: bool,
    /// Lowest probability among the kept candidates, for telemetry.
    pub confidence: Option<f64>,
}

impl Ranked {
    /// Keep every candidate, in the caller's order: the deferral value.
    pub fn all(ids: &[String]) -> Self {
        Self {
            keep: ids.to_vec(),
            deferred: true,
            confidence: None,
        }
    }

    /// True when the ranking changed nothing.
    pub fn is_deferred(&self) -> bool {
        self.deferred
    }
}

/// Keeps the candidates whose `noul` answer clears `floor`, best first.
///
/// Defers (keeps everything in the original order) when an answer is missing or
/// is not a `noul` — a partial answer set must never silently drop candidates.
pub fn rank_by_noul(
    answers: &JevAnswerSet,
    ids: &[String],
    floor: f64,
    keep_when_empty: bool,
) -> Ranked {
    let mut scored: Vec<(f64, &String)> = Vec::with_capacity(ids.len());
    for id in ids {
        let question = format!("{RANK_PREFIX}{id}");
        match answers.answers.get(&question) {
            Some(Answer::Noul { noul }) => scored.push((*noul, id)),
            // Missing or wrong-typed answer ⇒ defer, never guess.
            Some(_) => return Ranked::all(ids),
            None => return Ranked::all(ids),
        }
    }
    let mut kept: Vec<(f64, &String)> = scored.into_iter().filter(|(p, _)| *p >= floor).collect();
    if kept.is_empty() && keep_when_empty {
        return Ranked::all(ids);
    }
    kept.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let confidence = kept
        .iter()
        .map(|(p, _)| *p)
        .fold(None, |acc: Option<f64>, p| {
            Some(acc.map_or(p, |a| a.min(p)))
        });
    Ranked {
        keep: kept.into_iter().map(|(_, id)| id.clone()).collect(),
        deferred: false,
        confidence,
    }
}

/// Answers for one ranking battery: `rank_<id>` `noul` per candidate.
pub fn rank_questions(
    ids: &[String],
    instructions: impl Fn(&str) -> String,
    yes: &str,
    no: &str,
) -> Result<
    std::collections::BTreeMap<String, crate::jev::types::Question>,
    crate::jev::error::JevError,
> {
    use crate::jev::types::Question;
    if ids.is_empty() {
        return Err(crate::jev::error::JevError::invalid(
            "no candidates to rank",
        ));
    }
    let mut questions = std::collections::BTreeMap::new();
    for id in ids {
        questions.insert(
            format!("{RANK_PREFIX}{id}"),
            Question::noul_with_criteria(instructions(id), yes, no),
        );
    }
    Ok(questions)
}

/// The result of a single-choice decision among alternatives that already exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    /// The chosen label, when it cleared the floor.
    pub choice: Option<String>,
    /// Confidence reported for the choice.
    pub confidence: Option<f64>,
    /// True when the caller must keep its default behaviour.
    pub deferred: bool,
}

impl Pick {
    /// Defer to the caller's default.
    pub fn defer() -> Self {
        Self {
            choice: None,
            confidence: None,
            deferred: true,
        }
    }
}

/// Picks one label from a `choice` answer, requiring `min_confidence`.
///
/// A missing answer, a wrong type, a label outside `allowed`, or a confidence
/// below the floor all defer.
pub fn pick_one(
    answers: &JevAnswerSet,
    question: &str,
    allowed: &[&str],
    min_confidence: f64,
) -> Pick {
    let Some(answer) = answers.answers.get(question) else {
        return Pick::defer();
    };
    let Answer::Choice {
        choice, confidence, ..
    } = answer
    else {
        return Pick::defer();
    };
    if !allowed.iter().any(|a| a == choice) {
        return Pick::defer();
    }
    let confidence = *confidence;
    if confidence.unwrap_or(0.0) < min_confidence {
        return Pick::defer();
    }
    Pick {
        choice: Some(choice.clone()),
        confidence,
        deferred: false,
    }
}

/// Normalized score (0..=1) for a `score` answer, or `None` when it is missing
/// or of the wrong type.
pub fn score_of(answers: &JevAnswerSet, question: &str) -> Option<f64> {
    answers.score_normalized(question)
}

/// `noul` probability for a question, or `None` when missing/wrong-typed.
pub fn noul_of(answers: &JevAnswerSet, question: &str) -> Option<f64> {
    answers.noul(question)
}

/// Renders candidate ids for telemetry without leaking content.
pub fn short_ids(ids: &[String], max: usize) -> Vec<String> {
    ids.iter().take(max).cloned().collect()
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::jev::types::{Answer, JevAnswerSet, Usage};
    use std::collections::BTreeMap;

    /// Builds an answer set from `(question_id, answer)` pairs.
    pub fn answers(entries: Vec<(&str, Answer)>) -> JevAnswerSet {
        JevAnswerSet {
            model: "jev-1.13.0".to_owned(),
            answers: entries
                .into_iter()
                .map(|(id, answer)| (id.to_owned(), answer))
                .collect(),
            usage: Usage {
                input_tokens: Some(400),
                output_tokens: Some(40),
            },
            request_id: Some("req-catalog".to_owned()),
            latency_ms: 120,
        }
    }

    /// `noul` answer helper.
    pub fn noul(value: f64) -> Answer {
        Answer::Noul { noul: value }
    }

    /// `score` answer helper on a three-level rubric.
    pub fn score(value: f64) -> Answer {
        Answer::Score {
            score: value,
            legend: (0..3)
                .map(|i| (i.to_string(), serde_json::Value::Null))
                .collect(),
            probabilities: BTreeMap::new(),
            confidence: Some(0.8),
        }
    }

    /// `choice` answer helper.
    pub fn choice(label: &str, confidence: f64, probabilities: &[(&str, f64)]) -> Answer {
        Answer::Choice {
            choice: label.to_owned(),
            probabilities: probabilities
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
            confidence: Some(confidence),
        }
    }

    /// The ids `n` candidates of a ranking battery.
    pub fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("cand-{i}")).collect()
    }
}
