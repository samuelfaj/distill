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
    questions: BTreeMap<String, serde_json::Value>,
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

#[async_trait]
impl distill_tools::types::resources::JevDecider for ShellJevDecider {
    async fn decide(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
        let input: JevDecideInput = serde_json::from_value(input).map_err(|e| {
            distill_tool_runtime::ToolError::custom("invalid_jev_decide_input", e.to_string())
        })?;
        if input.questions.is_empty() {
            return Err(distill_tool_runtime::ToolError::custom(
                "invalid_jev_decide_input",
                "questions must not be empty",
            ));
        }
        let state_text = serde_json::to_string(&input.state).unwrap_or_default();
        let questions_text = serde_json::to_string(&input.questions).unwrap_or_default();
        if source_has_secret(&state_text) || source_has_secret(&questions_text) {
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
        let (state, coverage, long_context) = build_state(input.state, &sources);
        let serialized_len = serde_json::to_vec(&state).map_err(|e| {
            distill_tool_runtime::ToolError::custom("jev_state_error", e.to_string())
        })?.len();
        let use_long = long_context || serialized_len > STANDARD_STATE_BUDGET;
        let cfg = crate::jev::resolve_config_from_disk();
        let flags = crate::jev::flags_from(&cfg);
        if !flags.enabled || flags.shadow {
            return Err(distill_tool_runtime::ToolError::custom("jev_unavailable", "Jev decisions are disabled or shadow-only"));
        }
        let client = crate::jev::client_cached().filter(|client| client.credential_present())
            .ok_or_else(|| distill_tool_runtime::ToolError::custom("jev_unavailable", "Jev client or OpenRouter credential unavailable"))?;
        let observed = crate::jev::observed_client(client, "jev_decide");
        let client = observed.as_ref().unwrap_or(client);
        tracing::debug!(session_id = %self.session_id, long_context = use_long, "jev_decide request");
        let answer = if use_long { client.ask_long(&state, &questions).await } else { client.ask(&state, &questions).await }
            .map_err(|e| distill_tool_runtime::ToolError::custom("jev_decision_failed", e.to_string()))?;
        let answers = serde_json::to_value(&answer.answers).map_err(|e| distill_tool_runtime::ToolError::custom("jev_result_error", e.to_string()))?;
        Ok(serde_json::json!({
            "model": answer.model,
            "answers": answers,
            "usage": answer.usage,
            "latency_ms": answer.latency_ms,
            "long_context": use_long,
            "coverage": coverage.into_iter().map(|entry| serde_json::json!({
                "id": entry.id, "path": entry.path, "total_lines": entry.total_lines,
                "sent_lines": entry.sent_lines, "complete": entry.complete
            })).collect::<Vec<_>>(),
        }))
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
