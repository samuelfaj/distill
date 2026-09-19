//! Single source of truth for the `x.ai/mcp/*` ACP wire strings.
//!
//! These method/`_meta` keys are part of the cross-language MCP-over-ACP protocol the SDK speaks (mirrors the SDK's `_mcp_wire.py` / `mcpWire.ts`).
//! Reference these constants instead of re-typing the literals so the agent and SDK can't drift apart.

/// Forward tool-invocation method (client to agent): `x.ai/mcp/call`.
///
/// The pager/client asks the agent to invoke an MCP tool on a server the agent is connected to, outside the LLM loop.
/// See `extensions::mcp::handle_call`.
pub const MCP_CALL: &str = "x.ai/mcp/call";

/// Reverse zero-IPC tool-invocation method (agent to client): `x.ai/mcp/sdk_call`.
/// The agent invokes a tool in the SDK's in-process MCP server by sending the MCP JSON-RPC message back to the client over the ACP reverse channel.
/// It is distinct from [`MCP_CALL`] so the two disjoint schemas don't share a method string for metrics/tracing.
pub const MCP_SDK_CALL: &str = "x.ai/mcp/sdk_call";

/// `session/new` `_meta` key listing in-process SDK MCP servers: `x.ai/mcp/servers`.
pub const MCP_SERVERS: &str = "x.ai/mcp/servers";

/// `initialize` `_meta` capability flag advertising in-process SDK MCP support (enables the SDK's `transport="acp"`): `x.ai/mcp/sdk`.
pub const MCP_SDK: &str = "x.ai/mcp/sdk";

/// Reverse elicitation method (agent to client): `x.ai/mcp/elicit`.
///
/// The agent forwards an MCP server's `elicitation/create` request to the client, which shows the user a popup and returns accept/decline/cancel.
pub const MCP_ELICIT: &str = "x.ai/mcp/elicit";

/// Elicitation-complete notification (agent to client): `x.ai/mcp/elicit_complete`.
///
/// The agent forwards a server's `notifications/elicitation/complete` so the client can dismiss the popup for the given `elicitationId`.
pub const MCP_ELICIT_COMPLETE: &str = "x.ai/mcp/elicit_complete";
