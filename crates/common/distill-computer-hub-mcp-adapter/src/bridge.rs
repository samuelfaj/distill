// Modified for Distill by Samuel Fajreldines, 2026.
//! Core bridge that connects an MCP server to the computer hub.
//!
//! [`McpBridge`] discovers tools from an [`McpTransport`] and registers
//! them with a hub `ToolServer` via one `ToolServerHandler` per
//! tool. Incoming hub calls are translated to MCP `tools/call` and the
//! response is handed back in the MCP `CallToolResult` shape.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info, warn};
use distill_tool_protocol::{SessionId, ToolId};
use distill_tool_runtime::{ToolCallContext, ToolError, ToolStream, TypedToolOutput, terminal_only};
use distill_tool_types::ToolDescription;

use crate::transport::McpTransport;
use crate::types::{McpCallResult, McpContent, McpError, McpServerInfo, McpToolDefinition};

/// Configuration for an [`McpBridge`] instance.
#[derive(Debug, Clone)]
pub struct McpBridgeConfig {
    /// Hub session to bind tools to.
    pub session_id: SessionId,
    /// Optional namespace prefix for tool descriptions.
    pub namespace: Option<String>,
}

/// Result of a successful [`McpBridge::connect`] call.
///
/// Contains the bridge handle and the server info returned during the
/// MCP initialize handshake.
pub struct McpBridgeHandle {
    /// The bridge managing the MCP-to-hub tool registrations.
    pub bridge: McpBridge,
    /// Server metadata from the MCP `initialize` response.
    pub server_info: McpServerInfo,
}

impl std::fmt::Debug for McpBridgeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpBridgeHandle")
            .field("server_info", &self.server_info.name)
            .field("tool_count", &self.bridge.tool_count())
            .finish_non_exhaustive()
    }
}

/// Bridges an MCP server's tools into the computer hub.
///
/// On construction the bridge performs the MCP `initialize` handshake,
/// discovers tools via `tools/list`, and builds a handler
/// for each one. Callers wire these handlers into a
/// [`distill_computer_hub_sdk::ToolServerBuilder`] to register them
/// with the hub.
///
/// Callers **must** call [`McpBridge::shutdown`] before dropping to
/// close the underlying MCP transport cleanly. If the bridge is dropped
/// without an explicit shutdown, a best-effort `close()` is spawned on
/// the tokio runtime (mirroring `ToolServer`'s drop behavior).
pub struct McpBridge {
    transport: Arc<dyn McpTransport>,
    handlers: Vec<Arc<McpToolHandler>>,
    server_info: McpServerInfo,
}

impl std::fmt::Debug for McpBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpBridge")
            .field("server", &self.server_info.name)
            .field("tool_count", &self.handlers.len())
            .finish_non_exhaustive()
    }
}

impl McpBridge {
    /// Initialize the MCP server, discover its tools, and build handlers.
    ///
    /// Returns `Err` if the MCP handshake or tool discovery fails.
    pub async fn connect(
        transport: Arc<dyn McpTransport>,
        config: &McpBridgeConfig,
    ) -> Result<McpBridgeHandle, McpError> {
        let server_info = match transport.initialize().await {
            Ok(info) => info,
            Err(e) => {
                crate::metrics::mcp_error();
                return Err(e);
            }
        };
        info!(
            server_name = %server_info.name,
            version = %server_info.version,
            "MCP server initialized"
        );

        let tools = match transport.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                // Close the transport so the initialized connection is not leaked
                // when list_tools fails after a successful initialize.
                if let Err(close_err) = transport.close().await {
                    warn!(
                        ?close_err,
                        "failed to close transport after list_tools error"
                    );
                }
                crate::metrics::mcp_error();
                return Err(e);
            }
        };
        debug!(
            server_name = %server_info.name,
            tool_count = tools.len(),
            "discovered MCP tools"
        );

        let handlers: Vec<Arc<McpToolHandler>> = tools
            .into_iter()
            .filter_map(|def| {
                let tool_id = match ToolId::new(&def.name) {
                    Ok(id) => id,
                    Err(err) => {
                        warn!(
                            tool_name = %def.name,
                            %err,
                            "skipping MCP tool with invalid name"
                        );
                        return None;
                    }
                };
                Some(Arc::new(McpToolHandler {
                    tool_id,
                    definition: def,
                    transport: Arc::clone(&transport),
                    namespace: config.namespace.clone(),
                }))
            })
            .collect();

        crate::metrics::mcp_tools_bridged_set(handlers.len() as i64);

        let bridge = McpBridge {
            transport,
            handlers,
            server_info: server_info.clone(),
        };

        Ok(McpBridgeHandle {
            bridge,
            server_info,
        })
    }

    /// Handlers to register with a [`distill_computer_hub_sdk::ToolServerBuilder`].
    ///
    /// Each handler implements `ToolServerHandler` for one MCP tool.
    pub fn handlers(&self) -> &[Arc<McpToolHandler>] {
        &self.handlers
    }

    /// Server metadata from the MCP `initialize` response.
    pub fn server_info(&self) -> &McpServerInfo {
        &self.server_info
    }

    /// Number of tools discovered and registered.
    pub fn tool_count(&self) -> usize {
        self.handlers.len()
    }

    /// Close the underlying MCP transport.
    pub async fn shutdown(&self) -> Result<(), McpError> {
        crate::metrics::mcp_tools_bridged_set(0);
        self.transport.close().await
    }
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        crate::metrics::mcp_tools_bridged_set(0);
        let transport = Arc::clone(&self.transport);
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                if let Err(err) = transport.close().await {
                    warn!(?err, "best-effort transport close on drop failed");
                }
            });
        }
    }
}

