// Modified for Distill by Samuel Fajreldines, 2026.
//! Typed field extraction (`POST /api/alpha/extractions`) on Liquid's d1.2
//! models.
//!
//! A field asks for a value that is *in* the text: a string, a number or a list
//! of strings, each returned with the span of `state` it was read from, or an
//! estimate with its distribution. Spans count Unicode scalar values
//! (characters, not bytes). The harness trusts a value only when its span reads
//! back the same text, so an answer can never put words in the state's mouth:
//! a value whose span does not match is dropped, like an invented unit id.

use std::collections::BTreeMap;

use serde::Serialize;

use super::error::JevError;
use super::types::{Json, Usage};

/// Endpoint path under the OpenRouter API root.
pub const EXTRACTIONS_PATH: &str = "/alpha/extractions";
/// Longest instruction one field may carry.
pub const MAX_FIELD_INSTRUCTIONS_CHARS: usize = 2_000;
/// Most fields one request may ask.
pub const MAX_FIELDS: usize = 16;
/// State ceiling for one extraction request: d1.2 reads 65,536 tokens, and
/// source text runs at roughly three bytes per token or more.
pub const MAX_EXTRACTION_STATE_BYTES: usize = 128 * 1024;

/// One field to extract.
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    /// A string copied from the state, or nothing when `p_null` crosses
    /// `null_threshold`.
    Text {
        instructions: String,
        null_threshold: f64,
    },
    /// A number read from the state.
    Number {
        instructions: String,
        null_threshold: f64,
    },
    /// Every passage of the state that matches the instructions.
    TextList { instructions: String },
    /// An estimate between `min` and `max`, with its distribution.
    Estimate {
        instructions: String,
        min: f64,
        max: f64,
        step: Option<f64>,
    },
}

impl Field {
    fn instructions(&self) -> &str {
        match self {
            Self::Text { instructions, .. }
            | Self::Number { instructions, .. }
            | Self::TextList { instructions }
            | Self::Estimate { instructions, .. } => instructions,
        }
    }

    /// The wire kind, as the answer's `type` names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Number { .. } => "number",
            Self::TextList { .. } => "list",
            Self::Estimate { .. } => "estimate",
        }
    }

    /// Rejects what the endpoint would refuse with a `400`, before sending.
    pub fn validate(&self) -> Result<(), JevError> {
        let instructions = self.instructions().trim();
        if instructions.is_empty() {
            return Err(JevError::invalid("field instructions must not be empty"));
        }
        if instructions.chars().count() > MAX_FIELD_INSTRUCTIONS_CHARS {
            return Err(JevError::invalid(format!(
                "field instructions are over {MAX_FIELD_INSTRUCTIONS_CHARS} characters"
            )));
        }
        match self {
            // d1.2 requires a threshold above zero on text and number fields.
            Self::Text { null_threshold, .. } | Self::Number { null_threshold, .. }
                if !(*null_threshold > 0.0 && *null_threshold <= 1.0) =>
            {
                Err(JevError::invalid("null_threshold must be in (0, 1]"))
            }
            Self::Estimate { min, max, step, .. } => {
                if !(min.is_finite() && max.is_finite() && min < max) {
                    return Err(JevError::invalid("estimate needs finite min < max"));
                }
                if step.is_some_and(|step| !(step.is_finite() && step > 0.0)) {
                    return Err(JevError::invalid("estimate step must be positive"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The field spec on the wire.
    pub fn to_json(&self) -> Json {
        match self {
            Self::Text {
                instructions,
                null_threshold,
            } => serde_json::json!({
                "type": "text",
                "instructions": instructions,
                "null_threshold": null_threshold,
            }),
            Self::Number {
                instructions,
                null_threshold,
            } => serde_json::json!({
                "type": "number",
                "instructions": instructions,
                "null_threshold": null_threshold,
            }),
            Self::TextList { instructions } => serde_json::json!({
                "type": "list",
                "items": "text",
                "instructions": instructions,
            }),
            Self::Estimate {
                instructions,
                min,
                max,
                step,
            } => {
                let mut spec = serde_json::Map::new();
                spec.insert("type".to_owned(), Json::from("estimate"));
                spec.insert("instructions".to_owned(), Json::from(instructions.as_str()));
                spec.insert("min".to_owned(), serde_json::json!(min));
                spec.insert("max".to_owned(), serde_json::json!(max));
                if let Some(step) = step {
                    spec.insert("step".to_owned(), serde_json::json!(step));
                }
                Json::Object(spec)
            }
        }
    }
}

/// Field ids are short identifiers: they become JSON keys and log labels.
pub fn validate_field_id(id: &str) -> Result<(), JevError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(JevError::invalid(format!(
            "field id `{id}` must be 1-64 ASCII letters, digits, `_` or `-`"
        )))
    }
}

/// The request body for `state` and `fields`.
#[derive(Debug, Serialize)]
pub struct ExtractionRequest<'a> {
    pub model: &'a str,
    pub state: &'a str,
    pub fields: BTreeMap<&'a str, Json>,
}

/// A passage of the state and where it sits, in characters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Passage {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub confidence: Option<f64>,
}

/// One field's answer, after the span check.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Extracted {
    /// `value` is `None` when the state holds nothing for the field, or when
    /// the span did not read back the value (`dropped` says which).
    Text {
        value: Option<Passage>,
        p_null: Option<f64>,
        dropped: bool,
    },
    Number {
        value: Option<f64>,
        passage: Option<Passage>,
        p_null: Option<f64>,
        dropped: bool,
    },
    /// Only the items whose span reads back their text; `dropped` counts the rest.
    TextList {
        items: Vec<Passage>,
        p_null: Option<f64>,
        dropped: usize,
    },
    Estimate {
        value: f64,
        median: Option<f64>,
        p10: Option<f64>,
        p90: Option<f64>,
    },
}

