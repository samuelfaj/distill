// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP integration for the workspace server.
//!
//! Bridges [`McpClient`] to the server's [`McpTransport`] trait.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use distill_computer_hub_mcp_adapter::{
    McpBridge, McpBridgeConfig, McpBridgeHandle, McpCallResult, McpContent, McpServerInfo,
    McpToolDefinition, McpTransport,
};
use distill_computer_hub_sdk::ToolServerHandler;
use distill_mcp::rmcp;
use distill_mcp::servers::{
    InitClaimGuard, MCP_TOOL_NAME_DELIMITER, McpClient, McpClientTimeoutOverrides, McpSpawnCtx,
    OauthInteractivity, SharedMcpState, parse_mcp_qualified_name,
};
use distill_tools::util::mcp_structured_content::render_structured_content;
use distill_tool_protocol::{SessionId, ToolId};
use distill_tool_runtime::{ToolCallContext, ToolStream, TypedToolOutput};
use distill_tool_types::ToolDescription;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::mcp_claim::{ClaimOffer, McpServerTier, plan_claims};
use crate::session::{SessionMcpServer, WorkspaceMcpBinding, WorkspaceSession};

/// The slice of the hub's per-session tool surface that MCP reconfiguration drives: what a session currently advertises, plus dynamic registration in both directions.
/// The seam exists so the reload tests can substitute an in-memory hub — a real `ToolServer` cannot be built without a live hub connection.
pub(crate) trait HubToolRegistry: Send + Sync {
    /// `life` is the session's MCP life epoch. The hub ledger is life-tagged, so a stale unregister cannot remove a newer life's registration.
    /// An unregister never touches a non-dynamic handler, including a resolver-installed native sharing the id.
    fn register_tool_dynamic(
        &self,
        handler: Arc<dyn ToolServerHandler>,
        sessions: Vec<SessionId>,
        life: u64,
    ) -> impl Future<Output = Result<(), distill_computer_hub_sdk::ClientError>> + Send;

    fn unregister_tool_dynamic(
        &self,
        tool_id: &ToolId,
        session_id: &SessionId,
        life: u64,
    ) -> impl Future<Output = Result<bool, distill_computer_hub_sdk::ClientError>> + Send;
}

impl HubToolRegistry for distill_computer_hub_sdk::ToolServer {
    async fn register_tool_dynamic(
        &self,
        handler: Arc<dyn ToolServerHandler>,
        sessions: Vec<SessionId>,
        life: u64,
    ) -> Result<(), distill_computer_hub_sdk::ClientError> {
        Self::register_tool_dynamic(self, handler, sessions, life).await
    }

    async fn unregister_tool_dynamic(
        &self,
        tool_id: &ToolId,
        session_id: &SessionId,
        life: u64,
    ) -> Result<bool, distill_computer_hub_sdk::ClientError> {
        Self::unregister_tool_dynamic(self, tool_id, session_id, life).await
    }
}

/// Adapts [`McpClient`] to the [`McpTransport`] trait for [`McpBridge`].
pub(crate) struct McpClientTransportAdapter {
    client: Arc<McpClient>,
}

impl McpClientTransportAdapter {
    pub fn new(client: Arc<McpClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl McpTransport for McpClientTransportAdapter {
    async fn initialize(&self) -> Result<McpServerInfo, distill_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| distill_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        let info = service.peer_info().ok_or_else(|| {
            distill_computer_hub_mcp_adapter::McpError::Transport("no peer info after init".into())
        })?;
        // rmcp 3.x makes the server implementation identity optional on peer info.
        let server_info = info.server_info.as_ref();
        Ok(McpServerInfo {
            name: server_info.map(|si| si.name.clone()).unwrap_or_default(),
            version: server_info.map(|si| si.version.clone()).unwrap_or_default(),
            capabilities: serde_json::to_value(&info.capabilities).unwrap_or_default(),
        })
    }

    async fn list_tools(
        &self,
    ) -> Result<Vec<McpToolDefinition>, distill_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| distill_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_tools(Some(
                    rmcp::model::PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await
                .map_err(|e| distill_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

            all_tools.extend(result.tools.into_iter().map(|t| McpToolDefinition {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: serde_json::to_value(&t.input_schema).ok(),
            }));

            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(all_tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpCallResult, distill_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| distill_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        // MCP spec requires arguments to be an object; coerce if needed.
        let args_object = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                let mut wrapper = serde_json::Map::new();
                wrapper.insert("value".to_string(), other);
                Some(wrapper)
            }
        };
        let result = service
            .call_tool({
                let mut params = rmcp::model::CallToolRequestParams::new(name.to_string());
                params.arguments = args_object;
                params
            })
            .await
            .map_err(|e| distill_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        Ok(mcp_call_result_from_rmcp(result))
    }

    async fn close(&self) -> Result<(), distill_computer_hub_mcp_adapter::McpError> {
        // No-op: cleanup happens when McpClient is dropped.
        Ok(())
    }
}

/// `McpCallResult` has no structured field, so `structuredContent` rides as a trailing text block.
fn mcp_call_result_from_rmcp(result: rmcp::model::CallToolResult) -> McpCallResult {
    let mut content: Vec<McpContent> = result
        .content
        .into_iter()
        .map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => McpContent::Text { text: t.text },
            rmcp::model::ContentBlock::Image(img) => McpContent::Image {
                mime_type: img.mime_type,
                data: img.data,
            },
            _ => McpContent::Text {
                text: "[unsupported content type]".to_string(),
            },
        })
        .collect();
    let texts = content.iter().filter_map(|c| match c {
        McpContent::Text { text } => Some(text.as_str()),
        _ => None,
    });
    let structured = render_structured_content(result.structured_content.as_ref(), texts);
    content.extend(structured.map(|text| McpContent::Text { text }));
    McpCallResult {
        content,
        is_error: result.is_error.unwrap_or(false),
    }
}

/// Preserves the legacy `server__tool` names used by `workspace.configure_mcp`.
pub(crate) struct QualifiedMcpToolHandler {
    tool_id: ToolId,
    inner: Arc<dyn ToolServerHandler>,
}

impl QualifiedMcpToolHandler {
    pub fn from_namespaced(inner: Arc<dyn ToolServerHandler>) -> Option<Self> {
        let description = inner.description();
        let namespace = description.namespace?;
        let qualified_name = format!("{namespace}{MCP_TOOL_NAME_DELIMITER}{}", inner.tool_id());
        let Some((tool_id, _, _)) = parse_mcp_qualified_name(&qualified_name) else {
            tracing::warn!(qualified_name, "skipping invalid qualified MCP tool name");
            return None;
        };
        Some(Self { tool_id, inner })
    }
}

#[async_trait]
impl ToolServerHandler for QualifiedMcpToolHandler {
    fn tool_id(&self) -> ToolId {
        self.tool_id.clone()
    }

