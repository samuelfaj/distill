// Modified for Distill by Samuel Fajreldines, 2026.
//! Session runtime for the native Jev decision tool.
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::BTreeMap;

const STANDARD_STATE_BUDGET: usize = 32 * 1024;
const LONG_SOURCE_BUDGET: usize = 700 * 1024;
const SERIALIZED_STATE_BUDGET: usize = 760 * 1024;
const CONTEXT_METADATA_RESERVE: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JevDecideInput {
    state: Option<serde_json::Value>,
    #[serde(default)]
    questions: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    images: Vec<String>,
}

#[derive(Debug)]
struct SourceCoverage {
    id: String,
    path: String,
    total_lines: usize,
    sent_lines: usize,
    complete: bool,
}

fn source_has_secret(text: &str) -> bool {
    distill_workspace::jev::crushers::utility_secret_presence(text).is_some()
}

pub(crate) struct ShellJevDecider {
    pub session_id: String,
}

fn build_state(
    context: Option<serde_json::Value>,
    sources: &[(String, String, String)],
) -> (serde_json::Value, Vec<SourceCoverage>, bool) {
    let total_source_bytes: usize = sources.iter().map(|(_, _, text)| text.len()).sum();
    let long_context = total_source_bytes > STANDARD_STATE_BUDGET;
    let budget = if long_context { LONG_SOURCE_BUDGET } else { usize::MAX };
    let mut remaining = budget.saturating_sub(if long_context { CONTEXT_METADATA_RESERVE } else { 0 });
    let mut coverage = Vec::with_capacity(sources.len());
    let mut source_values = Vec::with_capacity(sources.len());
    for (id, path, text) in sources {
        let lines: Vec<&str> = text.lines().collect();
        let mut numbered = String::new();
        let mut sent_lines = 0;
        for (line_index, line) in lines.iter().enumerate() {
            let formatted = format!("{}: {}\n", line_index + 1, line);
            let escaped_len = serde_json::to_string(&formatted).map_or(usize::MAX, |value| value.len().saturating_sub(2));
            if escaped_len > remaining {
                break;
            }
            remaining -= escaped_len;
            numbered.push_str(&formatted);
            sent_lines += 1;
        }
        source_values.push(serde_json::json!({"id": id, "path": path, "lines": numbered}));
        coverage.push(SourceCoverage {
            id: id.clone(),
            path: path.clone(),
            total_lines: lines.len(),
            sent_lines,
            complete: sent_lines == lines.len(),
        });
    }
    (
        serde_json::json!({"context": context, "sources": source_values}),
        coverage,
        long_context,
    )
}

/// The text an extraction reads: each source under a one-line header, its
/// lines verbatim and unnumbered (a value is copied from the text, so a line
/// number prefix would end up inside it), and where each line starts, in
/// characters, to turn an answer's span back into `S<n>:<line>`.
struct ExtractionText {
    text: String,
    /// `(first character, characters, source id, 1-based line)`, in order.
    lines: Vec<(usize, usize, String, usize)>,
    coverage: Vec<SourceCoverage>,
}

