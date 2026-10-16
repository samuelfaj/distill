// Modified for Distill by Samuel Fajreldines, 2026.
//! `ask_stored_output` — ask the utility model a question about a stored tool output.
//!
//! The session-bound work (path check, lane, model calls) lives behind
//! [`StoredOutputAskerClient`], implemented in distill-shell.

use crate::types::output::ToolOutput;
use crate::types::resources::StoredOutputAskerClient;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_io::ToolInput;

pub const ASK_STORED_OUTPUT_TOOL_NAME: &str = "ask_stored_output";

const DESCRIPTION: &str = "Ask a question about a stored tool output (the path in a `full output stored at …` or truncated `full output at:` footer). Omit path to search outputs from this session. Returns verbatim excerpts with original paths and line ranges.";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct AskStoredOutputInput {
    /// Path from a stored-output footer. Omit to search this session’s outputs.
    pub path: Option<String>,
    /// What to find in that output.
    pub question: String,
}

impl TryFrom<ToolInput> for AskStoredOutputInput {
    type Error = String;
    fn try_from(value: ToolInput) -> Result<Self, Self::Error> {
        match value {
            ToolInput::Dynamic(v) => {
                serde_json::from_value(v).map_err(|e| format!("AskStoredOutputInput: {e}"))
            }
            _ => Err("expected Dynamic variant for AskStoredOutputInput".into()),
        }
    }
}

impl From<AskStoredOutputInput> for ToolInput {
    fn from(value: AskStoredOutputInput) -> Self {
        ToolInput::Dynamic(
            serde_json::to_value(value).expect("AskStoredOutputInput serializes to JSON"),
        )
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskStoredOutputOutput {
    pub text: String,
}

impl distill_tool_runtime::ToolOutput for AskStoredOutputOutput {}

impl From<AskStoredOutputOutput> for ToolOutput {
    fn from(output: AskStoredOutputOutput) -> Self {
        ToolOutput::Text(output.text.into())
    }
}

#[derive(Debug, Default)]
pub struct AskStoredOutputTool;

impl crate::types::tool_metadata::ToolMetadata for AskStoredOutputTool {
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

impl distill_tool_runtime::Tool for AskStoredOutputTool {
    type Args = AskStoredOutputInput;
    type Output = AskStoredOutputOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(ASK_STORED_OUTPUT_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            ASK_STORED_OUTPUT_TOOL_NAME,
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

    #[tracing::instrument(name = "tool.ask_stored_output", skip_all)]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: AskStoredOutputInput,
    ) -> Result<AskStoredOutputOutput, distill_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let asker = resources
            .lock()
            .await
            .get::<StoredOutputAskerClient>()
            .cloned()
            .ok_or_else(|| {
                distill_tool_runtime::ToolError::custom(
                    "stored_output_unavailable",
                    "ask_stored_output is unavailable in this session; read the file with read_file offset/limit or grep.",
                )
            })?;
        let text = match input.path {
            Some(path) => asker.0.ask(&path, &input.question).await?,
            None => asker.0.search(&input.question).await?,
        };
        Ok(AskStoredOutputOutput { text })
    }
}
