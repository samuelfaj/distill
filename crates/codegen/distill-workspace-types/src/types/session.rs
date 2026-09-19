//! Session-related shapes referenced from `SessionChunk` and `WorkspaceEvent`.
//!
//! TODO: align with the canonical session types when the
//! session subsystem moves into the workspace crate. The fields below
//! are minimal placeholders sufficient for the wire surface to compile.
//!
//! Notably absent: `SessionEndReason`, `PromptMode`, `StopReason`, `CancelReason`, and `SubagentStatus`.
//! Those existed only to support the `SessionEvent` enum, which has been removed entirely.
//! Session lifecycle, prompt boundaries, tool-call lifecycle, plan mode, and subagent state are all caused by the sampler.
//! They flow back to the sampler via direct call returns or stream chunks and never belonged on the EventBus.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::SessionId;
use crate::types::config::IsolationMode;

/// Snapshot of a session emitted by `SessionChunk::SessionInfo` (one per session in the response stream of `SessionLifecycleRequest::List`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub id: SessionId,
    /// Optional parent session id (set on subagents).
    #[serde(default)]
    pub parent: Option<SessionId>,
    /// Agent identifier (e.g. `"main"`, `"subagent-explore"`).
    #[serde(default)]
    pub agent_id: String,
    /// Filesystem isolation mode.
    #[serde(default)]
    pub isolation: IsolationMode,
    /// Wall-clock creation time.
    /// Default is Unix epoch, not `Utc::now()`, so a missing field does not impersonate the receiver's clock.
    #[serde(default)]
    pub created_at: DateTime<Utc>,
}

/// Result of a `SessionLifecycleRequest::Rewind`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindResult {
    /// Session id rewound.
    pub session: SessionId,
    /// Prompt index that is now the head of the conversation.
    #[serde(default)]
    pub head_prompt_index: u64,
    /// Number of prompts dropped by the rewind.
    #[serde(default)]
    pub prompts_dropped: u64,
}

/// One rewind point returned in `SessionChunk::RewindPoints`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPoint {
    /// Prompt index (monotonically increasing per session).
    pub prompt_index: u64,
    /// Wall-clock time the prompt was started.
    /// Default is Unix epoch, not `Utc::now()`, so a missing field does not impersonate the receiver's clock.
    #[serde(default)]
    pub at: DateTime<Utc>,
    /// Optional summary of the prompt that occurred at this index.
    #[serde(default)]
    pub summary: String,
}

/// Filesystem event kind reported by `WorkspaceEvent::FsChanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEventKind {
    /// File or directory created.
    Created,
    /// File modified (content or metadata).
    #[default]
    Modified,
    /// File or directory removed.
    Removed,
    /// File renamed (path is the new path).
    Renamed,
}

/// Generic server-status enum used for both MCP and LSP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    /// Server is starting.
    Starting,
    /// Server is running normally.
    #[default]
    Running,
    /// Server stopped (clean shutdown).
    Stopped,
    /// Server failed (returns to caller via the event payload).
    Failed,
}

/// MCP server status reported by `WorkspaceEvent::McpServerStateChanged`.
/// TODO: alias of [`ServerStatus`] while MCP and LSP share a shape; the type system will not stop a cross-use.
pub type McpServerStatus = ServerStatus;

/// LSP server status reported by `WorkspaceEvent::LspServerStateChanged`.
/// TODO: see `McpServerStatus` for the alias-vs-distinct-enum trade-off.
pub type LspServerStatus = ServerStatus;
