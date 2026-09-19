// Modified for Distill by Samuel Fajreldines, 2026.
//! Tool-server and harness SDK.
//!
//! Single crate hosting both the tool-server runtime and the
//! harness-side dispatch surface. The shared substrate —
//! [`HubConnectionPool`], [`HubConnection`], the inbound demux, the
//! refcount-managed bound-session set, and the transparent reconnect /
//! replay state machine — lives here so both ends speak through one
//! frame multiplex on top of one WebSocket per `(url, principal)`.
//!
//! The server entry point is [`ToolServer`]: build it via
//! [`ToolServerBuilder`], wire one or more [`ToolServerHandler`]
//! implementations, and call [`ToolServer::run`] to drive the inbound
//! loop. The harness entry point is [`ToolHarness`]: build it via
//! [`ToolHarnessBuilder`], optionally seed it with in-process
//! [`distill_tool_runtime::Tool`] implementations, and call
//! [`ToolHarness::call`] to dispatch a tool call. Authorisation
//! credentials (`AuthCredential`) plus the target URL determine
//! which pool entry the consumer attaches to; multiple
//! [`ToolServer`] / [`ToolHarness`] instances against the same
//! `(url, principal)` share a single connection and refcount their
//! session bindings.

#![forbid(unsafe_code)]

pub(crate) mod admission;
pub mod auth;
pub(crate) mod cancel;
pub mod connection;
pub(crate) mod connection_borrow;
pub mod demux;
pub mod discovery;
pub(crate) mod donate_pump;
pub mod error;
pub mod handshake;
pub mod harness;
pub mod log_donate;
#[cfg(feature = "metrics")]
pub mod metric_donate;
pub mod metrics;
pub mod notification;
pub mod observability;
pub mod pool;
pub mod refcount;
pub mod server;
pub mod trace_donate;

pub mod oidc_provider;

pub use auth::{AuthCredential, AuthIdentity, AuthProvider, PrincipalKey, SharedAuthProvider};
pub use connection::{
    CLOSE_CODE_SANDBOX_TERMINATED, ConnKey, HubConnection, InitialConnectPolicy, ReconnectEvent,
};
pub use error::{ClientError, MAX_REFUSAL_CODE_LEN, RefusalCode};
pub use harness::{
    CancelOnDrop, LocalRegistry, ModelOutputExtractor, SessionBindReport, ToolHarness,
    ToolHarnessBuilder, extractor_for,
};
pub use log_donate::{DonatingLogLayer, LogDonationPump, LogDonationSender, flush_log_layer};
#[cfg(feature = "metrics")]
pub use metric_donate::MetricDonationPump;
pub use notification::HubNotification;
pub use observability::ObservabilityBridge;
pub use oidc_provider::{
    OidcAuthProvider, OidcAuthProviderBuilder, OnRefreshCallback, RefreshEvent,
};
pub use pool::HubConnectionPool;
pub use server::{
    ResolvedSessionHandlers, SessionHandlerResolver, SessionUnboundCallback, SystemNotifyAck,
    ToolServer, ToolServerBuilder, ToolServerHandler, WeakToolServer,
};
pub use trace_donate::{HubDonatingReporter, TraceDonationPump};
pub use distill_computer_hub_core::{
    GROK_BOT_DEFAULT_TOOL_IDS, GROK_BOT_TOOL_DESCRIPTIONS, GROK_BOT_TOOL_IDS,
    grok_bot_tool_arguments_schema, grok_bot_tool_description, is_grok_bot_default_tool,
    is_grok_bot_tool,
};
// Re-exported so consumers that depend only on the SDK can recognize the
// server's `workspace_unavailable` error without also pulling in the core crate.
pub use distill_computer_hub_core::is_workspace_unavailable;
