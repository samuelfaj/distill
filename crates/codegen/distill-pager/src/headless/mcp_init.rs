// Modified for Distill by Samuel Fajreldines, 2026.
//! One bounded `x.ai/mcp/list` snapshot for Messages `init.mcp_servers`.
//! The shell's Blocking startup grace runs later on the prompt; this must not add a second wait.

use std::path::Path;
use std::time::Duration;

use agent_client_protocol as acp;
use distill_acp_lib::{AcpAgentTx, acp_send};
use distill_shell::util::config as cli_config;

use crate::headless::reducer::McpServer;
use crate::views::mcps_modal::{
    McpServerDisplayStatus, McpServerInfo, McpsListResponse, convert_list_response,
};

/// Bound on the single `x.ai/mcp/list` RPC so a hung handler cannot stall `init`.
const MCP_LIST_RPC_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) async fn resolve_mcp_servers_for_init(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    cwd: &Path,
) -> Vec<McpServer> {
    let deadline = tokio::time::Instant::now() + MCP_LIST_RPC_TIMEOUT;
    resolve_mcp_servers_until(acp_tx, session_id, cwd, deadline).await
}

pub(crate) async fn resolve_mcp_servers_until(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    cwd: &Path,
    deadline: tokio::time::Instant,
) -> Vec<McpServer> {
    match fetch_mcp_list_bounded(acp_tx, session_id, deadline).await {
        Some(list) => servers_from_list(list),
        None => pending_from_config(cwd),
    }
}

fn servers_from_list(list: McpsListResponse) -> Vec<McpServer> {
    let is_resolved = list.session_mcp_resolved == Some(true);
    convert_list_response(list)
        .into_iter()
        .map(|info| McpServer {
            status: init_wire_status(&info, is_resolved).to_string(),
            name: info.name,
        })
        .collect()
}

/// Messages tokens: Ready is `connected`; NeedsAuth and SetupRequired share `needs-auth`.
/// `disabled` requires `sessionMcpResolved`: list `enabled` is claim-set membership and is
/// empty before the init pass, so an unresolved Unavailable+!enabled row is still `pending`.
fn init_wire_status(info: &McpServerInfo, is_resolved: bool) -> &'static str {
    match info.status {
        McpServerDisplayStatus::Ready => "connected",
        McpServerDisplayStatus::NeedsAuth | McpServerDisplayStatus::SetupRequired => "needs-auth",
        McpServerDisplayStatus::Initializing => "pending",
        McpServerDisplayStatus::BlockedByPolicy => "failed",
        McpServerDisplayStatus::Unavailable if is_resolved && !info.enabled => "disabled",
        McpServerDisplayStatus::Unavailable if is_resolved => "failed",
        McpServerDisplayStatus::Unavailable => "pending",
    }
}

fn pending_from_config(cwd: &Path) -> Vec<McpServer> {
    cli_config::load_mcp_servers(cwd, &distill_tools::types::compat::CompatConfig::default())
        .iter()
        .filter_map(|server| {
            let name = match server {
                acp::McpServer::Http(http) => http.name.clone(),
                acp::McpServer::Sse(sse) => sse.name.clone(),
                acp::McpServer::Stdio(stdio) => stdio.name.clone(),
                _ => return None,
            };
            Some(McpServer {
                name,
                status: "pending".to_string(),
            })
        })
        .collect()
}

async fn fetch_mcp_list_bounded(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    deadline: tokio::time::Instant,
) -> Option<McpsListResponse> {
    match tokio::time::timeout_at(deadline, fetch_mcp_list(acp_tx, session_id)).await {
        Ok(list) => list,
        Err(_) => {
            tracing::warn!("headless: mcp/list exceeded the init-status deadline");
            None
        }
    }
}

async fn fetch_mcp_list(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
) -> Option<McpsListResponse> {
    let params = serde_json::value::to_raw_value(&serde_json::json!({
        "sessionId": session_id.0.as_ref(),
        "cache": true,
    }))
    .ok()?;
    let req = acp::ExtRequest::new("x.ai/mcp/list", params.into());
    match acp_send(req, acp_tx).await {
        Ok(resp) => decode_mcp_list(resp.0.get()),
        Err(e) => {
            tracing::warn!(error = %e, "headless: mcp/list failed");
            None
        }
    }
}

fn decode_mcp_list(raw: &str) -> Option<McpsListResponse> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let payload = value.get("result").unwrap_or(&value);
    match serde_json::from_value(payload.clone()) {
        Ok(list) => Some(list),
        Err(e) => {
            tracing::warn!(error = %e, "headless: mcp/list payload did not match McpsListResponse");
            None
        }
    }
}

#[cfg(test)]
#[path = "mcp_init_tests.rs"]
mod tests;
