// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP extension methods and business logic.
//!
//! - `x.ai/mcp/list`: list available MCP servers (agent-scoped or session-annotated)
//! - `x.ai/mcp/call`: invoke an MCP tool directly, outside the LLM loop
//! - `x.ai/mcp/servers_updated`: the local and plugin catalog after launch-dir discovery or a folder-trust grant (not gateway connectors)
//! - `x.ai/mcp/server_status`: per-server delta pushed by the `StatusDispatcher`.
//!   The triggers: transport-closed pollers, handshake failures, config diffs, and server-pushed list-changed notifications.
//!   See [`crate::session::mcp_dispatcher`] for the coalescing and payload-shaping logic.
//!   Re-exported below so other crates have a single import point.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
// rmcp is quarantined in distill-mcp; see that crate's docs.
use distill_mcp::rmcp;
// `wire::MCP_CALL` is the one cross-SDK contract literal; the agent-only siblings live in `mcp_methods` below
use distill_mcp::wire;

use super::{ExtResult, parse_params, to_ext_response};

/// Agent-only `x.ai/mcp/*` ACP method/notification names. Unlike [`wire::MCP_CALL`] (the cross-SDK contract, which stays in `distill_mcp::wire`), these methods are NOT spoken by the SDK.
/// They are private to the channel between the agent and the client. They are centralized here only to avoid scattering the same string literal across dispatch and notification send sites.
pub mod mcp_methods {
    /// Shared prefix that routes every MCP ext method to this module's dispatcher.
    pub const PREFIX: &str = "x.ai/mcp/";

    pub const LIST: &str = "x.ai/mcp/list";
    pub const READ_RESOURCE: &str = "x.ai/mcp/read_resource";
    pub const AUTH_STATUS: &str = "x.ai/mcp/auth_status";
    pub const AUTH_TRIGGER: &str = "x.ai/mcp/auth_trigger";
    pub const SETUP: &str = "x.ai/mcp/setup";
    pub const TOGGLE: &str = "x.ai/mcp/toggle";
    pub const TOGGLE_TOOL: &str = "x.ai/mcp/toggle_tool";
    pub const UPSERT: &str = "x.ai/mcp/upsert";
    pub const DELETE: &str = "x.ai/mcp/delete";

    pub const SERVERS_UPDATED: &str = "x.ai/mcp/servers_updated";
    pub const TOOLS_CHANGED: &str = "x.ai/mcp/tools_changed";
    pub const INIT_PROGRESS: &str = "x.ai/mcp/init_progress";
}
use crate::agent::MvpAgent;
use crate::session::mcp_servers::{MCP_TOOL_NAME_DELIMITER, McpClient, McpState, SharedMcpState};

// ── Wire types: mcp/list ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    /// When false, bypass cache and refetch from cli-chat-proxy, then sync into live sessions so `search_tool` sees new tools.
    /// Use after OAuth enrollment or disconnect.
    #[serde(default = "default_true")]
    pub cache: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpListResponse {
    pub servers: Vec<McpServerEntry>,
    /// Session-scoped: true when `is_initialized` is set. A missing client is then a failed handshake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_mcp_resolved: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<distill_mcp::servers::McpIcon>,
    pub source: McpServerSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<crate::util::config::McpSetupConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_values: Option<HashMap<String, String>>,
    #[serde(flatten)]
    pub config: McpServerConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<McpServerSessionState>,
}

/// MCP server config for the `mcp/list` catalog response.
/// Distinct from `acp::McpServer` (session/new input) because: HTTP: exposes `scope`, `scope_id`, and `scope_name` for connector selection, NOT headers (auth tokens stay private)
/// Stdio: same structure but optimized for JSON wire format
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpServerConfig {
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(rename = "scopeId", skip_serializing_if = "Option::is_none")]
        scope_id: Option<String>,
        #[serde(rename = "scopeName", skip_serializing_if = "Option::is_none")]
        scope_name: Option<String>,
    },
    #[serde(rename = "stdio")]
    Stdio {
        command: std::path::PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<McpEnvVar>,
    },
    #[serde(rename = "managedGateway")]
    ManagedGateway,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerSource {
    Managed,
    Local,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSessionState {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<McpSessionStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup_required: bool,
    /// Managed-policy verdict for a server the merge dropped, so `/mcps` can say "blocked by policy"
    /// instead of a generic "unavailable"; old pagers ignore the extra field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpSessionStatus {
    Ready,
    Initializing,
    SetupRequired,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<distill_mcp::servers::McpIcon>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// ── Wire types: mcp/call ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpCallRequest {
    /// When present: session pool. When absent: agent pool (config.toml only).
    #[serde(default)]
    pub session_id: Option<String>,
    pub server: String,
    /// Endpoint URL; disambiguates when multiple servers share a name.
    #[serde(default)]
    pub server_url: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallResponse {
    pub content: Vec<McpContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

// ── Internal types (not serialized to wire) ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct McpStatusSnapshot {
    pub configs: Vec<acp::McpServer>,
    pub clients: Vec<McpClientStatus>,
    pub auth_required: std::collections::HashSet<String>,
    /// Mirrors `McpState::is_initialized`. A missing client then means handshake failed.
    pub is_resolved: bool,
}

#[derive(Debug, Clone)]
pub struct McpClientStatus {
    pub name: String,
    pub status: McpSessionStatus,
    pub tools: Vec<McpToolEntry>,
    pub icons: Vec<distill_mcp::servers::McpIcon>,
}

// ── Notification: mcp/servers_updated ────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersUpdated {
    pub mcp_servers: Vec<McpServerEntry>,
}

/// Per-server tool-list change push. Emitted by [`crate::session::acp_session::AcpSession`] on the post-handshake, auth-recovery, and toggle-tool paths.
/// The `session_id` field lets the pager route the push to the owning agent via `find_session_match`. Falling back to `app.active_view` was a latent multi-agent bug.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolsChanged {
    /// Session that owns this push.
    /// The pager routes via `find_session_match` so a background-agent push does not land on the foregrounded agent's modal.
    pub session_id: String,
    /// MCP server whose tool list changed. Currently unread by the pager. The pager treats every `tools_changed` push as a trigger to schedule a debounced `mcp/list` refetch and re-reads the full catalog.
    /// The toggle-tool path therefore leaves this empty for forward-compat. A future field-aware pager optimization would need to special-case empty as "not scoped to one server"; no consumer reads that today.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_name: String,
    /// New tool entries for the named server. Currently unread by the pager for the same reason as `server_name` above. Empty on the toggle-tool path.
    /// Populated on the post-handshake and auth-recovery paths so future field-aware consumers can avoid the `mcp/list` round trip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolEntry>,
}

// Re-export the `x.ai/mcp/server_status` schema and method constant from the dispatcher module External callers then have a single import point alongside the other `x.ai/mcp/*` types
// The canonical definitions stay in [`crate::session::mcp_dispatcher`]: their primary consumer is the dispatcher loop and its unit tests
// This import from `session` into `extensions` inverts the typical `extensions` to `session` flow Moving the types here would require either making the dispatcher import from `extensions` (the same inversion) or duplicating the schema Leaving the re-export here keeps the single import point without duplicating definitions
pub use crate::session::mcp_dispatcher::{
    McpServerStatus, McpServerStatusPayload, McpServerStatusReason, SERVER_STATUS_METHOD,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpReadResourceRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    pub server: String,
    pub uri: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceResponse {
    pub contents: Vec<McpReadResourceContent>,
}

/// A single resource content block from `resources/read`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// Push the full MCP catalog to the client.
/// Called in the background after launch-dir MCP discovery so `initialize()` isn't blocked by config walks.
pub async fn notify_servers_updated(
    gateway: &distill_acp_lib::AcpAgentGatewaySender,
    local_servers: &[acp::McpServer],
) {
    let catalog = build_mcp_catalog(local_servers);
    let payload = McpServersUpdated {
        mcp_servers: catalog,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&payload) {
        let notification = acp::ExtNotification::new(mcp_methods::SERVERS_UPDATED, params.into());
        let _ = gateway.ext_notification(notification).await;
        tracing::info!("Sent x.ai/mcp/servers_updated notification to client");
    }
}

// ── Dispatch ────────────────────────────────────────────────────────

/// Inbound `x.ai/mcp/*` methods this agent services, resolved from the wire string. Single source of truth for forward-method routing: [`handle`] maps each variant to its handler.
/// An unknown method yields `None`, which `handle` answers with `method_not_found`. The reverse method [`wire::MCP_SDK_CALL`] is emit-only (agent to client) and has no variant here.
/// A stray inbound reverse call is therefore never misrouted to the forward `handle_call`.
#[derive(Debug, PartialEq, Eq)]
enum McpRoute {
    List,
    Call,
    ReadResource,
    AuthStatus,
    AuthTrigger,
    Setup,
    Toggle,
    ToggleTool,
    Upsert,
    Delete,
}

fn route_mcp_method(method: &str) -> Option<McpRoute> {
    Some(match method {
        mcp_methods::LIST => McpRoute::List,
        wire::MCP_CALL => McpRoute::Call,
        mcp_methods::READ_RESOURCE => McpRoute::ReadResource,
        mcp_methods::AUTH_STATUS => McpRoute::AuthStatus,
        mcp_methods::AUTH_TRIGGER => McpRoute::AuthTrigger,
        mcp_methods::SETUP => McpRoute::Setup,
        mcp_methods::TOGGLE => McpRoute::Toggle,
        mcp_methods::TOGGLE_TOOL => McpRoute::ToggleTool,
        mcp_methods::UPSERT => McpRoute::Upsert,
        mcp_methods::DELETE => McpRoute::Delete,
        _ => return None,
    })
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match route_mcp_method(args.method.as_ref()) {
        Some(McpRoute::List) => handle_list(agent, args).await,
        Some(McpRoute::Call) => handle_call(agent, args).await,
        Some(McpRoute::ReadResource) => handle_read_resource(agent, args).await,
        Some(McpRoute::AuthStatus) => handle_auth_status(agent, args).await,
        Some(McpRoute::AuthTrigger) => handle_auth_trigger(agent, args).await,
        Some(McpRoute::Setup) => handle_setup(agent, args).await,
        Some(McpRoute::Toggle) => handle_toggle(agent, args).await,
        Some(McpRoute::ToggleTool) => handle_toggle_tool(agent, args).await,
        Some(McpRoute::Upsert) => handle_upsert(agent, args).await,
        Some(McpRoute::Delete) => handle_delete(agent, args).await,
        None => Err(acp::Error::method_not_found()),
    }
}

// ── Catalog (shared by mcp/list and InitializeResponse._meta) ───────

/// Extract URL from an MCP server (HTTP and SSE only, None for Stdio).
fn mcp_server_url(server: &acp::McpServer) -> Option<&str> {
    match server {
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => Some(url.as_str()),
        acp::McpServer::Stdio(acp::McpServerStdio { .. }) => None,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => None,
    }
}

/// Build the MCP server catalog: gateway rows and local servers, deduplicated by name.
/// Pure function, no I/O. Used by `mcp/list`, `InitializeResponse._meta`, and `mcp/servers_updated`.
pub fn build_mcp_catalog(local_servers: &[acp::McpServer]) -> Vec<McpServerEntry> {
    build_mcp_catalog_with_gateway_tools(local_servers, None, &Default::default())
}

pub(crate) fn build_mcp_catalog_with_gateway_tools(
    local_servers: &[acp::McpServer],
    gateway_catalog: Option<&crate::session::managed_mcp::GatewayToolCatalog>,
    disabled_tools: &HashMap<String, HashSet<String>>,
) -> Vec<McpServerEntry> {
    let mut servers: Vec<McpServerEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(catalog) = gateway_catalog {
        // Structured ids are authoritative: when any is present the display-name list is ignored, even for connectors the ids do not mention. Names collide (two accounts of one service) where ids do not, so mixing the two would flag working connectors. The name list only serves proxies that predate ids, and only for tool-backed rows: a synthesized row for a connector with no tools needs a stable id.
        let reauth_ids: HashSet<&str> = catalog
            .reauth_connectors
            .iter()
            .filter(|c| !c.connector_id.is_empty())
            .map(|c| c.connector_id.as_str())
            .collect();
        let reauth_names_fallback: &[String] = if reauth_ids.is_empty() {
            &catalog.connectors_needing_reauth
        } else {
            &[]
        };
        let mut by_connector: BTreeMap<&str, Vec<&crate::session::managed_mcp::GatewayTool>> =
            BTreeMap::new();
        for tool in &catalog.tools {
            by_connector
                .entry(tool.connector_id.as_str())
                .or_default()
                .push(tool);
        }

        let disabled_connectors =
            disabled_tools.get(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY);
        let mut emitted_ids = HashSet::<&str>::new();
        for (connector_id, tools) in by_connector {
            let connector_name = tools
                .first()
                .map(|tool| tool.connector_name.as_str())
                .unwrap_or(connector_id);
            let disabled = disabled_tools.get(connector_id);
            let server_disabled = disabled_connectors.is_some_and(|set| set.contains(connector_id));
            let auth_required = reauth_ids.contains(connector_id)
                || reauth_names_fallback.iter().any(|name| {
                    name.eq_ignore_ascii_case(connector_id)
                        || name.eq_ignore_ascii_case(connector_name)
                });
            emitted_ids.insert(connector_id);
            let entry = managed_gateway_server_entry(
                connector_id,
                connector_name,
                tools
                    .into_iter()
                    .map(|tool| {
                        let qualified_name = tool.qualified_name();
                        McpToolEntry {
                            name: qualified_name.clone(),
                            icons: Vec::new(),
                            display_name: Some(tool.tool_name.clone()),
                            description: Some(tool.description.clone()),
                            meta: None,
                            enabled: disabled.is_none_or(|set| !set.contains(&qualified_name)),
                        }
                    })
                    .collect(),
                auth_required,
                server_disabled,
            );
            seen.insert(entry.name.clone());
            servers.push(entry);
        }

        for reauth in &catalog.reauth_connectors {
            if reauth.connector_id.is_empty() {
                continue;
            }
            if !emitted_ids.insert(&reauth.connector_id) {
                continue;
            }
            let server_disabled =
                disabled_connectors.is_some_and(|set| set.contains(&reauth.connector_id));
            let entry = managed_gateway_server_entry(
                &reauth.connector_id,
                &reauth.connector_name,
                Vec::new(),
                /*auth_required*/ true,
                server_disabled,
            );
            seen.insert(entry.name.clone());
            servers.push(entry);
        }
    }

    // Local servers (HTTP or Stdio)
    for server in local_servers {
        let name = crate::session::mcp_servers::mcp_server_name(server).to_string();
        if seen.insert(name.clone()) {
            let source = McpServerSource::Local;
            let config = match server {
                acp::McpServer::Http(acp::McpServerHttp { url, .. })
                | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => McpServerConfig::Http {
                    url: url.clone(),
                    scope: None,
                    scope_id: None,
                    scope_name: None,
                },
                acp::McpServer::Stdio(acp::McpServerStdio {
                    command, args, env, ..
                }) => McpServerConfig::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env
                        .iter()
                        .map(|e| McpEnvVar {
                            name: e.name.clone(),
                            value: e.value.clone(),
                        })
                        .collect(),
                },
                // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
                _ => continue,
            };
            servers.push(McpServerEntry {
                name,
                display_name: None,
                icons: Vec::new(),
                source,
                config,
                source_label: None,
                setup: None,
                setup_values: None,
                session: None,
            });
        }
    }

    servers
}