    fn description(&self) -> ToolDescription {
        let inner = self.inner.description();
        ToolDescription::new(self.tool_id.as_str().to_owned(), inner.description)
    }

    fn input_schema(&self) -> Option<Value> {
        self.inner.input_schema()
    }

    async fn handle_call(&self, ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        self.inner.handle_call(ctx, args).await
    }
}

/// Result of a `workspace.configure_mcp` RPC call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartResult {
    /// Server names that started successfully.
    pub started: Vec<String>,
    pub failed: Vec<McpStartFailure>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartFailure {
    pub name: String,
    /// Human-readable error description.
    pub error: String,
}

/// One MCP server that started and finished tool discovery. The bridge holds
/// the tools it discovered.
pub(crate) struct StartedMcpServer {
    pub(crate) name: String,
    pub(crate) tier: McpServerTier,
    pub(crate) bridge: McpBridgeHandle,
}

/// Outcome of [`connect_servers`].
pub(crate) struct StartedMcp {
    pub(crate) servers: Vec<StartedMcpServer>,
    pub(crate) failed: Vec<McpStartFailure>,
}

/// A server whose transport connected and whose tools were discovered, before
/// the session has committed to it.
struct BridgedServer {
    name: String,
    tier: McpServerTier,
    client: Arc<McpClient>,
    bridge: McpBridgeHandle,
}

type BridgeOutcome = Result<BridgedServer, McpStartFailure>;

/// Record one server's start outcome in the session's [`McpState`] and translate it for the caller. Returns `None` when the LIFE the drive started under is no longer current — the client is dropped, which closes its transport and kills its process.
async fn record_bridge_outcome(
    outcome: BridgeOutcome,
    session: &WorkspaceSession,
    remaining_names: &mut HashSet<String>,
    life: u64,
) -> Option<Result<StartedMcpServer, McpStartFailure>> {
    match outcome {
        Ok(BridgedServer {
            name: server_name,
            tier,
            client,
            bridge,
        }) => {
            remaining_names.remove(&server_name);
            // Commit gate, life-coherent by construction: the binding lock is HELD across the `owned_clients` write (lock order binding → mcp_state, the same order teardown's in-critical-section sweep uses), and the gate compares the LIFE — not merely not-`Closed`.
            // A teardown-then-revive during the server's start leaves the binding `Active` again, but for a NEW life; a state-only gate would commit this stale client into it (and publish the outcome to the drive's consumer).
            // Epoch equality subsumes the `Closed` check: teardown bumps before flipping.
            let stale = {
                let _binding = session.mcp_binding.lock().await;
                let stale = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) != life;
                let mut state = session.mcp_state.lock().await;
                if stale {
                    state.owned_clients.remove(&server_name);
                } else {
                    state.owned_clients.insert(server_name.clone(), client);
                    state.mark_server_ready(&server_name);
                }
                stale
            };
            if stale {
                tracing::info!(
                    server = %server_name,
                    "dropping MCP client that finished connecting after its life ended"
                );
                return None;
            }
            Some(Ok(StartedMcpServer {
                name: server_name,
                tier,
                bridge,
            }))
        }
        Err(failure) => {
            remaining_names.remove(&failure.name);
            {
                // Same nesting and life gate as the Ok arm, so a failure's
                // bookkeeping cannot race a teardown's sweep or land in a
                // revived life's state.
                let _binding = session.mcp_binding.lock().await;
                if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
                    let mut state = session.mcp_state.lock().await;
                    state.owned_clients.remove(&failure.name);
                    state.record_init_failure(&failure.name, false, Some(failure.error.clone()));
                    state.mark_server_ready(&failure.name);
                }
            }
            tracing::warn!(
                server = %failure.name,
                error = %failure.error,
                "McpBridge::connect failed"
            );
            Some(Err(failure))
        }
    }
}

/// Hard bound on the MCP tools one session advertises, across all of its servers. Tool lists come from external servers and every advertised tool is model-visible, so the fan-in carries an explicit cap; tools past it are dropped deterministically (first-party servers first, then name order, tools in server order) and stay unowned, so a later removal cannot unregister a tool that was never advertised.
pub(crate) const MAX_ADVERTISED_MCP_TOOLS: usize = 256;

