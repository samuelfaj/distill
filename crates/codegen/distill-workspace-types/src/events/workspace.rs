//! Workspace-scoped events (FS changes, server lifecycle, discovery, ...).
//!
//! `WorkspaceEvent` carries only **workspace-observed external state**.
//! Examples: filesystem watcher fires, background subprocess (LSP / MCP) lifecycle, background indexing progress, config changes detected on disk.
//!
//! Sampler-caused state never goes here.
//! Hunk events (`HunkRecorded`, `HunkAccepted`, `HunkRejected`) are deliberately absent.
//! Hunks come from tool writes and `act_on_hunk` RPCs, both sampler-caused, so the sampler already has that state from the originating call.
//! It can re-snapshot via `list_hunks()` if it needs to reconcile.
//!
//! `ToolsChanged` is included on this enum because the tool registry is a workspace state observation.
//! An MCP server snapshot change (workspace-observed) and a sampler-initiated `update_tool_config` produce the same downstream effect.
//! Either way, other subscribers and the sampler's UI need to re-fetch `tool_definitions`.
//! Tool execution itself is **not** broadcast here; it stays on the sampler-caused tool-result path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::types::{
    FsEventKind, HookInfo, LspServerStatus, McpServerStatus, PluginInfo, SkillInfo, VcsKind,
};

/// One event per fact, broadcast to every subscriber of the workspace channel.
/// Topic filtering happens client-side via [`WorkspaceTopicSet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    /// Filesystem watcher fired: every path that changed the same way within one settle window,
    /// coalesced so a checkout is one frame per session rather than one per file (the producer
    /// splits a window past ~1024 paths into several frames). A `Renamed` batch is `[from, to]`
    /// when the watcher saw both halves, or the one path it saw; a rename across the root boundary
    /// arrives as the half inside it, `Removed` (left the root) or `Created` (arrived).
    FsChanged {
        /// Affected paths, relative to the workspace root. Paths outside it are never reported.
        paths: Vec<PathBuf>,
        kind: FsEventKind,
    },
    /// Git HEAD moved.
    GitHeadChanged {
        /// New commit sha.
        commit: String,
        /// New branch name (None for detached HEAD).
        branch: Option<String>,
        vcs: VcsKind,
    },
    /// Git lock is held by an external process; reads may block until `until`.
    GitLockHeld {
        /// Best-effort wall-clock estimate of when the lock will be released.
        /// Not an `Instant`: `std::time::Instant` is process-local and not `Serialize`, so it is meaningless to a receiver in another process.
        until: DateTime<Utc>,
    },
    /// Skill discovery found changes.
    SkillsChanged {
        /// Newly added skills.
        added: Vec<SkillInfo>,
        /// Ids of removed skills.
        removed: Vec<String>,
    },
    /// Plugin discovery found changes.
    PluginsChanged {
        /// Current plugin set.
        plugins: Vec<PluginInfo>,
        /// Whether the project is trusted (gates plugin execution).
        project_trusted: bool,
    },
    /// Hook discovery found changes.
    HooksChanged {
        /// Current hook set.
        hooks: Vec<HookInfo>,
        /// Whether the project is trusted (gates hook execution).
        project_trusted: bool,
    },
    /// MCP server transitioned state.
    McpServerStateChanged {
        /// Server identifier.
        server: String,
        /// New status.
        status: McpServerStatus,
    },
    /// LSP server transitioned state.
    LspServerStateChanged {
        /// Server identifier.
        server: String,
        /// New status.
        status: LspServerStatus,
    },
    /// Codebase index ingested more files.
    CodebaseIndexUpdated {
        /// Total files indexed so far.
        files_indexed: u64,
    },
    /// Project config changed on disk.
    ProjectConfigChanged,
    /// Permission policy changed on disk.
    PermissionPolicyChanged,
    /// A session's tool registry was rebuilt (tool-config swap or MCP snapshot re-resolve).
    /// Subscribers should re-fetch definitions for the affected session.
    ToolsChanged {
        /// Affected session id.
        session_id: String,
    },
}

