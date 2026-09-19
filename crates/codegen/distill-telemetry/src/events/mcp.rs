//! MCP server and tool product telemetry events.

pub use crate::enums::McpInitStrategy as McpStrategy;
use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

#[derive(Serialize, Clone, Copy, strum::AsRefStr, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum McpErrorType {
    Connection,
    Auth,
    Protocol,
    Timeout,
    SpawnFailed,
    HandshakeFailed,
}

#[derive(Serialize)]
pub struct McpServerConnected {
    pub server_name: String,
    pub tool_count: u32,
    pub transport: McpTransport,
    pub duration_ms: u64,
}

#[derive(Serialize)]
pub struct McpServerFailed {
    pub server_name: String,
    pub error_type: McpErrorType,
    pub duration_ms: u64,
    pub timeout_sec: u64,
    /// Failure text for the external `error_message` attr (CONTENT gate).
    /// `#[serde(skip)]`.
    #[serde(skip)]
    pub error_message: Option<String>,
}

#[derive(Serialize)]
pub struct McpInitCompleted {
    pub total_duration_ms: u64,
    pub spawn_duration_ms: u64,
    pub server_count: u32,
    pub servers_succeeded: u32,
    pub servers_failed: u32,
    pub servers_auth_required: u32,
    pub total_tools_registered: u32,
    pub strategy: McpStrategy,
    pub is_reinit: bool,
}

#[derive(Serialize)]
pub struct McpToolCalled {
    pub server_name: String,
    pub tool_name: String,
    pub qualified_name: String,
    pub success: bool,
    pub duration_ms: u64,
    /// Set when the server never answered with a result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<McpCallFailure>,
    /// The server's own outcome code from `structuredContent.outcome`, when it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The server's `structuredContent.mode` label (computer use: `remote` or `companion`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// How a `tools/call` failed before the server answered with a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallFailure {
    Timeout,
    Transport,
}