/// Typed answers for one extraction request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtractionAnswerSet {
    pub model: String,
    pub answers: BTreeMap<String, Extracted>,
    pub usage: Usage,
    pub request_id: Option<String>,
    pub latency_ms: u64,
}

/// The characters of `state` in `[start, end)`, or `None` when the span is
/// empty, reversed or past the end.
fn slice_chars(state: &str, start: usize, end: usize) -> Option<String> {
    if start >= end {
        return None;
    }
    let mut chars = state
        .char_indices()
        .map(|(index, _)| index)
        .chain([state.len()]);
    let from = chars.nth(start)?;
    let to = chars.nth(end - start - 1)?;
    state.get(from..to).map(str::to_owned)
}

fn span_of(value: &Json) -> Option<(usize, usize)> {
    let pair = value.as_array()?;
    match pair.as_slice() {
        [start, end] => Some((
            usize::try_from(start.as_u64()?).ok()?,
            usize::try_from(end.as_u64()?).ok()?,
        )),
        _ => None,
    }
}

/// A passage when the span reads back `text` exactly.
fn checked_passage(
    state: &str,
    text: &str,
    span: (usize, usize),
    confidence: Option<f64>,
) -> Option<Passage> {
    (slice_chars(state, span.0, span.1)? == text).then(|| Passage {
        text: text.to_owned(),
        start: span.0,
        end: span.1,
        confidence,
    })
}

/// The number written in `text`, ignoring thousands separators.
fn number_in(text: &str) -> Option<f64> {
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' '))
        .collect();
    cleaned.parse::<f64>().ok()
}

/// Reads one field's answer and checks its spans against `state`.
pub fn read_answer(state: &str, field: &Field, answer: &Json) -> Result<Extracted, JevError> {
    let kind = answer
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if kind != field.kind() {
        return Err(JevError::invalid(format!(
            "answer is a `{kind}` but a `{}` field was asked",
            field.kind()
        )));
    }
    let p_null = answer.get("p_null").and_then(Json::as_f64);
    let value = answer.get("value").unwrap_or(&Json::Null);
    let span = answer.get("span").and_then(span_of);
    Ok(match field {
        Field::Text { .. } => match value.as_str() {
            None => Extracted::Text {
                value: None,
                p_null,
                dropped: false,
            },
            Some(text) => {
                let passage = span.and_then(|span| checked_passage(state, text, span, None));
                Extracted::Text {
                    dropped: passage.is_none(),
                    value: passage,
                    p_null,
                }
            }
        },
        Field::Number { .. } => match value.as_f64() {
            None => Extracted::Number {
                value: None,
                passage: None,
                p_null,
                dropped: false,
            },
            Some(number) => {
                let passage = span.and_then(|(start, end)| {
                    let text = slice_chars(state, start, end)?;
                    let read = number_in(&text)?;
                    ((read - number).abs() <= f64::EPSILON * number.abs().max(1.0)).then_some(
                        Passage {
                            text,
                            start,
                            end,
                            confidence: None,
                        },
                    )
                });
                Extracted::Number {
                    value: passage.as_ref().map(|_| number),
                    dropped: passage.is_none(),
                    passage,
                    p_null,
                }
            }
        },
        Field::TextList { .. } => {
            let values = value.as_array().cloned().unwrap_or_default();
            let spans = answer
                .get("spans")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let confidences = answer
                .get("confidences")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let mut items = Vec::new();
            let mut dropped = 0;
            for (index, item) in values.iter().enumerate() {
                let passage = item.as_str().and_then(|text| {
                    let span = spans.get(index).and_then(span_of)?;
                    let confidence = confidences.get(index).and_then(Json::as_f64);
                    checked_passage(state, text, span, confidence)
                });
                match passage {
                    Some(passage) => items.push(passage),
                    None => dropped += 1,
                }
            }
            Extracted::TextList {
                items,
                p_null,
                dropped,
            }
        }
        Field::Estimate { .. } => {
            let value = value
                .as_f64()
                .ok_or_else(|| JevError::invalid("estimate answer has no numeric value"))?;
            Extracted::Estimate {
                value,
                median: answer.get("median").and_then(Json::as_f64),
                p10: answer.get("p10").and_then(Json::as_f64),
                p90: answer.get("p90").and_then(Json::as_f64),
            }
        }
    })
}

