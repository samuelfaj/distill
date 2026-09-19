use serde::{Deserialize, Serialize};

use crate::identity::{SessionId, ToolCallId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolRequest {
    /// Execute a tool.
    /// The streaming response is a sequence of `ToolChunk::Output` / `Progress` chunks ending with exactly one `ToolChunk::Final`.
    Call(ToolCallArgs),
    /// List the registered tool definitions.
    /// The response is a single `ToolChunk::Definitions(Vec<ToolDef>)`.
    Definitions,
}

/// Arguments for `ToolRequest::Call`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallArgs {
    /// Session id the tool runs in.
    pub session: SessionId,
    /// Registered tool name (e.g. `"read_file"`).
    pub tool_name: String,
    /// JSON-encoded input arguments. The shape is tool-specific.
    #[serde(default)]
    pub input_json: String,
    /// Caller-assigned tool call id (used for cancellation and for correlating tool-stream chunks back to this call).
    pub call_id: ToolCallId,
}
