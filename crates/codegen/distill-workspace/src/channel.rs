//! Shared transport types kept for backward compatibility with code that still references them.
//! Sessions use `WorkspaceHandle` directly (local mode) or `ToolHarness` RPC calls (proxy mode).

use serde_json::Value;

/// Context passed alongside transport calls (session routing, tracing, etc.).
#[derive(Debug, Clone, Default)]
pub struct TransportContext {
    pub session_id: Option<String>,
}

/// Transport-level error (distinct from [`WorkspaceError`]).
pub type TransportError = anyhow::Error;
pub type TransportCallResult = Value;
pub type TransportNotification = Value;
