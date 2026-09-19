// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_client_protocol as acp;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use serial_test::serial;
use distill_mcp::credentials::McpCredentialStore;
use distill_mcp::rmcp;
use distill_mcp::servers::{McpOauthDiscovery, McpSpawnCtx, OauthInteractivity, start_mcp_server};

const TOKEN: &str = "at-123";

async fn spawn_counting_gated_server() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let token_grants = Arc::new(AtomicUsize::new(0));
    let handler_requests = Arc::clone(&requests);
    let handler_token_grants = Arc::clone(&token_grants);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake server");
    let addr = listener.local_addr().expect("fake server addr");
    // rmcp 3.x enforces RFC 8414 issuer validation: the advertised issuer must equal
    // the issuer the client derived from the MCP base URL (including its path).
    let issuer = format!("http://{addr}/mcp");
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let requests = Arc::clone(&handler_requests);
        let token_grants = Arc::clone(&handler_token_grants);
        let issuer = issuer.clone();
        async move {
            requests.fetch_add(1, Ordering::SeqCst);
            let path = req.uri().path().to_string();
            if path.contains("oauth-authorization-server") {
                return axum::Json(json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                }))
                .into_response();
            }
            if path.contains("oauth-protected-resource") {
                return StatusCode::NOT_FOUND.into_response();
            }
            if path.ends_with("/token") {
                token_grants.fetch_add(1, Ordering::SeqCst);
                return axum::Json(json!({
                    "access_token": TOKEN,
                    "token_type": "bearer",
                    "expires_in": 3600,
                    "refresh_token": "rt-fresh",
                }))
                .into_response();
            }
            let authorized = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Bearer {TOKEN}"));
            if !authorized {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            if req.method() == axum::http::Method::GET {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(futures::stream::pending::<Result<String, std::io::Error>>()),
                )
                    .into_response();
            }
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            let msg: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            match msg["method"].as_str() {
                Some("initialize") => {
                    let result = json!({
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "result": {
                            "protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": {},
                            "serverInfo": {"name": "fake", "version": "0.0.0"},
                        },
                    });
                    ([("mcp-session-id", "fake-session-1")], axum::Json(result)).into_response()
                }
                Some("tools/list") => axum::Json(json!({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"tools": []},
                }))
                .into_response(),
                // Legacy server: reject the SEP-2575 `server/discover` probe so the
                // client falls back to `initialize` without waiting out the probe timeout.
                Some("server/discover") => axum::Json(json!({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "error": {"code": -32601, "message": "Method not found"},
                }))
                .into_response(),
                _ => StatusCode::ACCEPTED.into_response(),
            }
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/mcp"), requests, token_grants)
}

fn make_http_server(name: &str, url: &str) -> acp::McpServer {
    acp::McpServer::Http(acp::McpServerHttp::new(name, url))
}

fn session_ctx(event_writer: &distill_session_events::EventWriter) -> McpSpawnCtx<'_> {
    distill_mcp::isolate_distill_home_for_tests();
    McpSpawnCtx::for_session(
        "sess",
        event_writer,
        OauthInteractivity::NonInteractive,
        None,
    )
}

fn seed_login_without_token(server_name: &str, url: &url::Url) {
    distill_mcp::isolate_distill_home_for_tests();
    McpCredentialStore::load_default()
        .unwrap_or_default()
        .insert_and_save(
            server_name,
            url,
            rmcp::transport::auth::StoredCredentials::new("c".to_string(), None, Vec::new(), None),
        )
        .expect("seed credential store");
}

fn seed_stored_token(server_name: &str, url: &url::Url) {
    distill_mcp::isolate_distill_home_for_tests();
    let creds: rmcp::transport::auth::StoredCredentials = serde_json::from_str(&format!(
        r#"{{"client_id":"c","token_response":{{"access_token":"{TOKEN}","token_type":"bearer","refresh_token":"rt-1"}}}}"#
    ))
    .expect("stored credentials literal");
    McpCredentialStore::load_default()
        .unwrap_or_default()
        .insert_and_save(server_name, url, creds)
        .expect("seed credential store");
}

