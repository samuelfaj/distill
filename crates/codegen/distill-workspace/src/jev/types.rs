//! Wire types for the Jev (TypeSafe System One) `POST /v1/systemone` API.
//!
//! Contract source of truth: `plan/plan.md` §12.5 — typed questions tagged by
//! `type` (`noul` | `score` | `choice`), answers carrying
//! `probabilities`/`confidence` (choice, score) or `noul` (0..=1), and `usage`
//! in snake_case. `Choice.criteria` is a label->description|null map bounded by
//! [`MAX_CHOICE_OPTIONS`]; `Score.criteria` is an ordered array with at least
//! [`MIN_SCORE_LEVELS`] levels; `Noul.criteria` is `{true?, false?}`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::error::JevError;

/// The API accepts at most 255 options per `Choice` question.
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// `Score.criteria` needs at least two ordered levels to be a rubric.
pub const MIN_SCORE_LEVELS: usize = 2;
/// `Score.criteria` levels are indexed from zero; ten is the documented ceiling.
pub const MAX_SCORE_LEVELS: usize = 10;

/// Arbitrary JSON as accepted by `state`, `instructions` and criteria entries.
pub type Json = serde_json::Value;

/// Name of a question inside one request; answers come back under the same name.
pub type QuestionId = String;

/// `Noul.criteria` — optional descriptions of the `true`/`false` poles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub is_true: Option<Json>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub is_false: Option<Json>,
}

/// One typed question. Serialized as `{"type": "...", ...}` (plan §12.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Probability that the answer to `instructions` is "yes".
    Noul {
        instructions: Json,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Pick one label from `criteria` (label -> description|null).
    Choice {
        instructions: Json,
        criteria: BTreeMap<String, Json>,
    },
    /// Place the state on an ordered rubric of at least two levels.
    Score {
        instructions: Json,
        criteria: Vec<Json>,
    },
}

impl Question {
    /// A bare yes/no question (`instructions` is required by the wire contract).
    pub fn noul(instructions: impl Into<Json>) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    /// A yes/no question with an explicit `true`/`false` pole description.
    pub fn noul_with_criteria(
        instructions: impl Into<Json>,
        yes: impl Into<Json>,
        no: impl Into<Json>,
    ) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: Some(NoulCriteria {
                is_true: Some(yes.into()),
                is_false: Some(no.into()),
            }),
        }
    }

    /// A closed-set question. Rejects empty and > [`MAX_CHOICE_OPTIONS`] criteria.
    pub fn choice(
        instructions: impl Into<Json>,
        criteria: BTreeMap<String, Json>,
    ) -> Result<Self, JevError> {
        if criteria.is_empty() {
            return Err(JevError::invalid("choice criteria must not be empty"));
        }
        if criteria.len() > MAX_CHOICE_OPTIONS {
            return Err(JevError::invalid(format!(
                "choice criteria has {} options, over the {MAX_CHOICE_OPTIONS} ceiling",
                criteria.len()
            )));
        }
        Ok(Self::Choice {
            instructions: instructions.into(),
            criteria,
        })
    }

    /// A rubric question. Rejects rubrics outside [`MIN_SCORE_LEVELS`]..=[`MAX_SCORE_LEVELS`].
    pub fn score(instructions: impl Into<Json>, criteria: Vec<Json>) -> Result<Self, JevError> {
        if criteria.len() < MIN_SCORE_LEVELS {
            return Err(JevError::invalid(format!(
                "score criteria needs at least {MIN_SCORE_LEVELS} levels, got {}",
                criteria.len()
            )));
        }
        if criteria.len() > MAX_SCORE_LEVELS {
            return Err(JevError::invalid(format!(
                "score criteria has {} levels, over the {MAX_SCORE_LEVELS} ceiling",
                criteria.len()
            )));
        }
        Ok(Self::Score {
            instructions: instructions.into(),
            criteria,
        })
    }

    /// Re-checks the shape invariants; called on every request so a bad catalog
    /// fails before it leaves the process.
    pub fn validate(&self) -> Result<(), JevError> {
        match self {
            Self::Noul { instructions, .. } if instructions.is_null() => {
                Err(JevError::invalid("noul instructions must not be null"))
            }
            Self::Noul { .. } => Ok(()),
            Self::Choice {
                instructions,
                criteria,
            } => {
                if instructions.is_null() {
                    return Err(JevError::invalid("choice instructions must not be null"));
                }
                if criteria.is_empty() {
                    return Err(JevError::invalid("choice criteria must not be empty"));
                }
                if criteria.len() > MAX_CHOICE_OPTIONS {
                    return Err(JevError::invalid(format!(
                        "choice criteria has {} options, over the {MAX_CHOICE_OPTIONS} ceiling",
                        criteria.len()
                    )));
                }
                Ok(())
            }
            Self::Score {
                instructions,
                criteria,
            } => {
                if instructions.is_null() {
                    return Err(JevError::invalid("score instructions must not be null"));
                }
                if criteria.len() < MIN_SCORE_LEVELS {
                    return Err(JevError::invalid(format!(
                        "score criteria needs at least {MIN_SCORE_LEVELS} levels, got {}",
                        criteria.len()
                    )));
                }
                Ok(())
            }
        }
    }

    /// The `type` tag this question serializes to.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }
}

