// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for [`super`] (the grok.com relay connection loop).
//! Extracted from `relay.rs` so the implementation reads top-to-bottom; wired in via `#[path = "relay_tests.rs"] mod tests;`.
use super::*;
use distill_login::AuthMode;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio_tungstenite::tungstenite::{Utf8Bytes, protocol::Role};
/// Create an in-memory WebSocket pair (no network, no handshake needed).
async fn ws_pair() -> (
    tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
    tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let client_ws =
        tokio_tungstenite::WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let server_ws =
        tokio_tungstenite::WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    (client_ws, server_ws)
}
#[test]
fn test_handshake_401_detected_through_anyhow_context() {
    use tokio_tungstenite::tungstenite::Error as WsError;
    let resp = axum::http::Response::builder()
        .status(401)
        .body(None::<Vec<u8>>)
        .unwrap();
    let err =
        anyhow::Error::from(WsError::Http(Box::new(resp))).context("WebSocket connection failed");
    assert!(is_handshake_unauthorized(&err));
}
#[test]
fn test_handshake_non_401_and_non_ws_errors_rejected() {
    use tokio_tungstenite::tungstenite::Error as WsError;
    let resp = axum::http::Response::builder()
        .status(403)
        .body(None::<Vec<u8>>)
        .unwrap();
    let err =
        anyhow::Error::from(WsError::Http(Box::new(resp))).context("WebSocket connection failed");
    assert!(!is_handshake_unauthorized(&err));
    let err = anyhow::anyhow!("some random error");
    assert!(!is_handshake_unauthorized(&err));
}
#[tokio::test]
async fn test_ws_session_auth_error_returns_auth_error() {
    let (client_ws, server_ws) = ws_pair().await;
    let (mut server_tx, _server_rx) = server_ws.split();
    let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        let auth_error = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32000, "message": "Authentication required" }
        });
        let _ = server_tx
            .send(Message::Text(Utf8Bytes::from(auth_error.to_string())))
            .await;
        let _ = server_tx.close().await;
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
    )
    .await
    .expect("test timed out")
    .expect("session should not error");
    assert_eq!(result, SessionEndReason::AuthError);
}
/// The writer can win the reader/writer select while the `-32000` frame is still unread (here: the agent outbound
/// channel is already closed, so the writer exits at once). The session must still be classified as an auth error,
/// not a normal close, or the reconnect loop would reset its backoff. Repeated because `select!` picks a random order.
#[tokio::test]
async fn test_ws_session_writer_exit_does_not_mask_pending_auth_error() {
    for _ in 0..20 {
        let (client_ws, server_ws) = ws_pair().await;
        let (mut server_tx, _server_rx) = server_ws.split();
        let auth_error = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": AUTH_ERROR_CODE, "message": "Authentication required" }
        });
        server_tx
            .send(Message::Text(Utf8Bytes::from(auth_error.to_string())))
            .await
            .unwrap();
        let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
        let (agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
        drop(agent_out_tx);
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
        )
        .await
        .expect("test timed out")
        .expect("session should not error");
        assert_eq!(result, SessionEndReason::AuthError);
    }
}
#[tokio::test]
async fn test_ws_session_non_auth_error_skipped() {
    let (client_ws, server_ws) = ws_pair().await;
    let (mut server_tx, _server_rx) = server_ws.split();
    let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        let other_error = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32600, "message": "Invalid Request" }
        });
        let _ = server_tx
            .send(Message::Text(Utf8Bytes::from(other_error.to_string())))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = server_tx.close().await;
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
    )
    .await
    .expect("test timed out")
    .expect("session should not error");
    assert_eq!(
        result,
        SessionEndReason::Normal {
            authenticated: false
        }
    );
}
#[tokio::test]
async fn test_ws_session_normal_close_returns_normal() {
    let (client_ws, server_ws) = ws_pair().await;
    let (mut server_tx, _server_rx) = server_ws.split();
    let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        let _ = server_tx.send(Message::Close(None)).await;
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
    )
    .await
    .expect("test timed out")
    .expect("session should not error");
    assert_eq!(
        result,
        SessionEndReason::Normal {
            authenticated: false
        }
    );
}
#[tokio::test]
async fn test_ws_session_read_liveness_timeout_ends_session() {
    let (client_ws, server_ws) = ws_pair().await;
    let _silent_server = server_ws;
    let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session_with_liveness(
            client_ws,
            &to_agent_tx,
            &mut agent_out_rx,
            &cancel,
            Duration::from_millis(100),
        ),
    )
    .await
    .expect("session must end via read-liveness timeout instead of hanging")
    .expect("session should not error");
    assert_eq!(
        result,
        SessionEndReason::Normal {
            authenticated: false
        }
    );
}
#[tokio::test]
async fn test_ws_session_inbound_traffic_resets_liveness_window() {
    let (client_ws, server_ws) = ws_pair().await;
    let (mut server_tx, _server_rx) = server_ws.split();
    let (to_agent_tx, mut to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        for i in 0..12 {
            let msg = json!({ "jsonrpc": "2.0", "method": "ping", "id": i });
            if server_tx
                .send(Message::Text(Utf8Bytes::from(msg.to_string())))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = server_tx.close().await;
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session_with_liveness(
            client_ws,
            &to_agent_tx,
            &mut agent_out_rx,
            &cancel,
            Duration::from_millis(200),
        ),
    )
    .await
    .expect("test timed out")
    .expect("session should not error");
    assert_eq!(
        result,
        SessionEndReason::Normal {
            authenticated: true
        }
    );
    let mut forwarded = 0;
    while to_agent_rx.try_recv().is_ok() {
        forwarded += 1;
    }
    assert_eq!(forwarded, 12);
}
#[tokio::test]
async fn test_ws_session_forwards_text_to_agent() {
    let (client_ws, server_ws) = ws_pair().await;
    let (mut server_tx, _server_rx) = server_ws.split();
    let (to_agent_tx, mut to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let test_msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let msg_str = test_msg.to_string();
    tokio::spawn(async move {
        let _ = server_tx
            .send(Message::Text(Utf8Bytes::from(msg_str)))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = server_tx.close().await;
    });
    let _result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
    )
    .await
    .expect("test timed out");
    let received = to_agent_rx
        .try_recv()
        .expect("should have forwarded message to agent");
    let received_json: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(
        received_json.get("method").and_then(|v| v.as_str()),
        Some("initialize")
    );
}
#[tokio::test]
async fn test_ws_session_cancel_stops_session() {
    let (client_ws, server_ws) = ws_pair().await;
    let _server_ws = server_ws;
    let (to_agent_tx, _to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (_agent_out_tx, mut agent_out_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_websocket_session(client_ws, &to_agent_tx, &mut agent_out_rx, &cancel),
    )
    .await
    .expect("test timed out")
    .expect("session should not error");
    assert_eq!(
        result,
        SessionEndReason::Normal {
            authenticated: false
        }
    );
}
/// Helper to create a test GrokAuth with the given key.
fn test_auth(key: &str) -> GrokAuth {
    GrokAuth {
        key: key.to_string(),
        refresh_token: Some("rt".to_string()),
        ..GrokAuth::test_default()
    }
}
#[test]
fn for_session_builds_only_for_xai_issuer() {
    use distill_login::XAI_OAUTH2_ISSUER;
    let cfg = GrokComConfig::default();
    let builds = |a: &GrokAuth| RelayConfig::for_session(a, &cfg, None, None).is_some();
    let xai = GrokAuth {
        auth_mode: AuthMode::Oidc,
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..test_auth("xai-bearer")
    };
    assert!(xai.is_xai_auth(), "precondition: is_xai_auth");
    assert!(builds(&xai));
    let external_xai = GrokAuth {
        auth_mode: AuthMode::External,
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..test_auth("ext-bearer")
    };
    assert!(external_xai.is_xai_auth(), "precondition: is_xai_auth");
    assert!(builds(&external_xai));
    assert!(!builds(&GrokAuth {
        key: String::new(),
        ..xai.clone()
    }));
    assert!(!builds(&GrokAuth {
        auth_mode: AuthMode::ApiKey,
        ..test_auth("k")
    }));
    assert!(!builds(&GrokAuth {
        auth_mode: AuthMode::External,
        ..test_auth("k")
    }));
    assert!(!builds(&GrokAuth {
        auth_mode: AuthMode::WebLogin,
        ..test_auth("k")
    }));
    assert!(!builds(&GrokAuth {
        auth_mode: AuthMode::Oidc,
        oidc_issuer: Some("https://login.acme-corp.example/oauth2".to_string()),
        ..test_auth("k")
    }));
    assert!(!builds(&GrokAuth {
        auth_mode: AuthMode::External,
        oidc_issuer: Some("https://login.acme-corp.example/oauth2".to_string()),
        ..test_auth("k")
    }));
}
/// Helper: write a GrokAuth to disk under the given scope.
fn write_test_auth_to_disk(dir: &std::path::Path, scope: &str, auth: &GrokAuth) {
    let path = dir.join("auth.json");
    let mut map = distill_login::read_auth_json(&path).unwrap_or_default();
    map.insert(scope.to_owned(), auth.clone());
    let json = serde_json::to_string_pretty(&map).unwrap();
    std::fs::write(&path, json).unwrap();
}
/// Regression: `auth.json` vanishes (deleted, corrupt, or externally removed). The process still holds an expired access token and a valid refresh token in `AuthManager` memory.
/// Relay 401 recovery must drive the full refresh chain (mint a fresh token via the refresher and REWRITE `auth.json`) instead of dead-ending.
/// A relay holding a private, refresher-less `AuthManager` fails this: it can only adopt sibling disk tokens, and there are none.
#[tokio::test]
async fn auth_recovery_refreshes_and_heals_missing_auth_json() {
    use distill_login::XAI_OAUTH2_ISSUER;
    use distill_login::refresh::{RefreshOutcome, TokenRefresher};
    use std::sync::atomic::AtomicU32;
    struct CountingRefresher {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl TokenRefresher for CountingRefresher {
        async fn refresh(&self, _reason: distill_login::manager::RefreshReason) -> RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::Success(Box::new(GrokAuth {
                key: "fresh-from-authority".into(),
                auth_mode: AuthMode::Oidc,
                oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
                refresh_token: Some("rt-rotated".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            }))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = distill_login::GrokComConfig::default();
    let scope = cfg.auth_scope();
    let am = Arc::new(
        AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
    );
    let expired_session = GrokAuth {
        auth_mode: AuthMode::Oidc,
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        refresh_token: Some("rt-valid-unconsumed".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(14)),
        ..test_auth("expired-overnight")
    };
    am.hot_swap(expired_session.clone());
    assert!(
        !dir.path().join("auth.json").exists(),
        "precondition: no auth.json on disk"
    );
    let calls = Arc::new(AtomicU32::new(0));
    am.set_refresher(Arc::new(CountingRefresher {
        calls: calls.clone(),
    }));
    let mut config = RelayConfig::for_session(&expired_session, &cfg, None, Some(am.clone()))
        .expect("x.ai OIDC session is relay-eligible");
    let cancel = CancellationToken::new();
    let recovered = attempt_auth_recovery(&mut config, &cancel, "test 401").await;
    assert!(recovered, "recovery must succeed via the shared refresher");
    assert!(!cancel.is_cancelled(), "relay must keep running");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one IdP refresh");
    assert_eq!(config.auth.key, "fresh-from-authority");
    let store = distill_login::read_auth_json(&dir.path().join("auth.json"))
        .expect("auth.json must be recreated");
    let healed = store.get(&scope).expect("scope entry restored");
    assert_eq!(healed.key, "fresh-from-authority");
    assert_eq!(healed.refresh_token.as_deref(), Some("rt-rotated"));
}
/// Recovery returning the *unchanged* token (fresh-mint guard) must report no recovery, without cancelling the relay or touching the IdP.
/// The caller then backs off before reconnecting instead of tight-looping.
#[tokio::test]
async fn attempt_auth_recovery_same_key_backs_off_without_cancel() {
    use distill_login::XAI_OAUTH2_ISSUER;
    use distill_login::refresh::{RefreshOutcome, TokenRefresher};
    struct PanicRefresher;
    #[async_trait::async_trait]
    impl TokenRefresher for PanicRefresher {
        async fn refresh(&self, _reason: distill_login::manager::RefreshReason) -> RefreshOutcome {
            panic!("fresh-mint guard must keep recovery away from the IdP");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = distill_login::GrokComConfig::default();
    let am = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
    let fresh_session = GrokAuth {
        auth_mode: AuthMode::Oidc,
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        refresh_token: Some("rt-valid".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..test_auth("fresh-key")
    };
    am.hot_swap(fresh_session.clone());
    am.set_refresher(Arc::new(PanicRefresher));
    let mut config = RelayConfig::for_session(&fresh_session, &cfg, None, Some(am.clone()))
        .expect("x.ai OIDC session is relay-eligible");
    let cancel = CancellationToken::new();
    let recovered = attempt_auth_recovery(&mut config, &cancel, "test 401").await;
    assert!(!recovered, "same-key recovery must take the backoff path");
    assert!(!cancel.is_cancelled(), "relay must keep reconnecting");
    assert_eq!(config.auth.key, "fresh-key", "config auth stays unchanged");
}
#[tokio::test]
async fn test_auth_refresh_via_auth_manager_on_auth_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connection_count = Arc::new(AtomicU32::new(0));
    let count_clone = connection_count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let count = count_clone.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut tx, _rx) = ws.split();
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let auth_err = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": { "code": -32000, "message": "Token expired" }
                    });
                    let _ = tx
                        .send(Message::Text(Utf8Bytes::from(auth_err.to_string())))
                        .await;
                } else {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = distill_login::GrokComConfig::default();
    let scope = cfg.auth_scope();
    let am = Arc::new(AuthManager::new(dir.path(), cfg));
    am.hot_swap(test_auth("old-key"));
    write_test_auth_to_disk(dir.path(), &scope, &test_auth("new-key"));
    let config = RelayConfig {
        ws_url: format!("ws://{}", addr),
        ws_origin: format!("http://{}", addr),
        token_header: "test-token".to_string(),
        auth: test_auth("old-key"),
        auth_manager: Some(am),
    };
    let cancel = CancellationToken::new();
    let (from_relay_tx, _from_relay_rx) = mpsc::unbounded_channel();
    let (_to_relay_tx, _handle) = spawn_relay_connection(config, from_relay_tx, cancel.clone());
    tokio::time::sleep(Duration::from_secs(3)).await;
    cancel.cancel();
    assert!(
        connection_count.load(Ordering::SeqCst) >= 2,
        "should have connected at least twice (original + after refresh), got {}",
        connection_count.load(Ordering::SeqCst)
    );
}
#[tokio::test]
async fn test_auth_refresh_failure_continues_with_backoff() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connection_count = Arc::new(AtomicU32::new(0));
    let count_clone = connection_count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let count = count_clone.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut tx, _rx) = ws.split();
                count.fetch_add(1, Ordering::SeqCst);
                let auth_err = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": { "code": -32000, "message": "Token expired" }
                });
                let _ = tx
                    .send(Message::Text(Utf8Bytes::from(auth_err.to_string())))
                    .await;
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = distill_login::GrokComConfig::default();
    let scope = cfg.auth_scope();
    let am = Arc::new(AuthManager::new(dir.path(), cfg));
    am.hot_swap(test_auth("old-key"));
    write_test_auth_to_disk(dir.path(), &scope, &test_auth("old-key"));
    let config = RelayConfig {
        ws_url: format!("ws://{}", addr),
        ws_origin: format!("http://{}", addr),
        token_header: "test-token".to_string(),
        auth: test_auth("old-key"),
        auth_manager: Some(am),
    };
    let cancel = CancellationToken::new();
    let (from_relay_tx, _from_relay_rx) = mpsc::unbounded_channel();
    let (_to_relay_tx, _handle) = spawn_relay_connection(config, from_relay_tx, cancel.clone());
    tokio::time::sleep(Duration::from_secs(4)).await;
    cancel.cancel();
    assert!(
        connection_count.load(Ordering::SeqCst) >= 2,
        "should have retried after failed refresh, got {}",
        connection_count.load(Ordering::SeqCst)
    );
}
/// A non-sticky auth verdict (`ProviderInteractiveRequired`) must neither cancel the relay nor reconnect-storm, and a recovered credential must start the backoff over.
/// The mock relay accepts every WebSocket and rejects the bearer on the first frame, so the backoff must survive the connect. Connections 1–3 carry the rejected key: gaps grow 2s → 4s. Before connection 3 a new key lands on disk, so recovery adopts it and reconnects at once (connection 4). That key is rejected too; the following delay must be the base 2s again rather than the inherited 8s.
#[tokio::test]
async fn non_sticky_verdict_keeps_reconnecting_with_growing_backoff() {
    use distill_login::error::RefreshTokenFailedReason;
    use distill_login::refresh::{RefreshOutcome, TokenRefresher};
    use std::sync::Mutex;
    use tokio::sync::Notify;
    use tokio::time::Instant;
    struct InteractiveRequiredRefresher;
    #[async_trait::async_trait]
    impl TokenRefresher for InteractiveRequiredRefresher {
        async fn refresh(&self, _reason: distill_login::manager::RefreshReason) -> RefreshOutcome {
            RefreshOutcome::permanent(RefreshTokenFailedReason::ProviderInteractiveRequired, None)
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = distill_login::GrokComConfig::default();
    let scope = cfg.auth_scope();
    let am = Arc::new(AuthManager::new(dir.path(), cfg));
    am.hot_swap(test_auth("old-key"));
    write_test_auth_to_disk(dir.path(), &scope, &test_auth("old-key"));
    am.set_refresher(Arc::new(InteractiveRequiredRefresher));
    let connects: Arc<Mutex<Vec<(Instant, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let connected = Arc::new(Notify::new());
    let auth_dir = dir.path().to_path_buf();
    {
        let connects = connects.clone();
        let connected = connected.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let bearer = Arc::new(Mutex::new(String::new()));
                let bearer_cb = bearer.clone();
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                          resp| {
                        let auth = req
                            .headers()
                            .get("Authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .trim_start_matches("Bearer ")
                            .to_string();
                        *bearer_cb.lock().unwrap() = auth;
                        Ok(resp)
                    },
                )
                .await
                else {
                    continue;
                };
                let n = {
                    let mut c = connects.lock().unwrap();
                    c.push((Instant::now(), bearer.lock().unwrap().clone()));
                    c.len()
                };
                if n == 3 {
                    write_test_auth_to_disk(&auth_dir, &scope, &test_auth("new-key"));
                }
                connected.notify_one();
                let (mut tx, _rx) = ws.split();
                let auth_err = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": { "code": AUTH_ERROR_CODE, "message": "Token expired" }
                });
                let _ = tx
                    .send(Message::Text(Utf8Bytes::from(auth_err.to_string())))
                    .await;
            }
        });
    }
    let config = RelayConfig {
        ws_url: format!("ws://{}", addr),
        ws_origin: format!("http://{}", addr),
        token_header: "test-token".to_string(),
        auth: test_auth("old-key"),
        auth_manager: Some(am),
    };
    let cancel = CancellationToken::new();
    let (from_relay_tx, _from_relay_rx) = mpsc::unbounded_channel();
    let (_to_relay_tx, handle) = spawn_relay_connection(config, from_relay_tx, cancel.clone());
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            connected.notified().await;
            if connects.lock().unwrap().len() >= 5 {
                break;
            }
        }
    })
    .await
    .expect("relay stopped reconnecting");
    let relay_cancelled = handle.cancel.is_cancelled();
    cancel.cancel();
    assert!(
        !relay_cancelled,
        "a non-sticky verdict must not cancel the relay loop"
    );
    let connects = connects.lock().unwrap().clone();
    let keys: Vec<&str> = connects.iter().map(|(_, k)| k.as_str()).collect();
    assert_eq!(
        keys,
        ["old-key", "old-key", "old-key", "new-key", "new-key"],
        "recovery must adopt the rotated key and reconnect with it"
    );
    let gaps: Vec<Duration> = connects
        .windows(2)
        .filter_map(|w| {
            let [a, b] = w else {
                return None;
            };
            Some(b.0 - a.0)
        })
        .collect();
    let [g0, g1, g2, g3] = gaps.as_slice() else {
        panic!("expected 4 reconnect gaps: {gaps:?}");
    };
    assert!(
        *g1 >= *g0 + Duration::from_secs(1),
        "backoff must keep growing across auth failures, got gaps {gaps:?}"
    );
    assert!(
        *g2 < Duration::from_secs(1),
        "recovery with a new key must reconnect immediately, got gaps {gaps:?}"
    );
    assert!(
        *g3 < *g1,
        "a new key must start the backoff over, got gaps {gaps:?}"
    );
}
#[test]
fn relay_initialize_gains_user_message_echo_capability() {
    let mut frame = serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": { "_meta": { "x.ai/fs_notify": true } }
        }
    });
    assert!(declare_relay_client_capabilities(&mut frame));
    let meta = frame
        .pointer("/params/clientCapabilities/_meta")
        .expect("_meta present");
    assert_eq!(
        meta.get("x.ai/userMessageEcho"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(meta.get("x.ai/fs_notify"), Some(&serde_json::json!(true)));
    assert_eq!(frame.get("id"), Some(&serde_json::json!(7)));
}
#[test]
fn relay_initialize_without_capabilities_block_gets_one() {
    let mut frame = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": 1 }
    });
    assert!(declare_relay_client_capabilities(&mut frame));
    assert_eq!(
        frame.pointer("/params/clientCapabilities/_meta/x.ai~1userMessageEcho"),
        Some(&serde_json::json!(true))
    );
}
#[test]
fn relay_explicit_user_message_echo_is_respected() {
    let mut frame = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "clientCapabilities": { "_meta": { "x.ai/userMessageEcho": false } } }
    });
    assert!(!declare_relay_client_capabilities(&mut frame));
    assert_eq!(
        frame.pointer("/params/clientCapabilities/_meta/x.ai~1userMessageEcho"),
        Some(&serde_json::json!(false))
    );
}
#[test]
fn non_initialize_frames_are_left_alone() {
    for frame in [
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": "/w", "_meta": {} } }),
        serde_json::json!({ "jsonrpc": "2.0", "id": 3, "result": { "ok": true } }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s" } }),
    ] {
        let original = frame.clone();
        let mut frame = frame;
        assert!(!declare_relay_client_capabilities(&mut frame));
        assert_eq!(frame, original);
    }
}
