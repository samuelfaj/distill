// Modified for Distill by Samuel Fajreldines, 2026.
//! In-process SDK MCP servers over the ACP reverse channel (`x.ai/mcp/sdk_call`).
//!
//! The official `grok-agent-sdk` lets a host define in-process tools (`@tool` / `create_sdk_mcp_server`).
//! When `transport="acp"`, the SDK registers them in `session/new` `_meta["x.ai/mcp/servers"] = [{ "name", "serverId" }]`.
//! The agent invokes their tools by sending each MCP JSON-RPC message back to the client as a reverse `x.ai/mcp/sdk_call` request.
//! [`GatewayAcpInvoker`] handles those requests here.
//!
//! The reverse route (agent to client, `x.ai/mcp/sdk_call`) invokes a tool that lives in the SDK's process, with no extra IPC.
//! It mirrors the forward route (client to agent, `x.ai/mcp/call` in `extensions::mcp`), which invokes a tool on a server the agent is connected to.
//! The two routes use distinct method strings and sit on opposite request handlers, so they never collide.

use std::time::Duration;

use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender;
use distill_mcp::acp_transport::AcpReverseInvoker;
use distill_mcp::servers::AcpServerEntry;
use distill_mcp::wire;

/// Each entry deserializes directly into the canonical type, so serde checks the `serverId` wire field rather than hand-reading it.
/// Entries missing `name`/`serverId` are skipped with a warning.
/// A name seen twice keeps the first entry: server names are the tool namespace, so a duplicate would otherwise silently shadow the first.
pub(crate) fn parse_acp_mcp_servers(meta: Option<&acp::Meta>) -> Vec<AcpServerEntry> {
    let Some(array) = meta
        .and_then(|m| m.get(wire::MCP_SERVERS))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    for entry in array {
        let server: AcpServerEntry = match serde_json::from_value(entry.clone()) {
            Ok(server) => server,
            Err(err) => {
                tracing::warn!(entry = %entry, %err, "ignoring malformed x.ai/mcp/servers entry");
                continue;
            }
        };
        if !seen.insert(server.name.clone()) {
            tracing::warn!(name = %server.name, "ignoring duplicate x.ai/mcp/servers entry");
            continue;
        }
        servers.push(server);
    }
    servers
}

/// Sends each SDK MCP `invoke` as one `x.ai/mcp/sdk_call` reverse request through the gateway.
/// `AcpAgentGatewaySender::send` is `Send`, unlike `acp::Client::ext_method`, so the rmcp invoker bound is met with no relay task.
/// Calls may run concurrently; the gateway serializes them onto the session channel.
pub(crate) struct GatewayAcpInvoker {
    gateway: AcpAgentGatewaySender,
}

impl GatewayAcpInvoker {
    pub(crate) fn new(gateway: AcpAgentGatewaySender) -> Self {
        Self { gateway }
    }
}

/// Reverse `x.ai/mcp/sdk_call` params.
/// Declares the on-wire field names once (mirroring the forward side's typed `McpCallRequest`) so `serverId` is never hand-spelled.
#[derive(serde::Serialize)]
struct SdkCallParams<'a> {
    #[serde(rename = "serverId")]
    server_id: &'a str,
    message: serde_json::Value,
}

#[async_trait::async_trait]
impl AcpReverseInvoker for GatewayAcpInvoker {
    async fn invoke(
        &self,
        server_id: &str,
        message: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let params = serde_json::value::to_raw_value(&SdkCallParams { server_id, message })
            .map_err(|err| err.to_string())?;
        let request = acp::ExtRequest::new(wire::MCP_SDK_CALL, params.into());
        // Bound the round trip: a missing or hung client fails this reverse call at the per-server tool timeout instead of stalling the tool loop
        let response = tokio::time::timeout(timeout, self.gateway.send(request))
            .await
            .map_err(|_| {
                format!(
                    "{} to server {server_id} timed out after {}ms",
                    wire::MCP_SDK_CALL,
                    timeout.as_millis()
                )
            })?
            .map_err(|err| err.to_string())?;
        serde_json::from_str(response.0.get()).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_entries_and_skips_malformed() {
        let meta = serde_json::json!({
            "x.ai/mcp/servers": [
                { "name": "harness-tools", "serverId": "srv_0" },
                { "name": "missing-id" },
                { "serverId": "no_name" },
            ]
        });
        let servers = parse_acp_mcp_servers(meta.as_object());
        let [server] = servers.as_slice() else {
            panic!("expected one MCP server: {servers:?}");
        };
        assert_eq!(server.name, "harness-tools");
        assert_eq!(server.server_id, "srv_0");
    }

    #[test]
    fn duplicate_names_keep_the_first() {
        let meta = serde_json::json!({
            "x.ai/mcp/servers": [
                { "name": "tools", "serverId": "srv_0" },
                { "name": "tools", "serverId": "srv_1" },
            ]
        });
        let servers = parse_acp_mcp_servers(meta.as_object());
        let [server] = servers.as_slice() else {
            panic!("expected one MCP server: {servers:?}");
        };
        assert_eq!(server.server_id, "srv_0");
    }

    #[test]
    fn absent_meta_yields_none() {
        assert!(parse_acp_mcp_servers(None).is_empty());
        assert!(parse_acp_mcp_servers(serde_json::json!({}).as_object()).is_empty());
    }
}