/// Hub-facing handler for a single MCP tool.
///
/// Translates hub `tool_call_request` frames into MCP `tools/call`
/// invocations and hands the result back in the MCP `CallToolResult` shape.
pub struct McpToolHandler {
    tool_id: ToolId,
    definition: McpToolDefinition,
    transport: Arc<dyn McpTransport>,
    namespace: Option<String>,
}

impl std::fmt::Debug for McpToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolHandler")
            .field("tool_id", &self.tool_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl distill_computer_hub_sdk::ToolServerHandler for McpToolHandler {
    fn tool_id(&self) -> ToolId {
        self.tool_id.clone()
    }

    fn description(&self) -> ToolDescription {
        let desc = ToolDescription::new(
            self.definition.name.clone(),
            self.definition.description.clone().unwrap_or_default(),
        );
        match self.namespace {
            Some(ref ns) => desc.with_namespace(ns.clone()),
            None => desc,
        }
    }

    fn input_schema(&self) -> Option<Value> {
        self.definition.input_schema.clone()
    }

    async fn handle_call(&self, _ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        let _start = std::time::Instant::now();
        let tool_id = self.tool_id.clone();
        let result = self
            .transport
            .call_tool(self.definition.name.as_str(), args)
            .await;
        crate::metrics::mcp_call_duration_observe(_start.elapsed().as_secs_f64());

        let terminal = match result {
            Ok(call_result) => translate_mcp_result(call_result)
                .map(|value| TypedToolOutput::from_value(tool_id, value))
                .map_err(|e| {
                    crate::metrics::mcp_error();
                    ToolError::execution(self.tool_id.clone(), e.to_string()).with_source(e)
                }),
            Err(mcp_err) => {
                crate::metrics::mcp_error();
                Err(ToolError::execution(
                    self.tool_id.clone(),
                    format!("{mcp_err}"),
                ))
            }
        };

        terminal_only(terminal)
    }
}

/// Project an [`McpCallResult`] into the value handed to the hub.
///
/// The gateway renders the MCP `CallToolResult` shape one block per content
/// entry (a `ToolOutputWire` serialised here gets wrapped again by the SDK
/// and lands as one JSON text block). It needs a non-empty `content` array,
/// so a side-effect-only result gets one empty text block.
fn translate_mcp_result(mut result: McpCallResult) -> Result<Value, serde_json::Error> {
    if result.content.is_empty() {
        result.content.push(McpContent::Text {
            text: String::new(),
        });
    }
    serde_json::to_value(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{McpCallResult, McpContent, McpServerInfo, McpToolDefinition};
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Mutex;
    use distill_computer_hub_sdk::ToolServerHandler;
    use distill_tool_runtime::{ContentBlock, ToolStreamItem};

    struct MockTransport {
        server_info: McpServerInfo,
        tools: Vec<McpToolDefinition>,
        call_response: Mutex<Option<McpCallResult>>,
        call_error: Mutex<Option<McpError>>,
        closed: AtomicBool,
    }

    impl MockTransport {
        fn new(server_info: McpServerInfo, tools: Vec<McpToolDefinition>) -> Self {
            Self {
                server_info,
                tools,
                call_response: Mutex::new(None),
                call_error: Mutex::new(None),
                closed: AtomicBool::new(false),
            }
        }

        fn with_call_response(self, response: McpCallResult) -> Self {
            Self {
                call_response: Mutex::new(Some(response)),
                ..self
            }
        }

        fn with_call_error(self, error: McpError) -> Self {
            Self {
                call_error: Mutex::new(Some(error)),
                ..self
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn initialize(&self) -> Result<McpServerInfo, McpError> {
            Ok(self.server_info.clone())
        }

        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: Value,
        ) -> Result<McpCallResult, McpError> {
            if let Some(err) = self.call_error.lock().await.take() {
                return Err(err);
            }
            self.call_response
                .lock()
                .await
                .clone()
                .ok_or_else(|| McpError::Transport("no canned response".into()))
        }

        async fn close(&self) -> Result<(), McpError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn sample_server_info() -> McpServerInfo {
        McpServerInfo {
            name: "test-server".into(),
            version: "1.0.0".into(),
            capabilities: Value::Null,
        }
    }

    fn sample_tools() -> Vec<McpToolDefinition> {
        vec![
            McpToolDefinition {
                name: "search".into(),
                description: Some("Search for items".into()),
                input_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } }
                })),
            },
            McpToolDefinition {
                name: "create".into(),
                description: Some("Create an item".into()),
                input_schema: None,
            },
        ]
    }

    fn make_transport(mock: MockTransport) -> Arc<dyn McpTransport> {
        Arc::new(mock) as Arc<dyn McpTransport>
    }

    #[tokio::test]
    async fn bridge_discovers_and_builds_handlers() {
        let transport = make_transport(MockTransport::new(sample_server_info(), sample_tools()));
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: None,
        };

        let handle = McpBridge::connect(transport, &config).await.unwrap();
        assert_eq!(handle.server_info.name, "test-server");
        assert_eq!(handle.bridge.tool_count(), 2);

        let ids: Vec<String> = handle
            .bridge
            .handlers()
            .iter()
            .map(|h| h.tool_id.as_str().to_string())
            .collect();
        assert!(ids.contains(&"search".to_string()));
        assert!(ids.contains(&"create".to_string()));
    }

    #[tokio::test]
    async fn bridge_handler_descriptions() {
        let transport = make_transport(MockTransport::new(sample_server_info(), sample_tools()));
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: Some("mcp".into()),
        };

        let handle = McpBridge::connect(transport, &config).await.unwrap();
        let handler = handle
            .bridge
            .handlers()
            .iter()
            .find(|h| h.tool_id.as_str() == "search")
            .unwrap();

        let desc = handler.description();
        assert_eq!(desc.name, "search");
        assert_eq!(desc.description, "Search for items");
        assert_eq!(desc.namespace.as_deref(), Some("mcp"));
        assert!(handler.input_schema().is_some());
    }

    /// 1x1 transparent PNG.
    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    /// Connect a bridge whose transport answers every call with
    /// `response`, run the first handler as the daemon does, and return
    /// its terminal output. `model_output` is what the model sees: the
    /// SDK server forwards `value` verbatim and the gateway re-derives
    /// the blocks from it the same way `from_value` does here.
    async fn call_with_response(response: McpCallResult) -> TypedToolOutput {
        let transport = make_transport(
            MockTransport::new(sample_server_info(), sample_tools()).with_call_response(response),
        );
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: None,
        };
        let handle = McpBridge::connect(transport, &config).await.unwrap();
        let handler = &handle.bridge.handlers()[0];
        let mut stream = handler
            .handle_call(ToolCallContext::default(), Value::Null)
            .await;
        match stream.next().await.unwrap() {
            ToolStreamItem::Terminal(Ok(typed)) => typed,
            other => panic!("expected Terminal(Ok(_)), got {other:?}"),
        }
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn png_content() -> McpContent {
        McpContent::Image {
            mime_type: "image/png".into(),
            data: PNG_BASE64.into(),
        }
    }

    fn png_block() -> ContentBlock {
        ContentBlock::Image {
            mime_type: "image/png".into(),
            data: PNG_BASE64.into(),
            media_id: None,
            filename: None,
            path: None,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn bridge_forwards_call_text_response() {
        let typed = call_with_response(McpCallResult {
            content: vec![McpContent::Text {
                text: "found 3 results".into(),
            }],
            is_error: false,
        })
        .await;

        assert_eq!(
            typed.value,
            json!({"content": [{"type": "text", "text": "found 3 results"}], "isError": false})
        );
        assert_eq!(typed.model_output, vec![text_block("found 3 results")]);
    }

    #[tokio::test]
    async fn bridge_forwards_call_image_response() {
        let typed = call_with_response(McpCallResult {
            content: vec![
                McpContent::Text {
                    text: "result text".into(),
                },
                png_content(),
            ],
            is_error: false,
        })
        .await;

        assert_eq!(
            typed.model_output,
            vec![text_block("result text"), png_block()]
        );
    }

    /// An error result keeps every block: a failed browser action's
    /// screenshot is what lets the model self-correct.
    #[tokio::test]
    async fn bridge_handles_mcp_error_response() {
        let typed = call_with_response(McpCallResult {
            content: vec![
                McpContent::Text {
                    text: "permission denied".into(),
                },
                png_content(),
            ],
            is_error: true,
        })
        .await;

        assert_eq!(typed.value["isError"], json!(true));
        assert_eq!(
            typed.model_output,
            vec![text_block("permission denied"), png_block()]
        );
    }

    #[tokio::test]
    async fn bridge_handles_transport_error() {
        let transport = make_transport(
            MockTransport::new(sample_server_info(), sample_tools())
                .with_call_error(McpError::Transport("connection reset".into())),
        );
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: None,
        };

        let handle = McpBridge::connect(transport, &config).await.unwrap();
        let handler = &handle.bridge.handlers()[0];

        let ctx = ToolCallContext::default();
        let mut stream = handler.handle_call(ctx, Value::Null).await;

        let item = stream.next().await.unwrap();
        match item {
            ToolStreamItem::Terminal(Err(ref e))
                if e.kind == distill_tool_runtime::ToolErrorKind::Execution =>
            {
                assert!(
                    e.detail.contains("connection reset"),
                    "expected 'connection reset' in: {}",
                    e.detail
                );
            }
            other => panic!("expected Terminal(Err(Execution)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bridge_shutdown_closes_transport() {
        let mock = Arc::new(MockTransport::new(sample_server_info(), sample_tools()));
        let transport: Arc<dyn McpTransport> = Arc::clone(&mock) as Arc<dyn McpTransport>;
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: None,
        };

        let handle = McpBridge::connect(transport, &config).await.unwrap();
        assert!(!mock.closed.load(Ordering::SeqCst));

        handle.bridge.shutdown().await.unwrap();
        assert!(mock.closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bridge_skips_tools_with_invalid_names() {
        let tools = vec![
            McpToolDefinition {
                name: "valid_tool".into(),
                description: Some("a valid tool".into()),
                input_schema: None,
            },
            McpToolDefinition {
                name: "".into(),
                description: Some("empty name".into()),
                input_schema: None,
            },
        ];
        let transport = make_transport(MockTransport::new(sample_server_info(), tools));
        let config = McpBridgeConfig {
            session_id: SessionId::new("test-session").unwrap(),
            namespace: None,
        };

        let handle = McpBridge::connect(transport, &config).await.unwrap();
        assert_eq!(handle.bridge.tool_count(), 1);
        assert_eq!(handle.bridge.handlers()[0].tool_id.as_str(), "valid_tool");
    }

    #[test]
    fn translate_mcp_result_keeps_call_tool_result_shape() {
        let value = translate_mcp_result(McpCallResult {
            content: vec![
                McpContent::Text {
                    text: "hello".into(),
                },
                McpContent::Image {
                    mime_type: "image/png".into(),
                    data: "AAAA".into(),
                },
                McpContent::Resource {
                    uri: "file:///test".into(),
                    mime_type: Some("text/plain".into()),
                    text: Some("content".into()),
                },
            ],
            is_error: false,
        })
        .unwrap();

        assert_eq!(
            value,
            json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                    {"type": "resource", "uri": "file:///test", "mimeType": "text/plain", "text": "content"},
                ],
                "isError": false,
            })
        );
    }

    #[test]
    fn translate_mcp_result_empty_content_reaches_model_as_empty_text() {
        for is_error in [false, true] {
            let value = translate_mcp_result(McpCallResult {
                content: vec![],
                is_error,
            })
            .unwrap();
            assert_eq!(
                value,
                json!({"content": [{"type": "text", "text": ""}], "isError": is_error})
            );

            let typed = TypedToolOutput::from_value(ToolId::new("search").unwrap(), value);
            assert_eq!(typed.model_output, vec![text_block("")]);
        }
    }
}
