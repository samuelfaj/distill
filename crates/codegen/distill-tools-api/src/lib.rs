// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared API definitions for Grok tools: protobuf types, config validation,
//! and canonical slash-command wording.
//!
//! Used by both the tools library and the gRPC server, and by host services
//! that must not depend on the tools implementation crate.

#![allow(clippy::derive_partial_eq_without_eq)]
#![deny(clippy::indexing_slicing)]

/// Generated protobuf types.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/xai.grok.tools.v1.rs"));
}

pub mod config_validation;
pub mod slash_commands;

// Re-export commonly used types at the crate root for convenience
pub use pb::{
    // Agent types
    AgentCompletionRequirement,
    AgentToolExecConfig,
    AgentToolRetryConfig,
    // Request/response types
    CallbackStatus,
    ClearToolOverrideRequest,
    ClearToolOverrideResponse,
    DisableToolRequest,
    DisableToolResponse,
    EnableToolRequest,
    EnableToolResponse,
    // Enums
    ErrorCode,
    ExecuteToolRequest,
    ExecuteToolResponse,
    ExecutionMetadata,
    ExecutionOptions,
    FinalizeAgentRequest,
    FinalizeAgentResponse,
    FinalizeConfigValidationDetails,
    FinalizeConfigViolation,
    // Tool server config (finalize-time)
    FinalizeToolServerConfigRequest,
    FinalizeToolServerConfigResponse,
    GetAgentInfoRequest,
    GetAgentInfoResponse,
    GetCompletionStateRequest,
    GetCompletionStateResponse,
    GetSystemPromptRequest,
    GetSystemPromptResponse,
    GetSystemRemindersRequest,
    GetSystemRemindersResponse,
    GetToolInfoRequest,
    GetToolOptionsRequest,
    GetToolOptionsResponse,
    // Tool state
    GetToolStateRequest,
    GetToolStateResponse,
    // Truncation config
    GetTruncationConfigRequest,
    GetTruncationConfigResponse,
    ListToolsRequest,
    ListToolsResponse,
    // Output format specs
    OutputFieldSpec,
    OutputFormat,
    OutputFormatSpec,
    ResetCompletionStateRequest,
    ResetCompletionStateResponse,
    ResetToolOptionsRequest,
    ResetToolOptionsResponse,
    SetSystemRemindersRequest,
    SetSystemRemindersResponse,
    SetToolOptionsRequest,
    SetToolOptionsResponse,
    SetToolOverrideRequest,
    SetToolOverrideResponse,
    SetTruncationConfigRequest,
    SetTruncationConfigResponse,
    SpawnSubagentRequest,
    // Streaming types
    StreamDataChunk,
    StreamDataKind,
    StreamFinalResult,
    SubagentResultMsg,
    // Capability/metadata types
    ToolCapabilities,
    ToolCategory,
    // Per-tool config entry
    ToolConfigEntry,
    ToolError,
    ToolInfo,
    ToolNotificationMsg,
    ToolSource,
    ToolStreamChunk,
    ToolSuccess,
    TruncationConfig,
    // Version lifecycle warnings
    VersionWarning,
};

/// Default client-facing tool name derived from a namespaced tool id. Tool ids are colon-separated `Namespace:tool`
/// (e.g. `Distill:grep`); the default name is the segment after the FIRST colon, so an id with embedded colons
/// (`ns:a:b`) resolves to `a`. Ids without a colon are returned as-is.
pub fn default_client_name(id: &str) -> &str {
    id.split(':').nth(1).unwrap_or(id)
}

/// Convert ToolCategory enum to a string representation. Prost generates this enum, so a strum
/// derive cannot be attached here.
/// xai-codegen-lint: allow(manual_strum)
impl ToolCategory {
    /// Get the string representation of the category.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::File => "file",
            Self::Search => "search",
            Self::Shell => "shell",
            Self::Workflow => "workflow",
            Self::External => "external",
            Self::Custom => "custom",
        }
    }
}

#[cfg(test)]
mod default_client_name_tests {
    use super::default_client_name;

    #[test]
    fn pins_first_colon_derivation() {
        assert_eq!(default_client_name("Distill:grep"), "grep");
        assert_eq!(default_client_name("ns:a:b"), "a");
        assert_eq!(default_client_name("bare"), "bare");
        assert_eq!(default_client_name(""), "");
    }
}
