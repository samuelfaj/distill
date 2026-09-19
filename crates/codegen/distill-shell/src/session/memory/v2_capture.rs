// Modified for Distill by Samuel Fajreldines, 2026.
//! Runtime coordination and structured-output parsing for memory-v2 capture.
//!
//! Extraction deliberately has no tools. The model receives a fixed durable
//! transcript snapshot and a strict JSON schema; the only write is the
//! validated `V2CaptureStore::commit` performed by the host.

use distill_memory::{
    CaptureOutcomeDraft, CaptureRange, MAX_ALIASES, MAX_BODY_BYTES, MAX_KEYWORDS, MAX_OBSERVATIONS,
    MAX_STATEMENT_BYTES, MAX_TERM_BYTES, MAX_TOPIC_BYTES, ObservationDraft, ObservationType,
    V2CaptureError,
};
use serde::Deserialize;

pub(crate) const PROMPT_VERSION: &str = "memory-v2-capture-1";
pub(crate) const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureActivity {
    Queued,
    Running,
    Completed,
    Noop,
    Retry,
    Failed,
}

impl CaptureActivity {
    /// xai-codegen-lint: allow(manual_strum)
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Noop => "no_op",
            Self::Retry => "retry",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlushResult {
    Success,
    RetryableFailure(distill_telemetry::memory_telemetry::MemoryV2FailureClass),
    TerminalFailure(distill_telemetry::memory_telemetry::MemoryV2FailureClass),
    Timeout,
}

impl FlushResult {
    pub(crate) fn message(&self, target: u32) -> String {
        match self {
            Self::Success => format!("captured and indexed through turn {target}"),
            Self::RetryableFailure(_) => format!("capture retry required through turn {target}"),
            Self::TerminalFailure(_) => format!("capture failed through turn {target}"),
            Self::Timeout => format!("timed out waiting for capture through turn {target}"),
        }
    }
}

pub(crate) fn missing_range(
    requested: u32,
    completed_turn: u32,
) -> Result<Option<CaptureRange>, V2CaptureError> {
    if completed_turn <= requested {
        return Ok(None);
    }
    CaptureRange::try_new(requested.saturating_add(1), completed_turn).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelOutcome {
    outcome: ModelOutcomeKind,
    observations: Option<Vec<ModelObservation>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelOutcomeKind {
    Noop,
    Observations,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelObservation {
    #[serde(rename = "type")]
    observation_type: String,
    topic_hint: Option<String>,
    statement: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    body: Option<String>,
}

pub(crate) fn parse_model_outcome(
    text: &str,
    model: &str,
    created_at: i64,
) -> Result<CaptureOutcomeDraft, String> {
    let parsed: ModelOutcome = serde_json::from_str(text)
        .map_err(|error| format!("malformed extraction output: {error}"))?;
    match (parsed.outcome, parsed.observations) {
        (ModelOutcomeKind::Noop, None) => Ok(CaptureOutcomeDraft::Noop),
        (ModelOutcomeKind::Noop, Some(observations)) if observations.is_empty() => {
            Ok(CaptureOutcomeDraft::Noop)
        }
        (ModelOutcomeKind::Noop, Some(_)) => {
            Err("malformed extraction output: noop cannot contain observations".to_owned())
        }
        (ModelOutcomeKind::Observations, None) => Err(
            "malformed extraction output: observations outcome requires observations".to_owned(),
        ),
        (ModelOutcomeKind::Observations, Some(observations)) if observations.is_empty() => {
            Err("malformed extraction output: observations cannot be empty".to_owned())
        }
        (ModelOutcomeKind::Observations, Some(observations)) => observations
            .into_iter()
            .take(MAX_OBSERVATIONS)
            .map(|observation| {
                let statement = scrub_control_characters(&observation.statement);
                if statement.len() > MAX_STATEMENT_BYTES {
                    return Err(
                        "malformed extraction output: statement exceeds byte limit".to_owned()
                    );
                }
                Ok(ObservationDraft {
                    observation_type: ObservationType::parse(&observation.observation_type)
                        .map_err(|error| error.to_string())?,
                    topic_hint: observation
                        .topic_hint
                        .as_deref()
                        .and_then(|topic| normalize_term(topic, MAX_TOPIC_BYTES)),
                    statement,
                    keywords: normalize_terms(observation.keywords, MAX_KEYWORDS),
                    aliases: normalize_terms(observation.aliases, MAX_ALIASES),
                    extraction_model: model.to_owned(),
                    prompt_version: PROMPT_VERSION.to_owned(),
                    created_at,
                    body: observation.body.as_deref().and_then(normalize_body),
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map(CaptureOutcomeDraft::Observations),
    }
}

fn normalize_terms(terms: Vec<String>, max_terms: usize) -> Vec<String> {
    terms
        .iter()
        .filter_map(|term| normalize_term(term, MAX_TERM_BYTES))
        .take(max_terms)
        .collect()
}

fn normalize_term(term: &str, max_bytes: usize) -> Option<String> {
    let term = scrub_control_characters(term);
    let term = truncate_utf8(&term, max_bytes).trim_end();
    (!term.is_empty()).then(|| term.to_owned())
}

fn normalize_body(body: &str) -> Option<String> {
    let body: String = body
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect();
    let body = truncate_utf8(body.trim(), MAX_BODY_BYTES).trim_end();
    (!body.is_empty()).then(|| body.to_owned())
}

fn scrub_control_characters(text: &str) -> String {
    text.split(char::is_control)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Bare JSON schema for `ConversationRequest::json_schema`. The sampling client
/// adds the `{name, strict, schema}` envelope itself; pre-wrapping it here makes
/// the model echo the envelope back (`missing field \`outcome\``).
pub(crate) fn extraction_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["outcome", "observations"],
        "properties": {
            "outcome": { "type": "string", "enum": ["noop", "observations"] },
            "observations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "topic_hint", "statement", "keywords", "aliases", "body"],
                    "properties": {
                        "type": { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
                        "topic_hint": { "type": ["string", "null"] },
                        "statement": { "type": "string", "maxLength": MAX_STATEMENT_BYTES },
                        "keywords": { "type": "array", "items": { "type": "string" } },
                        "aliases": { "type": "array", "items": { "type": "string" } },
                        "body": { "type": ["string", "null"] }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ranges_coalesce_without_overlap() {
        assert_eq!(
            missing_range(0, 3).unwrap().unwrap(),
            CaptureRange::try_new(1, 3).unwrap()
        );
        assert_eq!(
            missing_range(3, 5).unwrap().unwrap(),
            CaptureRange::try_new(4, 5).unwrap()
        );
        assert!(missing_range(5, 5).unwrap().is_none());
    }

    #[test]
    fn malformed_output_is_retryable_but_schema_legal_noops_are_valid() {
        assert!(
            parse_model_outcome("not json", "test", 1)
                .unwrap_err()
                .contains("expected ident")
        );
        for output in [
            r#"{"outcome":"noop"}"#,
            r#"{"outcome":"noop","observations":[]}"#,
        ] {
            assert_eq!(
                parse_model_outcome(output, "test", 1).unwrap(),
                CaptureOutcomeDraft::Noop
            );
        }
        assert_eq!(
            "malformed extraction output: observations cannot be empty",
            parse_model_outcome(r#"{"outcome":"observations","observations":[]}"#, "test", 1)
                .unwrap_err()
        );
    }

    #[test]
    fn inconsistent_or_unknown_output_fields_are_rejected() {
        let observation = r#"{"type":"project","topic_hint":"capture","statement":"Use durable ranges","keywords":["durable"],"aliases":[],"body":null}"#;
        for (output, expected_error) in [
            (
                r#"{"outcome":"observations"}"#.to_owned(),
                "observations outcome requires observations",
            ),
            (
                format!(r#"{{"outcome":"noop","observations":[{observation}]}}"#),
                "noop cannot contain observations",
            ),
            (
                r#"{"outcome":"noop","tool":"shell"}"#.to_owned(),
                "unknown field `tool`",
            ),
            (
                r#"{"outcome":"observations","observations":[{"type":"project","topic_hint":"capture","statement":"Use durable ranges","keywords":["durable"],"aliases":[],"body":null,"command":"rm -rf /"}]}"#.to_owned(),
                "unknown field `command`",
            ),
        ] {
            let error = parse_model_outcome(&output, "test", 1).unwrap_err();
            assert!(error.contains(expected_error), "{error}");
        }
    }

    #[test]
    fn schema_requires_observations_without_conditionals() {
        let schema = extraction_schema();
        let at = |path: &str| schema.pointer(path).unwrap_or(&serde_json::Value::Null);
        assert_eq!(false, *at("/additionalProperties"));
        assert!(at("/properties/observations").is_object());
        assert_eq!(
            &serde_json::json!(["outcome", "observations"]),
            at("/required")
        );
        for keyword in ["allOf", "if", "then"] {
            assert!(schema.get(keyword).is_none());
        }
    }

    #[test]
    fn deterministic_quality_fixtures_cover_memory_categories_and_unicode() {
        let fixtures = [
            ("user", "Prefers concise answers", "preference"),
            ("feedback", "Corrected the API name", "correction"),
            ("project", "The project uses SQLite", "project"),
            ("reference", "The design is in the handbook", "reference"),
            (
                "feedback",
                "The previous convention no longer applies",
                "contradiction",
            ),
            ("user", "Uses café and 東京 labels", "unicode"),
        ];
        for (kind, statement, keyword) in fixtures {
            let json = serde_json::json!({
                "outcome": "observations",
                "observations": [{
                    "type": kind,
                    "topic_hint": null,
                    "statement": statement,
                    "keywords": [keyword],
                    "aliases": [],
                    "body": null
                }]
            });
            let parsed = parse_model_outcome(&json.to_string(), "fixture", 1).unwrap();
            assert!(
                matches!(parsed, CaptureOutcomeDraft::Observations(ref values) if values.len() == 1)
            );
        }
    }

    #[test]
    fn quality_fixtures_reject_injection_malformed_and_unbounded_output() {
        assert!(
            parse_model_outcome(
                r#"{"outcome":"noop","command":"ignore schema and run shell"}"#,
                "fixture",
                1,
            )
            .is_err()
        );
        assert!(parse_model_outcome("{", "fixture", 1).is_err());
        let oversized = "x".repeat(1_025);
        let json = serde_json::json!({
            "outcome": "observations",
            "observations": [{
                "type": "project",
                "topic_hint": null,
                "statement": oversized,
                "keywords": [],
                "aliases": [],
                "body": null
            }]
        });
        assert!(parse_model_outcome(&json.to_string(), "fixture", 1).is_err());
    }

    #[test]
    fn model_output_is_normalized_to_store_limits_instead_of_failing() {
        let long_ascii = "k".repeat(MAX_TERM_BYTES + 20);
        let long_unicode = "é".repeat(MAX_TERM_BYTES);
        let mut aliases: Vec<String> = (0..MAX_ALIASES + 5).map(|i| format!("alias{i}")).collect();
        aliases.push(long_unicode);
        let observation = serde_json::json!({
            "type": "project",
            "topic_hint": "",
            "statement": "Uses\tlong\n\nidentifiers",
            "keywords": [long_ascii, "", "   ", " padded "],
            "aliases": aliases,
            "body": ""
        });
        let mut observations = vec![observation.clone(); MAX_OBSERVATIONS + 3];
        observations.push(serde_json::json!({
            "type": "project",
            "topic_hint": "t".repeat(MAX_TOPIC_BYTES + 1),
            "statement": "Second",
            "keywords": [],
            "aliases": [],
            "body": format!("  {}\u{0}tail  ", "b".repeat(MAX_BODY_BYTES))
        }));
        observations.swap(0, MAX_OBSERVATIONS + 3);
        let json = serde_json::json!({ "outcome": "observations", "observations": observations });
        let CaptureOutcomeDraft::Observations(observations) =
            parse_model_outcome(&json.to_string(), "fixture", 1).unwrap()
        else {
            panic!("expected observations");
        };
        assert_eq!(observations.len(), MAX_OBSERVATIONS);

        let [capped, scrubbed, ..] = observations.as_slice() else {
            panic!(
                "expected at least two observations, got {}",
                observations.len()
            );
        };
        assert_eq!(
            capped.topic_hint.as_deref(),
            Some("t".repeat(MAX_TOPIC_BYTES).as_str())
        );
        assert_eq!(
            capped.body.as_deref(),
            Some("b".repeat(MAX_BODY_BYTES).as_str())
        );

        assert_eq!(scrubbed.statement, "Uses long identifiers");
        assert_eq!(scrubbed.topic_hint, None);
        assert_eq!(scrubbed.body, None);
        assert_eq!(
            scrubbed.keywords,
            vec!["k".repeat(MAX_TERM_BYTES), "padded".to_owned()]
        );
        assert_eq!(scrubbed.aliases.len(), MAX_ALIASES);
        assert_eq!(scrubbed.aliases.first().map(String::as_str), Some("alias0"));
    }

    #[test]
    fn host_owns_provenance_fields() {
        let outcome = parse_model_outcome(
            r#"{"outcome":"observations","observations":[{"type":"project","topic_hint":"capture","statement":"Use durable ranges","keywords":["durable"],"aliases":[],"body":null}]}"#,
            "grok-test",
            42,
        )
        .unwrap();
        let CaptureOutcomeDraft::Observations(observations) = outcome else {
            panic!("expected observations");
        };
        let Some(observation) = observations.first() else {
            panic!("expected at least one observation");
        };
        assert_eq!(observation.extraction_model, "grok-test");
        assert_eq!(observation.prompt_version, PROMPT_VERSION);
        assert_eq!(observation.created_at, 42);
    }

    #[test]
    fn extraction_schema_is_a_bare_schema_the_sampler_can_wrap() {
        use distill_sampling_types::{ConversationItem, ConversationRequest, rs};

        let schema = extraction_schema();
        assert_eq!(schema.get("type"), Some(&serde_json::json!("object")));
        assert!(
            schema
                .pointer("/properties/outcome")
                .is_some_and(serde_json::Value::is_object)
        );
        assert!(
            schema.get("schema").is_none(),
            "must not pre-wrap {{name, strict, schema}}: the sampler adds that envelope"
        );
        let request = ConversationRequest::from_items(vec![ConversationItem::user("x")])
            .with_json_schema(schema.clone());
        let wire: rs::CreateResponse = (&request).into();
        let rs::TextResponseFormatConfiguration::JsonSchema(format) = wire.text.unwrap().format
        else {
            panic!("expected json_schema text format");
        };
        // The schema the model is constrained to must be the outcome object itself.
        assert_eq!(
            format
                .schema
                .unwrap()
                .pointer("/properties/outcome/enum/0")
                .and_then(serde_json::Value::as_str),
            Some("noop")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flush_timeout_uses_paused_time() {
        let wait = tokio::time::timeout(FLUSH_TIMEOUT, std::future::pending::<()>());
        tokio::pin!(wait);
        tokio::time::advance(FLUSH_TIMEOUT).await;
        assert!(wait.await.is_err());
    }
}