/// Decide what each of a session's servers may advertise, record it on the server, and return the flat list. The configured path's only writer of `tool_ids`, which is what keeps a removal unregistering exactly what its server contributed.
pub(crate) async fn claim_tools(
    session: &WorkspaceSession,
    native: &HashSet<ToolId>,
) -> (Vec<Arc<dyn ToolServerHandler>>, u64) {
    let mut binding = session.mcp_binding.lock().await;
    // The life these claims belong to, observed under the same lock that
    // recorded them; `settle_registrations` re-checks it so registrations
    // made for this life are never committed to (or left registered under) a newer one.
    let life = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst);
    let Some(active) = binding.active_mut() else {
        return (Vec::new(), life);
    };

    let offers: Vec<ClaimOffer> = active
        .servers
        .iter()
        .map(|(name, server)| ClaimOffer {
            name: name.clone(),
            tier: server.tier,
            tool_ids: server
                .bridge
                .bridge
                .handlers()
                .iter()
                .map(|handler| handler.tool_id())
                .collect(),
        })
        .collect();
    let plan = plan_claims(offers, native, MAX_ADVERTISED_MCP_TOOLS);

    let mut advertised = Vec::new();
    for (name, claimed) in plan.claims {
        let Some(server) = active.servers.get_mut(&name) else {
            continue;
        };
        let claimed_ids: HashSet<&ToolId> = claimed.iter().collect();
        advertised.extend(
            server
                .bridge
                .bridge
                .handlers()
                .iter()
                .filter(|handler| claimed_ids.contains(&handler.tool_id()))
                .map(|handler| Arc::clone(handler) as Arc<dyn ToolServerHandler>),
        );
        server.tool_ids = claimed;
    }
    if plan.rejected > 0 {
        tracing::warn!(
            rejected = plan.rejected,
            "skipping MCP tools whose IDs collide with native or sibling MCP tools"
        );
    }
    if plan.over_cap > 0 {
        tracing::warn!(
            over_cap = plan.over_cap,
            cap = MAX_ADVERTISED_MCP_TOOLS,
            "skipping MCP tools past the per-session advertisement cap"
        );
    }
    (advertised, life)
}

/// Open a drive scope for a session's MCP life: fail closed on a torn-down binding and — inside the same critical section — snapshot the life's cancel token AND epoch.
/// Teardown cancels that token (captured under this same lock) to drop in-flight start futures, and every downstream commit ([`record_bridge_outcome`], [`install_servers`], `settle_registrations`) gates on the epoch — so a teardown+revive during a start can neither leave the drive uncancellable nor let its outcomes commit into the new life.
/// The CALLER holds the scope so the install half of a convergence gates on the same life the drive verified.
pub(crate) async fn begin_mcp_drive(
    session: &WorkspaceSession,
) -> WorkspaceResult<(tokio_util::sync::CancellationToken, u64)> {
    let binding = session.mcp_binding.lock().await;
    if matches!(*binding, WorkspaceMcpBinding::Closed) {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    }
    Ok((
        session.mcp_cancel.lock().clone(),
        session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst),
    ))
}

/// Start `configs` and send each server down `outcomes` AS IT FINISHES, all bounded by a single absolute discovery deadline so one hanging server cannot delay the rest — neither their starts (per-server futures run concurrently) nor their publication (outcomes stream per arrival, so a caller can advertise a fast server's tools while a slow sibling is still discovering).
/// One future per server, driven directly: a server still starting when the deadline fires is cancelled (its future dropped) and reported as a timeout failure.
/// Cancel safety rests on the crate conventions — child processes die with their `kill_on_drop` process groups and token state is written atomically — and a cancelled or failed server is absent from the session map, so the next convergence retries it.
pub(crate) async fn drive_server_starts(
    session: &WorkspaceSession,
    session_id: &str,
    configs: Vec<agent_client_protocol::McpServer>,
    discovery_timeout: Duration,
    first_party: &HashSet<String>,
    event_writer: distill_session_events::EventWriter,
    outcomes: tokio::sync::mpsc::Sender<Result<StartedMcpServer, McpStartFailure>>,
    drive_scope: (tokio_util::sync::CancellationToken, u64),
) -> WorkspaceResult<()> {
    let (cancel, life) = drive_scope;
    if configs.is_empty() {
        return Ok(());
    }
    let sid = SessionId::new(session_id)
        .map_err(|error| WorkspaceError::HubError(format!("invalid session_id: {error}")))?;

    let discovery_deadline = tokio::time::Instant::now() + discovery_timeout;
    let mut remaining_names: HashSet<String> = configs
        .iter()
        .map(|config| distill_mcp::servers::mcp_server_name(config).to_owned())
        .collect();
    // Dropping the guard mid-drive releases the claim; the normal exits complete init through it first.
    let init_claim = {
        let _binding = session.mcp_binding.lock().await;
        let mut state = session.mcp_state.lock().await;
        if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
            let claim = state.try_start_init();
            if claim.is_some() {
                state.mark_servers_initializing(remaining_names.iter().cloned());
            }
            claim
        } else {
            None
        }
    };
    // Per-server startup watchdog sized so the WHOLE handshake (the server/discover probe phase plus the legacy phase running on this startup budget) fits the shared deadline, keeping the invariant that a hung handshake fails on its own before the deadline has to cancel it.
    // Sizing to the raw deadline would let a swallowed probe burn the legacy phase's window and convert per-server errors into the generic discovery-timeout failure.
    let deadline_secs = discovery_timeout
        .as_secs()
        .saturating_add(u64::from(discovery_timeout.subsec_nanos() != 0));
    let startup_timeout_sec =
        distill_mcp::servers::McpClient::max_startup_within_deadline(deadline_secs);
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(startup_timeout_sec),
        ..Default::default()
    };
    // Two spawn contexts, chosen PER SERVER: only first-party app endpoints get the agent-id header (which carries the bound session id and flips the transport to the local-agent posture — no OAuth probe, no proxy, no redirects).
    // A per-drive flag here would leak the session id to every user-configured third-party server in the bind config and break their auth.
    let ctx_plain = McpSpawnCtx::for_session(
        session_id,
        &event_writer,
        OauthInteractivity::Interactive,
        None,
    );
    let ctx_first_party = McpSpawnCtx::for_session(
        session_id,
        &event_writer,
        OauthInteractivity::Interactive,
        None,
    )
    .with_grok_agent_id_header();

    let mut pending: FuturesUnordered<_> = configs
        .into_iter()
        .map(|config| {
            let server_name = distill_mcp::servers::mcp_server_name(&config).to_owned();
            let bridge_session_id = sid.clone();
            let overrides = &overrides;
            // The tier and the spawn context are the same decision: the set
            // that earns the agent-id header is the set that outranks
            // third-party servers when tool ids collide.
            let (ctx, tier) = if first_party.contains(&server_name) {
                (&ctx_first_party, McpServerTier::FirstParty)
            } else {
                (&ctx_plain, McpServerTier::ThirdParty)
            };
            async move {
                let client = distill_mcp::servers::start_mcp_server(
                    config,
                    Some(overrides),
                    None,
                    None,
                    ctx,
                )
                .await
                .map_err(|error| McpStartFailure {
                    name: server_name.clone(),
                    error: error.to_string(),
                })?;
                let client = Arc::new(client);
                let transport: Arc<dyn McpTransport> =
                    Arc::new(McpClientTransportAdapter::new(Arc::clone(&client)));
                let config = McpBridgeConfig {
                    session_id: bridge_session_id,
                    // The bridge namespaces every tool by server name;
                    // `claim_tools` relies on that to tell siblings'
                    // same-named tools apart.
                    namespace: Some(server_name.clone()),
                };
                let bridge = McpBridge::connect(transport, &config)
                    .await
                    .map_err(|error| McpStartFailure {
                        name: server_name.clone(),
                        error: error.to_string(),
                    })?;
                Ok::<_, McpStartFailure>(BridgedServer {
                    name: server_name,
                    tier,
                    client,
                    bridge,
                })
            }
        })
        .collect();

    let deadline = tokio::time::sleep_until(discovery_deadline);
    tokio::pin!(deadline);
    let mut deadline_fired = false;
    let mut cancelled = false;
    let mut receiver_gone = false;
    while !pending.is_empty() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancelled = true;
                break;
            }
            _ = &mut deadline => {
                deadline_fired = true;
                break;
            }
            outcome = pending.next() => {
                let Some(outcome) = outcome else { break };
                if let Some(result) =
                    record_bridge_outcome(outcome, session, &mut remaining_names, life).await
                    && outcomes.send(result).await.is_err()
                {
                    receiver_gone = true;
                    break;
                }
            }
        }
    }
    if cancelled || receiver_gone {
        // Teardown or caller abort: drop the pending starts (killing their
        // children) and report nothing further.
        drop(pending);
        finish_init_if_life(session, life, init_claim).await;
        return if cancelled {
            Err(WorkspaceError::SessionNotFound(session_id.to_owned()))
        } else {
            Ok(())
        };
    }
    if deadline_fired {
        // Outcomes that are already complete keep their real result...
        while let Some(Some(outcome)) = futures::FutureExt::now_or_never(pending.next()) {
            if let Some(result) =
                record_bridge_outcome(outcome, session, &mut remaining_names, life).await
            {
                let _ = outcomes.send(result).await;
            }
        }
        // ...and stragglers are cancelled by the drop.
        drop(pending);
        for name in remaining_names.drain() {
            {
                // Life-gated like every state write: a straggler timing out
                // after a teardown+revive must not smear failures onto the
                // new life's state.
                let _binding = session.mcp_binding.lock().await;
                if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
                    let mut state = session.mcp_state.lock().await;
                    state.record_init_failure(
                        &name,
                        false,
                        Some(format!(
                            "MCP discovery timed out after {discovery_timeout:?}"
                        )),
                    );
                    state.mark_server_ready(&name);
                }
            }
            let _ = outcomes
                .send(Err(McpStartFailure {
                    name,
                    error: format!("MCP discovery timed out after {discovery_timeout:?}"),
                }))
                .await;
        }
    }
    finish_init_if_life(session, life, init_claim).await;
    Ok(())
}

