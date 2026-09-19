//! Per-session event log (`events.jsonl`).
//!
//! Each entry is a typed [`Event`].
//! The shared [`EventWriter`] appends entries to the file as JSON lines.
//! The per-session [`EventTracker`] keeps the state for the running turn and derives the events from it.

#![deny(clippy::indexing_slicing)]

pub mod log;
pub mod tracker;
pub mod types;

pub use log::EventWriter;
pub use tracker::EventTracker;
pub use types::{
    CancellationCategory, EVENT_SCHEMA_VERSION, Event, McpConfigServer, McpErrorCategory,
    PermissionDecision, Phase, SessionRelationship, ToolCompletedSource, ToolOutcome,
    TurnOutcomeLabel,
};
