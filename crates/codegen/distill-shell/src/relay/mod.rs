//! Syncs TUI sessions to the relay backend over WebSocket for cross-machine session persistence and real-time sharing.
//!
//! # Architecture
//!
//! - Local disk remains the source of truth
//! - [`RelaySync`] streams updates to the relay in real-time
//! - Reconnection is handled by `run_relay_loop` in the agent relay module
//! - Connection state is observable via [`RelaySync::connection_state`] (Disconnected, then Connecting, then Connected)
//! - A disk-based sync cursor (`relay_sync.json`) tracks the last synced event so sync can resume after going offline
pub mod sync;
pub mod types;

pub use sync::{ConnectionState, RelaySync, RelaySyncState, StatusCallback, SyncStatus};
pub use types::AgentType;