/// Close out the init-progress bookkeeping, but only if `life` is still the
/// session's current MCP life — a drive outlived by a teardown+revive must
/// not stamp the NEW life's init progress — and only while the drive still
/// owns init; a config change's successor completes its own.
async fn finish_init_if_life(session: &WorkspaceSession, life: u64, claim: Option<InitClaimGuard>) {
    let Some(claim) = claim else {
        return;
    };
    let _binding = session.mcp_binding.lock().await;
    if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
        let _ = session
            .mcp_state
            .write_if_owner(claim, |state, _claim| {
                state.finish_init();
                state.complete_init();
            })
            .await;
    }
}

/// Dedupe MCP server configs by name, LAST definition wins (JSON-object semantics — name-keyed sources can only produce duplicates through list-shaped construction).
/// One helper for BOTH config chokepoints — `BindMcpConfig::new` (machine-owned) and `start_session_mcp_servers` (client-driven `workspace.configure_mcp`) — so the two paths cannot drift.
pub(crate) fn dedupe_servers_last_wins(servers: &mut Vec<agent_client_protocol::McpServer>) {
    let mut seen = std::collections::HashSet::new();
    let mut dropped = 0usize;
    // Iterate from the back so the LAST occurrence of each name is the one
    // kept, preserving its position.
    for index in (0..servers.len()).rev() {
        let Some(server) = servers.get(index) else {
            continue;
        };
        let name = distill_mcp::servers::mcp_server_name(server).to_owned();
        if !seen.insert(name) {
            servers.remove(index);
            dropped += 1;
        }
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "MCP config has duplicate server names; keeping the last definition of each"
        );
    }
}

/// Compose a host-owned built-in `entry` into `servers`: same-named entries are dropped so none
/// can take its first-party posture, and it goes first so [`cap_servers`] keeps it.
pub fn compose_built_in(
    servers: Vec<agent_client_protocol::McpServer>,
    entry: agent_client_protocol::McpServer,
) -> Vec<agent_client_protocol::McpServer> {
    let name = distill_mcp::servers::mcp_server_name(&entry);
    let (same_name, mut composed): (Vec<_>, Vec<_>) = servers
        .into_iter()
        .partition(|server| distill_mcp::servers::mcp_server_name(server) == name);
    let impersonating = same_name.iter().filter(|server| **server != entry).count();
    if impersonating > 0 {
        tracing::warn!(
            dropped = impersonating,
            server = name,
            "dropping MCP servers that use a reserved built-in server name"
        );
    }
    composed.insert(0, entry);
    composed
}

