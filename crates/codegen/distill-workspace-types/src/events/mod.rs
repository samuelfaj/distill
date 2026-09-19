//! Pub/sub event types and topic-filter sets.
//!
//! Only the wire types live here; the broadcast channel and `EventStream` wrapper are runtime concerns.
//!
//! There is **no** `SessionEvent` enum; the EventBus only carries [`WorkspaceEvent`].
//! Session-scoped state is sampler-caused and comes back via the originating call's return value or stream chunks.
//! That covers prompt boundaries, tool-call lifecycle, plan-mode transitions, subagent lifecycle, compaction, and memory flushes.
//! The sampler owns that state and forwards to its UI channel as needed.

pub mod lag;
pub mod workspace;

pub use lag::EventLag;
pub use workspace::{WorkspaceEvent, WorkspaceTopic, WorkspaceTopicSet};
