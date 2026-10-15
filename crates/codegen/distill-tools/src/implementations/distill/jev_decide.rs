// Modified for Distill by Samuel Fajreldines, 2026.
//! Native tool for consulting Jev about typed choices.
use crate::types::output::ToolOutput;
use crate::types::resources::JevDeciderClient;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_io::ToolInput;
use std::collections::BTreeMap;
pub const JEV_DECIDE_TOOL_NAME: &str = "jev_decide";
const DESCRIPTION: &str = r#"Consult Jev for evidence-backed decisions among caller-supplied choices or scores. Supply nonempty questions keyed by stable IDs and either non-null state or source/image IDs. Each question is tagged: {"type":"noul","instructions":"...","criteria":{"true":"...","false":"..."}}; {"type":"choice","instructions":"...","criteria":{"label":"meaning"}}; or {"type":"score","instructions":"...","criteria":["low","high"]}. Criteria must define exact options/rubric and evaluation rules. `sources` and `images` are identifiers resolved by the runtime, not payloads; image use requires runtime image support. Jev advises only; it does not authorize actions or generate free-form choices. Read-only call may incur network cost."#;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JevDecideInput {
    /// Small context needed to decide; avoid copying long source payloads.
    pub state: Option<serde_json::Value>,
    /// Nonempty map of stable question IDs to tagged question objects.
    /// Example: {"retain":{"type":"noul","instructions":"Keep this evidence?","criteria":{"true":"needed","false":"irrelevant"}}};
    /// choice uses `{"type":"choice","instructions":"Pick best","criteria":{"a":"...","b":"..."}}`;
    /// score uses `{"type":"score","instructions":"Rate fit","criteria":["poor","good"]}`.
    pub questions: BTreeMap<String, serde_json::Value>,
    /// Source IDs resolvable by runtime. Provide non-null `state` or at least one source/image ID.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Image IDs resolvable only when runtime image support exists; image support is not guaranteed.
    #[serde(default)]
    pub images: Vec<String>,
}
impl TryFrom<ToolInput> for JevDecideInput {
    type Error = String;
    fn try_from(value: ToolInput) -> Result<Self, Self::Error> {
        let input: Self = match value {
            ToolInput::Dynamic(v) => {
                serde_json::from_value(v).map_err(|e| format!("JevDecideInput: {e}"))?
            }
            _ => return Err("expected Dynamic variant for JevDecideInput".into()),
        };
        input.validate()?;
        Ok(input)
    }
}
impl JevDecideInput {
    fn validate(&self) -> Result<(), String> {
        if self.questions.is_empty() {
            return Err("questions must contain at least one decision question".into());
        }
        if self.state.as_ref().is_none_or(serde_json::Value::is_null)
            && self.sources.is_empty()
            && self.images.is_empty()
        {
            return Err("provide non-null state or at least one source or image identifier".into());
        }
        for (id, question) in &self.questions {
            let Some(object) = question.as_object() else {
                return Err(format!("question `{id}` must be an object"));
            };
            let Some(kind) = object.get("type").and_then(serde_json::Value::as_str) else {
                return Err(format!("question `{id}` must include string `type`"));
            };
            if !object.contains_key("instructions") {
                return Err(format!("question `{id}` must include `instructions`"));
            }
            let valid = match kind {
                "noul" => object
                    .get("criteria")
                    .is_none_or(|criteria| criteria.is_object()),
                "choice" => object
                    .get("criteria")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|c| !c.is_empty()),
                "score" => object
                    .get("criteria")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|c| c.len() >= 2),
                _ => false,
            };
            if !valid {
                return Err(format!(
                    "question `{id}` has malformed `{kind}` type or criteria"
                ));
            }
        }
        let payload = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if payload.len() > 64 * 1024 {
            return Err("jev_decide input exceeds 64 KiB payload limit".into());
        }
        Ok(())
    }
}
impl From<JevDecideInput> for ToolInput {
    fn from(value: JevDecideInput) -> Self {
        ToolInput::Dynamic(serde_json::to_value(value).expect("JevDecideInput serializes to JSON"))
    }
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JevDecideOutput {
    pub result: serde_json::Value,
}
impl distill_tool_runtime::ToolOutput for JevDecideOutput {}
impl From<JevDecideOutput> for ToolOutput {
    fn from(output: JevDecideOutput) -> Self {
        ToolOutput::Text(
            serde_json::to_string(&output.result)
                .expect("Jev decision is JSON")
                .into(),
        )
    }
}
#[derive(Debug, Default)]
pub struct JevDecideTool;
impl crate::types::tool_metadata::ToolMetadata for JevDecideTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }
    fn description_template(&self) -> &str {
        DESCRIPTION
    }
}
impl distill_tool_runtime::Tool for JevDecideTool {
    type Args = JevDecideInput;
    type Output = JevDecideOutput;
    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(JEV_DECIDE_TOOL_NAME).expect("valid tool id")
    }
    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            JEV_DECIDE_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }
    fn capabilities(&self) -> distill_tool_protocol::ToolCapabilities {
        distill_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(distill_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }
    #[tracing::instrument(name = "tool.jev_decide", skip_all)]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: JevDecideInput,
    ) -> Result<JevDecideOutput, distill_tool_runtime::ToolError> {
        input
            .validate()
            .map_err(|e| distill_tool_runtime::ToolError::custom("invalid_jev_decide_input", e))?;
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let decider = resources
            .lock()
            .await
            .get::<JevDeciderClient>()
            .cloned()
            .ok_or_else(|| {
                distill_tool_runtime::ToolError::custom(
                    "jev_unavailable",
                    "jev_decide is unavailable in this session",
                )
            })?;
        let result = decider
            .0
            .decide(serde_json::to_value(input).expect("JevDecideInput serializes to JSON"))
            .await?;
        Ok(JevDecideOutput { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::{JevDecider, JevDeciderClient, Resources};
    use distill_tool_runtime::Tool as _;
    use std::sync::{Arc, Mutex};

    fn input(value: serde_json::Value) -> Result<JevDecideInput, String> {
        JevDecideInput::try_from(ToolInput::Dynamic(value))
    }

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "state": {"task":"choose"},
            "questions": {"pick":{"type":"choice","instructions":"Pick one","criteria":{"a":"first","b":"second"}}}
        })
    }

    #[test]
    fn model_schema_documents_typed_questions_and_evidence_inputs() {
        let schema = serde_json::to_string(&schemars::schema_for!(JevDecideInput)).unwrap();
        for expected in [
            "noul",
            "choice",
            "score",
            "criteria",
            "Source IDs",
            "Image IDs",
        ] {
            assert!(
                schema.contains(expected),
                "schema must document {expected}: {schema}"
            );
        }
    }

    #[test]
    fn input_requires_nonempty_questions_and_state_or_evidence() {
        let mut value = valid();
        value["questions"] = serde_json::json!({});
        assert!(input(value).unwrap_err().contains("at least one"));
        let mut value = valid();
        value["state"] = serde_json::Value::Null;
        assert!(input(value).unwrap_err().contains("provide non-null state"));
        let mut value = valid();
        value["state"] = serde_json::Value::Null;
        value["sources"] = serde_json::json!(["tool-output:1"]);
        assert!(input(value).is_ok());
    }

    #[test]
    fn input_rejects_unknown_fields_and_malformed_questions() {
        let mut extra = valid();
        extra["model"] = serde_json::json!("not-allowed");
        assert!(input(extra).is_err());
        for question in [
            serde_json::json!("not an object"),
            serde_json::json!({"instructions":"missing type"}),
            serde_json::json!({"type":"choice","instructions":"pick","criteria":{}}),
            serde_json::json!({"type":"score","instructions":"rate","criteria":["only"]}),
            serde_json::json!({"type":"other","instructions":"unknown"}),
        ] {
            let mut value = valid();
            value["questions"]["bad"] = question;
            assert!(input(value).is_err());
        }
    }

    #[test]
    fn tool_is_read_only() {
        assert!(JevDecideTool.capabilities().is_read_only);
    }

    struct MockDecider {
        result: serde_json::Value,
        input: Mutex<Option<serde_json::Value>>,
    }
    #[async_trait::async_trait]
    impl JevDecider for MockDecider {
        async fn decide(
            &self,
            input: serde_json::Value,
        ) -> Result<serde_json::Value, distill_tool_runtime::ToolError> {
            *self.input.lock().unwrap() = Some(input);
            Ok(self.result.clone())
        }
    }

    #[tokio::test]
    async fn unavailable_resource_returns_clear_error() {
        let mut ctx = distill_tool_runtime::ToolCallContext::default();
        ctx.insert(Resources::new().into_shared());
        let error = JevDecideTool
            .run(ctx, serde_json::from_value(valid()).unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("jev_decide is unavailable"));
    }

    #[tokio::test]
    async fn mock_client_preserves_complete_result_and_input() {
        let result = serde_json::json!({"answers":{"pick":{"choice":"a","probabilities":{"a":0.71,"b":0.29},"confidence":0.8}}});
        let mock = Arc::new(MockDecider {
            result: result.clone(),
            input: Mutex::new(None),
        });
        let mut resources = Resources::new();
        resources.insert(JevDeciderClient(mock.clone()));
        let mut ctx = distill_tool_runtime::ToolCallContext::default();
        ctx.insert(resources.into_shared());
        let args: JevDecideInput = serde_json::from_value(valid()).unwrap();
        let output = JevDecideTool.run(ctx, args.clone()).await.unwrap();
        assert_eq!(output.result, result);
        assert_eq!(
            *mock.input.lock().unwrap(),
            Some(serde_json::to_value(args).unwrap())
        );
        let surface: ToolOutput = output.into();
        let ToolOutput::Text(text) = surface else {
            panic!("expected JSON text output")
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text.text).unwrap(),
            result
        );
    }
}