/// Cap a server-config list at [`crate::config::BindMcpConfig::MAX_SERVERS`], keeping the first entries in config order.
/// Shared by BOTH config chokepoints — `BindMcpConfig::new` (machine-owned) and `start_session_mcp_servers` (client-driven `workspace.configure_mcp`) — so every entry ends up costing bounded resources (process, connection, discovery work) no matter which path configured it.
pub(crate) fn cap_servers(servers: &mut Vec<agent_client_protocol::McpServer>) {
    if servers.len() > crate::config::BindMcpConfig::MAX_SERVERS {
        tracing::warn!(
            configured = servers.len(),
            cap = crate::config::BindMcpConfig::MAX_SERVERS,
            "MCP config exceeds the server cap; keeping the first entries in config order"
        );
        servers.truncate(crate::config::BindMcpConfig::MAX_SERVERS);
    }
}

/// [`drive_server_starts`], collected into one batch result: the
/// `workspace.configure_mcp` RPC path, whose wire contract wants the full
/// started/failed report at once.
pub(crate) async fn connect_servers(
    session: &WorkspaceSession,
    session_id: &str,
    configs: Vec<agent_client_protocol::McpServer>,
    discovery_timeout: Duration,
    first_party: &HashSet<String>,
    event_writer: distill_session_events::EventWriter,
) -> WorkspaceResult<(StartedMcp, u64)> {
    let drive_scope = begin_mcp_drive(session).await?;
    let life = drive_scope.1;
    // One outcome per config entry, and the entry count is capped at
    // MAX_SERVERS by both config chokepoints — so this named capacity can
    // never fill and a send never blocks.
    let (tx, mut rx) = tokio::sync::mpsc::channel(crate::config::BindMcpConfig::MAX_SERVERS);
    let drive = drive_server_starts(
        session,
        session_id,
        configs,
        discovery_timeout,
        first_party,
        event_writer,
        tx,
        drive_scope,
    );
    let collect = async {
        let mut servers = Vec::new();
        let mut failed = Vec::new();
        while let Some(outcome) = rx.recv().await {
            match outcome {
                Ok(server) => servers.push(server),
                Err(failure) => failed.push(failure),
            }
        }
        (servers, failed)
    };
    let (driven, (servers, failed)) = tokio::join!(drive, collect);
    driven?;
    tracing::info!(
        session_id,
        started = servers.len(),
        failed = failed.len(),
        "MCP servers connected"
    );
    Ok((StartedMcp { servers, failed }, life))
}

/// Add `started` to the session's server map. The caller drops the bridges, which closes their transports.
pub(crate) async fn install_servers(
    session: &WorkspaceSession,
    started: Vec<StartedMcpServer>,
    expected_life: u64,
) -> WorkspaceResult<()> {
    let mut binding = session.mcp_binding.lock().await;
    // Life-gated, not merely state-gated: a server recorded under an older life can still be IN FLIGHT through
    // a convergence's publish channel when a teardown+revive opens a new life — a bare `join()` would insert it
    // into that new life, which would then treat the stale client as already running and never start a fresh one.
    if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) != expected_life {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    }
    let Some(active) = binding.join() else {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    };
    for server in started {
        active.servers.insert(
            server.name,
            SessionMcpServer {
                bridge: server.bridge,
                tool_ids: Vec::new(),
                tier: server.tier,
            },
        );
    }
    Ok(())
}

/// Stop `names` for this session: unadvertise exactly the ids each owns, then shut its bridge down. A tool call in flight against a stopped server fails when its transport closes.
/// That is deliberate — the alternative is waiting on an arbitrary third-party server that may never answer, which would hang the convergence.
pub(crate) async fn stop_servers(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    names: &[String],
) -> Vec<String> {
    let (stopped, life) = {
        let mut binding = session.mcp_binding.lock().await;
        // The life whose registrations this stop removes, observed under
        // the same lock the extraction holds: a stale post-CS unregister
        // can then never hit a newer life's same-id registration.
        let life = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let Some(active) = binding.active_mut() else {
            return Vec::new();
        };
        let stopped: Vec<(String, SessionMcpServer)> = names
            .iter()
            .filter_map(|name| active.servers.remove_entry(name))
            .collect();
        (stopped, life)
    };
    let mut names = Vec::with_capacity(stopped.len());
    for (name, server) in stopped {
        for tool_id in &server.tool_ids {
            if let Err(error) = tool_server
                .unregister_tool_dynamic(tool_id, session_id, life)
                .await
            {
                tracing::warn!(%session_id, tool = %tool_id, %error, "failed to unadvertise MCP tool");
            }
        }
        if let Err(error) = server.bridge.bridge.shutdown().await {
            tracing::warn!(%session_id, server = %name, %error, "MCP bridge shutdown failed");
        }
        names.push(name);
    }
    names
}

/// Install `started` and advertise its tools under the legacy qualified `server__tool` names — `workspace.configure_mcp`'s wire contract.
/// The second `tool_ids` writer besides [`claim_tools`]: the qualified names are pre-namespaced per server, so there is nothing to disambiguate, and the two writers are mutually exclusive by construction — this RPC is refused on a workspace whose servers come from local configuration.
pub(crate) async fn install_and_advertise_qualified(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    started: Vec<StartedMcpServer>,
    life: u64,
) -> WorkspaceResult<()> {
    // Same per-session advertisement cap as the bind path's `claim_tools`:
    // the legacy path REPLACES the session's servers (`stop_servers` ran),
    // so the count starts from zero. First tools in server order win.
    let mut total = 0usize;
    let mut over_cap = 0usize;
    let advertised: Vec<(String, Vec<Arc<dyn ToolServerHandler>>)> = started
        .iter()
        .map(|server| {
            let handlers = server
                .bridge
                .bridge
                .handlers()
                .iter()
                .filter_map(|handler| {
                    QualifiedMcpToolHandler::from_namespaced(handler.clone())
                        .map(|qualified| Arc::new(qualified) as Arc<dyn ToolServerHandler>)
                })
                .filter(|_| {
                    if total >= MAX_ADVERTISED_MCP_TOOLS {
                        over_cap += 1;
                        false
                    } else {
                        total += 1;
                        true
                    }
                })
                .collect();
            (server.name.clone(), handlers)
        })
        .collect();
    if over_cap > 0 {
        tracing::warn!(
            over_cap,
            cap = MAX_ADVERTISED_MCP_TOOLS,
            "skipping MCP tools past the per-session advertisement cap"
        );
    }
    install_servers(session, started, life).await?;
    for (name, handlers) in advertised {
        let mut owned = Vec::new();
        for handler in handlers {
            let tool_id = handler.tool_id();
            match tool_server
                .register_tool_dynamic(handler, vec![session_id.clone()], life)
                .await
            {
                Ok(()) => owned.push(tool_id),
                Err(error) => tracing::warn!(
                    %session_id,
                    tool = %tool_id,
                    %error,
                    "failed to register MCP tool on hub"
                ),
            }
        }
        // Records this server's contribution, or — when a teardown
        // interleaved with the registrations above — unregisters them again
        // so the hub does not keep routing into dropped bridges.
        let recorded = owned.clone();
        settle_registrations(
            session,
            session_id,
            tool_server,
            owned,
            life,
            move |active| {
                if let Some(server) = active.servers.get_mut(&name) {
                    server.tool_ids = recorded;
                }
            },
        )
        .await;
    }
    Ok(())
}