fn build_extraction_text(
    context: Option<&serde_json::Value>,
    sources: &[(String, String, String)],
) -> ExtractionText {
    let budget = distill_workspace::jev::extractions::MAX_EXTRACTION_STATE_BYTES;
    let mut text = String::new();
    let mut chars = 0usize;
    let mut push = |text: &mut String, chars: &mut usize, piece: &str| {
        text.push_str(piece);
        *chars += piece.chars().count();
    };
    if let Some(context) = context.filter(|context| !context.is_null()) {
        let rendered = match context.as_str() {
            Some(text) => text.to_owned(),
            None => context.to_string(),
        };
        push(&mut text, &mut chars, &format!("Context: {rendered}\n\n"));
    }
    let mut lines = Vec::new();
    let mut coverage = Vec::with_capacity(sources.len());
    for (id, path, source) in sources {
        let all: Vec<&str> = source.lines().collect();
        let header = format!("=== {id} ({path}) ===\n");
        let mut sent_lines = 0;
        if text.len() + header.len() < budget {
            push(&mut text, &mut chars, &header);
            for (index, line) in all.iter().enumerate() {
                // The line, its newline and the blank line that ends the source.
                if text.len() + line.len() + 2 > budget {
                    break;
                }
                lines.push((chars, line.chars().count(), id.clone(), index + 1));
                push(&mut text, &mut chars, line);
                push(&mut text, &mut chars, "\n");
                sent_lines += 1;
            }
            push(&mut text, &mut chars, "\n");
        }
        coverage.push(SourceCoverage {
            id: id.clone(),
            path: path.clone(),
            total_lines: all.len(),
            sent_lines,
            complete: sent_lines == all.len(),
        });
    }
    ExtractionText { text, lines, coverage }
}

impl ExtractionText {
    /// `S<n>:<line>` of the source line holding character `at`; `None` for
    /// text that is not a source line (the context, a source header).
    fn locate(&self, at: usize) -> Option<String> {
        let index = self.lines.partition_point(|(start, ..)| *start <= at);
        let (start, len, id, line) = self.lines.get(index.checked_sub(1)?)?;
        (at <= start + len).then(|| format!("{id}:{line}"))
    }

    /// One field's answer as the tool returns it: each value with the source
    /// line it was read from. The client already dropped values the text does
    /// not hold; a value read from the caller's own context or from a source
    /// header is not from a source either, so it is dropped here and counted
    /// with them.
    fn render(&self, extracted: &distill_workspace::jev::extractions::Extracted) -> serde_json::Value {
        use distill_workspace::jev::extractions::Extracted;
        match extracted {
            Extracted::Text { value, p_null, dropped } => {
                let found = value
                    .as_ref()
                    .and_then(|p| Some((p.text.clone(), self.locate(p.start)?)));
                serde_json::json!({
                    "type": "text",
                    "value": found.as_ref().map(|(text, _)| text),
                    "at": found.as_ref().map(|(_, at)| at),
                    "p_null": p_null,
                    "dropped_unverified": *dropped || (value.is_some() && found.is_none()),
                })
            }
            Extracted::Number { value, passage, p_null, dropped } => {
                let at = passage.as_ref().and_then(|p| self.locate(p.start));
                serde_json::json!({
                    "type": "number",
                    "value": value.filter(|_| at.is_some()),
                    "at": at,
                    "p_null": p_null,
                    "dropped_unverified": *dropped || (value.is_some() && at.is_none()),
                })
            }
            Extracted::TextList { items, p_null, dropped } => {
                let found: Vec<serde_json::Value> = items
                    .iter()
                    .filter_map(|p| {
                        let at = self.locate(p.start)?;
                        Some(serde_json::json!({"text": p.text, "at": at, "confidence": p.confidence}))
                    })
                    .collect();
                serde_json::json!({
                    "type": "list",
                    "dropped_unverified": dropped + (items.len() - found.len()),
                    "items": found,
                    "p_null": p_null,
                })
            }
            Extracted::Estimate { value, median, p10, p90 } => serde_json::json!({
                "type": "estimate", "value": value, "median": median, "p10": p10, "p90": p90,
            }),
        }
    }
}