/// Topic discriminator for workspace events, used by `EventBus::subscribe_filtered` to skip uninteresting events.
/// Filtering is delivery-only; it never changes the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTopic {
    /// Filesystem-watcher events.
    Fs,
    /// VCS background events (HEAD moves, external git lock held).
    /// Hunk events are not on the EventBus; see the module-level doc-comment.
    Vcs,
    /// Skill / plugin / hook discovery.
    Discovery,
    /// MCP / LSP server state.
    Servers,
    /// Codebase index progress.
    Index,
    /// Config / permission policy.
    Config,
    /// Session tool-registry rebuilds (capability filter, MCP merge, explicit `update_tool_config`).
    Tools,
}

impl WorkspaceEvent {
    /// Topic this event belongs to (used for filtering).
    pub fn topic(&self) -> WorkspaceTopic {
        match self {
            Self::FsChanged { .. } => WorkspaceTopic::Fs,
            Self::GitHeadChanged { .. } | Self::GitLockHeld { .. } => WorkspaceTopic::Vcs,
            Self::SkillsChanged { .. }
            | Self::PluginsChanged { .. }
            | Self::HooksChanged { .. } => WorkspaceTopic::Discovery,
            Self::McpServerStateChanged { .. } | Self::LspServerStateChanged { .. } => {
                WorkspaceTopic::Servers
            }
            Self::CodebaseIndexUpdated { .. } => WorkspaceTopic::Index,
            Self::ProjectConfigChanged | Self::PermissionPolicyChanged => WorkspaceTopic::Config,
            Self::ToolsChanged { .. } => WorkspaceTopic::Tools,
        }
    }
}

/// Implemented as a small bitmask to keep filter checks branch-free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceTopicSet {
    /// Bitmask: bit `n` is `WorkspaceTopic` discriminant `n`.
    bits: u32,
}

impl WorkspaceTopicSet {
    /// Empty set (matches no events).
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// Set containing every topic.
    pub fn all() -> Self {
        let mut s = Self::empty();
        for t in [
            WorkspaceTopic::Fs,
            WorkspaceTopic::Vcs,
            WorkspaceTopic::Discovery,
            WorkspaceTopic::Servers,
            WorkspaceTopic::Index,
            WorkspaceTopic::Config,
            WorkspaceTopic::Tools,
        ] {
            s = s.with(t);
        }
        s
    }

    /// Return a copy of this set with `topic` added.
    #[must_use]
    pub fn with(mut self, topic: WorkspaceTopic) -> Self {
        self.bits |= 1u32 << topic_index(topic);
        self
    }

    /// Whether `topic` is contained in the set.
    pub fn contains(&self, topic: WorkspaceTopic) -> bool {
        self.bits & (1u32 << topic_index(topic)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }
}

fn topic_index(topic: WorkspaceTopic) -> u32 {
    // Stable indices (do not reorder existing variants without bumping the wire-compat manifest)
    match topic {
        WorkspaceTopic::Fs => 0,
        WorkspaceTopic::Vcs => 1,
        WorkspaceTopic::Discovery => 2,
        WorkspaceTopic::Servers => 3,
        WorkspaceTopic::Index => 4,
        WorkspaceTopic::Config => 5,
        WorkspaceTopic::Tools => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_set_contains_what_was_added() {
        let s = WorkspaceTopicSet::empty()
            .with(WorkspaceTopic::Fs)
            .with(WorkspaceTopic::Vcs);
        assert!(s.contains(WorkspaceTopic::Fs));
        assert!(s.contains(WorkspaceTopic::Vcs));
        assert!(!s.contains(WorkspaceTopic::Discovery));
        assert!(!s.is_empty());
    }

    #[test]
    fn all_contains_every_topic() {
        let s = WorkspaceTopicSet::all();
        for t in [
            WorkspaceTopic::Fs,
            WorkspaceTopic::Vcs,
            WorkspaceTopic::Discovery,
            WorkspaceTopic::Servers,
            WorkspaceTopic::Index,
            WorkspaceTopic::Config,
            WorkspaceTopic::Tools,
        ] {
            assert!(s.contains(t));
        }
    }

    #[test]
    fn topic_classification_matches_variants() {
        assert_eq!(
            WorkspaceEvent::FsChanged {
                paths: vec![PathBuf::from("x")],
                kind: FsEventKind::Created,
            }
            .topic(),
            WorkspaceTopic::Fs
        );
        assert_eq!(
            WorkspaceEvent::ProjectConfigChanged.topic(),
            WorkspaceTopic::Config
        );
        assert_eq!(
            WorkspaceEvent::ToolsChanged {
                session_id: "s".into()
            }
            .topic(),
            WorkspaceTopic::Tools
        );
    }
}