pub const MANAGED_GATEWAY_ENTRY_PREFIX: &str = "managed_gateway:";

fn managed_gateway_entry_name(connector_id: &str) -> String {
    format!("{MANAGED_GATEWAY_ENTRY_PREFIX}{connector_id}")
}

/// An empty wire name would win over the entry name in the pager and paint a blank row, so the
/// display name falls back to the connector id.
fn managed_gateway_server_entry(
    connector_id: &str,
    display_name: &str,
    tools: Vec<McpToolEntry>,
    auth_required: bool,
    server_disabled: bool,
) -> McpServerEntry {
    let display_name = if display_name.is_empty() {
        connector_id
    } else {
        display_name
    };
    McpServerEntry {
        name: managed_gateway_entry_name(connector_id),
        display_name: Some(display_name.to_owned()),
        icons: Vec::new(),
        source: McpServerSource::Managed,
        config: McpServerConfig::ManagedGateway,
        source_label: None,
        setup: None,
        setup_values: None,
        session: Some(McpServerSessionState {
            enabled: !server_disabled,
            status: (!auth_required && !server_disabled).then_some(McpSessionStatus::Ready),
            tools,
            auth_required,
            setup_required: false,
            blocked_reason: None,
        }),
    }
}

fn managed_gateway_connector_id(entry_name: &str) -> Option<&str> {
    entry_name.strip_prefix(MANAGED_GATEWAY_ENTRY_PREFIX)
}

fn disabled_server_placeholder_entry(name: &str) -> McpServerEntry {
    let is_managed_gateway = name.starts_with(MANAGED_GATEWAY_ENTRY_PREFIX);
    let source = if is_managed_gateway {
        McpServerSource::Managed
    } else {
        McpServerSource::Local
    };
    let config = if is_managed_gateway {
        McpServerConfig::ManagedGateway
    } else {
        McpServerConfig::Stdio {
            command: std::path::PathBuf::new(),
            args: Vec::new(),
            env: Vec::new(),
        }
    };
    McpServerEntry {
        name: name.to_owned(),
        display_name: name
            .strip_prefix(MANAGED_GATEWAY_ENTRY_PREFIX)
            .map(str::to_owned),
        icons: Vec::new(),
        source,
        source_label: None,
        setup: None,
        setup_values: None,
        config,
        session: Some(McpServerSessionState {
            enabled: false,
            status: None,
            tools: vec![],
            auth_required: false,
            setup_required: false,
            blocked_reason: None,
        }),
    }
}

// ── Session-level operations (called via SessionCommand) ────────────

/// Build session MCP status: which servers are enabled, healthy, and what tools they expose.
/// Clones state under lock then releases; it does not hold the lock across awaits.
pub(crate) async fn build_mcp_status(
    mcp_state: &Arc<TokioMutex<McpState>>,
    tool_bridge: &Arc<distill_tools::bridge::ToolBridge>,
    event_writer: Option<&distill_session_events::EventWriter>,
) -> McpStatusSnapshot {
    let _build_mcp_status_timer = crate::instrumentation::timer("build_mcp_status");
    let (
        configs,
        clients,
        is_initialized,
        initializing_servers,
        mcp_tool_meta,
        mcp_tool_icons,
        auth_required,
        init_failed,
        disabled_regs,
    ) = {
        let state = mcp_state.lock().await;
        (
            state.configs.clone(),
            state
                .all_clients()
                .map(|(_, c)| c.clone())
                .collect::<Vec<_>>(),
            state.is_initialized(),
            state.handshaking_servers_cloned(),
            state.mcp_tool_meta.clone(),
            state.mcp_tool_icons.clone(),
            state.auth_required.clone(),
            state.init_failed.clone(),
            // Collect (qualified_name, description) for disabled tools so we can include them in the snapshot without cloning the full registration
            state
                .disabled_tool_registrations
                .iter()
                .map(|(k, v)| (k.clone(), v.description.clone()))
                .collect::<Vec<_>>(),
        )
    };

    let mut client_statuses = Vec::with_capacity(clients.len());
    let _client_loop_timer = crate::instrumentation::timer("mcp_status_client_loop");

    for client in &clients {
        let name = client.server_name().to_string();
        let prefix = format!("{}{}", name, MCP_TOOL_NAME_DELIMITER);

        let healthy = client.is_healthy().await;
        if let Some(ew) = event_writer {
            ew.emit(distill_session_events::Event::McpHealthCheck {
                server_name: name.clone(),
                healthy,
                client_state: Some(if healthy { "ready" } else { "unavailable" }.to_string()),
            });
        }
        // A server whose background init failed (a handshake or `tools/list` error, or a timeout) is reported as Unavailable even when the
        // transport is still alive
        // Otherwise a server that connected but hung on `tools/list` (0 tools registered) would misleadingly show as Ready
        let ready = healthy && !init_failed.contains_key(name.as_str());
        let (status, tools) = if ready {
            let _tool_defs_timer = crate::instrumentation::timer("mcp_status_tool_definitions");
            let mut tools: Vec<McpToolEntry> = tool_bridge
                .tool_definitions()
                .await
                .into_iter()
                .filter(|t| t.function.name.starts_with(&prefix))
                .map(|t| {
                    let qualified_name = &t.function.name;
                    let unqualified = qualified_name
                        .strip_prefix(&prefix)
                        .unwrap_or(qualified_name)
                        .to_string();
                    let meta = mcp_tool_meta.get(qualified_name).cloned();
                    let icons = mcp_tool_icons
                        .get(qualified_name)
                        .cloned()
                        .unwrap_or_default();
                    McpToolEntry {
                        name: unqualified,
                        display_name: None,
                        description: t.function.description.clone(),
                        meta,
                        icons,
                        enabled: true,
                    }
                })
                .collect();

            // Include disabled tools from stashed registrations.
            for (qname, desc) in &disabled_regs {
                if qname.starts_with(&prefix) {
                    let unqualified = qname.strip_prefix(&prefix).unwrap_or(qname).to_string();
                    let meta = mcp_tool_meta.get(qname).cloned();
                    let icons = mcp_tool_icons.get(qname).cloned().unwrap_or_default();
                    tools.push(McpToolEntry {
                        name: unqualified,
                        display_name: None,
                        description: Some(desc.clone()),
                        meta,
                        icons,
                        enabled: false,
                    });
                }
            }

            // Stable alphabetical order so tools don't jump around when toggled between enabled and disabled
            tools.sort_by(|a, b| a.name.cmp(&b.name));

            (McpSessionStatus::Ready, tools)
        } else {
            (McpSessionStatus::Unavailable, vec![])
        };

        let icons = client.server_icons().await;
        client_statuses.push(McpClientStatus {
            name,
            status,
            tools,
            icons,
        });
    }

    // A server configured but not yet handshaked (either global init or per-server background init) reports Initializing
    // We use initializing_servers (populated before spawning handshakes) so that slow servers continue showing Initializing after we call
    // finish_init() early
    for config in &configs {
        let cname = crate::session::mcp_servers::mcp_server_name(config);
        if !client_statuses.iter().any(|c| c.name == cname) && initializing_servers.contains(cname)
        {
            client_statuses.push(McpClientStatus {
                name: cname.to_string(),
                status: McpSessionStatus::Initializing,
                tools: vec![],
                icons: Vec::new(),
            });
        }
    }

    McpStatusSnapshot {
        configs,
        clients: client_statuses,
        auth_required,
        is_resolved: is_initialized,
    }
}

/// Ensure the agent-level MCP pool is initialized, waiting if another caller is already initializing. Safe to call concurrently.
async fn ensure_agent_pool_initialized(mcp_state: &Arc<TokioMutex<McpState>>) {
    loop {
        let state = mcp_state.lock().await;
        if state.is_initialized() {
            return;
        }
        if state.is_initializing() {
            // Another call is initializing; wait and retry
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        drop(state);
        let cwd = std::env::current_dir().unwrap_or_default();
        init_agent_mcp_pool(mcp_state, &cwd).await;
        return;
    }
}

/// Spawn config.toml MCP clients into the agent pool. Handshakes happen lazily on first `CallMcpTool`.
pub(crate) async fn init_agent_mcp_pool(
    mcp_state: &Arc<TokioMutex<McpState>>,
    cwd: &std::path::Path,
) {
    use crate::session::mcp_servers::start_mcp_servers;

    // Generation is read under the same lock as `try_start_init` (which creates the claim guard): this pass may only finish what it claimed.
    let (configs, generation, init_claim) = {
        let mut state = mcp_state.lock().await;
        let Some(init_claim) = state.try_start_init() else {
            return;
        };
        let generation = state.current_generation();
        (state.configs.clone(), generation, init_claim)
    };

    let noop = distill_session_events::EventWriter::noop();
    let ctx = crate::session::mcp_servers::McpSpawnCtx::standalone(&noop)
        .with_oauth_discovery(crate::session::mcp_servers::McpOauthDiscovery::Network);
    let meta = Default::default();
    let oauth = Default::default();
    let results = start_mcp_servers(configs, Some(cwd), &meta, &oauth, &ctx).await;
    let clients: distill_mcp::owned_clients::OwnedClients = results
        .into_iter()
        .filter_map(|r| match r {
            Ok(client) => {
                tracing::info!("Agent MCP server '{}' spawned", client.server_name());
                let name = client.server_name().to_string();
                Some((name, Arc::new(client)))
            }
            Err(e) => {
                tracing::warn!("Failed to spawn agent MCP server: {}", e);
                None
            }
        })
        .collect();

    finish_pool_init(mcp_state, &generation, init_claim, clients).await;
}

/// A pool pass finishes only the generation it claimed: current installs and finishes, superseded discards its clients and cancels only if it still owns init.
async fn finish_pool_init(
    mcp_state: &Arc<TokioMutex<McpState>>,
    generation: &crate::session::mcp_servers::Generation,
    init_claim: crate::session::mcp_servers::InitClaimGuard,
    clients: distill_mcp::owned_clients::OwnedClients,
) {
    let finished = mcp_state
        .write_if_current(generation, |state| {
            state.owned_clients = clients;
            state.finish_init();
            state.complete_init();
            tracing::info!(
                "Agent MCP pool: {} servers ready",
                state.owned_clients.len()
            );
        })
        .await;
    if finished.is_err() {
        tracing::info!("agent MCP pool superseded by a config change; discarding spawned clients");
    }
    drop(init_claim);
}

/// Call an MCP tool directly (outside the LLM tool-use loop).
#[tracing::instrument(name = "mcp.call_tool", skip_all, fields(server_name, tool_name))]
pub async fn call_mcp_tool(
    mcp_state: &Arc<TokioMutex<McpState>>,
    server_name: &str,
    server_url: Option<&str>,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<McpCallResponse, String> {
    let span = tracing::Span::current();
    span.record("tool_name", tool_name);
    let (client, target) = {
        let state = mcp_state.lock().await;

        // Resolve: (name + url) > url-only > name-only.
        let target = if let Some(url) = server_url {
            let config_name =
                |c: &acp::McpServer| crate::session::mcp_servers::mcp_server_name(c).to_string();
            state
                .configs
                .iter()
                .find(|c| {
                    crate::session::mcp_servers::mcp_server_name(c) == server_name
                        && mcp_server_url(c) == Some(url)
                })
                .map(&config_name)
                .or_else(|| {
                    state
                        .configs
                        .iter()
                        .find(|c| mcp_server_url(c) == Some(url))
                        .map(&config_name)
                })
                .unwrap_or_else(|| server_name.to_string())
        } else {
            server_name.to_string()
        };

        let client = state.get_client(&target).cloned();
        (client, target)
    };

    span.record("server_name", target.as_str());

    let client = client.ok_or_else(|| format!("server '{}' not found", target))?;

    let tool_timeout_sec = client.tool_timeout_for(tool_name);
    let timeout = std::time::Duration::from_secs(tool_timeout_sec);
    let result = tokio::time::timeout(timeout, client.call_tool(tool_name, arguments))
        .await
        .map_err(|_| format!("tool '{}' timed out after {}s", tool_name, tool_timeout_sec))?
        .map_err(|e| format!("tool call failed: {}", e))?;

    let content = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(McpContentBlock {
                kind: "text".to_string(),
                text: t.text.clone(),
            }),
            rmcp::model::ContentBlock::Resource(r) => {
                serde_json::to_string(r).ok().map(|json| McpContentBlock {
                    kind: "resource".to_string(),
                    text: json,
                })
            }
            _ => None,
        })
        .collect();

    Ok(McpCallResponse {
        content,
        is_error: result.is_error,
    })
}