/// The tool's field specs as typed fields; text and number default to a 0.5
/// null threshold, which d1.2 requires above zero.
fn parse_fields(
    fields: BTreeMap<String, serde_json::Value>,
) -> Result<BTreeMap<String, distill_workspace::jev::extractions::Field>, String> {
    use distill_workspace::jev::extractions::Field;
    fields
        .into_iter()
        .map(|(id, spec)| {
            let instructions = spec["instructions"].as_str().unwrap_or_default().to_owned();
            let null_threshold = spec["null_threshold"].as_f64().unwrap_or(0.5);
            let field = match spec["type"].as_str() {
                Some("text") => Field::Text { instructions, null_threshold },
                Some("number") => Field::Number { instructions, null_threshold },
                Some("list") => Field::TextList { instructions },
                Some("estimate") => Field::Estimate {
                    instructions,
                    min: spec["min"].as_f64().ok_or("estimate needs min")?,
                    max: spec["max"].as_f64().ok_or("estimate needs max")?,
                    step: spec["step"].as_f64(),
                },
                _ => return Err(format!("field `{id}` has an unknown type")),
            };
            field.validate().map_err(|e| format!("field `{id}`: {}", e.detail()))?;
            Ok((id, field))
        })
        .collect()
}

impl ShellJevDecider {
    /// Extracts `fields` from the sources on the extraction model.
    async fn extract_fields(
        &self,
        context: Option<&serde_json::Value>,
        fields: BTreeMap<String, serde_json::Value>,
        sources: &[(String, String, String)],
    ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
        let fields = parse_fields(fields)
            .map_err(|e| distill_tool_runtime::ToolError::custom("invalid_jev_field", e))?;
        let built = build_extraction_text(context, sources);
        let client = crate::jev::client_for("jev_decide_fields")
            .filter(|client| client.credential_present())
            .ok_or_else(|| distill_tool_runtime::ToolError::custom("jev_unavailable", "extraction client or OpenRouter credential unavailable"))?;
        let observed = crate::jev::observed_client(client, "jev_decide_fields");
        let client = observed.as_ref().unwrap_or(client);
        tracing::debug!(session_id = %self.session_id, fields = fields.len(), "jev_decide extraction");
        let answer = client
            .extract(&built.text, &fields)
            .await
            .map_err(|e| distill_tool_runtime::ToolError::custom("jev_extraction_failed", e.to_string()))?;
        Ok(serde_json::json!({
            "model": answer.model,
            "fields": answer.answers.iter().map(|(id, extracted)| (id.clone(), built.render(extracted))).collect::<serde_json::Map<_, _>>(),
            "usage": answer.usage,
            "latency_ms": answer.latency_ms,
            "coverage": built.coverage.iter().map(|entry| serde_json::json!({
                "id": entry.id, "path": entry.path, "total_lines": entry.total_lines,
                "sent_lines": entry.sent_lines, "complete": entry.complete
            })).collect::<Vec<_>>(),
        }))
    }
}

