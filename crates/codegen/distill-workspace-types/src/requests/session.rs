use serde::{Deserialize, Serialize};

use crate::identity::SessionId;
use crate::types::AgentSessionConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionLifecycleRequest {
    /// Fork a new session. Response: `SessionChunk::SessionId`.
    Fork(AgentSessionConfig),
    /// Destroy a session. Response: `SessionChunk::Ack`.
    Destroy(SessionId),
    /// List all sessions. Streams `SessionChunk::SessionInfo` (one per session).
    List,
    /// Apply a (sub)session's worktree back into the parent.
    /// Response: `SessionChunk::Ack`.
    ApplyWorktree(SessionId),
    /// Mark the start of a prompt. Response: `SessionChunk::Ack`.
    BeginPrompt {
        session: SessionId,
        /// Monotonically increasing prompt index.
        ///
        /// `u64` (not `usize`) for wire stability: `usize` is host-dependent and would arbitrarily codegen to `uint64`.
        idx: u64,
    },
    /// Mark the end of a prompt. Response: `SessionChunk::Ack`.
    EndPrompt {
        session: SessionId,
        /// Prompt index that just finished.
        idx: u64,
    },
    /// Rewind a session to a target prompt index. Response: `SessionChunk::RewindResult`.
    Rewind {
        session: SessionId,
        /// Target prompt index (0 means the beginning).
        target: u64,
    },
    /// Enumerate the available rewind points for a session. Response: `SessionChunk::RewindPoints`.
    GetRewindPoints(SessionId),
}