// ── mcp/list handler ────────────────────────────────────────────────

/// The `mcp/list` blockedReason wire map: verdicts for every discovered definition the merge
/// would drop, plus the project-pin arm; wire strings name the policy file only.
pub(crate) fn list_blocked_reasons<'a>(
    definitions: impl IntoIterator<
        Item = (
            &'a str,
            &'a acp::McpServer,
            distill_workspace::permission::resolution::McpSubject,
        ),
    >,
    local_servers: &[acp::McpServer],
    project_names: impl FnOnce() -> std::collections::HashSet<String>,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> HashMap<String, String> {
    let mut blocked = crate::session::managed_mcp::mcp_blocked_reasons(definitions, ms);
    if ms.project_mcp.is_disabled() {
        let project = project_names();
        for server in local_servers {
            let name = crate::session::mcp_servers::mcp_server_name(server);
            // Raw catalog entries carry no source tier; classify through the one classifier with the
            // fail-closed non-native tier (only `project_scoped` feeds the pin arm).
            let subject = crate::session::managed_mcp::mcp_subject_for_tier(name, false, &project);
            if let Some(reason) = ms.mcp_project_pin_block(server, subject) {
                blocked.entry(name.to_string()).or_insert(reason);
            }
        }
    }
    blocked
        .into_iter()
        .map(|(name, reason)| (name, reason.user_facing_reason()))
        .collect()
}

/// Discovery walks are synchronous disk scans: hop to the blocking pool, never the session
/// actor's LocalSet. `McpDiscoveryInputs` borrows, so the owned inputs are rebuilt inside the task.
async fn spawn_discovery<T: Send + 'static>(
    cwd: std::path::PathBuf,
    plugin_registry: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    compat: distill_tools::types::compat::CompatConfig,
    walk: impl FnOnce(&crate::session::managed_mcp::McpDiscoveryInputs<'_>) -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        walk(&crate::session::managed_mcp::McpDiscoveryInputs {
            cwd: &cwd,
            plugin_registry: plugin_registry.as_deref(),
            compat: &compat,
        })
    })
    .await
}

async fn handle_list(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    // The gateway catalog fetch and the session-state branch run concurrently via tokio::join!
    // The session-state branch is a conditional `retry_auth_required_servers` followed by `build_mcp_status`
    // OAuth retries only fire on explicit refresh (cache=false); cached opens skip them so the warm path stays fast
    let req = parse_params::<McpListRequest>(args)?;

    let cwd = req
        .session_id
        .as_ref()
        .and_then(|sid| agent.get_session_cwd(&acp::SessionId::new(sid.clone())))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Resolve the session handle synchronously up front so the session-state future can be polled alongside the gateway catalog fetch
    let session_handle = req.session_id.as_ref().and_then(|sid| {
        let acp_id = acp::SessionId::new(sid.clone());
        agent.get_session_handle(&acp_id)
    });
    if let (Some(sid), None) = (req.session_id.as_ref(), session_handle.as_ref()) {
        tracing::debug!(
            session_id = %sid,
            "mcp/list: session not found, returning agent-level catalog only"
        );
    }

    let cache = req.cache;
    let session_state_fut = async {
        let handle = session_handle.as_ref()?;
        // Auth retries belong on explicit refresh: skipping them on cached opens saves ~500ms when multiple OAuth servers are configured
        if !cache {
            handle.retry_auth_required_servers().await;
        }
        Some(handle.get_mcp_status().await)
    };

    let (gateway_catalog, session_snapshot) = tokio::join!(
        agent.fetch_gateway_catalog_for_mcp_list(cache),
        session_state_fut
    );

    let compat = agent.cfg.borrow().compat_resolved;
    let plugin_registry_snapshot = agent.plugin_registry_snapshot();
    let local_servers = crate::util::config::load_mcp_servers(&cwd, &compat);
    let disabled_tools = crate::util::config::get_all_mcp_disabled_tools(&cwd);
    let mut servers = build_mcp_catalog_with_gateway_tools(
        &local_servers,
        gateway_catalog.as_ref(),
        &disabled_tools,
    );
    let disabled_names = crate::util::config::disabled_mcp_server_names(&cwd);
    let setup_entries = crate::util::config::collect_mcp_setup_configs(
        &cwd,
        plugin_registry_snapshot.as_deref(),
        &compat,
    );
    let preferences = crate::util::config::load_mcp_preferences().file();
    for (name, setup_entry) in setup_entries {
        if servers.iter().any(|entry| entry.name == name) {
            continue;
        }
        let enabled = !disabled_names.contains(&name);
        let setup_schema = setup_entry.config.setup.clone();
        let (setup, setup_required, status) = match setup_entry
            .config
            .resolve_setup(preferences.servers.get(&name))
        {
            crate::util::config::McpSetupResolution::Required(setup) => {
                (Some(setup), true, Some(McpSessionStatus::SetupRequired))
            }
            // Surface schema/template breakage instead of dropping the row.
            crate::util::config::McpSetupResolution::Invalid(_) => {
                (setup_schema, true, Some(McpSessionStatus::SetupRequired))
            }
            crate::util::config::McpSetupResolution::Resolved(_) => continue,
        };
        let values = preferences
            .servers
            .get(&name)
            .map(|prefs| prefs.values.clone());
        servers.push(McpServerEntry {
            name: name.clone(),
            icons: Vec::new(),
            display_name: None,
            source: McpServerSource::Local,
            source_label: setup_entry
                .source
                .plugin
                .as_ref()
                .map(|plugin| format!("plugin: {plugin}")),
            setup,
            setup_values: values,
            config: McpServerConfig::Http {
                url: String::new(),
                scope: None,
                scope_id: None,
                scope_name: None,
            },
            session: Some(McpServerSessionState {
                enabled,
                status,
                tools: vec![],
                auth_required: false,
                setup_required,
                blocked_reason: None,
            }),
        });
    }

    // Disabled stubs: only names Space enable can still resolve (see `crate::util::config::mcp_reenable`)
    // Orphans with no definition stay hidden
    let catalog_names: HashSet<String> = servers.iter().map(|s| s.name.clone()).collect();
    // One discovery pass per request: the index serves both the disabled
    // stubs and (for a live session list) the blocked-reason verdicts.
    let needs_stub_scan =
        crate::util::config::needs_definition_scan(&disabled_names, &catalog_names);
    // On task failure this display-only index degrades to None.
    let definition_index = if needs_stub_scan || session_snapshot.is_some() {
        spawn_discovery(
            cwd.clone(),
            plugin_registry_snapshot.clone(),
            compat,
            crate::util::config::McpDefinitionIndex::build,
        )
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "mcp/list definition discovery task failed"))
        .ok()
    } else {
        None
    };
    if needs_stub_scan && let Some(index) = &definition_index {
        let ms = distill_workspace::permission::resolution::managed_settings();
        for name in index.reenableable_for_list(&disabled_names, &catalog_names, ms) {
            servers.push(disabled_server_placeholder_entry(&name));
        }
    }

    let session_mcp_resolved = session_snapshot
        .as_ref()
        .map(|snapshot| snapshot.is_resolved);
    // Carry the verdict for policy-dropped servers so the pager can say "blocked by policy"
    // instead of a generic "unavailable"; computed only with a session snapshot.
    let blocked_reasons: HashMap<String, String> = match (&session_snapshot, &definition_index) {
        (Some(_), Some(index)) => {
            let ms = distill_workspace::permission::resolution::managed_settings();
            list_blocked_reasons(
                index.definitions(),
                &local_servers,
                || crate::agent::folder_trust::project_scoped_mcp_names(&cwd),
                ms,
            )
        }
        _ => HashMap::new(),
    };

    if let Some(snapshot) = session_snapshot {
        if gateway_catalog.is_some()
            && let Some(disabled) = match session_handle.as_ref() {
                Some(h) => Some(h.managed_gateway_disabled_tool_names().await),
                None => None,
            }
        {
            for entry in &mut servers {
                if entry.source == McpServerSource::Managed
                    && let Some(session) = entry.session.as_mut()
                {
                    let connector_id =
                        managed_gateway_connector_id(&entry.name).unwrap_or(&entry.name);
                    if let Some(tools) = disabled.get(connector_id) {
                        for tool in &mut session.tools {
                            if tools.contains(&tool.name) {
                                tool.enabled = false;
                            }
                        }
                    }
                }
            }
        }
        // `session_snapshot` is `Some` only when `session_handle` resolved, which requires `req.session_id` to have been `Some` An `expect` would assert that non-local invariant here
        // A future refactor of `session_state_fut` could silently turn that `expect` into a panic in a request handler So use a local `if let` guard around the only consumer, the debug log
        // We emit `%sid` (Display) to match the sibling "session not found" log `?req.session_id` would wrap the bare string as `Some("...")` and diverge from the earlier format
        if let Some(sid) = req.session_id.as_ref() {
            tracing::debug!(session_id = %sid, "Annotating mcp/list with session state");
        }
        let catalog_names: std::collections::HashSet<String> =
            servers.iter().map(|s| s.name.clone()).collect();

        // Annotate catalog entries with session state.
        for entry in &mut servers {
            if entry
                .session
                .as_ref()
                .is_some_and(|session| session.setup_required)
            {
                continue;
            }
            let managed_gateway_session = entry.source == McpServerSource::Managed
                && matches!(&entry.config, McpServerConfig::ManagedGateway);
            if managed_gateway_session {
                if let Some(session) = entry.session.as_mut() {
                    let connector_id =
                        managed_gateway_connector_id(&entry.name).unwrap_or(&entry.name);
                    let managed_disabled = disabled_tools
                        .get(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY)
                        .is_some_and(|set| set.contains(connector_id));
                    session.enabled = !disabled_names.contains(&entry.name) && !managed_disabled;
                }
                continue;
            }
            let enabled = snapshot
                .configs
                .iter()
                .any(|c| crate::session::mcp_servers::mcp_server_name(c) == entry.name);
            let (status, tools, icons) = snapshot
                .clients
                .iter()
                .find(|c| c.name == entry.name)
                .map(|c| (Some(c.status.clone()), c.tools.clone(), c.icons.clone()))
                .unwrap_or((None, vec![], Vec::new()));
            entry.icons = icons;
            entry.session = Some(McpServerSessionState {
                enabled,
                status,
                tools,
                auth_required: snapshot.auth_required.contains(&entry.name),
                setup_required: false,
                // Only a server the merge actually dropped is "blocked" — a
                // live one keeps its real status.
                blocked_reason: (!enabled)
                    .then(|| blocked_reasons.get(&entry.name).cloned())
                    .flatten(),
            });
        }

        // Append session-only servers (passed via session/new but not in catalog).
        for client_status in &snapshot.clients {
            if !catalog_names.contains(&client_status.name) {
                servers.push(McpServerEntry {
                    name: client_status.name.clone(),
                    icons: client_status.icons.clone(),
                    display_name: None,
                    source: McpServerSource::Local,
                    source_label: None,
                    setup: None,
                    setup_values: None,
                    config: McpServerConfig::Stdio {
                        command: std::path::PathBuf::new(),
                        args: Vec::new(),
                        env: Vec::new(),
                    },
                    session: Some(McpServerSessionState {
                        enabled: true,
                        status: Some(client_status.status.clone()),
                        tools: client_status.tools.clone(),
                        auth_required: snapshot.auth_required.contains(&client_status.name),
                        setup_required: false,
                        blocked_reason: None,
                    }),
                });
            }
        }
    }

    // Tag servers with the owning plugin
    // This covers both a plugin's .mcp.json and its inline plugin.json mcpServers via the registry's deduped owner map
    if let Some(registry) = plugin_registry_snapshot.as_ref() {
        for entry in &mut servers {
            if entry.source_label.is_none()
                && let Some(plugin_name) = registry.mcp_server_owner(&entry.name)
            {
                entry.source_label = Some(format!("plugin: {plugin_name}"));
            }
        }
    }
    to_ext_response(Ok(McpListResponse {
        servers,
        session_mcp_resolved,
    }))
}

// ── mcp/call handler ────────────────────────────────────────────────

async fn handle_call(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpCallRequest>(args)?;

    let result = match req.session_id {
        Some(sid) => {
            // Session-provided servers: route through the session's MCP pool.
            // Waits for an in-flight `session/load` (a reconnect replay after a leader restart) before failing
            let acp_id = acp::SessionId::new(sid);
            let handle = agent
                .session_handle_waiting_for_load(&acp_id)
                .await
                .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
            handle
                .call_mcp_tool(req.server, req.server_url, req.tool, req.arguments)
                .await
        }
        None => {
            // No session: use the agent-level MCP pool (config.toml servers).
            let mcp_state = agent.agent_mcp_state();
            ensure_agent_pool_initialized(&mcp_state).await;
            call_mcp_tool(
                &mcp_state,
                &req.server,
                req.server_url.as_deref(),
                &req.tool,
                req.arguments,
            )
            .await
        }
    }
    .map_err(|e| acp::Error::internal_error().data(e))?;

    to_ext_response(Ok(result))
}

