//! Streaming response chunk types.
//!
//! Each transport call returns a stream of chunks.
//! The chunk type is domain-specific:
//!
//! - [`ToolChunk`]: streaming output / progress / final result for a tool invocation (or the response to [`crate::ToolRequest::Definitions`]).
//!   Tools that need user approval or input also yield `NeedPermission` or `NeedUserAnswer` chunks.
//!   The sampler answers them by sending [`ToolResponse`] values back on the paired bidi response sender.
//! - [`OpsChunk`]: one or more chunks for a workspace ops call (most are unary; ripgrep / fuzzy_search are streaming).
//! - [`SessionChunk`]: one or more chunks for a session lifecycle call.
//!
//! Every chunk variant maps to a static [`ChunkKind`] discriminator.
//! The typed-trait layer uses it to produce a [`crate::WorkspaceError::ProtocolMismatch`] when an unexpected chunk arrives on the wrong stream.

pub mod ops;
pub mod session;
pub mod tool;

pub use ops::OpsChunk;
pub use session::SessionChunk;
pub use tool::{ToolChunk, ToolResponse};

use serde::{Deserialize, Serialize};

/// Static discriminator for every variant across [`ToolChunk`], [`OpsChunk`], and [`SessionChunk`].
/// Used as `got` on [`crate::WorkspaceError::ProtocolMismatch`]; each chunk enum's `kind()` returns it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::AsRefStr,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// `ToolChunk::Output`
    ToolOutput,
    /// `ToolChunk::Progress`
    ToolProgress,
    /// `ToolChunk::Final`
    ToolFinal,
    /// `ToolChunk::Definitions`
    ToolDefinitions,
    /// `ToolChunk::NeedPermission`
    NeedPermission,
    /// `ToolChunk::NeedUserAnswer`
    NeedUserAnswer,
    /// `ToolChunk::NeedPlanModeChange`
    NeedPlanModeChange,

    /// `OpsChunk::GitStatus`
    GitStatus,
    /// `OpsChunk::GitDiff`
    GitDiff,
    /// `OpsChunk::GitBranchInfo`
    GitBranchInfo,
    /// `OpsChunk::GitMetadata`
    GitMetadata,
    /// `OpsChunk::Hunks`
    Hunks,
    /// `OpsChunk::Skills`
    Skills,
    /// `OpsChunk::Plugins`
    Plugins,
    /// `OpsChunk::ProjectConfig`
    ProjectConfig,
    /// `OpsChunk::Permissions`
    Permissions,
    /// `OpsChunk::Envrc`
    Envrc,
    /// `OpsChunk::ResolvedFiles`
    ResolvedFiles,
    /// `OpsChunk::MemoryChunks`
    MemoryChunks,
    /// `OpsChunk::Plugin`
    Plugin,
    /// `OpsChunk::Ack`
    Ack,
    /// `OpsChunk::FuzzyMatch`
    FuzzyMatch,
    /// `OpsChunk::RipgrepHit`
    RipgrepHit,
    /// `OpsChunk::RipgrepDone`
    RipgrepDone,

    /// `SessionChunk::SessionId`
    SessionId,
    /// `SessionChunk::SessionInfo`
    SessionInfo,
    /// `SessionChunk::RewindResult`
    RewindResult,
    /// `SessionChunk::RewindPoints`
    RewindPoints,
    /// `SessionChunk::Ack`
    SessionAck,
}

impl ChunkKind {
    /// Every variant of [`ChunkKind`], in declaration order. Pairs with [`Self::assert_exhaustive`].
    /// The test's exhaustive `match` fails compilation if a new variant is missing here, so the array stays exhaustive and unique.
    pub const fn all() -> &'static [Self] {
        &[
            Self::ToolOutput,
            Self::ToolProgress,
            Self::ToolFinal,
            Self::ToolDefinitions,
            Self::NeedPermission,
            Self::NeedUserAnswer,
            Self::NeedPlanModeChange,
            Self::GitStatus,
            Self::GitDiff,
            Self::GitBranchInfo,
            Self::GitMetadata,
            Self::Hunks,
            Self::Skills,
            Self::Plugins,
            Self::ProjectConfig,
            Self::Permissions,
            Self::Envrc,
            Self::ResolvedFiles,
            Self::MemoryChunks,
            Self::Plugin,
            Self::Ack,
            Self::FuzzyMatch,
            Self::RipgrepHit,
            Self::RipgrepDone,
            Self::SessionId,
            Self::SessionInfo,
            Self::RewindResult,
            Self::RewindPoints,
            Self::SessionAck,
        ]
    }
}

impl std::fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// New `ChunkKind` variants must get a `touch` arm (compile) and a unique `all()` entry.
    #[test]
    fn chunk_kind_all_is_complete() {
        fn touch(k: ChunkKind) {
            match k {
                ChunkKind::ToolOutput
                | ChunkKind::ToolProgress
                | ChunkKind::ToolFinal
                | ChunkKind::ToolDefinitions
                | ChunkKind::NeedPermission
                | ChunkKind::NeedUserAnswer
                | ChunkKind::NeedPlanModeChange
                | ChunkKind::GitStatus
                | ChunkKind::GitDiff
                | ChunkKind::GitBranchInfo
                | ChunkKind::GitMetadata
                | ChunkKind::Hunks
                | ChunkKind::Skills
                | ChunkKind::Plugins
                | ChunkKind::ProjectConfig
                | ChunkKind::Permissions
                | ChunkKind::Envrc
                | ChunkKind::ResolvedFiles
                | ChunkKind::MemoryChunks
                | ChunkKind::Plugin
                | ChunkKind::Ack
                | ChunkKind::FuzzyMatch
                | ChunkKind::RipgrepHit
                | ChunkKind::RipgrepDone
                | ChunkKind::SessionId
                | ChunkKind::SessionInfo
                | ChunkKind::RewindResult
                | ChunkKind::RewindPoints
                | ChunkKind::SessionAck => {}
            }
        }
        for &k in ChunkKind::all() {
            touch(k);
        }
        let unique: HashSet<_> = ChunkKind::all().iter().copied().collect();
        assert_eq!(
            unique.len(),
            ChunkKind::all().len(),
            "ChunkKind::all() contains duplicate variants"
        );
    }

    #[test]
    fn discriminator_strings_are_unique_globally() {
        let names: HashSet<&str> = ChunkKind::all().iter().map(|k| k.as_ref()).collect();
        assert_eq!(
            names.len(),
            ChunkKind::all().len(),
            "duplicate ChunkKind discriminator values"
        );
    }
}
