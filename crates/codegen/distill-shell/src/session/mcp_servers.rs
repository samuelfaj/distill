// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP server re-exports and shell-side wrappers for timeout override resolution.

pub use distill_mcp::servers::{
    AcpServerEntry, Generation, HttpConfig, InitClaimGuard, MCP_TOOL_NAME_DELIMITER, McpClient,
    McpClientTimeoutOverrides, McpConfigChange, McpConfigDiff, McpError, McpInitStrategy,
    McpMetaConfigMap, McpOauthDiscovery, McpServerMetaConfig, McpServerName, McpService,
    McpSpawnCtx, McpState, McpTool, McpToolRegistration, OauthInteractivity, Replacement,
    SharedMcpPool, SharedMcpState, Superseded, mcp_server_name, mcp_target_str, mcp_transport_str,
    parse_mcp_meta_config, parse_mcp_tool_name, qualify_mcp_tool_name, sanitize_descriptor_segment,
    validate_tool_name,
};

use std::collections::HashMap;
use std::path::Path;

use agent_client_protocol as acp;
use distill_mcp::oauth_config::{McpOAuthConfig, McpOAuthConfigMap};
use distill_mcp::servers as inner;

fn resolve_overrides(
    server_name: &str,
    cwd: Option<&Path>,
) -> Option<inner::McpClientTimeoutOverrides> {
    let config = match cwd {
        Some(cwd) => crate::util::config::get_mcp_server_config_with_project(server_name, cwd),
        None => crate::util::config::get_mcp_server_config(server_name),
    };
    // Fall back to the globally-resolved startup timeout
    // Servers without a per-server `startup_timeout_sec` (e.g. `~/.claude.json` imports) still get it.
    let global_startup = crate::util::config::resolved_mcp_startup_timeout_secs();
    Some(inner::McpClientTimeoutOverrides {
        startup_timeout_sec: config
            .as_ref()
            .and_then(|c| c.startup_timeout_sec)
            .or(Some(global_startup)),
        tool_timeout_sec: config.as_ref().and_then(|c| c.tool_timeout_sec),
        tool_timeouts: config.as_ref().and_then(|c| c.tool_timeouts.clone()),
        expose_image_base64: config.as_ref().and_then(|c| c.expose_image_base64),
    })
}

pub(crate) fn build_config_resolved_event(
    configs: &[acp::McpServer],
    cwd: &Path,
) -> distill_session_events::Event {
    let disabled: Vec<String> = crate::util::config::disabled_mcp_server_names(cwd)
        .into_iter()
        .collect();
    let servers = configs
        .iter()
        .map(|c| distill_session_events::McpConfigServer {
            name: inner::mcp_server_name(c).to_string(),
            transport: inner::mcp_transport_str(c).to_string(),
            source:
                match crate::session::mcp_dispatcher::classify_source(inner::mcp_server_name(c)) {
                    crate::extensions::mcp::McpServerSource::Managed => "managed",
                    crate::extensions::mcp::McpServerSource::Local => "local",
                }
                .to_string(),
        })
        .collect();
    distill_session_events::Event::McpConfigResolved { servers, disabled }
}

pub(crate) async fn start_mcp_server(
    mcp_server: acp::McpServer,
    cwd: Option<&Path>,
    meta_config: Option<&inner::McpServerMetaConfig>,
    byo_config: Option<&McpOAuthConfig>,
    ctx: &inner::McpSpawnCtx<'_>,
) -> Result<inner::McpClient, inner::McpError> {
    let overrides = resolve_overrides(inner::mcp_server_name(&mcp_server), cwd);
    inner::start_mcp_server(mcp_server, overrides.as_ref(), meta_config, byo_config, ctx).await
}

/// Config-declared servers (HTTP/stdio) are spawned lock-free via [`start_mcp_servers`].
/// SDK in-process servers are built under a brief lock via `McpState::build_pending_acp_clients`.
/// SDK clients never fail to build, so they enter as `Ok`.
pub(crate) async fn build_pending_clients(
    mcp_state: &tokio::sync::Mutex<inner::McpState>,
    configs_to_start: Vec<acp::McpServer>,
    cwd: Option<&Path>,
    meta_config_map: &inner::McpMetaConfigMap,
    oauth_config_map: &McpOAuthConfigMap,
    ctx: &inner::McpSpawnCtx<'_>,
) -> Vec<Result<inner::McpClient, inner::McpError>> {
    let mut results = start_mcp_servers(
        configs_to_start,
        cwd,
        meta_config_map,
        oauth_config_map,
        ctx,
    )
    .await;
    // Re-resolve SDK (ACP) config.toml overrides for THIS init, matching HTTP/stdio, so a mid-session config change applies on the next init
    // The overrides are resolved outside the lock (resolution reads config.toml), then handed to the pure, under-lock builder
    let acp_overrides: HashMap<String, inner::McpClientTimeoutOverrides> = {
        let names = mcp_state.lock().await.pending_acp_server_names();
        names
            .iter()
            .filter_map(|name| resolve_overrides(name, cwd).map(|o| (name.clone(), o)))
            .collect()
    };
    // Brief lock, no `.await` held: the SDK clients are built synchronously (pure).
    let acp_clients = mcp_state
        .lock()
        .await
        .build_pending_acp_clients(&acp_overrides);
    results.extend(acp_clients.into_iter().map(Ok));
    results
}

pub(crate) async fn start_mcp_servers(
    mcp_servers: Vec<acp::McpServer>,
    cwd: Option<&Path>,
    meta_config_map: &inner::McpMetaConfigMap,
    oauth_config_map: &McpOAuthConfigMap,
    ctx: &inner::McpSpawnCtx<'_>,
) -> Vec<Result<inner::McpClient, inner::McpError>> {
    let overrides_map: HashMap<String, inner::McpClientTimeoutOverrides> = mcp_servers
        .iter()
        .filter_map(|s| {
            let name = inner::mcp_server_name(s);
            resolve_overrides(name, cwd).map(|o| (name.to_string(), o))
        })
        .collect();
    inner::start_mcp_servers(
        mcp_servers,
        &overrides_map,
        meta_config_map,
        oauth_config_map,
        ctx,
    )
    .await
}
