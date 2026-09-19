// Modified for Distill by Samuel Fajreldines, 2026.
//! Wire types for the `distill-workspace` API.
//!
//! This crate is intentionally pure-data and depends on nothing more than `base64`, `serde`, `serde_json`, `thiserror`, and `chrono`.
//! There is no tokio, no async-trait, no I/O.
//! This makes it cheap to depend on from anywhere, including the eventual WASM browser SDK.
//!
//! # Module overview
//!
//! - [`identity`]: session/tool/hunk identifiers.
//! - [`metadata`]: typed-string metadata map plus standard metadata key constants (`META_*`).
//! - [`request`]: the `RequestMessage<T>` envelope shared by all RPC requests.
//! - [`error`]: `WorkspaceError` and its serializable `IoKind`.
//! - [`requests`]: the request enums (`WorkspaceRequest`, `ToolRequest`, `WorkspaceOpsRequest`, `SessionLifecycleRequest`).
//! - [`chunks`]: the streaming response chunks (`ToolChunk`, `OpsChunk`, `SessionChunk`) plus the `ChunkKind` discriminator.
//! - [`events`]: pub/sub events ([`WorkspaceEvent`]) plus the [`EventLag`] backpressure signal.
//!   The EventBus only carries external state the workspace observed; there is no `SessionEvent` enum.
//! - [`rpc`]: canonical wire types for the hub-proxied `workspace.*` RPC dispatch (trait, envelope, hub tool ids, per-method types).
//! - [`types`]: supporting structs/enums referenced from requests, chunks, and events.
//!   Many of these are minimal placeholders; the final shapes will land when the corresponding subsystems are extracted into the new workspace crate.
//!
//! # Wire format
//!
//! Every enum that appears on the wire is **adjacently tagged** with `#[serde(tag = "type", content = "data")]`.
//! The JSON wire shape is:
//!
//! ```json
//! {"type": "<variant_name>", "data": <payload>}
//! ```
//!
//! Adjacent (rather than internal) tagging is required: internal tagging fails for newtype variants wrapping non-struct payloads.
//! E.g. `OpsChunk::GitMetadata(Option<...>)`, `SessionChunk::SessionId(SessionId)` (a string), `WorkspaceOpsRequest::ResolveFileRefs(Vec<String>)`.
//! Adjacent tagging works uniformly across all payload shapes (struct, newtype, unit) and gives the JSON wire format an explicit discriminator.
//! Each variant maps directly to a protobuf `oneof` regardless of the serde tagging form; the choice here only affects the JSON representation.
//!
//! Wire-format struct fields use **snake_case** to match gRPC field conventions (proto field names are snake_case).
//!
//! # Wire integer types
//!
//! Every `usize` field is replaced here with `u64`, or `u32` for known-bounded sizes like `MemorySearch.limit`.
//! That covers `BeginPrompt.idx`, `EndPrompt.idx`, `Rewind.target`, `MemorySearch.limit`, and `CodebaseIndexUpdated.files_indexed`.
//!
//! Rationale: `usize` is host-dependent (32 vs 64 bit) and serializes inconsistently across producers.
//! A 32-bit publisher could silently truncate a value that a 64-bit subscriber reconstructs as something different.
//! `u64` codegens cleanly to protobuf `uint64` and pins the wire width regardless of host.
//!
//! # TODO: proto generation
//!
//! A planned `build.rs` will walk the request, chunk, and event enums via reflection and emit a `.proto`.
//! That codegen step is **not** implemented yet; it will land alongside the `distill-workspace-grpc` crate.
//! The Rust types defined here are the source of truth.

#![deny(clippy::indexing_slicing)]

pub mod binding;
pub mod chunks;
pub mod error;
pub mod events;
pub mod identity;
pub mod metadata;
pub mod request;
pub mod requests;
pub mod rpc;
pub mod types;

/// MCP tool name delimiter: server names are qualified as `"server__tool"`.
/// Lives here so the permission-validation and MCP transport layers can share it without dragging the full workspace or rmcp into each other.
/// Re-exported by `distill_workspace::permission` for callers that historically imported it from there.
pub const MCP_TOOL_NAME_DELIMITER: &str = "__";

pub use crate::chunks::{ChunkKind, OpsChunk, SessionChunk, ToolChunk, ToolResponse};
pub use crate::error::{IoKind, WorkspaceError};
pub use crate::events::{EventLag, WorkspaceEvent, WorkspaceTopic, WorkspaceTopicSet};
pub use crate::identity::{HunkId, SessionId, ToolCallId};
pub use crate::metadata::{
    META_CLIENT_ID, META_GRPC_TIMEOUT, META_PROMPT_INDEX, META_SESSION_ID, META_TRACEPARENT,
    META_TRACESTATE, Metadata, STANDARD_META_KEYS,
};
pub use crate::request::RequestMessage;
pub use crate::requests::{
    SessionLifecycleRequest, ToolCallArgs, ToolRequest, WorkspaceOpsRequest, WorkspaceRequest,
};
pub use crate::types::{
    AgentSessionConfig, AgentSessionInfo, CapabilityMode, ContentMatch, FileReference, FsEventKind,
    FuzzyMatch, FuzzySearchArgs, GitBranchInfo, GitDiff, GitDiffArgs, GitMetadata, GitStatus,
    GitStatusOpts, HookInfo, Hunk, HunkAction, IsolationMode, LspServerStatus, McpServerStatus,
    MemoryChunk, PermissionDecision, PermissionPolicy, PermissionRequest, PlanModeDecision,
    PlanModeTransition, PluginInfo, ProjectConfig, ResolvedFile, RewindPoint, RewindResult,
    RipgrepArgs, RipgrepStats, SkillInfo, ToolCallResult, ToolDef, ToolOutputChunk, ToolProgress,
    ToolServerConfig, UserAnswer, UserQuestion, UserQuestionOption, VcsKind,
};