async fn handle_read_resource(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpReadResourceRequest>(args)?;

    let result = if let Some(ref sid) = req.session_id {
        // Waits for an in-flight `session/load`; see `handle_call` above
        let acp_id = acp::SessionId::new(sid.clone());
        let handle = agent
            .session_handle_waiting_for_load(&acp_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
        handle
            .read_mcp_resource(req.server.clone(), req.uri.clone())
            .await
    } else {
        let mcp_state = agent.agent_mcp_state();
        ensure_agent_pool_initialized(&mcp_state).await;
        read_mcp_resource(&mcp_state, &req.server, &req.uri).await
    }
    .map_err(|e| acp::Error::internal_error().data(e))?;

    to_ext_response(Ok(result))
}

pub(crate) async fn read_mcp_resource(
    mcp_state: &Arc<TokioMutex<McpState>>,
    server_name: &str,
    uri: &str,
) -> Result<McpReadResourceResponse, String> {
    let client = {
        let state = mcp_state.lock().await;
        Arc::clone(
            state
                .get_client(server_name)
                .ok_or_else(|| format!("server '{}' not found", server_name))?,
        )
    };

    let mcp_service = client
        .ensure_initialized()
        .await
        .map_err(|e| format!("MCP init failed: {}", e))?;

    let result = mcp_service
        .read_resource(rmcp::model::ReadResourceRequestParams::new(uri))
        .await
        .map_err(|e| format!("resource read failed: {}", e))?;

    if result.contents.is_empty() {
        return Err("empty resource".to_string());
    }

    let contents: Vec<McpReadResourceContent> = result
        .contents
        .into_iter()
        .filter_map(|c| match c {
            rmcp::model::ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                meta,
                ..
            } => Some(McpReadResourceContent {
                uri,
                mime_type,
                text: Some(text),
                blob: None,
                meta: meta.and_then(|m| serde_json::to_value(m).ok()),
            }),
            rmcp::model::ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                meta,
                ..
            } => Some(McpReadResourceContent {
                uri,
                mime_type,
                text: None,
                blob: Some(blob),
                meta: meta.and_then(|m| serde_json::to_value(m).ok()),
            }),
            // `ResourceContents` is non_exhaustive; skip unknown variants so the rest of the resource still renders
            // Log the drop so the missing content is diagnosable
            _ => {
                tracing::warn!(
                    server = server_name,
                    uri,
                    "skipping unknown MCP resource content variant"
                );
                None
            }
        })
        .collect();

    if contents.is_empty() {
        return Err("resource contained only unsupported content variants".to_string());
    }

    Ok(McpReadResourceResponse { contents })
}

// ── McpResourceProvider bridge ───────────────────────────────────────
// Implements the `McpResourceProvider` trait from distill-tools
// The `ListMcpResources` and `FetchMcpResource` tools can then access MCP servers without depending on `distill-mcp` directly

/// Bridge from `McpState` to the `McpResourceProvider` trait.
/// Injected into the agent's `SharedResources` via `tool_bridge.update_resource()` at session startup.
/// Tools can then enumerate and fetch MCP resources.
pub(crate) struct McpStateResourceProvider(pub Arc<TokioMutex<McpState>>);

#[async_trait::async_trait]
impl distill_tools::types::resources::McpResourceProvider for McpStateResourceProvider {
    async fn list_resources(
        &self,
        server: Option<String>,
    ) -> Result<Vec<distill_tools::types::resources::McpResourceInfo>, String> {
        let clients: Vec<(String, Arc<McpClient>)> = {
            let state = self.0.lock().await;
            match &server {
                Some(name) => match state.get_client(name) {
                    Some(c) => vec![(name.clone(), Arc::clone(c))],
                    None => return Err(format!("MCP server '{name}' not found")),
                },
                None => state
                    .all_clients()
                    .map(|(name, client)| (name.to_string(), Arc::clone(client)))
                    .collect(),
            }
        };

        let mut resources = Vec::new();
        for (server_name, client) in clients {
            let mcp_service = match client.ensure_initialized().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "Failed to initialize MCP server for list_resources"
                    );
                    continue;
                }
            };

            match mcp_service.list_all_resources().await {
                Ok(all_resources) => {
                    for r in all_resources {
                        resources.push(distill_tools::types::resources::McpResourceInfo {
                            uri: r.uri.clone(),
                            name: Some(r.name.clone()),
                            description: r.description.clone(),
                            mime_type: r.mime_type.clone(),
                            server: server_name.clone(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "list_resources RPC failed"
                    );
                    if server.is_some() {
                        return Err(format!("list_resources failed for '{server_name}': {e}"));
                    }
                    // For all-servers mode, skip failures and continue.
                }
            }
        }

        Ok(resources)
    }

    async fn read_resource(
        &self,
        server: String,
        uri: String,
    ) -> Result<distill_tools::types::resources::McpResourceReadResult, String> {
        let client = {
            let state = self.0.lock().await;
            Arc::clone(
                state
                    .get_client(&server)
                    .ok_or_else(|| format!("MCP server '{server}' not found"))?,
            )
        };

        let mcp_service = client
            .ensure_initialized()
            .await
            .map_err(|e| format!("MCP init failed: {e}"))?;

        // `RunningService::read_resource` (not `Peer::read_resource`) drives SEP-2322
        // `input_required` rounds through the client handler, so elicitation requests on
        // resource reads reach the HITL bridge like tool calls do.
        let result = mcp_service
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.clone()))
            .await
            .map_err(|e| format!("resource read failed: {e}"))?;

        if result.contents.is_empty() {
            return Err(format!("Resource not found: {uri}"));
        }

        let first = result
            .contents
            .into_iter()
            .find(|c| {
                let supported = matches!(
                    c,
                    rmcp::model::ResourceContents::TextResourceContents { .. }
                        | rmcp::model::ResourceContents::BlobResourceContents { .. }
                );
                if !supported {
                    tracing::warn!(uri, "skipping unknown MCP resource content variant");
                }
                supported
            })
            .ok_or_else(|| format!("Unsupported resource content type for: {uri}"))?;
        match first {
            rmcp::model::ResourceContents::TextResourceContents {
                uri: content_uri,
                mime_type,
                text,
                ..
            } => Ok(distill_tools::types::resources::McpResourceReadResult {
                uri: content_uri,
                name: None,
                description: None,
                mime_type,
                content: Some(distill_tools::types::resources::McpResourceContent::Text(
                    text,
                )),
            }),
            rmcp::model::ResourceContents::BlobResourceContents {
                uri: content_uri,
                mime_type,
                blob,
                ..
            } => Ok(distill_tools::types::resources::McpResourceReadResult {
                uri: content_uri,
                name: None,
                description: None,
                mime_type,
                content: Some(distill_tools::types::resources::McpResourceContent::Blob(
                    blob.into_bytes(),
                )),
            }),
            // Unreachable: `first` is pre-filtered to supported variants, but `ResourceContents` is non_exhaustive so the match must be total
            _ => Err(format!("Unsupported resource content type for: {uri}")),
        }
    }
}

// ── Auth status and trigger ──────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpAuthStatusRequest {
    session_id: String,
}

#[derive(serde::Serialize)]
pub struct McpAuthStatusEntry {
    pub server_name: String,
    pub status: &'static str,
}

#[derive(serde::Serialize)]
struct McpAuthStatusResponse {
    servers: Vec<McpAuthStatusEntry>,
}

async fn handle_auth_status(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpAuthStatusRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id);
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let entries = handle.mcp_auth_status().await;
    to_ext_response(Ok(McpAuthStatusResponse { servers: entries }))
}

#[derive(serde::Deserialize)]
struct McpAuthTriggerRequest {
    session_id: String,
    server_name: String,
}