/// What converging one session onto a new configuration changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionMcpDelta {
    /// Servers started and advertised.
    pub added: Vec<String>,
    /// Servers stopped and unadvertised.
    pub removed: Vec<String>,
    /// Servers the convergence tried to start but could not.
    pub failed: Vec<String>,
}

impl SessionMcpDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.failed.is_empty()
    }
}

/// Whether a convergence with an unchanged server plan still re-claims tool ownership and reconciles the hub. A reload converges every session and skips the untouched ones (`IfChanged`); a bind must reconcile even when no server starts or stops, because its *native* tool set may have changed and a claimed MCP id could newly collide with it (`Always`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpReclaim {
    IfChanged,
    Always,
}

/// Which of a session's servers must stop, and which must start, to match a
/// configuration.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ConvergencePlan {
    pub(crate) stop: Vec<String>,
    pub(crate) start: Vec<String>,
}

/// Decide the plan from what a session runs (`live`), what the configuration asks for (`wanted`, in config order), and which names the configuration redefined (`redefined`).
/// Anything wanted that is then not running starts — which covers new servers, redefined ones just stopped, and servers whose previous start failed, since a failed server never entered `live`.
pub(crate) fn plan_convergence(
    live: &HashSet<String>,
    wanted: &[String],
    redefined: &HashSet<String>,
) -> ConvergencePlan {
    let keeps = |name: &String| wanted.contains(name) && !redefined.contains(name);
    let stop: Vec<String> = live.iter().filter(|name| !keeps(name)).cloned().collect();
    let start = wanted
        .iter()
        .filter(|name| !live.contains(*name) || !keeps(name))
        .cloned()
        .collect();
    ConvergencePlan { stop, start }
}

/// Make one session's MCP servers match `desired`. Stops servers the config dropped or redefined, starts everything it wants that the session is not already running, and leaves untouched servers alone — their client, process, and in-flight calls all survive.
/// No-op for sessions that never joined the configured set.
pub(crate) async fn converge_session(
    session: &WorkspaceSession,
    session_id: &str,
    desired: &crate::config::BindMcpConfig,
    tool_server: &impl HubToolRegistry,
    reclaim: McpReclaim,
    event_writer: distill_session_events::EventWriter,
) -> WorkspaceResult<SessionMcpDelta> {
    let sid = SessionId::new(session_id)
        .map_err(|error| WorkspaceError::HubError(format!("invalid session_id: {error}")))?;
    let Some(live) = session
        .mcp_binding
        .lock()
        .await
        .active()
        .map(|active| active.servers.keys().cloned().collect::<HashSet<_>>())
    else {
        return Ok(SessionMcpDelta::default());
    };

    let wanted: Vec<String> = desired
        .servers()
        .iter()
        .map(|config| distill_mcp::servers::mcp_server_name(config).to_owned())
        .collect();
    // The diff keeps unchanged servers' clients alive and forgets the rest.
    // Its `added` doubles as "redefined" for names already running.
    let redefined: HashSet<String> = {
        let mut state = session.mcp_state.lock().await;
        state
            .update_configs_diff(desired.servers().to_vec())
            .map(|diff| diff.added.into_iter().collect())
            .unwrap_or_default()
    };

    let plan = plan_convergence(&live, &wanted, &redefined);
    if plan == ConvergencePlan::default() && reclaim == McpReclaim::IfChanged {
        return Ok(SessionMcpDelta::default());
    }
    let mut delta = SessionMcpDelta {
        removed: stop_servers(session, &sid, tool_server, &plan.stop).await,
        ..Default::default()
    };
    // The bind recorded which ids are native; the hub snapshot cannot be used
    // for this, because it also holds already-advertised MCP tools and counting
    // those as taken would make a surviving server lose the very ids it is serving.
    let native = session.mcp_native_tool_ids.lock().clone();
    if !plan.start.is_empty() {
        let starting: HashSet<&str> = plan.start.iter().map(String::as_str).collect();
        let configs = desired
            .servers()
            .iter()
            .filter(|config| starting.contains(distill_mcp::servers::mcp_server_name(config)))
            .cloned()
            .collect();
        let drive_scope = begin_mcp_drive(session).await?;
        let drive_life = drive_scope.1;
        // Same bound as `connect_servers`: one outcome per (capped) entry.
        let (tx, mut rx) = tokio::sync::mpsc::channel(crate::config::BindMcpConfig::MAX_SERVERS);
        let drive = drive_server_starts(
            session,
            session_id,
            configs,
            desired.discovery_timeout(),
            desired.first_party_servers(),
            event_writer,
            tx,
            drive_scope,
        );
        // Publish EACH server's tools as its discovery completes, so one slow or hanging server never delays a healthy sibling's tools (the drive's deadline only bounds the stragglers).
        // Dropping the receiver on an install failure aborts the drive.
        let publish = async {
            while let Some(outcome) = rx.recv().await {
                match outcome {
                    Ok(server) => {
                        // Life-gated on the DRIVE's life: the outcome was recorded under it, but this
                        // publish half runs concurrently — a teardown+revive between the record and this
                        // install must refuse the stale server, or the revived life would treat it as already running.
                        let name = server.name.clone();
                        install_servers(session, vec![server], drive_life).await?;
                        delta.added.push(name);
                        reconcile_session_tools(session, &sid, tool_server, &native).await;
                    }
                    Err(failure) => delta.failed.push(failure.name),
                }
            }
            Ok::<(), WorkspaceError>(())
        };
        let (driven, published) = tokio::join!(drive, publish);
        driven?;
        published?;
    }
    // Reconciliation also runs when the convergence started nothing: stopping a
    // clashing server frees its tool id for a survivor to re-claim.
    reconcile_session_tools(session, &sid, tool_server, &native).await;
    Ok(delta)
}