/// One typed answer. Deserialized from the `answers` map (plan §12.5).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, Json>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
}

impl Answer {
    /// The `yes` probability of a `Noul` answer (0..=1).
    pub fn noul_value(&self) -> Option<f64> {
        match self {
            Self::Noul { noul } => Some(*noul),
            _ => None,
        }
    }

    /// The chosen label of a `Choice` answer.
    pub fn choice_value(&self) -> Option<&str> {
        match self {
            Self::Choice { choice, .. } => Some(choice.as_str()),
            _ => None,
        }
    }

    /// The rubric position of a `Score` answer (may fall between levels).
    pub fn score_value(&self) -> Option<f64> {
        match self {
            Self::Score { score, .. } => Some(*score),
            _ => None,
        }
    }

    /// `confidence` as reported by the API; `Noul` answers do not carry one.
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Self::Noul { .. } => None,
            Self::Choice { confidence, .. } | Self::Score { confidence, .. } => *confidence,
        }
    }

    /// Probability assigned to one label/level (numeric keys are strings on the wire).
    pub fn probability(&self, key: &str) -> Option<f64> {
        match self {
            Self::Noul { .. } => None,
            Self::Choice { probabilities, .. } | Self::Score { probabilities, .. } => {
                probabilities.get(key).copied()
            }
        }
    }

    /// Highest probability in the distribution, when the answer carries one.
    pub fn top_probability(&self) -> Option<f64> {
        match self {
            Self::Noul { .. } => None,
            Self::Choice { probabilities, .. } | Self::Score { probabilities, .. } => probabilities
                .values()
                .copied()
                .fold(None, |acc: Option<f64>, p| {
                    Some(acc.map_or(p, |a| a.max(p)))
                }),
        }
    }

    /// Rubric position normalized to `0.0..=1.0` using the legend length
    /// (plan item 7: never compare a raw score across rubrics of different sizes).
    pub fn score_normalized(&self) -> Option<f64> {
        match self {
            Self::Score { score, legend, .. } => {
                let levels = legend.len().max(MIN_SCORE_LEVELS);
                Some(*score / (levels.saturating_sub(1) as f64))
            }
            _ => None,
        }
    }

    /// The `type` tag this answer was deserialized from.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }
}

/// Token accounting reported by the API (`input_tokens`/`output_tokens`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

impl Usage {
    /// Input tokens, or zero when the server omitted the field.
    pub fn input(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
    }

    /// Output tokens, or zero when the server omitted the field.
    pub fn output(&self) -> u64 {
        self.output_tokens.unwrap_or(0)
    }
}

/// Request envelope: `{state, model, questions}` (plan §12.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemOneRequest {
    pub state: Json,
    pub model: String,
    pub questions: BTreeMap<QuestionId, Question>,
}

/// Response envelope: `{model, answers, usage?, id?}`.
///
/// `id` is the OpenRouter decisions endpoint's completion id; the direct service
/// reports the same thing in a header instead, so the field is optional.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SystemOneResponse {
    pub model: String,
    pub answers: BTreeMap<QuestionId, Answer>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub id: Option<String>,
}

/// One completed round trip: typed answers plus the metadata the plan records
/// as evidence (model version, tokens, request id, observed latency).
#[derive(Debug, Clone, PartialEq)]
pub struct JevAnswerSet {
    pub model: String,
    pub answers: BTreeMap<QuestionId, Answer>,
    pub usage: Usage,
    pub request_id: Option<String>,
    pub latency_ms: u64,
}

impl JevAnswerSet {
    /// A `Noul` answer's `yes` probability, or `None` when the type differs.
    pub fn noul(&self, id: &str) -> Option<f64> {
        self.answers.get(id).and_then(Answer::noul_value)
    }

    /// A `Choice` answer's label, or `None` when the type differs.
    pub fn choice(&self, id: &str) -> Option<&str> {
        self.answers.get(id).and_then(Answer::choice_value)
    }

    /// The reported confidence of a choice/score answer.
    pub fn confidence(&self, id: &str) -> Option<f64> {
        self.answers.get(id).and_then(Answer::confidence)
    }

    /// Normalized rubric position of a score answer (0.0..=1.0).
    pub fn score_normalized(&self, id: &str) -> Option<f64> {
        self.answers.get(id).and_then(Answer::score_normalized)
    }

    /// Probability of one label/level of a choice/score answer.
    pub fn probability(&self, id: &str, key: &str) -> Option<f64> {
        self.answers.get(id).and_then(|a| a.probability(key))
    }

    /// Highest probability of a choice/score answer's distribution.
    pub fn top_probability(&self, id: &str) -> Option<f64> {
        self.answers.get(id).and_then(Answer::top_probability)
    }