#[derive(serde::Serialize)]
struct McpAuthTriggerResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup: Option<crate::util::config::McpSetupConfig>,
    /// Descriptive failure reason from the shell.
    /// `None` on success and on failures with no detail; the TUI shows it verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn handle_auth_trigger(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpAuthTriggerRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id);
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let cwd = agent
        .get_session_cwd(&acp_id)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let setup_entries = crate::util::config::collect_mcp_setup_configs(
        &cwd,
        agent.plugin_registry_snapshot().as_deref(),
        &agent.cfg.borrow().compat_resolved,
    );
    let preferences = crate::util::config::load_mcp_preferences().file();
    if let Some(entry) = setup_entries.get(&req.server_name) {
        match entry
            .config
            .resolve_setup(preferences.servers.get(&req.server_name))
        {
            crate::util::config::McpSetupResolution::Required(setup) => {
                return to_ext_response(Ok(McpAuthTriggerResponse {
                    status: "setup_required",
                    setup: Some(setup),
                    error: None,
                }));
            }
            crate::util::config::McpSetupResolution::Invalid(reason) => {
                return to_ext_response(Ok(McpAuthTriggerResponse {
                    status: "setup_required",
                    setup: entry.config.setup.clone(),
                    error: Some(reason),
                }));
            }
            crate::util::config::McpSetupResolution::Resolved(_) => {}
        }
    }
    match handle.mcp_auth_trigger(req.server_name).await {
        Ok(()) => to_ext_response(Ok(McpAuthTriggerResponse {
            status: "authenticated",
            setup: None,
            error: None,
        })),
        Err(e) => {
            tracing::warn!(%e, "MCP auth trigger failed");
            to_ext_response(Ok(McpAuthTriggerResponse {
                status: "failed",
                setup: None,
                error: Some(e),
            }))
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpSetupRequest {
    session_id: String,
    server_name: String,
    values: HashMap<String, String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct McpSetupResponse {
    ok: bool,
}

async fn handle_setup(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpSetupRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let cwd = agent
        .get_session_cwd(&acp_id)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let setup_entries = crate::util::config::collect_mcp_setup_configs(
        &cwd,
        agent.plugin_registry_snapshot().as_deref(),
        &agent.cfg.borrow().compat_resolved,
    );
    let entry = setup_entries
        .get(&req.server_name)
        .ok_or_else(|| acp::Error::invalid_params().data("server setup not found"))?;
    let setup = entry
        .config
        .setup
        .as_ref()
        .ok_or_else(|| acp::Error::invalid_params().data("server setup not found"))?;

    // Only schema field ids (never arbitrary client keys).
    let filtered_values: HashMap<String, String> = setup
        .fields
        .iter()
        .filter_map(|field| {
            req.values
                .get(&field.id)
                .map(|value| (field.id.clone(), value.clone()))
        })
        .collect();

    let pending_preferences = crate::util::config::McpServerPreferences {
        values: filtered_values,
        source: Some(entry.source.clone()),
        updated_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    };
    match entry.config.resolve_setup(Some(&pending_preferences)) {
        crate::util::config::McpSetupResolution::Resolved(_) => {}
        crate::util::config::McpSetupResolution::Required(_) => {
            return Err(acp::Error::invalid_params().data("setup values incomplete"));
        }
        crate::util::config::McpSetupResolution::Invalid(reason) => {
            return Err(acp::Error::invalid_params().data(reason));
        }
    }

    let load = crate::util::config::load_mcp_preferences();
    if !load.is_writable() {
        return Err(acp::Error::internal_error().data(
            "MCP preferences file is unreadable; fix or remove mcp_preferences.json before saving",
        ));
    }
    let mut prefs = load.file();
    let previous_entry = prefs.servers.get(&req.server_name).cloned();
    prefs
        .servers
        .insert(req.server_name.clone(), pending_preferences);
    crate::util::config::save_mcp_preferences(&prefs)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    let rollback_prefs = || async {
        // Warn like the sibling enable-write rollback: a silent failure
        // leaves the refused server's setup values persisted with no trace.
        if let Err(e) = crate::util::config::restore_mcp_preference_server(
            &req.server_name,
            previous_entry.clone(),
        )
        .await
        {
            tracing::warn!(
                server = req.server_name.as_str(),
                error = %e,
                "Failed to restore MCP setup preferences after enable failure"
            );
        }
    };

    // Probe → verdict → enable write → re-merge → live toggle via the shared gated enable (it
    // undoes its own enable write on failure); the prefs write above is this handler's to roll back.
    if let Err(e) = enable_mcp_server_gated(agent, &handle, &cwd, &req.server_name).await {
        rollback_prefs().await;
        return Err(match e {
            GatedEnableError::NotFound => {
                acp::Error::internal_error().data("server did not resolve after setup")
            }
            GatedEnableError::PolicyRefused(message) => acp::Error::invalid_params().data(message),
            GatedEnableError::PersistFailed(detail) => acp::Error::internal_error().data(format!(
                "failed to clear disabled MCP server entry after setup resolve: {detail}"
            )),
            GatedEnableError::ToggleFailed(detail) | GatedEnableError::TaskFailed(detail) => {
                acp::Error::internal_error().data(detail)
            }
        });
    }

    to_ext_response(Ok(McpSetupResponse { ok: true }))
}

// ── mcp/toggle handler ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpToggleRequest {
    session_id: String,
    server_name: String,
    enabled: bool,
}

#[derive(serde::Serialize)]
struct McpToggleResponse {
    ok: bool,
}

/// The org-policy refusal for enabling or adding a server (one wording for both).
fn org_policy_message(
    name: &str,
    reason: &distill_workspace::permission::resolution::McpBlockReason,
) -> String {
    // The name is a case-sensitive identifier (long ones middle-truncate). The
    // policy file appears by name only (doctor/JSON/logs keep full paths).
    let path = reason.user_facing_source();
    format!(
        "The server {} is blocked by an organization policy ({path}).",
        clamped_server_name(name)
    )
}

/// Server names are unbounded user input; middle-truncate very long ones so the reason clause
/// survives the pager's ~200-char error truncation. Short names print verbatim.
fn clamped_server_name(name: &str) -> std::borrow::Cow<'_, str> {
    const MAX_CHARS: usize = 40;
    if name.chars().count() <= MAX_CHARS {
        return name.into();
    }
    let head: String = name.chars().take(MAX_CHARS / 2).collect();
    let tail_rev: Vec<char> = name.chars().rev().take(MAX_CHARS / 2 - 1).collect();
    let tail: String = tail_rev.into_iter().rev().collect();
    format!("{head}…{tail}").into()
}

/// User-facing refusal for enabling/spawning a policy-blocked server (`None` when it passes) —
/// the one chokepoint for the toggle, setup, and upsert paths.
pub(crate) fn policy_enable_error(
    ms: &distill_workspace::permission::resolution::ManagedSettings,
    server: &acp::McpServer,
    subject: distill_workspace::permission::resolution::McpSubject,
) -> Option<String> {
    match ms.mcp_verdict(server, subject) {
        distill_workspace::permission::resolution::McpVerdict::Allowed => None,
        distill_workspace::permission::resolution::McpVerdict::Blocked(reason) => {
            Some(org_policy_message(
                crate::session::mcp_servers::mcp_server_name(server),
                &reason,
            ))
        }
    }
}

/// `mcp/upsert`'s gate→persist sequence: the policy refusal comes BEFORE the config write, so a
/// refused upsert leaves no state behind; generic over the persist future (unit-testable).
pub(crate) async fn upsert_gate_then_persist<Fut>(
    ms: &distill_workspace::permission::resolution::ManagedSettings,
    server: &acp::McpServer,
    subject: distill_workspace::permission::resolution::McpSubject,
    persist: impl FnOnce() -> Fut,
) -> Result<Fut::Output, String>
where
    Fut: std::future::Future,
{
    if let Some(message) = policy_enable_error(ms, server, subject) {
        return Err(message);
    }
    Ok(persist().await)
}

/// Typed failure out of [`enable_mcp_server_gated`]; each caller maps the
/// arms onto its own wire error shape.
#[derive(Debug)]
pub(crate) enum GatedEnableError {
    /// No definition resolves for the name (probe miss, or post-write merge
    /// miss — the latter after rolling back the enable write).
    NotFound,
    /// Policy refused (probe verdict, or the post-write merge tag after
    /// rollback), formatted via [`org_policy_message`].
    PolicyRefused(String),
    /// Persisting the enable failed; `save_mcp_server_enabled_in` rolled back
    /// its own partial writes.
    PersistFailed(String),
    /// The live toggle failed after the enable write; the write has been
    /// rolled back.
    ToggleFailed(String),
    /// A blocking discovery/merge task did not complete (any enable write has
    /// been rolled back).
    TaskFailed(String),
}

/// The gated enable's re-merge leg: any discover/merge divergence fails closed AND rolls back
/// the just-persisted enable, or the write silently resurrects the server next session.
pub(crate) async fn confirm_enabled_or_rollback<Fut>(
    server_name: &str,
    found: Option<crate::session::managed_mcp::McpServerWithPolicy>,
    rollback: impl FnOnce() -> Fut,
) -> Result<acp::McpServer, GatedEnableError>
where
    Fut: std::future::Future<Output = ()>,
{
    match found {
        Some(s) => match s.disabled_reason {
            None => Ok(s.server),
            Some(reason) => {
                rollback().await;
                Err(GatedEnableError::PolicyRefused(org_policy_message(
                    server_name,
                    &reason,
                )))
            }
        },
        None => {
            rollback().await;
            Err(GatedEnableError::NotFound)
        }
    }
}

/// The gated-enable core, generic over its five effects so the ordering (verdict before write,
/// rollback on every failure past it) is unit-testable; [`enable_mcp_server_gated`] wires the real effects.
pub(crate) async fn run_gated_enable<P, ProbeFut, PersistFut, RollFut, MergeFut, ToggleFut>(
    server_name: &str,
    probe: impl FnOnce() -> ProbeFut,
    persist_enable: impl FnOnce() -> PersistFut,
    rollback: impl Fn(P) -> RollFut,
    merge_find: impl FnOnce() -> MergeFut,
    toggle: impl FnOnce(acp::McpServer) -> ToggleFut,
) -> Result<(), GatedEnableError>
where
    P: Clone,
    ProbeFut: std::future::Future<Output = Result<(), GatedEnableError>>,
    PersistFut: std::future::Future<Output = Result<P, String>>,
    RollFut: std::future::Future<Output = ()>,
    MergeFut: std::future::Future<
            Output = Result<Option<crate::session::managed_mcp::McpServerWithPolicy>, String>,
        >,
    ToggleFut: std::future::Future<Output = Result<(), String>>,
{
    probe().await?;

    // Clear the personal disable only after the policy passes. No-op (empty
    // path list) when the server wasn't disabled.
    let enable_paths = persist_enable()
        .await
        .map_err(GatedEnableError::PersistFailed)?;

    let found = match merge_find().await {
        Ok(found) => found,
        Err(detail) => {
            rollback(enable_paths).await;
            return Err(GatedEnableError::TaskFailed(detail));
        }
    };
    let server =
        confirm_enabled_or_rollback(server_name, found, || rollback(enable_paths.clone())).await?;

    if let Err(detail) = toggle(server).await {
        // Without this rollback the persisted enable silently spawns the
        // server in every later session while the client saw only an error.
        rollback(enable_paths).await;
        return Err(GatedEnableError::ToggleFailed(detail));
    }
    Ok(())
}

/// The ONE gated enable sequence for `mcp/toggle` and `mcp/setup`: policy verdict BEFORE any
/// config write; every failure past the write rolls the enable back ([`run_gated_enable`]).
async fn enable_mcp_server_gated(
    agent: &MvpAgent,
    handle: &crate::session::SessionHandle,
    cwd: &std::path::Path,
    server_name: &str,
) -> Result<(), GatedEnableError> {
    let plugin_reg = agent.plugin_registry_snapshot();
    let compat = agent.cfg.borrow().compat_resolved;

    let probe_reg = plugin_reg.clone();
    let probe = || async move {
        let discovered = spawn_discovery(
            cwd.to_path_buf(),
            probe_reg,
            compat,
            crate::session::managed_mcp::discover_mcp_definitions_ignoring_disable,
        )
        .await
        .map_err(|e| GatedEnableError::TaskFailed(format!("MCP discovery task failed: {e}")))?;
        let Some((probe, probe_subject)) = discovered.get(server_name) else {
            return Err(GatedEnableError::NotFound);
        };
        let ms = distill_workspace::permission::resolution::managed_settings();
        if let Some(message) = policy_enable_error(ms, probe, *probe_subject) {
            return Err(GatedEnableError::PolicyRefused(message));
        }
        Ok(())
    };

    let persist_enable = || async move {
        crate::util::config::save_mcp_server_enabled_in(server_name, true, cwd)
            .await
            .map_err(|e| e.to_string())
    };

    let rollback = |paths: Vec<std::path::PathBuf>| async move {
        if let Err(re) =
            crate::util::config::restore_mcp_server_enabled_after_enable(server_name, &paths).await
        {
            tracing::warn!(
                server = server_name,
                error = %re,
                "Failed to restore MCP enable state after enable failure"
            );
        }
    };

    let merge_reg = plugin_reg.clone();
    let merge_find = || async move {
        let cwd = cwd.to_path_buf();
        let server_name = server_name.to_string();
        // Full config walk: blocking pool, never the session actor's LocalSet.
        tokio::task::spawn_blocking(move || {
            crate::session::managed_mcp::merge_managed_mcp_servers_with_policy(
                vec![],
                &cwd,
                merge_reg.as_deref(),
                &compat,
            )
            .into_iter()
            .find(|s| crate::session::mcp_servers::mcp_server_name(&s.server) == server_name)
        })
        .await
        .map_err(|e| format!("MCP merge task failed: {e}"))
    };

    let toggle = |server: acp::McpServer| async move {
        handle
            .toggle_mcp_server(server_name.to_string(), true, Some(server))
            .await
            .map_err(|e| e.to_string())
    };

    run_gated_enable(
        server_name,
        probe,
        persist_enable,
        rollback,
        merge_find,
        toggle,
    )
    .await
}

async fn handle_toggle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpToggleRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    if let Some(connector_id) = managed_gateway_connector_id(&req.server_name) {
        // Managed-gateway connectors are exempt from the MCP server policy by design (server-side
        // curated); only local/plugin/project definitions pass the gated enable below.
        let enable_paths = if req.enabled {
            let cwd = agent
                .get_session_cwd(&acp_id)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            // Propagate like the local-server sibling (PersistFailed): `mcp/list` derives gateway
            // enablement from `disabled_mcp_servers`, so an unpersisted enable misreports ok.
            Some(
                crate::util::config::save_mcp_server_enabled_in(&req.server_name, true, &cwd)
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("failed to clear disabled MCP server entry: {e}"))
                    })?,
            )
        } else {
            None
        };
        if let Err(e) = handle
            .toggle_managed_gateway_tool(connector_id.to_string(), String::new(), req.enabled)
            .await
        {
            // Roll the persisted enable back like the gated local sibling: without it, `mcp/list` shows
            // enabled while the client only saw an error.
            if let Some(paths) = enable_paths
                && let Err(re) = crate::util::config::restore_mcp_server_enabled_after_enable(
                    &req.server_name,
                    &paths,
                )
                .await
            {
                tracing::warn!(
                    server = req.server_name.as_str(),
                    error = %re,
                    "Failed to restore MCP enable state after gateway toggle failure"
                );
            }
            return Err(acp::Error::internal_error().data(e.to_string()));
        }
        return to_ext_response(Ok(McpToggleResponse { ok: true }));
    }

    if req.enabled {
        let cwd = agent
            .get_session_cwd(&acp_id)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        enable_mcp_server_gated(agent, &handle, &cwd, &req.server_name)
            .await
            .map_err(|e| match e {
                GatedEnableError::NotFound => acp::Error::invalid_params()
                    .data(format!("server '{}' not found in config", req.server_name)),
                GatedEnableError::PolicyRefused(message) => {
                    acp::Error::invalid_params().data(message)
                }
                GatedEnableError::PersistFailed(detail) => acp::Error::internal_error().data(
                    format!("failed to clear disabled MCP server entry: {detail}"),
                ),
                GatedEnableError::ToggleFailed(detail) | GatedEnableError::TaskFailed(detail) => {
                    acp::Error::internal_error().data(detail)
                }
            })?;
        return to_ext_response(Ok(McpToggleResponse { ok: true }));
    }

    handle
        .toggle_mcp_server(req.server_name, false, None)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/toggle_tool handler ─────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpToggleToolRequest {
    session_id: String,
    server_name: String,
    tool_name: String,
    enabled: bool,
}