/// Re-claim the session's MCP tool ids and reconcile the hub with the outcome. Idempotent; runs after every server-set change.
/// The hub's same-life re-register is an idempotent no-op, so unless the previous owner's registration is removed first the hub keeps routing the id into the sibling — while the session's bookkeeping attributes it to the new owner, whose eventual removal is the only thing that would ever clean it up.
async fn reconcile_session_tools(
    session: &WorkspaceSession,
    sid: &SessionId,
    tool_server: &impl HubToolRegistry,
    native: &HashSet<ToolId>,
) {
    let owners_before = owned_tool_owners(session).await;
    let (claimed, life) = claim_tools(session, native).await;
    let owners_after = owned_tool_owners(session).await;
    for (tool_id, owner) in &owners_before {
        if owners_after.get(tool_id) == Some(owner) {
            continue;
        }
        // Life-tagged: if this id is no longer a THIS-life dynamic registration (a
        // resolver-installed native now holds it, or a newer life re-registered
        // it), the unregister is a no-op rather than stripping the other owner's handler.
        if let Err(error) = tool_server
            .unregister_tool_dynamic(tool_id, sid, life)
            .await
        {
            tracing::warn!(session_id = %sid, tool = %tool_id, %error, "failed to unadvertise MCP tool");
        }
    }

    let mut registered = Vec::new();
    for handler in claimed {
        let tool_id = handler.tool_id();
        // No is-it-already-on-the-hub skip: the hub's life-tagged ledger makes a same-life re-register an
        // idempotent no-op, an older life's stale registration is SUPERSEDED (an is-on-hub skip here would leave
        // the id routing into the closed life's dropped bridge), and a native's id was already excluded by `claim_tools`.
        match tool_server
            .register_tool_dynamic(handler, vec![sid.clone()], life)
            .await
        {
            Ok(()) => registered.push(tool_id),
            Err(error) => {
                tracing::warn!(session_id = %sid, tool = %tool_id, %error, "failed to advertise MCP tool on hub");
            }
        }
    }
    // Ownership was already recorded by `claim_tools`; the gate only has to
    // catch a teardown that landed while the ids above were registering.
    settle_registrations(session, sid, tool_server, registered, life, |_active| {}).await;
}

/// Settle hub tool registrations made outside the binding lock: either the life they were claimed under is still current (run `commit` under the binding lock — the qualified path records ownership there; the configured path has nothing left to record, `claim_tools` already did) or that life is gone (unregister the just-registered ids again).
/// The GATE is the point; the closure is only what a caller still needs made atomic with it. `expected_life` is the `mcp_epoch` the caller observed under the lock when it claimed/installed (see [`claim_tools`] / [`install_servers`]): a bare is-Active check would let registrations made for a closed life commit into a REVIVED life — recorded on the wrong life's servers, or left registered on the hub routing into dropped bridges.
pub(crate) async fn settle_registrations(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    registered: Vec<ToolId>,
    expected_life: u64,
    commit: impl FnOnce(&mut crate::session::ActiveMcp),
) {
    {
        let mut binding = session.mcp_binding.lock().await;
        if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == expected_life
            && let Some(active) = binding.active_mut()
        {
            commit(active);
            return;
        }
    }
    for tool_id in &registered {
        if let Err(error) = tool_server
            .unregister_tool_dynamic(tool_id, session_id, expected_life)
            .await
        {
            tracing::warn!(
                %session_id,
                tool = %tool_id,
                %error,
                "failed to unadvertise MCP tool after a teardown race"
            );
        }
    }
}