/// Reads a 200 body: every field must come back with an answer of its type.
pub fn read_reply(
    state: &str,
    fields: &BTreeMap<String, Field>,
    bytes: &[u8],
    request_id: Option<String>,
    latency_ms: u64,
) -> Result<ExtractionAnswerSet, JevError> {
    let body: Json = serde_json::from_slice(bytes)
        .map_err(|e| JevError::invalid(format!("malformed 200 response: {e}")))?;
    if body.get("truncated").and_then(Json::as_bool) == Some(true) {
        return Err(JevError::invalid("the extraction answer was truncated"));
    }
    let answers = body
        .get("answers")
        .and_then(Json::as_object)
        .ok_or_else(|| JevError::invalid("response has no answers"))?;
    let mut read = BTreeMap::new();
    for (id, field) in fields {
        let answer = answers
            .get(id)
            .ok_or_else(|| JevError::invalid(format!("missing answer for `{id}`")))?;
        let extracted = read_answer(state, field, answer)
            .map_err(|e| JevError::invalid(format!("field `{id}`: {}", e.detail())))?;
        read.insert(id.clone(), extracted);
    }
    let usage = body
        .get("usage")
        .and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok())
        .unwrap_or_default();
    Ok(ExtractionAnswerSet {
        model: body
            .get("model")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        answers: read,
        usage,
        request_id: request_id.or_else(|| body.get("id").and_then(Json::as_str).map(str::to_owned)),
        latency_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "test a ... ok\nthread 'b' panicked at src/b.rs:12:9:\nleft: 3\ntest result: FAILED. 1 passed; 2 failed\n";

    fn at(state: &str, needle: &str) -> [usize; 2] {
        let byte = state.find(needle).expect("needle in state");
        let start = state[..byte].chars().count();
        [start, start + needle.chars().count()]
    }

    fn text_field() -> Field {
        Field::Text {
            instructions: "The first panic location.".to_owned(),
            null_threshold: 0.5,
        }
    }

    /// The harness copies a value only when it is really in the state: a
    /// value the span does not read back is dropped, never shown as found.
    #[test]
    fn a_text_value_is_kept_only_when_its_span_reads_it_back() {
        let span = at(LOG, "src/b.rs:12:9");
        let good = serde_json::json!({"type": "text", "value": "src/b.rs:12:9", "span": span, "p_null": 0.01});
        let Extracted::Text { value, dropped, .. } =
            read_answer(LOG, &text_field(), &good).unwrap()
        else {
            panic!("text answer");
        };
        assert_eq!(value.map(|p| p.text).as_deref(), Some("src/b.rs:12:9"));
        assert!(!dropped);

        let invented = serde_json::json!({"type": "text", "value": "src/c.rs:1:1", "span": span});
        let Extracted::Text { value, dropped, .. } =
            read_answer(LOG, &text_field(), &invented).unwrap()
        else {
            panic!("text answer");
        };
        assert!(
            value.is_none() && dropped,
            "a value the state does not hold is dropped"
        );
    }

    /// Spans count characters, as the endpoint does, so text after accents
    /// and emoji still reads back.
    #[test]
    fn spans_count_characters_not_bytes() {
        let state = "L1 São Paulo — peça №4 😀\nL3 error: falha em São José\n";
        let span = at(state, "São José");
        let answer = serde_json::json!({"type": "text", "value": "São José", "span": span});
        let Extracted::Text { value, .. } = read_answer(state, &text_field(), &answer).unwrap()
        else {
            panic!("text answer");
        };
        assert_eq!(value.unwrap().text, "São José");
    }

    #[test]
    fn a_list_keeps_matching_items_and_counts_the_rest() {
        let field = Field::TextList {
            instructions: "Lines that say why a test failed.".to_owned(),
        };
        let answer = serde_json::json!({
            "type": "list",
            "value": ["thread 'b' panicked at src/b.rs:12:9:", "made up line", "left: 3"],
            "spans": [at(LOG, "thread 'b' panicked at src/b.rs:12:9:"), [0, 4], at(LOG, "left: 3")],
            "confidences": [0.98, 0.5, 0.7],
            "p_null": 0.02,
        });
        let Extracted::TextList { items, dropped, .. } = read_answer(LOG, &field, &answer).unwrap()
        else {
            panic!("list answer");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].text, "left: 3");
        assert_eq!(items[0].confidence, Some(0.98));
        assert_eq!(dropped, 1);
    }

    #[test]
    fn a_number_must_be_the_number_its_span_shows() {
        let field = Field::Number {
            instructions: "How many tests failed.".to_owned(),
            null_threshold: 0.5,
        };
        let span = at(LOG, "2 failed");
        let digit = [span[0], span[0] + 1];
        let good = serde_json::json!({"type": "number", "value": 2, "span": digit});
        assert!(matches!(
            read_answer(LOG, &field, &good).unwrap(),
            Extracted::Number { value: Some(v), dropped: false, .. } if v == 2.0
        ));
        let wrong = serde_json::json!({"type": "number", "value": 5, "span": digit});
        assert!(matches!(
            read_answer(LOG, &field, &wrong).unwrap(),
            Extracted::Number {
                value: None,
                dropped: true,
                ..
            }
        ));
    }

    #[test]
    fn a_null_value_is_an_honest_nothing_not_a_drop() {
        let answer = serde_json::json!({"type": "text", "value": null, "p_null": 0.97});
        assert!(matches!(
            read_answer(LOG, &text_field(), &answer).unwrap(),
            Extracted::Text { value: None, dropped: false, p_null: Some(p) } if p > 0.9
        ));
    }

    #[test]
    fn an_answer_of_the_wrong_kind_is_rejected() {
        let answer = serde_json::json!({"type": "list", "value": []});
        assert!(read_answer(LOG, &text_field(), &answer).is_err());
    }

    #[test]
    fn fields_reject_what_the_endpoint_would_refuse() {
        assert!(
            Field::Text {
                instructions: "x".into(),
                null_threshold: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(
            Field::Estimate {
                instructions: "x".into(),
                min: 5.0,
                max: 1.0,
                step: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Field::Estimate {
                instructions: "x".into(),
                min: 0.0,
                max: 30.0,
                step: Some(0.0)
            }
            .validate()
            .is_err()
        );
        assert!(
            Field::TextList {
                instructions: " ".into()
            }
            .validate()
            .is_err()
        );
        assert!(text_field().validate().is_ok());
        assert!(validate_field_id("failure_lines").is_ok());
        assert!(validate_field_id("bad id").is_err());
    }

    #[test]
    fn the_wire_spec_matches_the_documented_shape() {
        let list = Field::TextList {
            instructions: "x".into(),
        }
        .to_json();
        assert_eq!(list["type"], "list");
        assert_eq!(list["items"], "text");
        let estimate = Field::Estimate {
            instructions: "x".into(),
            min: 0.0,
            max: 30.0,
            step: Some(1.0),
        }
        .to_json();
        assert_eq!(estimate["step"], 1.0);
        assert!(text_field().to_json()["null_threshold"].as_f64().is_some());
    }

    #[test]
    fn a_reply_missing_a_field_or_truncated_is_rejected() {
        let fields: BTreeMap<String, Field> = [("loc".to_owned(), text_field())].into();
        let missing = br#"{"model":"liquid/d1.2-20261116","answers":{}}"#;
        assert!(read_reply(LOG, &fields, missing, None, 1).is_err());
        let truncated =
            br#"{"model":"m","answers":{"loc":{"type":"text","value":null}},"truncated":true}"#;
        assert!(read_reply(LOG, &fields, truncated, None, 1).is_err());
        let ok = br#"{"model":"liquid/d1.2-20261116","answers":{"loc":{"type":"text","value":null,"p_null":0.9}},"usage":{"input_tokens":70,"output_tokens":0},"id":"gen-1"}"#;
        let read = read_reply(LOG, &fields, ok, None, 5).unwrap();
        assert_eq!(read.model, "liquid/d1.2-20261116");
        assert_eq!(read.usage.input_tokens, Some(70));
        assert_eq!(read.request_id.as_deref(), Some("gen-1"));
    }
}