async fn handle_toggle_tool(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpToggleToolRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    // `managed_gateway:` is reserved, so route by prefix alone
    // Never consult the catalog, or a stale tool toggle would fall back to the local path
    let gateway_connector_id = managed_gateway_connector_id(&req.server_name);
    let is_managed_gateway = gateway_connector_id.is_some();

    if is_managed_gateway {
        handle
            .toggle_managed_gateway_tool(
                gateway_connector_id.unwrap_or(&req.server_name).to_string(),
                req.tool_name,
                req.enabled,
            )
            .await
    } else {
        handle
            .toggle_mcp_tool(req.server_name, req.tool_name, req.enabled)
            .await
    }
    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/upsert handler ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpUpsertRequest {
    session_id: String,
    server_name: String,
    #[serde(flatten)]
    config: crate::util::config::McpServerConfig,
}

/// Policy subject for an `mcp/upsert`: grok-native user config.toml unless a project source
/// claims the name — then foreign (fail closed), via the ONE shared origin classifier.
fn upsert_policy_subject(
    cwd: &std::path::Path,
    server_name: &str,
) -> distill_workspace::permission::resolution::McpSubject {
    crate::session::managed_mcp::mcp_subject_for_tier(
        server_name,
        true,
        &crate::agent::folder_trust::project_scoped_mcp_names(cwd),
    )
}

async fn handle_upsert(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpUpsertRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    // Resolve the session BEFORE the policy check and persist: a dead session id must fail
    // without a config write, and the subject classifies against the session's cwd.
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let cwd = agent
        .get_session_cwd(&acp_id)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Build the ACP server config before persisting so a refused upsert
    // leaves no state behind.
    let server_config = req
        .config
        .to_acp_mcp_server(&req.server_name)
        .ok_or_else(|| {
            // `to_acp_mcp_server` is `None` for a disabled config or one whose
            // setup is unresolved; name the actual cause.
            let detail = if req.config.enabled {
                "server config requires setup"
            } else {
                "server config is disabled"
            };
            acp::Error::invalid_params().data(detail)
        })?;

    // Policy check BEFORE persist and live spawn: /mcps Add/Edit is a spawn path, so a denied
    // server must fail closed exactly like the setup/toggle siblings.
    let subject = upsert_policy_subject(&cwd, &req.server_name);
    let ms = distill_workspace::permission::resolution::managed_settings();
    upsert_gate_then_persist(ms, &server_config, subject, || {
        crate::util::config::save_mcp_server_config(&req.server_name, &req.config)
    })
    .await
    .map_err(|message| acp::Error::invalid_params().data(message))?
    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    // Reuse the toggle path: enable=true with the built config.
    handle
        .toggle_mcp_server(req.server_name, true, Some(server_config))
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/delete handler ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpDeleteRequest {
    session_id: String,
    server_name: String,
}

async fn handle_delete(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpDeleteRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    // Verify the server exists in local config (not managed).
    let existed = crate::util::config::delete_mcp_server_config(&req.server_name)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    if !existed {
        return Err(acp::Error::invalid_params().data(format!(
            "server '{}' not found in config.toml (only locally-configured servers can be deleted)",
            req.server_name
        )));
    }

    // Live teardown: disable the server in the running session.
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    handle
        .toggle_mcp_server(req.server_name.clone(), false, None)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    // The toggle path spawns a task that adds the server to `disabled_mcp_servers`
    // Clear the user list only; leave any project-level disable in place
    let _ = crate::util::config::save_user_mcp_server_enabled(&req.server_name, true).await;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emit-only reverse method (`x.ai/mcp/sdk_call`) shares the `x.ai/mcp/` prefix. `mvp_agent`'s dispatcher therefore routes an inbound copy of it to this module's `handle`.
    /// It must NOT collide with any forward route, so it has no `McpRoute`. `handle` then returns `method_not_found` instead of misrouting a stray inbound reverse call to `handle_call`.
    #[test]
    fn inbound_sdk_call_has_no_forward_route() {
        assert!(
            wire::MCP_SDK_CALL.starts_with(mcp_methods::PREFIX),
            "reverse method must share the prefix so it reaches handle()"
        );
        assert_eq!(
            route_mcp_method(wire::MCP_SDK_CALL),
            None,
            "inbound x.ai/mcp/sdk_call must not resolve to a forward handler"
        );
        // Sanity: the forward sibling on the same prefix DOES route.
        assert_eq!(route_mcp_method(wire::MCP_CALL), Some(McpRoute::Call));
    }

    /// A pool pass superseded by a config change during spawn must not install its clients, finish the successor's pass, or release the successor's claim.
    #[tokio::test]
    async fn superseded_pool_pass_cannot_finish_the_successor() {
        fn stdio(name: &str) -> acp::McpServer {
            acp::McpServer::Stdio(acp::McpServerStdio::new(name.to_string(), "true"))
        }

        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![stdio("old")])));
        let (init_claim, generation, _successor_claim) = {
            let mut state = mcp_state.lock().await;
            let init_claim = state.try_start_init().expect("first pass claims");
            let generation = state.current_generation();
            assert!(state.update_configs(vec![stdio("new")]));
            let successor_claim = state
                .try_start_init()
                .expect("successor claims after the config change");
            (init_claim, generation, successor_claim)
        };

        let stale: distill_mcp::owned_clients::OwnedClients =
            [("old".to_string(), Arc::new(McpClient::stub("old")))]
                .into_iter()
                .collect();
        finish_pool_init(&mcp_state, &generation, init_claim, stale).await;

        let state = mcp_state.lock().await;
        assert!(
            state.owned_clients.is_empty(),
            "stale clients must be discarded"
        );
        assert!(
            state.is_initializing(),
            "the successor's claim must survive a stale settle"
        );
        assert!(
            !state.has_finished_init(),
            "a stale pass must not finish the successor"
        );
    }

    /// The shared enable/upsert gate refuses a policy-blocked server with the org-policy message and passes an allowed one.
    #[test]
    fn policy_enable_error_fails_closed_for_blocked_server() {
        use distill_workspace::permission::resolution::{McpSubject, PolicySubjectOrigin};

        let ms = deny_evil_corp();
        let subject = McpSubject {
            origin: PolicySubjectOrigin::GrokNative,
            project_scoped: false,
        };
        let denied = acp::McpServer::Http(
            acp::McpServerHttp::new("exfil", "https://evil.corp/mcp").headers(vec![]),
        );
        let message = policy_enable_error(&ms, &denied, subject)
            .expect("denied server must fail closed before spawn");
        assert!(
            message.contains("organization policy") && message.contains("managed_config.toml"),
            "got: {message}"
        );
        assert!(
            !message.contains("/etc/grok/"),
            "user-facing refusal must name the policy file only, got: {message}"
        );

        let allowed = acp::McpServer::Http(
            acp::McpServerHttp::new("ok", "https://ok.example.com/mcp").headers(vec![]),
        );
        assert_eq!(policy_enable_error(&ms, &allowed, subject), None);
    }

    /// Deny policy for `https://evil.corp/*` pinned by a full-path source.
    fn deny_evil_corp() -> distill_workspace::permission::resolution::ManagedSettings {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, ManagedSettings, McpServerAllowlist, McpServerPolicy,
        };
        let mut ms = ManagedSettings::default();
        ms.mcp_allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://evil.corp/*".into(),
            }],
            Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
        ));
        ms
    }

    /// The upsert seam: the policy gate runs BEFORE persist — a refused upsert leaves no config write behind.
    #[tokio::test]
    async fn upsert_gate_refuses_before_persist() {
        use distill_workspace::permission::resolution::{McpSubject, PolicySubjectOrigin};

        let ms = deny_evil_corp();
        let subject = McpSubject {
            origin: PolicySubjectOrigin::GrokNative,
            project_scoped: false,
        };
        let persisted = std::cell::Cell::new(false);

        let denied = acp::McpServer::Http(
            acp::McpServerHttp::new("exfil", "https://evil.corp/mcp").headers(vec![]),
        );
        let err = upsert_gate_then_persist(&ms, &denied, subject, || {
            persisted.set(true);
            std::future::ready(())
        })
        .await
        .expect_err("denied upsert must be refused");
        assert!(
            err.contains("organization policy") && err.contains("managed_config.toml"),
            "got: {err}"
        );
        assert!(
            !persisted.get(),
            "refused upsert must not reach the config write"
        );

        let allowed = acp::McpServer::Http(
            acp::McpServerHttp::new("ok", "https://ok.example.com/mcp").headers(vec![]),
        );
        upsert_gate_then_persist(&ms, &allowed, subject, || {
            persisted.set(true);
            std::future::ready(())
        })
        .await
        .expect("allowed upsert persists");
        assert!(persisted.get());
    }

    /// The toggle seam: a merge refusal rolls back the just-persisted enable; an allowed outcome keeps the write.
    #[tokio::test]
    async fn toggle_merge_refusal_rolls_back_enable() {
        use crate::session::managed_mcp::McpServerWithPolicy;
        use distill_workspace::permission::resolution::McpBlockReason;

        let server = || {
            acp::McpServer::Http(
                acp::McpServerHttp::new("corp", "https://denied.corp.com/mcp").headers(vec![]),
            )
        };
        let rolled_back = std::cell::Cell::new(false);
        let rollback = || {
            rolled_back.set(true);
            std::future::ready(())
        };

        // Blocked verdict: rollback, org-policy message (file name only).
        let blocked = McpServerWithPolicy {
            server: server(),
            disabled_reason: Some(McpBlockReason::Deny {
                source: std::path::PathBuf::from("/etc/grok/managed_config.toml"),
            }),
        };
        let err = confirm_enabled_or_rollback("corp", Some(blocked), rollback)
            .await
            .expect_err("blocked merge outcome must refuse");
        let GatedEnableError::PolicyRefused(message) = err else {
            panic!("blocked merge outcome must refuse as PolicyRefused, got {err:?}");
        };
        assert!(
            message.contains("organization policy")
                && message.contains("managed_config.toml")
                && !message.contains("/etc/grok/"),
            "got: {message}"
        );
        assert!(rolled_back.get(), "refusal must roll back the enable write");

        // Vanished from the merge: also rolls back.
        rolled_back.set(false);
        let err = confirm_enabled_or_rollback("corp", None, rollback)
            .await
            .expect_err("vanished server must refuse");
        assert!(
            matches!(err, GatedEnableError::NotFound),
            "vanished server must refuse as NotFound, got {err:?}"
        );
        assert!(rolled_back.get());

        // Allowed: the write stands and the live config comes back.
        rolled_back.set(false);
        let confirmed = confirm_enabled_or_rollback(
            "corp",
            Some(McpServerWithPolicy {
                server: server(),
                disabled_reason: None,
            }),
            rollback,
        )
        .await
        .expect("allowed server enables");
        assert_eq!(
            crate::session::mcp_servers::mcp_server_name(&confirmed),
            "corp"
        );
        assert!(!rolled_back.get(), "allowed enable must keep the write");
    }

    /// What the [`run_gated_enable`] harness recorded, in call order.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum GatedStep {
        Persist,
        Rollback,
        Toggle,
    }

    /// Drive the gated-enable seam with scripted outcomes, recording the persist/rollback/toggle order.
    async fn drive_gated_enable(
        probe_result: Result<(), GatedEnableError>,
        merge_result: Result<Option<crate::session::managed_mcp::McpServerWithPolicy>, String>,
        toggle_result: Result<(), String>,
    ) -> (Result<(), GatedEnableError>, Vec<GatedStep>) {
        let steps = std::cell::RefCell::new(Vec::new());
        let result = run_gated_enable(
            "corp",
            || std::future::ready(probe_result),
            || {
                steps.borrow_mut().push(GatedStep::Persist);
                std::future::ready(Ok::<u8, String>(7))
            },
            |_paths: u8| {
                steps.borrow_mut().push(GatedStep::Rollback);
                std::future::ready(())
            },
            || std::future::ready(merge_result),
            |_server| {
                steps.borrow_mut().push(GatedStep::Toggle);
                std::future::ready(toggle_result)
            },
        )
        .await;
        (result, steps.into_inner())
    }

    fn merged_allowed() -> Option<crate::session::managed_mcp::McpServerWithPolicy> {
        Some(crate::session::managed_mcp::McpServerWithPolicy {
            server: acp::McpServer::Http(
                acp::McpServerHttp::new("corp", "https://ok.example.com/mcp").headers(vec![]),
            ),
            disabled_reason: None,
        })
    }

    /// Probe leg: a probe refusal returns before the enable write; a clean run persists then toggles.
    #[tokio::test]
    async fn gated_enable_seam_gates_before_the_write() {
        let (result, steps) = drive_gated_enable(
            Err(GatedEnableError::PolicyRefused("blocked".into())),
            Ok(merged_allowed()),
            Ok(()),
        )
        .await;
        assert!(matches!(result, Err(GatedEnableError::PolicyRefused(_))));
        assert!(
            steps.is_empty(),
            "a probe refusal must precede any write, got {steps:?}"
        );

        let (result, steps) = drive_gated_enable(Ok(()), Ok(merged_allowed()), Ok(())).await;
        assert!(result.is_ok());
        assert_eq!(
            steps,
            vec![GatedStep::Persist, GatedStep::Toggle],
            "a clean enable must keep the write"
        );
    }

    /// Rollback legs: every failure past the enable write must roll it back (dropping `rollback` fails here).
    #[tokio::test]
    async fn gated_enable_seam_rolls_back_every_failure_past_the_write() {
        let (result, steps) =
            drive_gated_enable(Ok(()), Err("merge task failed".into()), Ok(())).await;
        assert!(matches!(result, Err(GatedEnableError::TaskFailed(_))));
        assert_eq!(steps, vec![GatedStep::Persist, GatedStep::Rollback]);

        let (result, steps) = drive_gated_enable(Ok(()), Ok(None), Ok(())).await;
        assert!(matches!(result, Err(GatedEnableError::NotFound)));
        assert_eq!(steps, vec![GatedStep::Persist, GatedStep::Rollback]);

        let (result, steps) =
            drive_gated_enable(Ok(()), Ok(merged_allowed()), Err("spawn failed".into())).await;
        assert!(matches!(result, Err(GatedEnableError::ToggleFailed(_))));
        assert_eq!(
            steps,
            vec![GatedStep::Persist, GatedStep::Toggle, GatedStep::Rollback]
        );
    }

    /// The verdict-map seam: pin-dropped project servers get annotated; a discovery verdict wins a shared name.
    #[test]
    fn list_blocked_reasons_covers_project_pin_arm() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, ManagedSettings, McpServerAllowlist, McpServerPolicy, McpSubject,
            PolicyLayerOwnership, PolicyPin, PolicySubjectOrigin,
        };

        let http = |name: &str, url: &str| {
            acp::McpServer::Http(acp::McpServerHttp::new(name, url).headers(vec![]))
        };
        let local_servers = vec![
            http("projsrv", "https://proj.example.com/mcp"),
            http("projallowed", "https://allowed.example.com/mcp"),
            http("usersrv", "https://user.example.com/mcp"),
        ];
        let project_names = || {
            ["projsrv".to_string(), "projallowed".to_string()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        };
        let pin = || PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/managed_config.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        let mut ms = ManagedSettings::default();
        // The /etc/grok grant is admin-owned like the pin, so it may carve the exception.
        ms.mcp_allowlist = McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![AllowedMcpServer::Http {
                    url_pattern: "https://allowed.example.com/*".into(),
                }],
                vec![],
                Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
            )
            .with_ownership(PolicyLayerOwnership::Admin),
        );
        ms.project_mcp = pin();

        let blocked = list_blocked_reasons(std::iter::empty(), &local_servers, project_names, &ms);
        let reason = blocked
            .get("projsrv")
            .expect("pinned-off project server must be annotated");
        assert!(
            reason.contains("enableAllProjectMcpServers = false")
                && reason.contains("managed_config.toml")
                && !reason.contains("/etc/grok/"),
            "got: {reason}"
        );
        assert!(
            !blocked.contains_key("projallowed"),
            "allow-granted project server stays clean"
        );
        assert!(
            !blocked.contains_key("usersrv"),
            "non-project server is not the pin's subject"
        );

        // A discovery verdict wins the shared name over the pin annotation.
        let mut deny_ms = ManagedSettings::default();
        deny_ms.mcp_allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://proj.example.com/*".into(),
            }],
            Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
        ));
        deny_ms.project_mcp = pin();
        let definition = http("projsrv", "https://proj.example.com/mcp");
        let subject = McpSubject {
            origin: PolicySubjectOrigin::Foreign,
            project_scoped: true,
        };
        let blocked = list_blocked_reasons(
            [("projsrv", &definition, subject)],
            &local_servers,
            project_names,
            &deny_ms,
        );
        let reason = blocked.get("projsrv").expect("shared name keeps a verdict");
        assert!(
            reason.contains("deniedMcpServers"),
            "discover reason must win the pin annotation, got: {reason}"
        );
    }

    /// A long unicode name must not push the reason clause past the pager's ~200-char truncation.
    #[test]
    fn org_policy_message_clamps_unbounded_server_names() {
        let name = format!("拒否-Sërver-🚫-{}", "a".repeat(120));
        let reason = distill_workspace::permission::resolution::McpBlockReason::Deny {
            source: std::path::PathBuf::from("/etc/grok/managed_config.toml"),
        };
        let message = org_policy_message(&name, &reason);
        assert!(
            message.contains("is blocked by an organization policy")
                && message.contains("managed_config.toml")
                && !message.contains("/etc/grok/"),
            "got: {message}"
        );
        assert!(
            message.chars().count() < 160,
            "message must survive client-side truncation ({} chars): {message}",
            message.chars().count()
        );
        // Short names stay verbatim.
        let short = org_policy_message("corp-denied", &reason);
        assert!(short.contains("The server corp-denied is blocked"));
    }

    /// The upsert gate's project-pin leg: a project-claimed name goes foreign and refuses under the pin.
    #[test]
    fn upsert_subject_and_pin_refuse_project_claimed_name() {
        use distill_workspace::permission::resolution::{
            ManagedSettings, PolicyLayerOwnership, PolicyPin, PolicySubjectOrigin,
        };

        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".cursor")).unwrap();
        std::fs::write(
            cwd.path().join(".cursor").join("mcp.json"),
            r#"{"mcpServers": {"claimed": {"command": "true"}}}"#,
        )
        .unwrap();

        let claimed = upsert_policy_subject(cwd.path(), "claimed");
        assert_eq!(claimed.origin, PolicySubjectOrigin::Foreign);
        assert!(claimed.project_scoped);
        let fresh = upsert_policy_subject(cwd.path(), "fresh");
        assert_eq!(fresh.origin, PolicySubjectOrigin::GrokNative);
        assert!(!fresh.project_scoped);

        let mut ms = ManagedSettings::default();
        ms.project_mcp = PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/requirements.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        let server = |name: &str| {
            acp::McpServer::Http(
                acp::McpServerHttp::new(name, "https://s.example.com/mcp").headers(vec![]),
            )
        };
        let message = policy_enable_error(&ms, &server("claimed"), claimed)
            .expect("pinned-off project-claimed upsert must refuse up front");
        assert!(
            message.contains("organization policy") && message.contains("requirements.toml"),
            "got: {message}"
        );
        assert_eq!(policy_enable_error(&ms, &server("fresh"), fresh), None);
    }

    fn gateway_tool(
        connector_id: &str,
        connector_name: &str,
        tool_id: &str,
        tool_name: &str,
        call_id: &str,
        description: &str,
    ) -> crate::session::managed_mcp::GatewayTool {
        crate::session::managed_mcp::GatewayTool {
            connector_id: connector_id.into(),
            connector_name: connector_name.into(),
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            description: description.into(),
            json_schema: serde_json::json!({"type": "object"}),
        }
    }

    /// **Pattern-regression test, not an end-to-end `handle_list` test.** `handle_list` takes an `&MvpAgent`, which has no lightweight test constructor.
    /// Spinning up a fake agent here would be a much larger refactor than this test warrants.
    /// Instead this test mirrors the production structure with stand-in futures and asserts the two latency invariants `handle_list` guarantees. If a future refactor of `handle_list` awaits the arms sequentially or runs the auth retry on cache=true, this test will *not* fail. It only guards the pattern; the real behavioural guard is reading the diff against the structure documented here.
    #[tokio::test(start_paused = true)]
    async fn handle_list_parallel_join_pattern_regression() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tokio::time::{Duration, Instant};

        async fn run(cache: bool) -> (Duration, bool, usize) {
            let auth_retried = Arc::new(AtomicBool::new(false));
            let max_concurrent = Arc::new(AtomicUsize::new(0));
            let in_flight = Arc::new(AtomicUsize::new(0));

            let bump = {
                let max_concurrent = Arc::clone(&max_concurrent);
                let in_flight = Arc::clone(&in_flight);
                move || {
                    let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(n, Ordering::SeqCst);
                }
            };
            let drop_ = {
                let in_flight = Arc::clone(&in_flight);
                move || {
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                }
            };

            // Stand-in for `agent.get_managed_mcp_gateway_tool_catalog()` (~1-2s proxy fetch).
            let managed_fut = {
                let bump = bump.clone();
                let drop_ = drop_.clone();
                async move {
                    bump();
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    drop_();
                }
            };

            // Stand-in for the session-state branch: conditional auth retry followed by `build_mcp_status`
            // Mirrors the closure in `handle_list`
            let session_fut = {
                let auth_retried = Arc::clone(&auth_retried);
                let bump = bump.clone();
                let drop_ = drop_.clone();
                async move {
                    bump();
                    if !cache {
                        auth_retried.store(true, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    // build_mcp_status is cheap (state-mutex inspect).
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    drop_();
                }
            };

            let start = Instant::now();
            tokio::join!(managed_fut, session_fut);
            (
                start.elapsed(),
                auth_retried.load(Ordering::SeqCst),
                max_concurrent.load(Ordering::SeqCst),
            )
        }

        // cache=true: no auth retry; the total is about the managed fetch alone
        let (cached_elapsed, cached_auth, cached_overlap) = run(true).await;
        assert!(!cached_auth, "auth retry must be skipped on cache=true");
        assert_eq!(cached_overlap, 2, "futures must run concurrently");
        assert!(
            cached_elapsed < Duration::from_millis(1600),
            "cached handle_list should finish in ~1.5s, got {:?}",
            cached_elapsed
        );

        // cache=false: the auth retry runs, but still concurrent with the managed fetch
        // The total is about max(1500, 500+50), roughly 1500ms, not 2050ms
        let (refresh_elapsed, refresh_auth, refresh_overlap) = run(false).await;
        assert!(refresh_auth, "auth retry must run on cache=false");
        assert_eq!(refresh_overlap, 2, "futures must run concurrently");
        assert!(
            refresh_elapsed < Duration::from_millis(1600),
            "refresh handle_list should still finish in ~1.5s (parallel), got {:?}",
            refresh_elapsed
        );
    }

    #[test]
    fn test_mcp_list_response_serialization() {
        let resp = McpListResponse {
            servers: vec![
                McpServerEntry {
                    name: "linear".to_string(),
                    icons: Vec::new(),
                    display_name: None,
                    source: McpServerSource::Local,
                    config: McpServerConfig::Http {
                        url: "https://mcp.linear.app".to_string(),
                        scope: Some("team".to_string()),
                        scope_id: Some("team-uuid-123".to_string()),
                        scope_name: Some("Grok CLI".to_string()),
                    },
                    source_label: None,
                    setup: None,
                    setup_values: None,
                    session: None,
                },
                McpServerEntry {
                    name: "filesystem".to_string(),
                    icons: Vec::new(),
                    display_name: None,
                    source: McpServerSource::Local,
                    source_label: None,
                    setup: None,
                    setup_values: None,
                    config: McpServerConfig::Stdio {
                        command: "/usr/bin/mcp-filesystem".into(),
                        args: vec!["--root".to_string(), "/home".to_string()],
                        env: vec![],
                    },
                    session: Some(McpServerSessionState {
                        enabled: true,
                        status: Some(McpSessionStatus::Ready),
                        auth_required: false,
                        setup_required: false,
                        tools: vec![McpToolEntry {
                            name: "read_file".to_string(),
                            icons: Vec::new(),
                            display_name: None,
                            description: Some("Read a file".to_string()),
                            meta: None,
                            enabled: true,
                        }],
                        blocked_reason: None,
                    }),
                },
            ],
            session_mcp_resolved: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        // [0] local HTTP
        assert_eq!(
            json.pointer("/servers/0/source"),
            Some(&serde_json::json!("local"))
        );
        assert_eq!(
            json.pointer("/servers/0/type"),
            Some(&serde_json::json!("http"))
        );
        assert_eq!(
            json.pointer("/servers/0/url"),
            Some(&serde_json::json!("https://mcp.linear.app"))
        );
        assert_eq!(
            json.pointer("/servers/0/scope"),
            Some(&serde_json::json!("team"))
        );
        assert_eq!(
            json.pointer("/servers/0/scopeId"),
            Some(&serde_json::json!("team-uuid-123"))
        );
        assert_eq!(
            json.pointer("/servers/0/scopeName"),
            Some(&serde_json::json!("Grok CLI"))
        );
        assert!(json.pointer("/servers/0/session").is_none());
        // Managed gateway connectors are not serialized as local transports.
        let gateway = serde_json::to_value(McpServerEntry {
            name: managed_gateway_entry_name("linear"),
            icons: Vec::new(),
            display_name: Some("linear".to_string()),
            source: McpServerSource::Managed,
            source_label: None,
            setup: None,
            setup_values: None,
            config: McpServerConfig::ManagedGateway,
            session: Some(McpServerSessionState {
                enabled: true,
                status: Some(McpSessionStatus::Ready),
                tools: vec![],
                auth_required: false,
                setup_required: false,
                blocked_reason: None,
            }),
        })
        .unwrap();
        assert_eq!(
            gateway.get("name"),
            Some(&serde_json::json!("managed_gateway:linear"))
        );
        assert_eq!(
            gateway.get("displayName"),
            Some(&serde_json::json!("linear"))
        );
        assert_eq!(
            gateway.get("type"),
            Some(&serde_json::json!("managedGateway"))
        );
        assert!(gateway.get("command").is_none());
        assert!(gateway.get("url").is_none());
        // [1] local Stdio
        assert_eq!(
            json.pointer("/servers/1/source"),
            Some(&serde_json::json!("local"))
        );
        assert_eq!(
            json.pointer("/servers/1/type"),
            Some(&serde_json::json!("stdio"))
        );
        assert_eq!(
            json.pointer("/servers/1/command"),
            Some(&serde_json::json!("/usr/bin/mcp-filesystem"))
        );
        assert_eq!(
            json.pointer("/servers/1/args"),
            Some(&serde_json::json!(["--root", "/home"]))
        );
        assert!(json.pointer("/servers/1/url").is_none());
        assert_eq!(
            json.pointer("/servers/1/session/enabled"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            json.pointer("/servers/1/session/status"),
            Some(&serde_json::json!("ready"))
        );
        assert_eq!(
            json.pointer("/servers/1/session/tools/0/name"),
            Some(&serde_json::json!("read_file"))
        );
    }

    fn reauth_connector(
        id: &str,
        name: &str,
    ) -> crate::session::managed_mcp::GatewayReauthConnector {
        crate::session::managed_mcp::GatewayReauthConnector {
            connector_uuid: format!("connector_{id}"),
            connector_id: id.to_owned(),
            connector_name: name.to_owned(),
        }
    }

    #[test]
    fn test_mcp_list_icons_serialization() {
        let entry = McpServerEntry {
            name: "custom".to_string(),
            display_name: Some("Custom".to_string()),
            icons: vec![distill_mcp::servers::McpIcon {
                src: "https://example.com/icon.png".to_string(),
                mime_type: Some("image/png".to_string()),
                sizes: Some(vec!["48x48".to_string()]),
                theme: Some(distill_mcp::servers::McpIconTheme::Dark),
            }],
            source: McpServerSource::Local,
            source_label: None,
            setup: None,
            setup_values: None,
            config: McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                scope: None,
                scope_id: None,
                scope_name: None,
            },
            session: Some(McpServerSessionState {
                enabled: true,
                status: Some(McpSessionStatus::Ready),
                tools: vec![McpToolEntry {
                    name: "ping".to_string(),
                    display_name: None,
                    description: None,
                    meta: None,
                    icons: vec![distill_mcp::servers::McpIcon {
                        src: "data:image/png;base64,aaa".to_string(),
                        mime_type: None,
                        sizes: None,
                        theme: None,
                    }],
                    enabled: true,
                }],
                auth_required: false,
                setup_required: false,
                blocked_reason: None,
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            json.pointer("/icons/0/src"),
            Some(&serde_json::json!("https://example.com/icon.png"))
        );
        assert_eq!(
            json.pointer("/icons/0/mimeType"),
            Some(&serde_json::json!("image/png"))
        );
        assert_eq!(
            json.pointer("/icons/0/sizes/0"),
            Some(&serde_json::json!("48x48"))
        );
        assert_eq!(
            json.pointer("/icons/0/theme"),
            Some(&serde_json::json!("dark"))
        );
        assert_eq!(
            json.pointer("/session/tools/0/icons/0/src"),
            Some(&serde_json::json!("data:image/png;base64,aaa"))
        );
    }

    #[test]
    fn gateway_catalog_groups_by_connector_name_and_exact_tool_names() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![
                gateway_tool(
                    "linear",
                    "Linear",
                    "list_issues",
                    "List issues",
                    "linear.list_issues",
                    "List Linear issues",
                ),
                gateway_tool(
                    "linear",
                    "Linear",
                    "create_issue",
                    "Create issue",
                    "linear.create_issue",
                    "Create a Linear issue",
                ),
                gateway_tool(
                    "slack",
                    "Slack",
                    "search",
                    "Search",
                    "slack.search",
                    "Search Slack",
                ),
            ],
            total_tools: 3,
            connectors_needing_reauth: vec!["Slack".into()],
            reauth_connectors: vec![reauth_connector("slack", "Slack")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());

        assert_eq!(servers.len(), 2);
        let [linear, slack] = servers.as_slice() else {
            panic!("expected two servers: {servers:?}");
        };
        assert_eq!(linear.name, "managed_gateway:linear");
        assert_eq!(linear.display_name.as_deref(), Some("Linear"));
        assert_eq!(linear.source, McpServerSource::Managed);
        assert!(matches!(linear.config, McpServerConfig::ManagedGateway));
        let linear_session = linear.session.as_ref().unwrap();
        assert_eq!(linear_session.status, Some(McpSessionStatus::Ready));
        assert!(!linear_session.auth_required);
        let linear_names: Vec<&str> = linear_session
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(
            linear_names,
            vec!["linear__list_issues", "linear__create_issue"]
        );

        assert_eq!(slack.name, "managed_gateway:slack");
        assert_eq!(slack.display_name.as_deref(), Some("Slack"));
        let slack_session = slack.session.as_ref().unwrap();
        assert!(slack_session.auth_required);
        assert!(slack_session.status.is_none());
        let Some(tool) = slack_session.tools.first() else {
            panic!("expected a slack tool: {:?}", slack_session.tools);
        };
        assert_eq!(tool.name, "slack__search");
        assert_eq!(tool.display_name.as_deref(), Some("Search"));
    }

    #[test]
    fn gateway_catalog_reauth_id_matches_connector_tools() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![gateway_tool(
                "google_drive",
                "Google Drive",
                "search",
                "Search",
                "google_drive_search",
                "Search Drive",
            )],
            total_tools: 1,
            connectors_needing_reauth: vec!["Google Drive".into()],
            reauth_connectors: vec![reauth_connector("google_drive", "Google Drive")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        assert_eq!(servers.len(), 1);
        let Some(server) = servers.first() else {
            panic!("expected one server: {servers:?}");
        };
        let session = server.session.as_ref().unwrap();
        assert_eq!(server.name, "managed_gateway:google_drive");
        assert!(session.auth_required);
        assert!(session.status.is_none());
        assert_eq!(session.tools.len(), 1);
    }

    #[test]
    fn gateway_catalog_legacy_name_list_matches_case_insensitively_when_no_ids() {
        // Older proxies send only display names. Matching must survive a case mismatch against
        // both the connector id and the display name, and must not synthesize a row without an id.
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![
                gateway_tool(
                    "google_drive",
                    "Google Drive",
                    "search",
                    "Search",
                    "google_drive_search",
                    "Search Drive",
                ),
                gateway_tool(
                    "slack",
                    "Slack",
                    "post",
                    "Post",
                    "slack_post",
                    "Post a message",
                ),
            ],
            total_tools: 2,
            connectors_needing_reauth: vec!["GOOGLE drive".into(), "SLACK".into(), "GitHub".into()],
            reauth_connectors: vec![],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        let by_name: HashMap<&str, bool> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.session.as_ref().unwrap().auth_required))
            .collect();
        assert_eq!(
            by_name,
            HashMap::from([
                ("managed_gateway:google_drive", true),
                ("managed_gateway:slack", true),
            ]),
            "name-only reauth entries flag their tool rows; GitHub has no id and no row"
        );
    }

    #[test]
    fn gateway_catalog_ignores_legacy_names_once_any_structured_id_is_present() {
        // Two Google Drive accounts share a display name. The structured id flags one; the name
        // list must not flag the other, even though it names "Google Drive".
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![
                gateway_tool(
                    "google_drive_work",
                    "Google Drive",
                    "search",
                    "Search",
                    "gdw_search",
                    "Search",
                ),
                gateway_tool(
                    "google_drive_home",
                    "Google Drive",
                    "search",
                    "Search",
                    "gdh_search",
                    "Search",
                ),
            ],
            total_tools: 2,
            connectors_needing_reauth: vec!["Google Drive".into()],
            reauth_connectors: vec![reauth_connector("google_drive_work", "Google Drive")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        let flagged: HashMap<&str, bool> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.session.as_ref().unwrap().auth_required))
            .collect();
        assert_eq!(
            flagged,
            HashMap::from([
                ("managed_gateway:google_drive_work", true),
                ("managed_gateway:google_drive_home", false),
            ])
        );
    }

    #[test]
    fn gateway_catalog_synthesizes_reauth_only_connector_without_tools() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![gateway_tool(
                "excalidraw",
                "Excalidraw",
                "read_me",
                "Read Me",
                "excalidraw___read_me",
                "Read me",
            )],
            total_tools: 1,
            connectors_needing_reauth: vec!["GitHub".into()],
            reauth_connectors: vec![reauth_connector("github", "GitHub")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        assert_eq!(servers.len(), 2);
        let github = servers
            .iter()
            .find(|s| s.display_name.as_deref() == Some("GitHub"))
            .expect("reauth-only GitHub row");
        assert_eq!(github.name, "managed_gateway:github");
        assert!(matches!(github.config, McpServerConfig::ManagedGateway));
        let session = github.session.as_ref().unwrap();
        assert!(session.auth_required);
        assert!(session.status.is_none());
        assert!(session.enabled);
        assert!(session.tools.is_empty());
        let excalidraw = servers
            .iter()
            .find(|s| s.name == "managed_gateway:excalidraw")
            .unwrap();
        assert!(!excalidraw.session.as_ref().unwrap().auth_required);
    }

    #[test]
    fn gateway_catalog_dedups_reauth_connectors_with_same_id() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![],
            total_tools: 0,
            connectors_needing_reauth: vec!["GitHub".into(), "GitHub".into()],
            reauth_connectors: vec![
                reauth_connector("github", "GitHub"),
                reauth_connector("github", "GITHUB"),
            ],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        assert_eq!(servers.len(), 1);
        let Some(server) = servers.first() else {
            panic!("expected one server: {servers:?}");
        };
        assert_eq!(server.name, "managed_gateway:github");
        assert_eq!(server.display_name.as_deref(), Some("GitHub"));
        assert!(server.session.as_ref().unwrap().auth_required);
    }

    #[test]
    fn gateway_catalog_skips_empty_reauth_connector_id() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![],
            total_tools: 0,
            connectors_needing_reauth: vec![],
            reauth_connectors: vec![reauth_connector("", "blank")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        assert!(servers.is_empty());
    }

    #[test]
    fn gateway_catalog_rows_without_name_display_their_id() {
        // Both the tool-backed and the reauth-only paths build rows through the shared constructor.
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![gateway_tool(
                "gmail",
                "",
                "search",
                "Search",
                "gmail_search",
                "Search mail",
            )],
            total_tools: 1,
            connectors_needing_reauth: vec![],
            reauth_connectors: vec![reauth_connector("github", "")],
        };
        let servers =
            build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &Default::default());
        let labels: Vec<(&str, Option<&str>)> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.display_name.as_deref()))
            .collect();
        assert_eq!(
            labels,
            [
                ("managed_gateway:gmail", Some("gmail")),
                ("managed_gateway:github", Some("github")),
            ]
        );
    }

    #[test]
    fn gateway_catalog_preserves_local_name_collision() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![gateway_tool(
                "linear",
                "Linear",
                "list_issues",
                "List issues",
                "linear.list_issues",
                "List Linear issues",
            )],
            total_tools: 1,
            connectors_needing_reauth: vec![],
            reauth_connectors: vec![],
        };
        let local = acp::McpServer::Stdio(
            acp::McpServerStdio::new("linear", "/usr/bin/local-linear")
                .args(vec![])
                .env(vec![]),
        );

        let servers =
            build_mcp_catalog_with_gateway_tools(&[local], Some(&catalog), &Default::default());

        assert_eq!(servers.len(), 2);
        let [managed, local] = servers.as_slice() else {
            panic!("expected two servers: {servers:?}");
        };
        assert_eq!(managed.name, "managed_gateway:linear");
        assert_eq!(managed.display_name.as_deref(), Some("Linear"));
        assert_eq!(managed.source, McpServerSource::Managed);
        assert_eq!(local.name, "linear");
        assert_eq!(local.display_name, None);
        assert_eq!(local.source, McpServerSource::Local);
        assert!(matches!(local.config, McpServerConfig::Stdio { .. }));
    }

    #[test]
    fn gateway_toggle_classification_requires_managed_gateway_entry_id() {
        assert_eq!(
            managed_gateway_connector_id("managed_gateway:linear"),
            Some("linear")
        );
        assert_eq!(managed_gateway_connector_id("linear"), None);
    }

    #[test]
    fn disabled_local_rows_keep_non_gateway_placeholder_config() {
        let entry = disabled_server_placeholder_entry("local_slack");
        assert_eq!(entry.source, McpServerSource::Local);
        assert!(matches!(entry.config, McpServerConfig::Stdio { .. }));
    }

    #[test]
    fn grok_com_local_name_is_not_managed_in_catalog() {
        let local = acp::McpServer::Http(
            acp::McpServerHttp::new("grok_com_slack", "https://mcp.example.test/sse")
                .headers(vec![]),
        );
        let servers = build_mcp_catalog_with_gateway_tools(&[local], None, &Default::default());
        assert_eq!(servers.len(), 1);
        let Some(server) = servers.first() else {
            panic!("expected one server: {servers:?}");
        };
        assert_eq!(server.name, "grok_com_slack");
        assert_eq!(server.source, McpServerSource::Local);
        assert!(matches!(server.config, McpServerConfig::Http { .. }));

        let placeholder = disabled_server_placeholder_entry("grok_com_slack");
        assert_eq!(placeholder.source, McpServerSource::Local);
        assert!(matches!(placeholder.config, McpServerConfig::Stdio { .. }));
    }

    #[test]
    fn gateway_catalog_honors_disabled_connectors_and_tools() {
        let catalog = crate::session::managed_mcp::GatewayToolCatalog {
            tools: vec![
                gateway_tool(
                    "linear",
                    "Linear",
                    "list_issues",
                    "List issues",
                    "linear.list_issues",
                    "List Linear issues",
                ),
                gateway_tool(
                    "linear",
                    "Linear",
                    "create_issue",
                    "Create issue",
                    "linear.create_issue",
                    "Create a Linear issue",
                ),
            ],
            total_tools: 2,
            connectors_needing_reauth: vec![],
            reauth_connectors: vec![],
        };
        let disabled: HashMap<String, HashSet<String>> = HashMap::from([
            (
                crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string(),
                HashSet::from(["linear".to_string()]),
            ),
            (
                "linear".to_string(),
                HashSet::from(["linear__create_issue".to_string()]),
            ),
        ]);
        let servers = build_mcp_catalog_with_gateway_tools(&[], Some(&catalog), &disabled);
        let Some(server) = servers.first() else {
            panic!("expected one server: {servers:?}");
        };
        let session = server.session.as_ref().unwrap();
        assert!(!session.enabled);
        assert!(session.status.is_none());
        let [t0, t1] = session.tools.as_slice() else {
            panic!("expected two tools: {:?}", session.tools);
        };
        assert!(t0.enabled);
        assert!(!t1.enabled);
    }

    #[test]
    fn test_mcp_call_response_serialization() {
        let resp = McpCallResponse {
            content: vec![McpContentBlock {
                kind: "text".to_string(),
                text: "Created issue LIN-123".to_string(),
            }],
            is_error: Some(false),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json.pointer("/content/0/type"),
            Some(&serde_json::json!("text"))
        );
        assert_eq!(
            json.pointer("/content/0/text"),
            Some(&serde_json::json!("Created issue LIN-123"))
        );
        assert_eq!(json.get("isError"), Some(&serde_json::json!(false)));
    }

    #[test]
    fn test_mcp_list_setup_required_serialization() {
        let entry = McpServerEntry {
            name: "acme".to_string(),
            icons: Vec::new(),
            display_name: None,
            source: McpServerSource::Local,
            source_label: Some("plugin: acme".to_string()),
            setup: Some(crate::util::config::McpSetupConfig {
                fields: vec![crate::util::config::McpSetupField {
                    id: "site".to_string(),
                    label: "Site".to_string(),
                    field_type: crate::util::config::McpSetupFieldType::Select,
                    required: true,
                    default: Some("us1".to_string()),
                    options: vec![crate::util::config::McpSetupOption {
                        label: "US5".to_string(),
                        value: "us5".to_string(),
                    }],
                }],
                variables: HashMap::new(),
            }),
            setup_values: Some(HashMap::from([("site".to_string(), "us5".to_string())])),
            config: McpServerConfig::Http {
                url: String::new(),
                scope: None,
                scope_id: None,
                scope_name: None,
            },
            session: Some(McpServerSessionState {
                enabled: true,
                status: Some(McpSessionStatus::SetupRequired),
                tools: vec![],
                auth_required: false,
                setup_required: true,
                blocked_reason: None,
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            json.pointer("/session/status"),
            Some(&serde_json::json!("setuprequired"))
        );
        assert_eq!(
            json.pointer("/session/setupRequired"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            json.pointer("/setup/fields/0/id"),
            Some(&serde_json::json!("site"))
        );
        assert_eq!(
            json.pointer("/setupValues/site"),
            Some(&serde_json::json!("us5"))
        );
    }

    #[test]
    fn test_mcp_auth_trigger_response_success_no_error_field() {
        let resp = McpAuthTriggerResponse {
            status: "authenticated",
            setup: None,
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json.get("status"),
            Some(&serde_json::json!("authenticated"))
        );
        assert!(
            json.get("error").is_none(),
            "error field must be omitted on success: {json}"
        );
    }

    #[test]
    fn test_mcp_auth_trigger_response_failure_carries_error() {
        let resp = McpAuthTriggerResponse {
            status: "failed",
            setup: None,
            error: Some("MCP server 'linear' does not use OAuth".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json.get("status"), Some(&serde_json::json!("failed")));
        assert_eq!(
            json.get("error"),
            Some(&serde_json::json!("MCP server 'linear' does not use OAuth")),
            "failure must carry the descriptive error verbatim: {json}"
        );
    }

    #[test]
    fn test_mcp_auth_trigger_response_failure_omits_error_when_none() {
        let resp = McpAuthTriggerResponse {
            status: "failed",
            setup: None,
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json.get("status"), Some(&serde_json::json!("failed")));
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_disabled_session_state_serialization() {
        let entry = McpServerEntry {
            name: "slack".to_string(),
            icons: Vec::new(),
            display_name: None,
            source: McpServerSource::Local,
            source_label: None,
            setup: None,
            setup_values: None,
            config: McpServerConfig::Http {
                url: "https://mcp.slack.com".to_string(),
                scope: Some("user".to_string()),
                scope_id: Some("user-uuid-456".to_string()),
                scope_name: None,
            },
            session: Some(McpServerSessionState {
                enabled: false,
                status: None,
                tools: vec![],
                auth_required: false,
                setup_required: false,
                blocked_reason: None,
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json.get("type"), Some(&serde_json::json!("http")));
        assert_eq!(json.get("scope"), Some(&serde_json::json!("user")));
        assert_eq!(
            json.get("scopeId"),
            Some(&serde_json::json!("user-uuid-456"))
        );
        assert_eq!(
            json.pointer("/session/enabled"),
            Some(&serde_json::json!(false))
        );
        assert!(json.get("session").and_then(|s| s.get("status")).is_none());
        assert!(json.get("session").and_then(|s| s.get("tools")).is_none());
    }
}