/// Every id the session's MCP servers currently own, keyed to the owning
/// server's configured name. Ids are unique across servers by construction
/// (`claim_tools` never lets two servers own one id).
async fn owned_tool_owners(session: &WorkspaceSession) -> HashMap<ToolId, String> {
    session
        .mcp_binding
        .lock()
        .await
        .active()
        .map(|active| {
            active
                .servers
                .iter()
                .flat_map(|(name, server)| {
                    server
                        .tool_ids
                        .iter()
                        .map(move |tool_id| (tool_id.clone(), name.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler;

    #[async_trait]
    impl ToolServerHandler for TestHandler {
        fn tool_id(&self) -> ToolId {
            ToolId::new("lookup").unwrap()
        }

        fn description(&self) -> ToolDescription {
            ToolDescription::new("lookup", "test").with_namespace("server")
        }

        fn input_schema(&self) -> Option<Value> {
            None
        }

        async fn handle_call(
            &self,
            _ctx: ToolCallContext,
            _args: Value,
        ) -> ToolStream<TypedToolOutput> {
            panic!("constructor test does not call the tool")
        }
    }

    #[test]
    fn legacy_handler_uses_qualified_name() {
        let handler = QualifiedMcpToolHandler::from_namespaced(Arc::new(TestHandler)).unwrap();
        assert_eq!(handler.tool_id().as_str(), "server__lookup");
        assert_eq!(handler.description().name, "server__lookup");
    }

    fn bridged_texts(result: rmcp::model::CallToolResult) -> Vec<String> {
        mcp_call_result_from_rmcp(result)
            .content
            .into_iter()
            .map(|c| match c {
                McpContent::Text { text } => text,
                other => panic!("expected text block, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn bridged_result_appends_structured_content_after_a_summary() {
        let folders = serde_json::json!({"folders": [{"id": "p1", "name": "Alpha"}]});
        let mut result =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                "7 product folders, 2 custom folders",
            )]);
        result.structured_content = Some(folders.clone());
        assert_eq!(
            bridged_texts(result),
            vec![
                "7 product folders, 2 custom folders".to_string(),
                folders.to_string()
            ]
        );
    }

    #[test]
    fn bridged_result_does_not_duplicate_inlined_structured_content() {
        let folders = serde_json::json!({"folders": [{"id": "p1", "name": "Alpha"}]});
        let mut result = rmcp::model::CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("7 product folders, 2 custom folders"),
            rmcp::model::ContentBlock::text(folders.to_string()),
        ]);
        result.structured_content = Some(folders.clone());
        assert_eq!(
            bridged_texts(result),
            vec![
                "7 product folders, 2 custom folders".to_string(),
                folders.to_string()
            ]
        );
    }

    /// The client-driven configure path honors the SAME server cap as the
    /// machine-owned chokepoint — `cap_servers` is the one helper both run, keeping
    /// the first entries in config order (matching `BindMcpConfig::new`'s documented cap semantics).
    #[test]
    fn cap_servers_keeps_the_first_entries_in_config_order() {
        let http = |name: &str| {
            agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new(name, "http://cap.invalid/mcp")
                    .headers(vec![]),
            )
        };
        let over = crate::config::BindMcpConfig::MAX_SERVERS + 5;
        let mut servers: Vec<_> = (0..over).map(|i| http(&format!("server-{i:03}"))).collect();
        cap_servers(&mut servers);
        assert_eq!(crate::config::BindMcpConfig::MAX_SERVERS, servers.len());
        let Some(last) = servers.last() else {
            panic!("expected capped servers");
        };
        assert_eq!(
            format!(
                "server-{:03}",
                crate::config::BindMcpConfig::MAX_SERVERS - 1
            ),
            distill_mcp::servers::mcp_server_name(last),
        );
    }

    #[test]
    fn compose_built_in_reserves_the_name_and_leads_the_list() {
        let composed = compose_built_in(
            vec![
                stdio("other", "/other"),
                stdio("built_in", "/impersonator"),
                stdio("built_in", "/impersonator-2"),
            ],
            stdio("built_in", "/host"),
        );
        assert_eq!(
            composed,
            vec![stdio("built_in", "/host"), stdio("other", "/other")]
        );
    }

    /// A host re-feeding its own composed list (hot reload) must not warn about itself.
    #[test]
    fn compose_built_in_warns_only_for_a_differing_same_named_entry() {
        let host = stdio("built_in", "/host");
        let ((), re_fed) = crate::capturing_warn_logs(|| {
            let composed =
                compose_built_in(vec![host.clone(), stdio("other", "/other")], host.clone());
            assert_eq!(2, composed.len());
        });
        assert!(!re_fed.contains("reserved"), "own entry re-fed: {re_fed}");
        let ((), impersonated) = crate::capturing_warn_logs(|| {
            compose_built_in(vec![stdio("built_in", "/impersonator")], host.clone());
        });
        assert!(
            impersonated.contains("reserved") && impersonated.contains("dropped=1"),
            "impersonator: {impersonated}"
        );
    }

    /// The shared dedupe both config chokepoints run — `BindMcpConfig::new` AND the client-driven
    /// `start_session_mcp_servers` list before `connect_servers`: one slot per name, LAST definition wins
    /// at its position, so a duplicate can never start two clients and drop the first bridge handle without shutdown.
    #[test]
    fn dedupe_servers_last_wins_keeps_last_definition_in_place() {
        let http = |name: &str, url: &str| {
            agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new(name, url).headers(vec![]),
            )
        };
        let mut servers = vec![
            http("dup", "http://first.invalid/mcp"),
            http("other", "http://other.invalid/mcp"),
            http("dup", "http://last.invalid/mcp"),
        ];
        dedupe_servers_last_wins(&mut servers);
        let named: Vec<(&str, &str)> = servers
            .iter()
            .map(|server| {
                let agent_client_protocol::McpServer::Http(http) = server else {
                    panic!("fixture builds http servers only");
                };
                (
                    distill_mcp::servers::mcp_server_name(server),
                    http.url.as_str(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("other", "http://other.invalid/mcp"),
                ("dup", "http://last.invalid/mcp"),
            ],
            "one slot per name, the LAST definition kept at its position"
        );
    }

    fn stdio(name: &str, command: &str) -> agent_client_protocol::McpServer {
        agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio::new(
            name, command,
        ))
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn name_set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn plan_adds_without_disturbing_running_servers() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma", "linear"]),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: Vec::new(),
                start: names(&["linear"]),
            }
        );
    }

    #[test]
    fn plan_stops_servers_the_config_dropped() {
        let plan = plan_convergence(
            &name_set(&["figma", "linear"]),
            &names(&["figma"]),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: names(&["linear"]),
                start: Vec::new(),
            }
        );
    }

    /// A redefined server is restarted, not left running on its old config.
    #[test]
    fn plan_restarts_a_redefined_server() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma"]),
            &name_set(&["figma"]),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: names(&["figma"]),
                start: names(&["figma"]),
            }
        );
    }

    /// A server whose start failed is not in `live`, so the next reload
    /// retries it. Without this, a server that was down when the app booted
    /// would stay dead until a restart.
    #[test]
    fn plan_retries_a_server_that_failed_to_start() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma", "flaky"]),
            &HashSet::new(),
        );
        assert_eq!(plan.stop, Vec::<String>::new());
        assert_eq!(plan.start, names(&["flaky"]));
    }

    #[test]
    fn plan_for_an_unchanged_config_is_empty() {
        let plan = plan_convergence(
            &name_set(&["figma", "linear"]),
            &names(&["linear", "figma"]),
            &HashSet::new(),
        );
        assert_eq!(plan, ConvergencePlan::default());
    }
}