fn seed_expired_token(server_name: &str, url: &url::Url, refresh_token: Option<&str>) {
    distill_mcp::isolate_distill_home_for_tests();
    let received_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        - 3600;
    let refresh = refresh_token
        .map(|rt| format!(r#","refresh_token":"{rt}""#))
        .unwrap_or_default();
    let creds: rmcp::transport::auth::StoredCredentials = serde_json::from_str(&format!(
        r#"{{"client_id":"c","token_response":{{"access_token":"stale","token_type":"bearer","expires_in":60{refresh}}},"token_received_at":{received_at}}}"#
    ))
    .expect("stored credentials literal");
    McpCredentialStore::load_default()
        .unwrap_or_default()
        .insert_and_save(server_name, url, creds)
        .expect("seed credential store");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn stored_token_lifecycle_across_spawns() {
    let (url, requests, token_grants) = spawn_counting_gated_server().await;
    seed_stored_token("seeded", &url::Url::parse(&url).unwrap());

    let event_writer = distill_session_events::EventWriter::noop();

    let requests_before = requests.load(Ordering::SeqCst);
    let client = start_mcp_server(
        make_http_server("seeded", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer),
    )
    .await
    .expect("stored credentials classify to a client");
    assert!(
        client.has_auth(),
        "stored token must yield an auth-managed client"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst) - requests_before,
        0,
        "the stored-credentials spawn branch must perform zero network requests"
    );

    let requests_before = requests.load(Ordering::SeqCst);
    let network_client = start_mcp_server(
        make_http_server("seeded", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer).with_oauth_discovery(McpOauthDiscovery::Network),
    )
    .await
    .expect("stored credentials classify to a client on the network path");
    assert!(
        network_client.has_auth(),
        "network-path spawn must reuse the stored token"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst) - requests_before,
        0,
        "disk-first: the network path spends zero requests on discovery"
    );

    seed_expired_token("expired", &url::Url::parse(&url).unwrap(), Some("rt-2"));
    let grants_before = token_grants.load(Ordering::SeqCst);
    let refreshed_client = start_mcp_server(
        make_http_server("expired", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer).with_oauth_discovery(McpOauthDiscovery::Network),
    )
    .await
    .expect("an expired-but-refreshable token must classify to a client");
    assert!(
        refreshed_client.has_auth(),
        "the probe must refresh, not fail closed as NeedsInteractiveLogin"
    );
    assert_eq!(
        token_grants.load(Ordering::SeqCst) - grants_before,
        1,
        "recovery went through exactly one refresh grant"
    );

    seed_expired_token("expired-norefresh", &url::Url::parse(&url).unwrap(), None);
    let grants_before = token_grants.load(Ordering::SeqCst);
    let Err(err) = start_mcp_server(
        make_http_server("expired-norefresh", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer).with_oauth_discovery(McpOauthDiscovery::Network),
    )
    .await
    else {
        panic!("a genuinely-unusable token must fail the spawn closed");
    };
    assert!(
        err.is_auth_rejection(),
        "post-hydration unusable tokens fail closed as auth-required: {err}"
    );
    assert_eq!(
        token_grants.load(Ordering::SeqCst) - grants_before,
        0,
        "no refresh grant is attempted without a refresh token"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn entry_without_token_fail_fasts_until_out_of_band_login() {
    let (url, requests, _token_grants) = spawn_counting_gated_server().await;
    let parsed_url = url::Url::parse(&url).unwrap();
    let event_writer = distill_session_events::EventWriter::noop();

    seed_login_without_token("gated", &parsed_url);

    let Err(err) = start_mcp_server(
        make_http_server("gated", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer),
    )
    .await
    else {
        panic!("an entry without a token must fail the spawn closed");
    };
    assert!(err.is_auth_rejection());
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the fail-fast is decided from disk, zero requests"
    );

    seed_stored_token("gated", &parsed_url);

    let client = start_mcp_server(
        make_http_server("gated", &url),
        None,
        None,
        None,
        &session_ctx(&event_writer),
    )
    .await
    .expect("stored token must classify to an auth-managed client");
    assert!(client.has_auth(), "rebuild must be auth-managed");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the rebuild itself is network-free"
    );
    client
        .ensure_initialized()
        .await
        .expect("handshake with the stored token, no discovery");
}
