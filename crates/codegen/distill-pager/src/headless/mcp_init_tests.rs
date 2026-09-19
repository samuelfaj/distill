// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentMessage;

use super::resolve_mcp_servers_until;
use crate::views::mcps_modal::{McpsListResponse, McpsServerEntry, McpsServerSession};

fn list_response(name: &str, status: &str, resolved: bool) -> serde_json::Value {
    serde_json::json!({
        "result": {
            "sessionMcpResolved": resolved,
            "servers": [{
                "name": name,
                "source": "local",
                "type": "stdio",
                "session": {
                    "enabled": true,
                    "status": status,
                    "authRequired": false,
                    "setupRequired": false
                }
            }]
        }
    })
}

fn session(
    name: &str,
    enabled: bool,
    status: Option<&str>,
    auth: bool,
    setup: bool,
) -> McpsServerEntry {
    McpsServerEntry {
        name: name.to_string(),
        display_name: None,
        source: Some("local".to_string()),
        source_label: None,
        config_type: Some("stdio".to_string()),
        setup: None,
        setup_values: None,
        session: Some(McpsServerSession {
            enabled,
            status: status.map(str::to_string),
            tools: vec![],
            auth_required: auth,
            setup_required: setup,
            blocked_reason: None,
        }),
    }
}

enum McpListReply {
    Body(serde_json::Value),
    Fail,
    Hang,
}

fn spawn_mcp_list_agent(
    replies: Vec<McpListReply>,
) -> (distill_acp_lib::AcpAgentTx, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_task = Arc::clone(&calls);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        let mut held = Vec::new();
        while let Some(msg) = rx.recv().await {
            let AcpAgentMessage::ExtMethod(args) = msg else {
                continue;
            };
            calls_task.fetch_add(1, Ordering::SeqCst);
            match replies.next() {
                Some(McpListReply::Body(body)) => {
                    let raw = serde_json::value::to_raw_value(&body).unwrap();
                    let _ = args
                        .response_tx
                        .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                }
                Some(McpListReply::Fail) => {
                    let _ = args.response_tx.send(Err(acp::Error::internal_error()));
                }
                Some(McpListReply::Hang) | None => held.push(args.response_tx),
            }
        }
    });
    (tx, calls)
}

fn status_map(
    servers: &[crate::headless::reducer::McpServer],
) -> std::collections::HashMap<&str, &str> {
    servers
        .iter()
        .map(|server| (server.name.as_str(), server.status.as_str()))
        .collect()
}

fn configured_pending_cwd() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated cwd");
    let grok = dir.path().join(".grok");
    std::fs::create_dir_all(&grok).expect(".grok");
    std::fs::write(
        grok.join("config.toml"),
        "[mcp_servers.configured]\ncommand = \"false\"\n",
    )
    .expect("mcp config");
    dir
}

#[test]
fn init_wire_status_uses_typed_classifier_and_messages_tokens() {
    let list = McpsListResponse {
        session_mcp_resolved: Some(true),
        servers: vec![
            session("ready", true, Some("ready"), false, false),
            session("auth", true, Some("unavailable"), true, false),
            session("setup", true, Some("setuprequired"), false, true),
            session("down", false, None, false, false),
            session("gone", true, Some("unavailable"), false, false),
            session("boot", true, Some("initializing"), false, false),
        ],
    };
    let mapped_servers = super::servers_from_list(list);
    let mapped = status_map(&mapped_servers);
    assert_eq!(mapped.get("ready").copied(), Some("connected"));
    assert_eq!(mapped.get("auth").copied(), Some("needs-auth"));
    assert_eq!(mapped.get("setup").copied(), Some("needs-auth"));
    assert_eq!(mapped.get("down").copied(), Some("disabled"));
    assert_eq!(mapped.get("gone").copied(), Some("failed"));
    assert_eq!(mapped.get("boot").copied(), Some("pending"));
}

#[test]
fn unresolved_unavailable_is_pending_even_when_list_enabled_is_false() {
    let list = McpsListResponse {
        session_mcp_resolved: Some(false),
        servers: vec![
            session("unclaimed", false, None, false, false),
            session("gone", true, Some("unavailable"), false, false),
        ],
    };
    let mapped_servers = super::servers_from_list(list);
    let mapped = status_map(&mapped_servers);
    assert_eq!(mapped.get("unclaimed").copied(), Some("pending"));
    assert_eq!(mapped.get("gone").copied(), Some("pending"));
}

#[tokio::test]
async fn resolved_list_returns_without_waiting_out_the_grace() {
    let (tx, calls) = spawn_mcp_list_agent(vec![McpListReply::Body(list_response(
        "goodmcp", "ready", true,
    ))]);
    let session_id = acp::SessionId::new("sess-1");
    let t0 = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let servers = resolve_mcp_servers_until(&tx, &session_id, Path::new("."), deadline).await;
    assert_eq!(
        status_map(&servers).get("goodmcp").copied(),
        Some("connected")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(t0.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn progressive_takes_one_unresolved_snapshot() {
    let (tx, calls) = spawn_mcp_list_agent(vec![McpListReply::Body(list_response(
        "slow",
        "initializing",
        false,
    ))]);
    let session_id = acp::SessionId::new("sess-1");
    let t0 = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let servers = resolve_mcp_servers_until(&tx, &session_id, Path::new("."), deadline).await;
    assert_eq!(status_map(&servers).get("slow").copied(), Some("pending"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(t0.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn unresolved_snapshot_does_not_sleep_for_a_second_list() {
    let (tx, calls) = spawn_mcp_list_agent(vec![
        McpListReply::Body(list_response("slow", "initializing", false)),
        McpListReply::Body(list_response("slow", "unavailable", true)),
    ]);
    let session_id = acp::SessionId::new("sess-1");
    let t0 = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let servers = resolve_mcp_servers_until(&tx, &session_id, Path::new("."), deadline).await;
    assert_eq!(status_map(&servers).get("slow").copied(), Some("pending"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(t0.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn hung_list_returns_without_polling_past_the_deadline() {
    let (tx, calls) = spawn_mcp_list_agent(vec![McpListReply::Hang]);
    let session_id = acp::SessionId::new("sess-1");
    let cwd = configured_pending_cwd();
    let t0 = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(80);
    let servers = resolve_mcp_servers_until(&tx, &session_id, cwd.path(), deadline).await;
    assert_eq!(
        status_map(&servers).get("configured").copied(),
        Some("pending")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(t0.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn failed_list_does_not_retry() {
    let (tx, calls) = spawn_mcp_list_agent(vec![
        McpListReply::Fail,
        McpListReply::Body(list_response("goodmcp", "ready", true)),
    ]);
    let session_id = acp::SessionId::new("sess-1");
    let cwd = configured_pending_cwd();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let servers = resolve_mcp_servers_until(&tx, &session_id, cwd.path(), deadline).await;
    assert_eq!(
        status_map(&servers).get("configured").copied(),
        Some("pending")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
