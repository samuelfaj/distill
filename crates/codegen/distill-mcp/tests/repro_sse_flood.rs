// Modified for Distill by Samuel Fajreldines, 2026.
//! Black-box check that `McpHttpClient` is invisible to a healthy streamable-HTTP MCP server.
//! The handshake and tools/list succeed and the standing GET opens exactly once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use distill_mcp::mcp_http_client::{McpHttpClient, WarnBudget};
use distill_mcp::rmcp::ServiceExt;
use distill_mcp::rmcp::transport::StreamableHttpClientTransport;
use distill_mcp::rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

#[derive(Clone)]
struct ServerState {
    gets: Arc<AtomicUsize>,
}

async fn handle_post(Json(req): Json<Value>) -> Response {
    match req["method"].as_str() {
        Some("initialize") => {
            let result = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "protocolVersion": req["params"]["protocolVersion"],
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "0.0.0"},
                },
            });
            ([("mcp-session-id", "fake-session-1")], Json(result)).into_response()
        }
        Some("tools/list") => {
            let result = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]},
            });
            Json(result).into_response()
        }
        // notifications/initialized and anything else.
        _ => StatusCode::ACCEPTED.into_response(),
    }
}

async fn handle_get(State(state): State<ServerState>) -> Response {
    state.gets.fetch_add(1, Ordering::Relaxed);
    let body = Body::from_stream(futures::stream::pending::<Result<String, std::io::Error>>());
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn spawn_fake_server() -> (String, Arc<AtomicUsize>) {
    let gets = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/mcp", get(handle_get).post(handle_post))
        .with_state(ServerState { gets: gets.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/mcp"), gets)
}

#[tokio::test(flavor = "multi_thread")]
async fn throttled_client_does_not_affect_healthy_server() {
    let (url, gets) = spawn_fake_server().await;
    let throttled = McpHttpClient::new(
        reqwest::Client::default(),
        "fake-server",
        WarnBudget::default(),
    );
    let client = ()
        .serve(StreamableHttpClientTransport::with_client(
            throttled,
            StreamableHttpClientTransportConfig::with_uri(url.as_str()),
        ))
        .await
        .expect("handshake against healthy server should succeed");

    let tools = client
        .list_tools(Default::default())
        .await
        .expect("tools/list should succeed through the throttled client");
    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "echo");

    tokio::time::sleep(Duration::from_secs(3)).await;
    let n = gets.load(Ordering::Relaxed);
    let _ = client.cancel().await;

    eprintln!("[repro] healthy server: {n} GET(s) in 3s, tools/list ok");
    assert_eq!(
        n, 1,
        "healthy stream must be opened once and never reconnected"
    );
}
