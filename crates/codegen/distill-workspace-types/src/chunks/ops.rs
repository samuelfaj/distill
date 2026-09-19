use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::chunks::ChunkKind;
use crate::types::{
    ContentMatch, FuzzyMatch, GitBranchInfo, GitDiff, GitMetadata, GitStatus, Hunk, MemoryChunk,
    PermissionPolicy, PluginInfo, ProjectConfig, ResolvedFile, RipgrepStats, SkillInfo,
};

/// Streaming chunk for an ops call. Most variants are unary; `FuzzyMatch` and `RipgrepHit` stream, and ripgrep ends with one `RipgrepDone`.
/// No `Eq`: [`MemoryChunks`](Self::MemoryChunks) carries an optional `f32` score. `PartialEq` is enough for round-trip tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum OpsChunk {
    /// Response to `WorkspaceOpsRequest::GitStatus`.
    GitStatus(GitStatus),
    /// Response to `WorkspaceOpsRequest::GitDiff`.
    GitDiff(GitDiff),
    /// Response to `WorkspaceOpsRequest::GitBranchInfo`.
    GitBranchInfo(GitBranchInfo),
    /// Response to `WorkspaceOpsRequest::GitMetadata`.
    /// None if the workspace is not a git repo.
    GitMetadata(Option<GitMetadata>),

    /// Response to `WorkspaceOpsRequest::ListHunks`.
    Hunks(Vec<Hunk>),
    /// Response to `WorkspaceOpsRequest::DiscoverSkills`.
    Skills(Vec<SkillInfo>),
    /// Response to `WorkspaceOpsRequest::DiscoverPlugins`.
    Plugins(Vec<PluginInfo>),
    /// Response to `WorkspaceOpsRequest::LoadProjectConfig`.
    ProjectConfig(ProjectConfig),
    /// Response to `WorkspaceOpsRequest::LoadPermissions`.
    Permissions(PermissionPolicy),
    /// Response to `WorkspaceOpsRequest::LoadEnvrc`.
    /// `BTreeMap` so JSON key order is deterministic; the on-wire shape is still a JSON object.
    Envrc(BTreeMap<String, String>),
    /// Response to `WorkspaceOpsRequest::ResolveFileRefs`.
    ResolvedFiles(Vec<ResolvedFile>),
    /// Response to `WorkspaceOpsRequest::MemorySearch`.
    MemoryChunks(Vec<MemoryChunk>),
    /// Response to `WorkspaceOpsRequest::InstallPlugin` (one plugin metadata snapshot for the freshly-installed plugin).
    Plugin(PluginInfo),

    /// Acknowledgement for void ops (`ActOnHunk`, `MemoryWrite`, `RefreshPlugins` accepted, ...).
    Ack,

    /// One match for `WorkspaceOpsRequest::FuzzySearch` (zero or more).
    FuzzyMatch(FuzzyMatch),
    /// One hit for `WorkspaceOpsRequest::Ripgrep` (zero or more before `RipgrepDone`).
    RipgrepHit(ContentMatch),
    /// Explicit terminator for ripgrep streams (since hits are repeatable, we need a positive end-of-stream marker).
    RipgrepDone(RipgrepStats),
}

impl OpsChunk {
    /// Discriminator for the current variant.
    pub fn kind(&self) -> ChunkKind {
        match self {
            Self::GitStatus(_) => ChunkKind::GitStatus,
            Self::GitDiff(_) => ChunkKind::GitDiff,
            Self::GitBranchInfo(_) => ChunkKind::GitBranchInfo,
            Self::GitMetadata(_) => ChunkKind::GitMetadata,
            Self::Hunks(_) => ChunkKind::Hunks,
            Self::Skills(_) => ChunkKind::Skills,
            Self::Plugins(_) => ChunkKind::Plugins,
            Self::ProjectConfig(_) => ChunkKind::ProjectConfig,
            Self::Permissions(_) => ChunkKind::Permissions,
            Self::Envrc(_) => ChunkKind::Envrc,
            Self::ResolvedFiles(_) => ChunkKind::ResolvedFiles,
            Self::MemoryChunks(_) => ChunkKind::MemoryChunks,
            Self::Plugin(_) => ChunkKind::Plugin,
            Self::Ack => ChunkKind::Ack,
            Self::FuzzyMatch(_) => ChunkKind::FuzzyMatch,
            Self::RipgrepHit(_) => ChunkKind::RipgrepHit,
            Self::RipgrepDone(_) => ChunkKind::RipgrepDone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn samples() -> Vec<OpsChunk> {
        vec![
            OpsChunk::GitStatus(GitStatus::default()),
            OpsChunk::GitDiff(GitDiff::default()),
            OpsChunk::GitBranchInfo(GitBranchInfo::default()),
            OpsChunk::GitMetadata(Some(GitMetadata::default())),
            OpsChunk::Hunks(vec![]),
            OpsChunk::Skills(vec![]),
            OpsChunk::Plugins(vec![]),
            OpsChunk::ProjectConfig(ProjectConfig::default()),
            OpsChunk::Permissions(PermissionPolicy::default()),
            OpsChunk::Envrc(BTreeMap::new()),
            OpsChunk::ResolvedFiles(vec![]),
            OpsChunk::MemoryChunks(vec![]),
            OpsChunk::Plugin(PluginInfo::default()),
            OpsChunk::Ack,
            OpsChunk::FuzzyMatch(FuzzyMatch::default()),
            OpsChunk::RipgrepHit(ContentMatch::default()),
            OpsChunk::RipgrepDone(RipgrepStats::default()),
        ]
    }

    #[test]
    fn kind_discriminators_are_unique() {
        let kinds: HashSet<ChunkKind> = samples().iter().map(OpsChunk::kind).collect();
        assert_eq!(kinds.len(), samples().len(), "duplicate OpsChunk kind()");
    }
}