    /// Total tokens (input + output) reported for this call.
    pub fn total_tokens(&self) -> u64 {
        self.usage.input() + self.usage.output()
    }

    /// Iterator over `(question_id, answer)` pairs in deterministic order.
    pub fn iter_answers(&self) -> impl Iterator<Item = (&QuestionId, &Answer)> {
        self.answers.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> BTreeMap<String, Json> {
        values
            .iter()
            .map(|v| ((*v).to_owned(), Json::Null))
            .collect()
    }

    #[test]
    fn question_serializes_with_type_tag_and_criteria_map() {
        let q = Question::choice(
            "Which family does this turn need?",
            labels(&["read", "edit", "execute"]),
        )
        .expect("choice is valid");
        let json = serde_json::to_value(&q).expect("serializes");
        assert_eq!(json["type"], "choice");
        assert!(json["criteria"].is_object());
        assert!(json["criteria"]["read"].is_null());
        assert!(json.get("criteria").is_some());
    }

    #[test]
    fn choice_rejects_more_than_the_option_ceiling() {
        let many: BTreeMap<String, Json> = (0..(MAX_CHOICE_OPTIONS + 1))
            .map(|i| (format!("option-{i}"), Json::Null))
            .collect();
        let err = Question::choice("pick one", many).expect_err("over ceiling must fail");
        assert_eq!(err.kind(), super::super::error::JevErrorKind::Invalid);
    }

    #[test]
    fn score_requires_two_levels_and_reports_them() {
        assert!(Question::score("how bad?", vec![Json::Null]).is_err());
        let q = Question::score(
            "how bad?",
            vec!["none".into(), "minor".into(), "serious".into()],
        )
        .expect("three levels are valid");
        assert_eq!(q.kind(), "score");
    }

    #[test]
    fn noul_omits_criteria_when_absent() {
        let q = Question::noul("does it write outside the workspace?");
        let json = serde_json::to_value(&q).expect("serializes");
        assert_eq!(json["type"], "noul");
        assert!(json.get("criteria").is_none());
    }

    #[test]
    fn answer_parses_the_documented_shapes() {
        let raw = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "risk": {"type": "choice", "choice": "mutating_local", "confidence": 0.3,
                          "probabilities": {"mutating_local": 0.53, "routine_build": 0.36, "destructive": 0.11}},
                "escapes": {"type": "noul", "noul": 0.15},
                "severity": {"type": "score", "score": 1.14, "confidence": 0.62,
                             "legend": {"0": "No damage", "1": "Minor", "2": "Serious"},
                             "probabilities": {"0": 0.06, "1": 0.75, "2": 0.19}}
            },
            "usage": {"input_tokens": 467, "output_tokens": 78}
        });
        let parsed: SystemOneResponse = serde_json::from_value(raw).expect("parses");
        let set = JevAnswerSet {
            model: parsed.model,
            answers: parsed.answers,
            usage: parsed.usage.unwrap_or_default(),
            request_id: None,
            latency_ms: 0,
        };
        assert_eq!(set.choice("risk"), Some("mutating_local"));
        assert_eq!(set.confidence("risk"), Some(0.3));
        assert_eq!(set.probability("risk", "destructive"), Some(0.11));
        assert_eq!(set.noul("escapes"), Some(0.15));
        // 1.14 on a three-level rubric (0..2) normalizes to 0.57, never the raw value.
        let normalized = set.score_normalized("severity").expect("score present");
        assert!((normalized - 0.57).abs() < 1e-9);
        assert_eq!(set.total_tokens(), 545);
    }

    /// The OpenRouter decisions endpoint reports its completion id in the body
    /// and the direct service in a header, so the envelope must carry `id` when
    /// it is there and parse without it when it is not.
    #[test]
    fn the_envelope_carries_an_optional_body_id() {
        let with_id = serde_json::json!({
            "model": "typesafe/jev-1.13-20260917",
            "answers": {"escapes": {"type": "noul", "noul": 0.17}},
            "usage": {"input_tokens": 393, "output_tokens": 77},
            "id": "gen-dec-1789771460-aYLoYIO7TRHU0lewVP1H",
            "provider": "TypeSafe"
        });
        let parsed: SystemOneResponse = serde_json::from_value(with_id).expect("parses");
        assert_eq!(
            parsed.id.as_deref(),
            Some("gen-dec-1789771460-aYLoYIO7TRHU0lewVP1H")
        );

        let without = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"escapes": {"type": "noul", "noul": 0.17}}
        });
        let parsed: SystemOneResponse = serde_json::from_value(without).expect("parses");
        assert_eq!(parsed.id, None);
    }

    #[test]
    fn noul_answers_have_no_confidence_field() {
        let a = Answer::Noul { noul: 0.9 };
        assert_eq!(a.confidence(), None);
        assert_eq!(a.noul_value(), Some(0.9));
        assert_eq!(a.top_probability(), None);
    }
}