#[async_trait]
impl distill_tools::types::resources::JevDecider for ShellJevDecider {
    async fn decide(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
        let mut input: JevDecideInput = serde_json::from_value(input).map_err(|e| {
            distill_tool_runtime::ToolError::custom("invalid_jev_decide_input", e.to_string())
        })?;
        if input.questions.is_empty() && input.fields.is_empty() {
            return Err(distill_tool_runtime::ToolError::custom(
                "invalid_jev_decide_input",
                "questions or fields must not be empty",
            ));
        }
        let state_text = serde_json::to_string(&input.state).unwrap_or_default();
        let questions_text = serde_json::to_string(&input.questions).unwrap_or_default();
        let fields_text = serde_json::to_string(&input.fields).unwrap_or_default();
        if source_has_secret(&state_text)
            || source_has_secret(&questions_text)
            || source_has_secret(&fields_text)
        {
            return Err(distill_tool_runtime::ToolError::custom("jev_source_secret", "state or question appears to contain a secret"));
        }
        if !input.images.is_empty() {
            return Err(distill_tool_runtime::ToolError::custom(
                "jev_images_unsupported",
                "OpenRouter Decisions currently ignores image assets (verified 2026-10-15); describe the image in state or use the main model",
            ));
        }
        if input.state.as_ref().is_none_or(serde_json::Value::is_null) && input.sources.is_empty() {
            return Err(distill_tool_runtime::ToolError::custom(
                "invalid_jev_decide_input",
                "provide non-null state or at least one source identifier",
            ));
        }
        let questions: BTreeMap<String, distill_workspace::jev::types::Question> = input
            .questions
            .into_iter()
            .map(|(id, value)| serde_json::from_value(value).map(|q| (id, q)))
            .collect::<Result<_, _>>()
            .map_err(|e| {
                distill_tool_runtime::ToolError::custom("invalid_jev_question", e.to_string())
            })?;
        for (id, question) in &questions {
            question.validate().map_err(|e| {
                distill_tool_runtime::ToolError::custom(
                    "invalid_jev_question",
                    format!("question `{id}`: {}", e.detail()),
                )
            })?;
        }
        let mut sources = Vec::with_capacity(input.sources.len());
        for (index, path) in input.sources.iter().enumerate() {
            let resolved = crate::stored_output_ask::resolve_stored_original(path)
                .map_err(|e| distill_tool_runtime::ToolError::custom("jev_source_invalid", e))?;
            let text = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
                distill_tool_runtime::ToolError::custom("jev_source_invalid", format!("cannot read stored source: {e}"))
            })?;
            if source_has_secret(&text) {
                return Err(distill_tool_runtime::ToolError::custom("jev_source_secret", "stored source appears to contain a secret"));
            }
            sources.push((format!("S{}", index + 1), resolved.display().to_string(), text));
        }
        let cfg = crate::jev::resolve_config_from_disk();
        let flags = crate::jev::flags_from(&cfg);
        if !flags.enabled || flags.shadow {
            return Err(distill_tool_runtime::ToolError::custom("jev_unavailable", "Jev decisions are disabled or shadow-only"));
        }
        let fields = if input.fields.is_empty() {
            None
        } else {
            Some(
                self.extract_fields(input.state.as_ref(), std::mem::take(&mut input.fields), &sources)
                    .await?,
            )
        };
        if questions.is_empty() {
            return fields.ok_or_else(|| {
                distill_tool_runtime::ToolError::custom("invalid_jev_decide_input", "nothing to ask")
            });
        }
        let (state, coverage, long_context) = build_state(input.state, &sources);
        let serialized_len = serde_json::to_vec(&state).map_err(|e| {
            distill_tool_runtime::ToolError::custom("jev_state_error", e.to_string())
        })?.len();
        let use_long = long_context || serialized_len > STANDARD_STATE_BUDGET;
        let client = crate::jev::client_cached().filter(|client| client.credential_present())
            .ok_or_else(|| distill_tool_runtime::ToolError::custom("jev_unavailable", "Jev client or OpenRouter credential unavailable"))?;
        let observed = crate::jev::observed_client(client, "jev_decide");
        let client = observed.as_ref().unwrap_or(client);
        tracing::debug!(session_id = %self.session_id, long_context = use_long, "jev_decide request");
        let answer = if use_long { client.ask_long(&state, &questions).await } else { client.ask(&state, &questions).await }
            .map_err(|e| distill_tool_runtime::ToolError::custom("jev_decision_failed", e.to_string()))?;
        let answers = serde_json::to_value(&answer.answers).map_err(|e| distill_tool_runtime::ToolError::custom("jev_result_error", e.to_string()))?;
        let mut result = serde_json::json!({
            "model": answer.model,
            "answers": answers,
            "usage": answer.usage,
            "latency_ms": answer.latency_ms,
            "long_context": use_long,
            "coverage": coverage.into_iter().map(|entry| serde_json::json!({
                "id": entry.id, "path": entry.path, "total_lines": entry.total_lines,
                "sent_lines": entry.sent_lines, "complete": entry.complete
            })).collect::<Vec<_>>(),
        });
        if let Some(fields) = fields {
            result["extraction"] = fields;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_tools::types::resources::JevDecider as _;

    #[test]
    fn state_builder_numbers_verbatim_lines_and_reports_coverage() {
        let source = ("S1".into(), "/store/item.txt".into(), "alpha\nbeta\n".into());
        let (state, coverage, long) = build_state(None, &[source]);
        assert!(!long);
        assert_eq!(state["sources"][0]["lines"], "1: alpha\n2: beta\n");
        assert_eq!(coverage[0].total_lines, 2);
        assert_eq!(coverage[0].sent_lines, 2);
        assert!(coverage[0].complete);
    }

    #[test]
    fn state_builder_marks_budget_truncation_incomplete() {
        let text = format!("{}\n{}", "a".repeat(32 * 1024), "b".repeat(700 * 1024));
        let source = ("S1".into(), "/store/large.txt".into(), text);
        let (state, coverage, long) = build_state(None, &[source]);
        assert!(long);
        assert!(state["sources"][0]["lines"].as_str().unwrap().len() <= LONG_SOURCE_BUDGET);
        assert_eq!(coverage[0].sent_lines, 1);
        assert!(!coverage[0].complete);
    }

    #[test]
    fn escaped_large_state_stays_under_long_context_ceiling() {
        let text = "\"".repeat(400 * 1024);
        let source = ("S1".into(), "/store/quotes.txt".into(), text);
        let (state, coverage, long) = build_state(None, &[source]);
        assert!(long);
        assert!(serde_json::to_vec(&state).unwrap().len() <= distill_workspace::jev::client::LONG_CONTEXT_MAX_STATE_BYTES);
        assert!(!coverage[0].complete);
    }

    /// A value is reported at the source line it was read from, so the main
    /// model can cite or open exactly that line instead of the whole output.
    #[test]
    fn extraction_text_maps_spans_back_to_source_lines() {
        let sources = vec![
            ("S1".to_owned(), "/store/a.txt".to_owned(), "ok\nerror: São José falhou\n".to_owned()),
            ("S2".to_owned(), "/store/b.txt".to_owned(), "x\ny\nz\n".to_owned()),
        ];
        let built = build_extraction_text(Some(&serde_json::json!("find the failure")), &sources);
        assert!(built.text.starts_with("Context: find the failure"));
        assert!(!built.text.contains("1: ok"), "lines go out unnumbered");
        let at = |needle: &str| {
            let byte = built.text.find(needle).expect("needle");
            built.text[..byte].chars().count()
        };
        assert_eq!(built.locate(at("José")).as_deref(), Some("S1:2"));
        assert_eq!(built.locate(at("z\n")).as_deref(), Some("S2:3"));
        assert_eq!(built.locate(at("=== S2")), None, "a header is not a source line");
        assert_eq!(built.locate(at("find the failure")), None, "nor is the context");
        assert!(built.coverage.iter().all(|entry| entry.complete));
    }

    /// "From the sources" means from a source line: a value the model copied
    /// from the caller's context or from a header passes the client's span
    /// check, and is still not evidence, so the tool drops it.
    #[test]
    fn a_value_read_outside_the_source_lines_is_dropped() {
        use distill_workspace::jev::extractions::{Extracted, Passage};
        let sources = vec![("S1".to_owned(), "/store/a.txt".to_owned(), "error: disk full\n".to_owned())];
        let built = build_extraction_text(Some(&serde_json::json!("look for req-73")), &sources);
        let passage = |needle: &str| {
            let byte = built.text.find(needle).expect("needle");
            let start = built.text[..byte].chars().count();
            Passage { text: needle.to_owned(), start, end: start + needle.chars().count(), confidence: None }
        };
        let from_context = built.render(&Extracted::Text { value: Some(passage("req-73")), p_null: None, dropped: false });
        assert_eq!(from_context["value"], serde_json::Value::Null);
        assert_eq!(from_context["dropped_unverified"], true);
        let from_source = built.render(&Extracted::Text { value: Some(passage("disk full")), p_null: None, dropped: false });
        assert_eq!(from_source["value"], "disk full");
        assert_eq!(from_source["at"], "S1:1");
        let list = built.render(&Extracted::TextList {
            items: vec![passage("/store/a.txt"), passage("error: disk full")],
            p_null: None,
            dropped: 1,
        });
        assert_eq!(list["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(list["dropped_unverified"], 2);
    }

    /// The text never exceeds the extraction ceiling, so a source cut at the
    /// budget is reported incomplete instead of failing the whole call.
    #[test]
    fn a_source_cut_at_the_budget_stays_under_the_ceiling() {
        let budget = distill_workspace::jev::extractions::MAX_EXTRACTION_STATE_BYTES;
        let header = "=== S1 (/store/big.txt) ===\n".len();
        // The first line ends exactly where the old bound let the closing newline overflow.
        let first = "a".repeat(budget - header - 1);
        let sources = vec![("S1".to_owned(), "/store/big.txt".to_owned(), format!("{first}\nrest\n"))];
        let built = build_extraction_text(None, &sources);
        assert!(built.text.len() <= budget, "{} > {budget}", built.text.len());
        assert!(!built.coverage[0].complete);
    }

    #[test]
    fn fields_parse_with_a_default_null_threshold() {
        let parsed = parse_fields(BTreeMap::from([
            ("n".to_owned(), serde_json::json!({"type": "number", "instructions": "count"})),
            ("e".to_owned(), serde_json::json!({"type": "estimate", "instructions": "rounds", "min": 0, "max": 30})),
        ]))
        .expect("fields parse");
        assert!(matches!(parsed["n"], distill_workspace::jev::extractions::Field::Number { null_threshold, .. } if null_threshold == 0.5));
        assert!(parse_fields(BTreeMap::from([("bad".to_owned(), serde_json::json!({"type": "table", "instructions": "x"}))])).is_err());
    }

    #[test]
    fn rejects_forged_paths_outside_stored_output_roots() {
        let err = crate::stored_output_ask::resolve_stored_original("/etc/hosts").unwrap_err();
        assert!(err.contains("not a stored tool output"));
    }

    #[tokio::test]
    async fn rejects_secret_in_caller_state_before_network() {
        let error = ShellJevDecider { session_id: "test".into() }
            .decide(serde_json::json!({
                "state": {"env": "OPENAI_API_KEY=sk-proj-FAKEKEYabcdefghijklmnopqrstuvwxyz0123456789"},
                "questions": {"q": {"type": "noul", "instructions": "Is it set?"}}
            }))
            .await
            .expect_err("secret-bearing state must be refused");
        assert_eq!(error.details.as_ref().and_then(|d| d["code"].as_str()), Some("jev_source_secret"));
    }

    #[test]
    fn rejects_secret_bearing_source_text() {
        assert!(source_has_secret("OPENAI_API_KEY=sk-proj-FAKEKEYabcdefghijklmnopqrstuvwxyz0123456789"));
    }

    #[tokio::test]
    async fn rejects_images_explicitly() {
        let error = ShellJevDecider { session_id: "test".into() }
            .decide(serde_json::json!({"state": {}, "questions": {"q": {"type": "noul", "instructions": "check"}}, "images": ["image"]}))
            .await
            .expect_err("images must not be sent to decisions API");
        assert!(format!("{error:?}").contains("jev_images_unsupported"));
    }

    #[test]
    fn stored_payload_roundtrips_into_line_numbered_context() {
        let payload = "literal original\nsecond line\n";
        let path = crate::jev_store::store_payload(payload).expect("store test payload");
        let read = std::fs::read_to_string(&path).expect("read stored payload");
        assert_eq!(read, payload);
        let source = ("S1".into(), path.display().to_string(), read);
        let (state, coverage, _) = build_state(None, &[source]);
        assert_eq!(state["sources"][0]["lines"], "1: literal original\n2: second line\n");
        assert!(coverage[0].complete);
    }
}
