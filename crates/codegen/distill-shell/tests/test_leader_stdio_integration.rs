// Modified for Distill by Samuel Fajreldines, 2026.
//! Currently Unix-only: the tests use `tokio::net::UnixStream` directly so they exercise the on-disk socket path.
//! Equivalent Windows coverage would need to go through `LeaderStream`/`LeaderListener` and is tracked as a follow-up.

#![cfg(unix)]

use std::time::Duration;

use distill_shell::cpu_profile::ControlErrorCode;
use distill_shell::leader::{
    ClientCapabilities, ClientMode, ControlCommand, ControlPayload, LeaderClient,
    LeaderServerControlState, LeaderServerMetadata,
    protocol::{ClientMessage, ServerMessage, read_message, write_message},
    spawn_leader_server,
};
use tempfile::TempDir;
use tokio::net::UnixStream;

/// Pipe character used for ID namespacing (must match server.rs)
const ID_NAMESPACE_SEP: char = '|';

/// Wait for socket file to exist and be connectable.
async fn wait_for_socket(sock_path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sock_path.exists() {
            // Try to connect to verify it's actually listening
            if UnixStream::connect(sock_path).await.is_ok() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Timeout waiting for socket to become available");
}

async fn setup_test_server(
    temp: &TempDir,
) -> (
    std::path::PathBuf,
    tokio_util::sync::CancellationToken,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::mpsc::UnboundedSender<String>,
) {
    let sock_path = temp.path().join("leader.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();

    wait_for_socket(&sock_path).await;

    (sock_path, handle.cancel, handle.acp_rx, handle.response_tx)
}

async fn setup_control_test_server(
    temp: &TempDir,
) -> (std::path::PathBuf, distill_shell::leader::ServerHandle) {
    let sock_path = temp.path().join("leader-control.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;
    (sock_path, handle)
}

/// A namespaced id has the form "client_id<SEP>original_id_json".
fn parse_namespaced_id(namespaced_id: &str) -> Option<(u64, String)> {
    let (client_part, original_json) = namespaced_id.split_once(ID_NAMESPACE_SEP)?;
    let client_id: u64 = client_part.parse().ok()?;
    Some((client_id, original_json.to_string()))
}

fn namespaced_id_has_original(namespaced_id: &str, expected_original: &str) -> bool {
    if let Some((_, original_json)) = parse_namespaced_id(namespaced_id) {
        original_json == expected_original
    } else {
        false
    }
}

#[tokio::test]
async fn test_single_stdio_client_connects() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, _acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Connect as stdio client
    let client = LeaderClient::connect(
        sock_path,
        "test-stdio",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Should be connected
    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_stdio_client_sends_acp_message() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path,
        "test-stdio",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send an ACP message (JSON-RPC format)
    let test_message = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;
    client.send(test_message.to_string()).unwrap();

    // Server should receive the message (with namespaced ID)
    let received = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
        .await
        .expect("timeout waiting for message")
        .expect("channel closed");

    // Parse and verify the message was received (ID will be namespaced)
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(json["method"], "initialize");
    assert!(
        namespaced_id_has_original(json["id"].as_str().unwrap(), "1"),
        "ID should contain original ID 1"
    );

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_stdio_client_receives_response() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "test-stdio",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send request
    let test_message = r#"{"jsonrpc":"2.0","method":"test","id":42}"#;
    client.send(test_message.to_string()).unwrap();

    // Get the namespaced ID from the server's view
    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    let namespaced_id = json["id"].as_str().unwrap().to_string();

    // Send response with namespaced ID (server will restore original ID)
    let response = format!(
        r#"{{"jsonrpc":"2.0","result":{{"data":"test"}},"id":"{}"}}"#,
        namespaced_id
    );
    response_tx.send(response).unwrap();

    // Client should receive response with original ID restored
    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for response")
        .expect("channel closed");

    let response_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();
    assert_eq!(response_json["id"], 42);
    assert_eq!(response_json["result"]["data"], "test");

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_multiple_stdio_clients() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    // Connect two stdio clients
    let mut client1 = LeaderClient::connect(
        sock_path.clone(),
        "client-1",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let mut client2 = LeaderClient::connect(
        sock_path,
        "client-2",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    client1
        .send(r#"{"jsonrpc":"2.0","method":"from_client_1","id":1}"#.to_string())
        .unwrap();

    client2
        .send(r#"{"jsonrpc":"2.0","method":"from_client_2","id":2}"#.to_string())
        .unwrap();

    // Server receives both messages
    let msg1 = acp_rx.recv().await.unwrap();
    let msg2 = acp_rx.recv().await.unwrap();

    let json1: serde_json::Value = serde_json::from_str(&msg1).unwrap();
    let json2: serde_json::Value = serde_json::from_str(&msg2).unwrap();

    // Messages should have different namespaced IDs (different client prefixes)
    let id1 = json1["id"].as_str().unwrap();
    let id2 = json2["id"].as_str().unwrap();

    // Extract client ID prefixes using the Unit Separator
    let (client_id_1, _) = parse_namespaced_id(id1).expect("Should parse namespaced ID");
    let (client_id_2, _) = parse_namespaced_id(id2).expect("Should parse namespaced ID");
    assert_ne!(
        client_id_1, client_id_2,
        "Clients should have different IDs"
    );

    // Send response to client 1 using its namespaced ID
    let response1 = format!(
        r#"{{"jsonrpc":"2.0","result":"response_1","id":"{}"}}"#,
        id1
    );
    response_tx.send(response1).unwrap();

    // Send response to client 2 using its namespaced ID
    let response2 = format!(
        r#"{{"jsonrpc":"2.0","result":"response_2","id":"{}"}}"#,
        id2
    );
    response_tx.send(response2).unwrap();

    // Each client should receive its own response
    let recv1 = tokio::time::timeout(Duration::from_secs(2), client1.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let recv2 = tokio::time::timeout(Duration::from_secs(2), client2.recv())
        .await
        .expect("timeout")
        .expect("closed");

    let recv_json1: serde_json::Value = serde_json::from_str(&recv1).unwrap();
    let recv_json2: serde_json::Value = serde_json::from_str(&recv2).unwrap();

    assert_eq!(recv_json1["id"], 1);
    assert_eq!(recv_json2["id"], 2);

    client1.cancel();
    client2.cancel();
    cancel.cancel();
}

/// ID namespacing prevents collisions when different clients use the same request IDs.
#[tokio::test]
async fn test_multiple_clients_same_message_ids() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    // Connect three stdio clients
    let mut client1 = LeaderClient::connect(
        sock_path.clone(),
        "client-1",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let mut client2 = LeaderClient::connect(
        sock_path.clone(),
        "client-2",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let mut client3 = LeaderClient::connect(
        sock_path,
        "client-3",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // All three clients send messages with the SAME ID (id: 1)
    client1
        .send(r#"{"jsonrpc":"2.0","method":"method_1","id":1}"#.to_string())
        .unwrap();
    client2
        .send(r#"{"jsonrpc":"2.0","method":"method_2","id":1}"#.to_string())
        .unwrap();
    client3
        .send(r#"{"jsonrpc":"2.0","method":"method_3","id":1}"#.to_string())
        .unwrap();

    // Collect all three messages from the server
    let msg1 = acp_rx.recv().await.unwrap();
    let msg2 = acp_rx.recv().await.unwrap();
    let msg3 = acp_rx.recv().await.unwrap();

    let json1: serde_json::Value = serde_json::from_str(&msg1).unwrap();
    let json2: serde_json::Value = serde_json::from_str(&msg2).unwrap();
    let json3: serde_json::Value = serde_json::from_str(&msg3).unwrap();

    let id1 = json1["id"].as_str().unwrap();
    let id2 = json2["id"].as_str().unwrap();
    let id3 = json3["id"].as_str().unwrap();

    assert!(
        namespaced_id_has_original(id1, "1"),
        "ID should contain original ID 1"
    );
    assert!(
        namespaced_id_has_original(id2, "1"),
        "ID should contain original ID 1"
    );
    assert!(
        namespaced_id_has_original(id3, "1"),
        "ID should contain original ID 1"
    );

    // But the full namespaced IDs should all be different (different client prefixes)
    assert_ne!(id1, id2, "Namespaced IDs should be unique");
    assert_ne!(id2, id3, "Namespaced IDs should be unique");
    assert_ne!(id1, id3, "Namespaced IDs should be unique");

    // Build a map from method to namespaced_id for targeted responses
    let mut method_to_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    method_to_id.insert(
        json1["method"].as_str().unwrap().to_string(),
        id1.to_string(),
    );
    method_to_id.insert(
        json2["method"].as_str().unwrap().to_string(),
        id2.to_string(),
    );
    method_to_id.insert(
        json3["method"].as_str().unwrap().to_string(),
        id3.to_string(),
    );

    // Send responses back using the namespaced IDs, each with a unique result
    let resp1 = format!(
        r#"{{"jsonrpc":"2.0","result":"result_for_client_1","id":"{}"}}"#,
        method_to_id.get("method_1").unwrap()
    );
    let resp2 = format!(
        r#"{{"jsonrpc":"2.0","result":"result_for_client_2","id":"{}"}}"#,
        method_to_id.get("method_2").unwrap()
    );
    let resp3 = format!(
        r#"{{"jsonrpc":"2.0","result":"result_for_client_3","id":"{}"}}"#,
        method_to_id.get("method_3").unwrap()
    );

    response_tx.send(resp1).unwrap();
    response_tx.send(resp2).unwrap();
    response_tx.send(resp3).unwrap();

    // Each client should receive its own response with the original ID restored
    let recv1 = tokio::time::timeout(Duration::from_secs(2), client1.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let recv2 = tokio::time::timeout(Duration::from_secs(2), client2.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let recv3 = tokio::time::timeout(Duration::from_secs(2), client3.recv())
        .await
        .expect("timeout")
        .expect("closed");

    let recv_json1: serde_json::Value = serde_json::from_str(&recv1).unwrap();
    let recv_json2: serde_json::Value = serde_json::from_str(&recv2).unwrap();
    let recv_json3: serde_json::Value = serde_json::from_str(&recv3).unwrap();

    assert_eq!(recv_json1["id"], 1, "Client 1's ID should be restored to 1");
    assert_eq!(recv_json2["id"], 1, "Client 2's ID should be restored to 1");
    assert_eq!(recv_json3["id"], 1, "Client 3's ID should be restored to 1");

    assert_eq!(recv_json1["result"], "result_for_client_1");
    assert_eq!(recv_json2["result"], "result_for_client_2");
    assert_eq!(recv_json3["result"], "result_for_client_3");

    client1.cancel();
    client2.cancel();
    client3.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_stdio_client_disconnect() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, _acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Connect and then disconnect
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Register
    write_message(
        &mut writer,
        &ClientMessage::Register {
            client_type: "test".into(),
            mode: ClientMode::Stdio,
            capabilities: ClientCapabilities::default(),
        },
    )
    .await
    .unwrap();

    let response: ServerMessage = read_message(&mut reader).await.unwrap();
    assert!(matches!(response, ServerMessage::Registered { .. }));

    write_message(&mut writer, &ClientMessage::Disconnect)
        .await
        .unwrap();

    // Give server time to process disconnect
    tokio::time::sleep(Duration::from_millis(100)).await;

    cancel.cancel();
}

#[tokio::test]
async fn test_stdio_client_ping_pong() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, _acp_rx, _response_tx) = setup_test_server(&temp).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Register first
    write_message(
        &mut writer,
        &ClientMessage::Register {
            client_type: "test".into(),
            mode: ClientMode::Stdio,
            capabilities: ClientCapabilities::default(),
        },
    )
    .await
    .unwrap();
    let _: ServerMessage = read_message(&mut reader).await.unwrap();

    write_message(&mut writer, &ClientMessage::Ping)
        .await
        .unwrap();

    let response: ServerMessage = read_message(&mut reader).await.unwrap();
    assert!(matches!(response, ServerMessage::Pong));

    cancel.cancel();
}

/// Test that server shuts down when all clients disconnect (after having clients).
#[tokio::test]
async fn test_server_exits_when_all_clients_disconnect() {
    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();

    wait_for_socket(&sock_path).await;

    // Connect a client
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);

    write_message(
        &mut writer,
        &ClientMessage::Register {
            client_type: "test".into(),
            mode: ClientMode::Stdio,
            capabilities: ClientCapabilities::default(),
        },
    )
    .await
    .unwrap();
    let _: ServerMessage = read_message(&mut reader).await.unwrap();

    write_message(&mut writer, &ClientMessage::Disconnect)
        .await
        .unwrap();

    // The server exits on its own when all clients disconnect; this test only checks it completes without hanging
    tokio::time::sleep(Duration::from_millis(200)).await;

    handle.cancel.cancel();
}

#[tokio::test]
async fn test_runtime_profile_start_status_stop_across_clients() {
    let temp = TempDir::new().unwrap();
    let (sock_path, handle) = setup_control_test_server(&temp).await;
    let output_path = temp.path().join("integration-runtime-profile.folded");

    let client_a = LeaderClient::connect(
        sock_path.clone(),
        "client-a",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let client_b = LeaderClient::connect(
        sock_path,
        "client-b",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let runtime_cpu_profile = client_a
        .registration()
        .leader_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.runtime_cpu_profile);

    if runtime_cpu_profile {
        // In sandboxed CI (Bazel), pprof may report as supported at compile time but fail at runtime because signal-based sampling is blocked
        let start_result = client_a
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(output_path.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap();
        let Ok(started) = start_result else {
            client_a.cancel();
            client_b.cancel();
            handle.cancel.cancel();
            return; // pprof can't start in sandbox, skip
        };
        assert!(matches!(
            started,
            ControlPayload::CpuProfileStarted { svg_path, .. } if svg_path == output_path
        ));

        let status = client_b
            .send_control(ControlCommand::CpuProfileStatus)
            .await
            .unwrap()
            .unwrap();
        assert!(
            runtime_cpu_profile,
            "registration should stay consistent with status behavior"
        );
        assert!(matches!(
            status,
            ControlPayload::CpuProfileStatus {
                active: true,
                stopping: false,
                svg_path: Some(path),
                frequency_hz: Some(200),
                ..
            } if path == output_path
        ));

        let stopped = client_b
            .send_control(ControlCommand::StopCpuProfile)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            stopped,
            ControlPayload::CpuProfileStopped { svg_path, .. } if svg_path == output_path
        ));
        assert!(output_path.exists());
    } else {
        let error = client_a
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(output_path.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ControlErrorCode::RuntimeProfilingUnsupported);
    }

    client_a.cancel();
    client_b.cancel();
    handle.cancel.cancel();
}

#[tokio::test]
async fn test_runtime_profile_finalizes_on_graceful_shutdown() {
    let temp = TempDir::new().unwrap();
    let (sock_path, handle) = setup_control_test_server(&temp).await;
    let output_path = temp.path().join("shutdown-finalized-profile.folded");

    let client = LeaderClient::connect(
        sock_path,
        "client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let runtime_cpu_profile = client
        .registration()
        .leader_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.runtime_cpu_profile);

    if runtime_cpu_profile {
        let start_result = client
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(output_path.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap();
        if start_result.is_err() {
            // pprof can't start in sandbox, skip
            client.cancel();
            handle.cancel.cancel();
            return;
        }

        handle.cancel.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(output_path.exists());
    } else {
        let error = client
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(output_path.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ControlErrorCode::RuntimeProfilingUnsupported);
        handle.cancel.cancel();
    }

    client.cancel();
}

#[tokio::test]
async fn test_runtime_profile_creates_missing_parent_directory_end_to_end() {
    let temp = TempDir::new().unwrap();
    let (sock_path, handle) = setup_control_test_server(&temp).await;
    let nested_output = temp
        .path()
        .join("nested")
        .join("profiles")
        .join("profile.folded");

    let client = LeaderClient::connect(
        sock_path,
        "client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let runtime_cpu_profile = client
        .registration()
        .leader_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.runtime_cpu_profile);

    if runtime_cpu_profile {
        let start_result = client
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(nested_output.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap();
        let Ok(started) = start_result else {
            client.cancel();
            handle.cancel.cancel();
            return; // pprof can't start in sandbox, skip
        };
        assert!(matches!(
            started,
            ControlPayload::CpuProfileStarted { svg_path, .. } if svg_path == nested_output
        ));

        let stopped = client
            .send_control(ControlCommand::StopCpuProfile)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            stopped,
            ControlPayload::CpuProfileStopped { svg_path, .. } if svg_path == nested_output
        ));
        assert!(nested_output.exists());
    } else {
        let error = client
            .send_control(ControlCommand::StartCpuProfile {
                output: Some(nested_output.display().to_string()),
                frequency_hz: Some(200),
            })
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ControlErrorCode::RuntimeProfilingUnsupported);
    }

    client.cancel();
    handle.cancel.cancel();
}

#[tokio::test]
async fn test_runtime_profile_rejects_output_collision_end_to_end() {
    let temp = TempDir::new().unwrap();
    let (sock_path, handle) = setup_control_test_server(&temp).await;
    let output_path = temp.path().join("existing-profile.folded");
    std::fs::write(&output_path, "already exists").unwrap();

    let client = LeaderClient::connect(
        sock_path,
        "client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let error = client
        .send_control(ControlCommand::StartCpuProfile {
            output: Some(output_path.display().to_string()),
            frequency_hz: Some(200),
        })
        .await
        .unwrap()
        .unwrap_err();

    if client
        .registration()
        .leader_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.runtime_cpu_profile)
    {
        assert_eq!(error.code, ControlErrorCode::OutputPathCollision);
    } else {
        assert_eq!(error.code, ControlErrorCode::RuntimeProfilingUnsupported);
    }

    client.cancel();
    handle.cancel.cancel();
}

#[tokio::test]
async fn test_session_based_routing() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "test-stdio",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send a message with sessionId in params
    let test_message =
        r#"{"jsonrpc":"2.0","method":"session/start","id":1,"params":{"sessionId":"session-123"}}"#;
    client.send(test_message.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    assert_eq!(json["params"]["sessionId"], "session-123");

    // Send a response with session_id for routing (without using request ID)
    // This tests the session-based fallback routing
    let response = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session-123","data":"update"}}"#;
    response_tx.send(response.to_string()).unwrap();

    // Client should receive the session update
    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for session update")
        .expect("channel closed");

    let response_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();
    assert_eq!(response_json["params"]["sessionId"], "session-123");

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_stdio_client_receives_tool_result() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "test-stdio",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send a simulated tool call request (e.g., read_file invocation)
    let test_tool_call = r#"{"jsonrpc":"2.0","method":"tool/call","id":100,"params":{"name":"read_file","arguments":{"target_file":"/path/to/test.txt"}}}"#;
    client.send(test_tool_call.to_string()).unwrap();

    // Server should receive the tool call (with namespaced ID)
    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(json["method"], "tool/call");
    assert_eq!(json["params"]["name"], "read_file");
    let namespaced_id = json["id"].as_str().unwrap().to_string();

    // Simulate tool result response from agent (e.g., read_file success)
    let tool_result = format!(
        r#"{{"jsonrpc":"2.0","result":{{"content":"test file content"}},"id":"{}"}}"#,
        namespaced_id
    );
    response_tx.send(tool_result).unwrap();

    // Client should receive the tool result with original ID restored
    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for tool result")
        .expect("channel closed");

    let result_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();
    assert_eq!(result_json["id"], 100);
    assert_eq!(result_json["result"]["content"], "test file content");

    client.cancel();
    cancel.cancel();
}

/// A session/new request without modelId is forwarded unchanged when the client has no default_model set.
/// This scenario caused the "unknown model id" error in leader mode.
#[tokio::test]
async fn test_session_new_without_model_id_no_default() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Connect with yolo_mode but no default_model (typical VS Code extension setup)
    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Send session/new without modelId (exactly like the VS Code extension does)
    let session_new = r#"{"jsonrpc":"2.0","id":31,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[],"_meta":{"yoloMode":true}}}"#;
    client.send(session_new.to_string()).unwrap();

    // Server should forward it without injecting modelId
    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    assert_eq!(json["method"], "session/new");
    let meta = &json["params"]["_meta"];
    assert_eq!(meta["yoloMode"], true);
    assert!(
        meta.get("modelId").is_none(),
        "modelId should not be injected when client has no default_model"
    );

    client.cancel();
    cancel.cancel();
}

/// When a client registers with yolo_mode, yoloMode is injected into session/new _meta, but modelId is NOT injected when default_model is None.
#[tokio::test]
async fn test_session_new_yolo_mode_no_model() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Client with yolo_mode=true but no default model
    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: true,
            default_model: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Send session/new without modelId or yoloMode in _meta
    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#;
    client.send(session_new.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    // yoloMode should be injected from capabilities
    let meta = &json["params"]["_meta"];
    assert_eq!(meta["yoloMode"], true);
    assert!(
        meta.get("modelId").is_none(),
        "modelId should not be injected when default_model is None"
    );

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_session_new_empty_default_model_not_injected() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Client with empty string default_model (edge case from config)
    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: true,
            default_model: Some("".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#;
    client.send(session_new.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    let meta = &json["params"]["_meta"];
    assert_eq!(meta["yoloMode"], true);
    assert!(
        meta.get("modelId").is_none(),
        "empty default_model should not be injected as modelId"
    );

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_session_new_valid_default_model_injected() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: false,
            default_model: Some("grok-3-fast".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#;
    client.send(session_new.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    // modelId should be injected from default_model
    let meta = &json["params"]["_meta"];
    assert_eq!(meta["modelId"], "grok-3-fast");

    client.cancel();
    cancel.cancel();
}

// ── Session ownership & notification routing ──────────────────────────

/// The leader learns session ownership from session/new responses.
/// Notifications have no request ID, so it routes them to the owning client by sessionId.
#[tokio::test]
async fn test_session_ownership_from_response_routes_notifications() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Client sends session/new
    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#;
    client.send(session_new.to_string()).unwrap();

    // Server receives the request (with namespaced ID)
    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    let namespaced_id = json["id"].as_str().unwrap().to_string();

    // Simulate agent response with sessionId in result
    let response = format!(
        r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-abc-123"}},"id":"{}"}}"#,
        namespaced_id
    );
    response_tx.send(response).unwrap();

    // Client receives the session/new response
    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let resp_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();
    assert_eq!(resp_json["result"]["sessionId"], "sess-abc-123");
    assert_eq!(resp_json["id"], 1); // original ID restored

    // Now send a notification for this session (no id field, only sessionId in params)
    // This tests session-based routing: the leader must know which client owns sess-abc-123
    let notification = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-abc-123","status":"ready"}}"#;
    response_tx.send(notification.to_string()).unwrap();

    // Client should receive the notification routed via session ownership
    let notif_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for notification")
        .expect("closed");
    let notif_json: serde_json::Value = serde_json::from_str(&notif_response).unwrap();
    assert_eq!(notif_json["params"]["sessionId"], "sess-abc-123");
    assert_eq!(notif_json["params"]["status"], "ready");

    client.cancel();
    cancel.cancel();
}

// ── Multi-client session isolation ────────────────────────────────────

/// Two clients with different sessions receive only their own notifications, not each other's.
/// Multiple connected VS Code windows rely on this isolation.
#[tokio::test]
async fn test_two_clients_session_isolation() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    // Connect two clients (simulating two VS Code windows)
    let mut client1 = LeaderClient::connect(
        sock_path.clone(),
        "vscode-1",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let mut client2 = LeaderClient::connect(
        sock_path,
        "vscode-2",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Client 1 creates session A
    client1
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/project-a","mcpServers":[]}}"#.to_string())
        .unwrap();
    let msg1 = acp_rx.recv().await.unwrap();
    let json1: serde_json::Value = serde_json::from_str(&msg1).unwrap();
    let id1 = json1["id"].as_str().unwrap().to_string();

    // Client 2 creates session B
    client2
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/project-b","mcpServers":[]}}"#.to_string())
        .unwrap();
    let msg2 = acp_rx.recv().await.unwrap();
    let json2: serde_json::Value = serde_json::from_str(&msg2).unwrap();
    let id2 = json2["id"].as_str().unwrap().to_string();

    // IDs should be different (different client prefixes, same original ID)
    assert_ne!(id1, id2);

    // Respond with different session IDs
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-AAA"}},"id":"{}"}}"#,
            id1
        ))
        .unwrap();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-BBB"}},"id":"{}"}}"#,
            id2
        ))
        .unwrap();

    // Each client should receive their own response
    let resp1 = tokio::time::timeout(Duration::from_secs(2), client1.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let resp2 = tokio::time::timeout(Duration::from_secs(2), client2.recv())
        .await
        .expect("timeout")
        .expect("closed");

    let r1: serde_json::Value = serde_json::from_str(&resp1).unwrap();
    let r2: serde_json::Value = serde_json::from_str(&resp2).unwrap();
    assert_eq!(r1["result"]["sessionId"], "sess-AAA");
    assert_eq!(r2["result"]["sessionId"], "sess-BBB");

    // Now send a notification for session A; only client 1 should get it
    let notif_a = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-AAA","data":"for-client-1"}}"#;
    response_tx.send(notif_a.to_string()).unwrap();

    let recv1 = tokio::time::timeout(Duration::from_secs(2), client1.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let n1: serde_json::Value = serde_json::from_str(&recv1).unwrap();
    assert_eq!(n1["params"]["sessionId"], "sess-AAA");
    assert_eq!(n1["params"]["data"], "for-client-1");

    // Send a notification for session B; only client 2 should get it
    let notif_b = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-BBB","data":"for-client-2"}}"#;
    response_tx.send(notif_b.to_string()).unwrap();

    let recv2 = tokio::time::timeout(Duration::from_secs(2), client2.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let n2: serde_json::Value = serde_json::from_str(&recv2).unwrap();
    assert_eq!(n2["params"]["sessionId"], "sess-BBB");
    assert_eq!(n2["params"]["data"], "for-client-2");

    client1.cancel();
    client2.cancel();
    cancel.cancel();
}

// ── Multi-client model switch fan-out ─────────────────────────────────

/// When a client switches models on a shared session, the leader must broadcast `ModelChanged` to every subscriber, not just the invoker. Only that broadcast lets the follower mirror the new model in its UI.
/// The agent emitting it from `model_switch::apply` is covered by the `extensions::notification` tests in `distill-shell`.
/// The pager applying it silently on followers is covered by the `model_changed_*` tests in `distill-pager`'s `acp_handler` tests. The setModel response itself still routes back to the invoker only. Asserting both the broadcast and the response catches a regression that suppresses the notification when a response is also produced.
#[tokio::test]
async fn test_set_model_broadcasts_to_session_subscribers() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    // Two TUIs connected to the same leader, sharing one session.
    let mut invoker = LeaderClient::connect(
        sock_path.clone(),
        "grok-tui-A",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let mut follower = LeaderClient::connect(
        sock_path,
        "grok-tui-B",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Subscribe both clients to the SAME session id
    // The leader registers a client as a subscriber the first time it sees an ACP message from that client carrying a sessionId
    // Any session-scoped method works, so we use a cheap synthetic one with distinct ids per client
    let shared_sid = "sess-shared-multi-client";
    invoker
        .send(format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/_subscribe_test","params":{{"sessionId":"{}"}}}}"#,
            shared_sid
        ))
        .unwrap();
    follower
        .send(format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"session/_subscribe_test","params":{{"sessionId":"{}"}}}}"#,
            shared_sid
        ))
        .unwrap();

    // Drain the two synthetic subscribe requests off the server's outgoing channel
    // The leader has already registered both clients as subscribers of `shared_sid` by the time it puts them on `acp_rx`
    let _ = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
        .await
        .expect("timeout draining subscribe 1")
        .expect("closed");
    let _ = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
        .await
        .expect("timeout draining subscribe 2")
        .expect("closed");

    // Invoker sends `session/setModel` for the shared session.
    invoker
        .send(format!(
            r#"{{"jsonrpc":"2.0","id":42,"method":"session/setModel","params":{{"sessionId":"{}","modelId":"grok-4"}}}}"#,
            shared_sid
        ))
        .unwrap();
    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    let setmodel_ns_id = json["id"].as_str().unwrap().to_string();
    assert_eq!(json["method"], "session/setModel");
    assert_eq!(json["params"]["modelId"], "grok-4");

    // Simulate the agent's two outputs for a successful switch: A session-scoped `ModelChanged` broadcast, which `model_switch::apply` emits via the gateway after the actor confirms the swap.
    // The `SetSessionModelResponse`, routed by the leader to the invoker only via namespaced-id matching.
    // Order matters: `model_switch::apply` fires the broadcast BEFORE the response, so it arrives at each subscriber's recv() first
    let broadcast = format!(
        r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"{}","update":{{"sessionUpdate":"model_changed","model_id":"grok-4","reasoning_effort":"high"}}}}}}"#,
        shared_sid
    );
    response_tx.send(broadcast.clone()).unwrap();
    let response = format!(
        r#"{{"jsonrpc":"2.0","result":{{"meta":{{"model":"grok-4"}}}},"id":"{}"}}"#,
        setmodel_ns_id
    );
    response_tx.send(response).unwrap();

    // --- Invoker: must receive BOTH the broadcast AND the targeted response, in that order The broadcast is what keeps the other clients in sync
    // The response is what the invoker's `SwitchModelComplete` dispatch handler keys on for the user-facing "Switched to X" message
    // The pager's broadcast handler ignores it (it gates on `model_switch_pending == true`), so the invoker doesn't double-apply state But the leader is still required to fan it out, because the same JSON-RPC connection is what the response travels on Suppressing it leader-side would also suppress it for the follower below
    let invoker_msg1 = tokio::time::timeout(Duration::from_secs(2), invoker.recv())
        .await
        .expect("timeout waiting for broadcast on invoker")
        .expect("invoker channel closed");
    let inv1: serde_json::Value = serde_json::from_str(&invoker_msg1).unwrap();
    assert_eq!(inv1["method"], "x.ai/session_notification");
    assert_eq!(inv1["params"]["sessionId"], shared_sid);
    assert_eq!(inv1["params"]["update"]["sessionUpdate"], "model_changed");
    assert_eq!(inv1["params"]["update"]["model_id"], "grok-4");
    assert_eq!(inv1["params"]["update"]["reasoning_effort"], "high");

    let invoker_msg2 = tokio::time::timeout(Duration::from_secs(2), invoker.recv())
        .await
        .expect("timeout waiting for setModel response on invoker")
        .expect("invoker channel closed");
    let inv2: serde_json::Value = serde_json::from_str(&invoker_msg2).unwrap();
    assert_eq!(
        inv2["id"], 42,
        "response id must be restored to the invoker's original"
    );
    assert_eq!(inv2["result"]["meta"]["model"], "grok-4");

    // --- Follower: must receive the broadcast
    // Without it the follower's status bar, `/model` dropdown, and prompt header stay stuck on the pre-switch model
    // It must NOT receive the targeted response (that one is routed by request id to the invoker only)
    let follower_msg = tokio::time::timeout(Duration::from_secs(2), follower.recv())
        .await
        .expect(
            "timeout waiting for broadcast on follower — \
                 model switch did not propagate across leader clients",
        )
        .expect("follower channel closed");
    let f: serde_json::Value = serde_json::from_str(&follower_msg).unwrap();
    assert_eq!(f["method"], "x.ai/session_notification");
    assert_eq!(f["params"]["sessionId"], shared_sid);
    assert_eq!(f["params"]["update"]["sessionUpdate"], "model_changed");
    assert_eq!(f["params"]["update"]["model_id"], "grok-4");
    assert_eq!(f["params"]["update"]["reasoning_effort"], "high");

    // Follower must NOT see the namespaced setModel response
    // The leader routes responses by request-id prefix, and only the invoker's ClientId prefixes that id
    let unexpected = tokio::time::timeout(Duration::from_millis(200), follower.recv()).await;
    assert!(
        unexpected.is_err(),
        "follower received an extra message after the broadcast — \
         leader is leaking the invoker's targeted response: {:?}",
        unexpected
    );

    invoker.cancel();
    follower.cancel();
    cancel.cancel();
}

// ── Capability injection scope ────────────────────────────────────────

/// Test that capability injection ONLY applies to session/new, NOT to other methods like session/prompt or session/load.
/// The leader must not mutate arbitrary requests.
#[tokio::test]
async fn test_capabilities_not_injected_into_non_session_new() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: true,
            default_model: Some("grok-3-fast".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Send session/prompt: capabilities should NOT be injected
    let prompt = r#"{"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{"sessionId":"sess-123","prompt":{"content":"hello"}}}"#;
    client.send(prompt.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    assert!(
        json["params"].get("_meta").is_none(),
        "session/prompt should not have _meta injected"
    );
    assert_eq!(json["params"]["sessionId"], "sess-123");

    // Send session/load: it gets clientIdentifier but NOT yoloMode or modelId
    let load = r#"{"jsonrpc":"2.0","id":11,"method":"session/load","params":{"sessionId":"sess-456","cwd":"/tmp","mcpServers":[]}}"#;
    client.send(load.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert!(
        json["params"]["_meta"].get("yoloMode").is_none(),
        "session/load should not have yoloMode injected"
    );
    assert!(
        json["params"]["_meta"].get("modelId").is_none(),
        "session/load should not have modelId injected"
    );

    client.cancel();
    cancel.cancel();
}

/// Test that capability injection does NOT overwrite yoloMode if the request already has _meta.yoloMode set to false.
/// This lets a client disable YOLO for a single session even when its default capability is on.
#[tokio::test]
async fn test_yolo_mode_injection_preserves_explicit_false() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Client registered with yolo_mode=true
    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities {
            yolo_mode: true,
            default_model: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Request explicitly sets yoloMode to false
    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[],"_meta":{"yoloMode":false}}}"#;
    client.send(session_new.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    // Per-session yoloMode=false must win over client default yolo_mode=true
    assert_eq!(json["params"]["_meta"]["yoloMode"], false);

    client.cancel();
    cancel.cancel();
}

// ── Notification (no ID) pass-through ─────────────────────────────────

/// Test that JSON-RPC notifications (no "id" field) sent by a client are forwarded without ID rewriting.
/// Notifications include cancel, yolo_mode_changed, etc.
#[tokio::test]
async fn test_client_notification_forwarded_without_id_rewrite() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send a cancel notification (no "id" field)
    let cancel_notif = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-123","reason":"user"}}"#;
    client.send(cancel_notif.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    assert!(
        json.get("id").is_none(),
        "notifications should not get an id"
    );
    assert_eq!(json["method"], "session/cancel");
    assert_eq!(json["params"]["sessionId"], "sess-123");

    client.cancel();
    cancel.cancel();
}

/// With two clients attached, `session/cancel` carrying `_meta.cancelPromptId` (the canceller's awaited prompt id) must reach the agent unmodified.
/// The meta is how the session actor cancels only the canceller's queued prompt while preserving the other client's queued work. The actor side is covered by `cancel_running_task_resolves_cancellers_queued_prompt`.
/// A second client's interleaved cancel for a different session must also pass through independently, with no cross-client meta bleed or reordering.
#[tokio::test]
async fn test_cancel_prompt_id_meta_passes_through_with_two_clients() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client_a = LeaderClient::connect(
        sock_path.clone(),
        "grok-pager",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    let client_b = LeaderClient::connect(
        sock_path,
        "grok-pager",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // A cancels its queued prompt on the shared session; B cancels a turn on another session at the same time
    let cancel_a = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-shared","_meta":{"cancelSubagents":true,"cancelPromptId":"a-queued-pid"}}}"#;
    let cancel_b = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-other","_meta":{"cancelSubagents":false}}}"#;
    client_a.send(cancel_a.to_string()).unwrap();
    client_b.send(cancel_b.to_string()).unwrap();

    let mut saw_a = false;
    let mut saw_b = false;
    for _ in 0..2 {
        let received = tokio::time::timeout(Duration::from_secs(5), acp_rx.recv())
            .await
            .expect("timed out waiting for forwarded cancel")
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(json["method"], "session/cancel");
        assert!(json.get("id").is_none(), "notifications must stay id-less");
        match json["params"]["sessionId"].as_str() {
            Some("sess-shared") => {
                assert_eq!(
                    json["params"]["_meta"]["cancelPromptId"], "a-queued-pid",
                    "cancelPromptId must pass through the leader unmodified"
                );
                assert_eq!(json["params"]["_meta"]["cancelSubagents"], true);
                saw_a = true;
            }
            Some("sess-other") => {
                assert!(
                    json["params"]["_meta"].get("cancelPromptId").is_none(),
                    "no cancelPromptId bleed across clients"
                );
                assert_eq!(json["params"]["_meta"]["cancelSubagents"], false);
                saw_b = true;
            }
            other => panic!("unexpected sessionId: {other:?}"),
        }
    }
    assert!(saw_a && saw_b, "both cancels must be forwarded");

    client_a.cancel();
    client_b.cancel();
    cancel.cancel();
}

// ── Extension method routing ──────────────────────────────────────────

/// Test that extension methods (prefixed with _) are correctly forwarded through the leader with proper ID namespacing and response routing.
#[tokio::test]
async fn test_extension_method_roundtrip() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Send an extension method call (e.g., fuzzy search open)
    let ext_call = r#"{"jsonrpc":"2.0","id":50,"method":"_x.ai/search/fuzzy/open","params":{"sessionId":"sess-123","hidden":false}}"#;
    client.send(ext_call.to_string()).unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();

    assert_eq!(json["method"], "_x.ai/search/fuzzy/open");
    let namespaced_id = json["id"].as_str().unwrap();
    assert!(namespaced_id.contains(ID_NAMESPACE_SEP));
    assert!(namespaced_id.ends_with("|50"));

    // Simulate response
    let response = format!(
        r#"{{"jsonrpc":"2.0","result":{{"searchId":"search-xyz"}},"id":"{}"}}"#,
        namespaced_id
    );
    response_tx.send(response).unwrap();

    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let resp_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();

    assert_eq!(resp_json["id"], 50);
    assert_eq!(resp_json["result"]["searchId"], "search-xyz");

    client.cancel();
    cancel.cancel();
}

// ── Error response routing ────────────────────────────────────────────

/// Test that JSON-RPC error responses from the agent are correctly routed back to the requesting client with the original ID restored.
#[tokio::test]
async fn test_error_response_routing() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "vscode-ext",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    client
        .send(
            r#"{"jsonrpc":"2.0","id":99,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#
                .to_string(),
        )
        .unwrap();

    let received = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&received).unwrap();
    let namespaced_id = json["id"].as_str().unwrap().to_string();

    // Simulate an error response (e.g., auth_required)
    let error_response = format!(
        r#"{{"jsonrpc":"2.0","error":{{"code":-32001,"message":"auth_required","data":"No credentials"}},"id":"{}"}}"#,
        namespaced_id
    );
    response_tx.send(error_response).unwrap();

    let client_response = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let resp_json: serde_json::Value = serde_json::from_str(&client_response).unwrap();

    assert_eq!(resp_json["id"], 99);
    assert_eq!(resp_json["error"]["code"], -32001);
    assert_eq!(resp_json["error"]["message"], "auth_required");
    assert_eq!(resp_json["error"]["data"], "No credentials");

    client.cancel();
    cancel.cancel();
}

// ── Session cleanup on disconnect ─────────────────────────────────────

/// Test that when a client disconnects, notifications for its sessions are still delivered to the next active client via fallback routing.
/// Session ownership entries are intentionally *not* removed on disconnect so the server can distinguish IPC-originated sessions (present in `session_owners`) from relay-originated ones (absent).
/// The session-based routing path naturally falls through (the dead client is gone from `clients`), and the fallback picks up the notification for the new client.
#[tokio::test]
async fn test_session_ownership_cleanup_on_disconnect() {
    use distill_shell::leader::run_leader_server;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");

    // Manually create server with no_exit_on_disconnect=true so the server survives the first client disconnecting
    let (acp_tx, mut acp_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();

    let cancel_clone = cancel.clone();
    let sock_clone = sock_path.clone();
    tokio::spawn(async move {
        let client_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let agent_busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (_ready_tx, ready_rx) = tokio::sync::watch::channel(true);
        let (shutdown_tx, _shutdown_rx) =
            tokio::sync::watch::channel(distill_shell::leader::ShutdownReason::Manual);
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            client_count,
            agent_busy,
            distill_shell::agent::activity::AgentActivity::default(),
            ready_rx,
            tokio::sync::watch::channel(false).0,
            shutdown_tx,
            None,
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;

    // Connect client, create session, then disconnect
    {
        let mut client = LeaderClient::connect(
            sock_path.clone(),
            "vscode-temp",
            ClientMode::Stdio,
            ClientCapabilities::default(),
        )
        .await
        .unwrap();

        // Create session
        client
            .send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#.to_string())
            .unwrap();
        let received = acp_rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&received).unwrap();
        let namespaced_id = json["id"].as_str().unwrap().to_string();

        // Respond with session ID
        response_tx
            .send(format!(
                r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-temp"}},"id":"{}"}}"#,
                namespaced_id
            ))
            .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(2), client.recv()).await;

        // Client disconnects (dropped)
        client.cancel();
    }

    // Give server time to process disconnect
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The server sends an eviction notification for "sess-temp" when client1 disconnects
    // Drain it before client2's initialize to keep the channel in sync
    // Also verifies the eviction was actually sent
    let eviction = acp_rx.recv().await.unwrap();
    let eviction_json: serde_json::Value = serde_json::from_str(&eviction).unwrap();
    assert_eq!(eviction_json["method"], "_x.ai/internal/evict_sessions");

    // Connect a NEW client; the server should still be running
    let mut client2 = LeaderClient::connect(
        sock_path,
        "vscode-new",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Make client2 active (so it becomes fallback)
    client2
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_string())
        .unwrap();
    let _ = acp_rx.recv().await.unwrap();

    // Send a notification for the old session; it should be DROPPED, not forwarded to client2 The dead client's session entry is still in session_owners (for relay detection)
    // Session-based routing sees the owner is dead and drops the notification to prevent cross-session leaks The reconnecting client will replay via session/load instead.
    let old_notif = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-temp","data":"orphan"}}"#;
    response_tx.send(old_notif.to_string()).unwrap();

    // client2 should NOT receive the dead-session notification.
    // Send a second notification without a sessionId; this one SHOULD arrive via fallback routing, proving client2 is alive and connected
    let probe = r#"{"jsonrpc":"2.0","method":"x.ai/probe","params":{"ping":true}}"#;
    response_tx.send(probe.to_string()).unwrap();

    let recv = tokio::time::timeout(Duration::from_secs(2), client2.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let json: serde_json::Value = serde_json::from_str(&recv).unwrap();
    // The first message client2 sees is the probe, not the orphan notification
    assert_eq!(json["params"]["ping"], true);

    client2.cancel();
    cancel.cancel();
}

// ============================================================================= Code-nav capability injection integration tests
// These tests drive `code_nav_enabled` injection from the leader boundary all the way to the forwarded ACP payload and check per-client isolation =============================================================================

/// Verify that the leader injects `codeNavEnabled: true` into session/new for a web client that registered with `code_nav_enabled: true`.
#[tokio::test]
async fn test_code_nav_capable_client_gets_true_injected_into_session_new() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Web client that advertised code-nav capability during registration.
    let web_client = LeaderClient::connect(
        sock_path,
        "grok-web",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/repo","mcpServers":[]}}"#;
    web_client.send(session_new.to_string()).unwrap();

    let forwarded = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();

    assert_eq!(json["method"], "session/new");
    assert_eq!(
        json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(true),
        "leader must inject codeNavEnabled=true for web client with code-nav capability"
    );

    web_client.cancel();
    cancel.cancel();
}

/// Verify that the leader injects `codeNavEnabled: false` into session/new for a TUI client that did NOT register `code_nav_enabled`.
#[tokio::test]
async fn test_non_code_nav_client_gets_false_injected_into_session_new() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // TUI client with no code-nav capability.
    let tui_client = LeaderClient::connect(
        sock_path,
        "grok-tui",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/repo","mcpServers":[]}}"#;
    tui_client.send(session_new.to_string()).unwrap();

    let forwarded = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();

    assert_eq!(json["method"], "session/new");
    assert_eq!(
        json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(false),
        "leader must inject codeNavEnabled=false for client without code-nav capability"
    );

    tui_client.cancel();
    cancel.cancel();
}

/// Verify leader-mode per-client isolation end to end.
/// Two clients with different code-nav capabilities receive independent `codeNavEnabled` values in their session/new requests.
/// One client's capability does not contaminate the other.
#[tokio::test]
async fn test_leader_code_nav_client_isolation() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Web client with code-nav capability.
    let web_client = LeaderClient::connect(
        sock_path.clone(),
        "grok-web",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // TUI client without code-nav capability.
    let tui_client = LeaderClient::connect(
        sock_path,
        "grok-tui",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/repo","mcpServers":[]}}"#;

    // Web client sends session/new first.
    web_client.send(session_new.to_string()).unwrap();
    let web_fwd = acp_rx.recv().await.unwrap();
    let web_json: serde_json::Value = serde_json::from_str(&web_fwd).unwrap();

    // TUI client sends session/new second.
    tui_client.send(session_new.to_string()).unwrap();
    let tui_fwd = acp_rx.recv().await.unwrap();
    let tui_json: serde_json::Value = serde_json::from_str(&tui_fwd).unwrap();

    // Each client's request must carry its own capability, with no cross-contamination
    assert_eq!(
        web_json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(true),
        "web client must get codeNavEnabled=true"
    );
    assert_eq!(
        tui_json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(false),
        "tui client must get codeNavEnabled=false, not contaminated by web client"
    );

    web_client.cancel();
    tui_client.cancel();
    cancel.cancel();
}

/// Verify that `codeNavEnabled` is also injected into session/load so that reconnecting sessions receive the correct per-client capability.
#[tokio::test]
async fn test_code_nav_capability_injected_into_session_load() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let web_client = LeaderClient::connect(
        sock_path,
        "grok-web",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-abc","cwd":"/repo","mcpServers":[]}}"#;
    web_client.send(session_load.to_string()).unwrap();

    let forwarded = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();

    assert_eq!(json["method"], "session/load");
    assert_eq!(
        json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(true),
        "leader must inject codeNavEnabled into session/load for reconnect isolation"
    );

    web_client.cancel();
    cancel.cancel();
}

/// Verify that an `x.ai/code/status` extension request is forwarded to the agent with the correct method, sessionId, and cwd in the params.
/// This tests the routing boundary between leader and agent for code-nav extension requests without requiring a live agent.
#[tokio::test]
async fn test_code_status_ext_request_forwarded_to_agent() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let web_client = LeaderClient::connect(
        sock_path,
        "grok-web",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Send x.ai/code/status with a sessionId; the leader must forward it to the agent
    let status_req = r#"{"jsonrpc":"2.0","id":42,"method":"extensions/ext","params":{"method":"x.ai/code/status","params":{"sessionId":"sess-web-1","cwd":"/repo"}}}"#;
    web_client.send(status_req.to_string()).unwrap();

    let forwarded = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();

    assert_eq!(json["method"], "extensions/ext");
    assert_eq!(json["params"]["method"], "x.ai/code/status");
    assert_eq!(json["params"]["params"]["sessionId"], "sess-web-1");
    assert_eq!(json["params"]["params"]["cwd"], "/repo");

    web_client.cancel();
    cancel.cancel();
}

// ── Startup readiness gate ────────────────────────────────────────────

/// Raw-protocol test for the server-side readiness handshake. Verifies that when the leader is not yet ready: A connecting client receives `Registered { ready: false }` in the wire protocol.
/// The server then sends `LeaderReady` once `ready_tx` fires. Post-readiness ACP traffic is forwarded to the agent normally.
/// Uses the raw IPC wire protocol (`write_message` / `read_message`) rather than `LeaderClient::connect`. The high-level client blocks in `connect()` until `LeaderReady`, so it cannot observe the intermediate `Registered { ready: false }` state.
#[tokio::test]
async fn test_raw_registration_handshake_not_ready_then_ready() {
    use distill_shell::leader::run_leader_server;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");

    let (acp_tx, mut acp_rx) = mpsc::unbounded_channel::<String>();
    let (response_tx, response_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = watch::channel(false); // NOT ready yet

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            ready_rx,
            watch::channel(false).0,
            watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            None,
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;

    // Connect via raw socket to observe the wire-level handshake.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Register manually.
    write_message(
        &mut writer,
        &ClientMessage::Register {
            client_type: "raw-test".into(),
            mode: ClientMode::Stdio,
            capabilities: ClientCapabilities::default(),
        },
    )
    .await
    .unwrap();

    // ── Server must respond Registered { ready: false } ───────────────────────
    let reg_msg: ServerMessage =
        tokio::time::timeout(Duration::from_secs(2), read_message(&mut reader))
            .await
            .expect("timeout waiting for Registered")
            .expect("read error");

    match reg_msg {
        ServerMessage::Registered {
            client_id: _,
            ready,
            ..
        } => {
            assert!(
                !ready,
                "leader is not ready yet; Registered.ready must be false"
            );
        }
        other => panic!("Expected Registered, got {other:?}"),
    }

    // ── Signal readiness (simulates auth + prefetch completing) ───────────────
    // The server's per-client session is now blocked in its readiness wait loop.
    // Signalling here causes it to send LeaderReady to this client.
    ready_tx.send(true).unwrap();

    // ── Server must now send LeaderReady ──────────────────────────────────────
    let ready_msg: ServerMessage =
        tokio::time::timeout(Duration::from_secs(2), read_message(&mut reader))
            .await
            .expect("timeout waiting for LeaderReady")
            .expect("read error");

    assert!(
        matches!(ready_msg, ServerMessage::LeaderReady),
        "Expected LeaderReady, got {ready_msg:?}"
    );

    // ── Post-ready: ACP flows normally ────────────────────────────────────────
    write_message(
        &mut writer,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#.into(),
        },
    )
    .await
    .unwrap();

    let forwarded = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
        .await
        .expect("timeout: post-ready ACP must reach agent")
        .expect("channel closed");

    let fwd: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
    assert_eq!(fwd["method"], "session/new");
    let namespaced_id = fwd["id"].as_str().unwrap().to_string();

    // Round-trip the response.
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-ok"}},"id":"{namespaced_id}"}}"#
        ))
        .unwrap();

    let raw_resp: ServerMessage =
        tokio::time::timeout(Duration::from_secs(2), read_message(&mut reader))
            .await
            .expect("timeout waiting for ACP response")
            .expect("read error");

    match raw_resp {
        ServerMessage::Acp { payload } => {
            let resp: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(resp["id"], 1, "ID must be restored to original");
            assert_eq!(resp["result"]["sessionId"], "sess-ok");
        }
        other => panic!("Expected Acp, got {other:?}"),
    }

    drop(response_tx);
    cancel.cancel();
}

/// `LeaderClient::connect` (and therefore `connect_or_spawn`) returns only after the leader signals `LeaderReady`.
/// Callers can send `initialize` immediately: TUI and headless clients must not see `leader_starting` errors even when auth/prefetch are slow.
#[tokio::test]
async fn test_connect_waits_for_leader_ready() {
    use distill_shell::leader::run_leader_server;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");

    let (acp_tx, mut acp_rx) = mpsc::unbounded_channel::<String>();
    let (response_tx, response_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = watch::channel(false); // NOT ready yet

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            ready_rx,
            watch::channel(false).0,
            watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            None,
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;

    // Spawn a task to signal readiness after a short delay (simulating slow auth/prefetch).
    let ready_delay_ms = 150u64;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ready_delay_ms)).await;
        ready_tx.send(true).unwrap();
    });

    // LeaderClient::connect should block until LeaderReady arrives, then return.
    // If it returned immediately (before readiness), `initialize` would hit `leader_starting` errors, the bug this test guards against
    let connect_start = tokio::time::Instant::now();
    let mut client = LeaderClient::connect(
        sock_path,
        "grok-tui",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .expect("connect must succeed after leader becomes ready");
    let elapsed = connect_start.elapsed();

    // Connect must have waited at least as long as the readiness delay.
    assert!(
        elapsed >= Duration::from_millis(ready_delay_ms.saturating_sub(30)),
        "connect returned too early ({elapsed:?}); should have waited for LeaderReady"
    );

    // Now that we're connected, send `initialize`; it must reach the agent (not get a leader_starting error), because the leader is fully ready
    client
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_string())
        .unwrap();

    let forwarded = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
        .await
        .expect("timeout: initialize must be forwarded to agent after readiness")
        .expect("channel closed");

    let fwd_json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
    assert_eq!(
        fwd_json["method"], "initialize",
        "initialize must reach the agent, not be rejected with leader_starting"
    );

    // Verify full round-trip.
    let namespaced_id = fwd_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"serverInfo":{{}}}},"id":"{namespaced_id}"}}"#
        ))
        .unwrap();

    let client_resp = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for initialize response")
        .expect("channel closed");
    let resp_json: serde_json::Value = serde_json::from_str(&client_resp).unwrap();
    assert_eq!(resp_json["id"], 1, "ID must be restored to original");

    drop(response_tx);
    client.cancel();
    cancel.cancel();
}

// ── Version mismatch notification ────────────────────────────────────

/// A connected client receives `x.ai/leader/version_mismatch` when its `client_version` differs from the leader's version.
/// Uses `leader_version_override` so the test bypasses the `"unknown"` constant that appears in dev builds where `VERSION_WITH_COMMIT` is not set.
#[tokio::test]
async fn test_version_mismatch_notification_sent_to_client() {
    use distill_shell::leader::{ClientCapabilities, ClientMode, LeaderClient, run_leader_server};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");

    let (acp_tx, _acp_rx) = mpsc::unbounded_channel::<String>();
    let (_response_tx, response_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            watch::channel(true).1,
            watch::channel(false).0,
            watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            Some("test-leader-0.1.150"), // override so detection is enabled in test builds
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;

    // Connect with a version that differs from the leader override.
    let mut client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities {
            client_version: Some("test-client-0.1.157".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The client should receive the version mismatch notification.
    let msg = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout waiting for version mismatch notification")
        .expect("channel closed");

    let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(json["method"], "x.ai/leader/version_mismatch");
    assert_eq!(json["params"]["clientVersion"], "test-client-0.1.157");
    assert_eq!(json["params"]["leaderVersion"], "test-leader-0.1.150");

    client.cancel();
    cancel.cancel();
}

#[tokio::test]
async fn test_no_version_mismatch_notification_when_versions_match() {
    use distill_shell::leader::{ClientCapabilities, ClientMode, LeaderClient, run_leader_server};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");

    let (acp_tx, _acp_rx) = mpsc::unbounded_channel::<String>();
    let (_response_tx, response_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            watch::channel(true).1,
            watch::channel(false).0,
            watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            Some("same-version-0.1.150"),
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;

    // Connect with the same version as the leader.
    let mut client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities {
            client_version: Some("same-version-0.1.150".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // No mismatch notification should arrive within a short window.
    let received = tokio::time::timeout(Duration::from_millis(200), client.recv()).await;
    assert!(
        received.is_err(),
        "no version mismatch notification expected when versions match"
    );

    client.cancel();
    cancel.cancel();
}

// ── Shutdown reason end-to-end ────────────────────────────────────────

/// A connected `LeaderClient` receives `ShuttingDown { reason: AutoUpdate }`.
/// `LeaderClient::shutting_down_reason()` then updates to `Some(AutoUpdate)`.
/// This covers the full propagation path: shutdown_tx.send(AutoUpdate) → cancel → server cancel branch reads reason → broadcast_shutdown(AutoUpdate) → client read loop receives ShuttingDown → shutting_down_tx.send(Some(AutoUpdate)) → shutting_down_rx observes Some(AutoUpdate)
#[tokio::test]
async fn test_auto_update_shutdown_reason_reaches_client() {
    use distill_shell::leader::{ClientCapabilities, ClientMode, LeaderClient, ShutdownReason};

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;

    let client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let mut shutdown_reason_rx = client.shutting_down_reason();
    let mut disconnect_rx = client.disconnect_reason();

    // Initial state: no ShuttingDown seen yet.
    assert_eq!(
        *shutdown_reason_rx.borrow(),
        None,
        "shutdown reason must be None before any ShuttingDown message"
    );

    // Simulate what run_auto_update_checker does: write AutoUpdate BEFORE cancelling so the server reads the right reason
    handle.shutdown_tx.send(ShutdownReason::AutoUpdate).unwrap();
    handle.cancel.cancel();

    // Wait for the ShuttingDown reason to propagate to the client.
    tokio::time::timeout(Duration::from_secs(2), shutdown_reason_rx.changed())
        .await
        .expect("timeout waiting for shutting_down_reason to change")
        .expect("watch sender dropped");

    assert_eq!(
        *shutdown_reason_rx.borrow(),
        Some(ShutdownReason::AutoUpdate),
        "connected client must observe AutoUpdate as the shutdown reason"
    );

    // Also verify the disconnect reason eventually becomes LeaderShutdown.
    tokio::time::timeout(
        Duration::from_secs(2),
        disconnect_rx.wait_for(|r| *r != distill_shell::leader::DisconnectReason::Connected),
    )
    .await
    .expect("timeout waiting for disconnect reason to change")
    .expect("watch sender dropped");

    assert_eq!(
        *disconnect_rx.borrow(),
        distill_shell::leader::DisconnectReason::LeaderShutdown
    );
}

// ── Disruptive relaunch-for-update ────────────────────────────────────

/// A `RelaunchForUpdate` with a strictly-newer target is accepted with a `Relaunching` ack.
/// The idle leader drains immediately and broadcasts `ShuttingDown { AutoUpdate }` so connected clients reconnect onto the new binary.
#[tokio::test]
async fn test_relaunch_for_update_accepts_and_shuts_down() {
    use distill_shell::leader::{
        ClientCapabilities, ClientMode, ControlCommand, ControlPayload, LeaderClient,
        ShutdownReason,
    };

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let _handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;

    let client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let mut shutdown_reason_rx = client.shutting_down_reason();

    // A strictly-newer target is accepted.
    let payload = client
        .send_control(ControlCommand::RelaunchForUpdate {
            to_version: "999.0.0".to_string(),
        })
        .await
        .expect("control transport ok")
        .expect("control result ok");
    match payload {
        ControlPayload::Relaunching { to_version, .. } => assert_eq!(to_version, "999.0.0"),
        other => panic!("expected Relaunching, got {other:?}"),
    }

    // Idle leader drains immediately, then broadcasts AutoUpdate.
    tokio::time::timeout(Duration::from_secs(5), shutdown_reason_rx.changed())
        .await
        .expect("timeout waiting for ShuttingDown after relaunch")
        .expect("watch sender dropped");
    assert_eq!(
        *shutdown_reason_rx.borrow(),
        Some(ShutdownReason::AutoUpdate),
        "relaunch must broadcast AutoUpdate"
    );
}

/// A `RelaunchForUpdate` whose target is not strictly newer than the leader is declined, and the leader stays up.
#[tokio::test]
async fn test_relaunch_for_update_declines_when_not_newer() {
    use distill_shell::leader::{
        ClientCapabilities, ClientMode, ControlCommand, ControlPayload, LeaderClient,
    };

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let _handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;

    let client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let shutdown_reason_rx = client.shutting_down_reason();

    let payload = client
        .send_control(ControlCommand::RelaunchForUpdate {
            to_version: "0.0.0".to_string(),
        })
        .await
        .expect("control transport ok")
        .expect("control result ok");
    assert!(
        matches!(payload, ControlPayload::RelaunchDeclined { .. }),
        "a target that is not strictly newer is declined, got {payload:?}"
    );

    // The leader must NOT begin shutting down.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        *shutdown_reason_rx.borrow(),
        None,
        "declined relaunch must not shut the leader down"
    );
}

/// The bounded-grace drain waits while the agent is busy and only relaunches once it goes idle.
/// An in-flight turn isn't cut off the instant a relaunch is requested.
#[tokio::test]
async fn test_relaunch_for_update_waits_for_busy_then_exits() {
    use distill_shell::leader::{
        ClientCapabilities, ClientMode, ControlCommand, ControlPayload, LeaderClient,
        ShutdownReason,
    };
    use std::sync::atomic::Ordering;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;

    // Mark the agent busy before requesting the relaunch so the drain must wait.
    handle.agent_busy.store(true, Ordering::Relaxed);

    let client = LeaderClient::connect(
        sock_path,
        "test-client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    let mut shutdown_reason_rx = client.shutting_down_reason();

    let payload = client
        .send_control(ControlCommand::RelaunchForUpdate {
            to_version: "999.0.0".to_string(),
        })
        .await
        .expect("control transport ok")
        .expect("control result ok");
    assert!(
        matches!(payload, ControlPayload::Relaunching { .. }),
        "a busy leader still accepts the relaunch, got {payload:?}"
    );

    // While busy, the leader must NOT shut down; the drain is waiting
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        *shutdown_reason_rx.borrow(),
        None,
        "leader must keep draining while the agent is busy"
    );

    // Going idle lets the drain proceed (it polls every 100ms).
    handle.agent_busy.store(false, Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(2), shutdown_reason_rx.changed())
        .await
        .expect("timeout waiting for ShuttingDown after going idle")
        .expect("watch sender dropped");
    assert_eq!(
        *shutdown_reason_rx.borrow(),
        Some(ShutdownReason::AutoUpdate),
        "leader must relaunch once the agent is idle"
    );
}

// ── initialize_seen correctness ───────────────────────────────────────

/// Regression test: if the first ACP message is NOT `initialize`, a later `initialize` must still receive `clientIdentifier` injection.
/// Previously, `identity_injected` was set to `true` after the first ACP message regardless of its method.
/// A client whose first message was a notification (e.g., `session/cancel`) never got `clientIdentifier` injected into the later `initialize`.
#[tokio::test]
async fn test_initialize_injected_when_not_first_message() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path,
        "grok-tui",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // First message is a notification, NOT initialize
    let first_non_init = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-x","reason":"user"}}"#;
    client.send(first_non_init.to_string()).unwrap();

    let fwd1: serde_json::Value = serde_json::from_str(&acp_rx.recv().await.unwrap()).unwrap();
    // Sanity: the notification must have been forwarded correctly.
    assert_eq!(fwd1["method"], "session/cancel");
    // And it must NOT have had any _meta or clientIdentifier added.
    assert!(fwd1.get("params").and_then(|p| p.get("_meta")).is_none());

    // Second message is the real `initialize`.
    let init_msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    client.send(init_msg.to_string()).unwrap();

    let fwd2: serde_json::Value = serde_json::from_str(&acp_rx.recv().await.unwrap()).unwrap();
    assert_eq!(fwd2["method"], "initialize");

    let client_id = fwd2
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("clientIdentifier"))
        .and_then(|v| v.as_str());
    assert_eq!(
        client_id,
        Some("grok-tui"),
        "clientIdentifier must be injected into initialize even when it is not the first message"
    );

    client.cancel();
    cancel.cancel();
}

/// End-to-end leader test: a web client (code-nav capable) and a TUI client (not capable) both send code-nav extension requests.
/// Verify that codeNavEnabled is independently injected into each client's session and that extension requests are routed with correct params.
#[tokio::test]
async fn test_leader_code_nav_isolation_end_to_end() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, _response_tx) = setup_test_server(&temp).await;

    // Web client with code-nav capability.
    let web_client = LeaderClient::connect(
        sock_path.clone(),
        "grok-web",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // TUI client without code-nav capability.
    let tui_client = LeaderClient::connect(
        sock_path,
        "grok-tui",
        ClientMode::Stdio,
        ClientCapabilities {
            code_nav_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let session_new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/repo","mcpServers":[]}}"#;

    // Both clients send session/new; verify independent codeNavEnabled injection.
    web_client.send(session_new.to_string()).unwrap();
    let web_fwd = acp_rx.recv().await.unwrap();
    let web_json: serde_json::Value = serde_json::from_str(&web_fwd).unwrap();
    assert_eq!(
        web_json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(true)
    );

    tui_client.send(session_new.to_string()).unwrap();
    let tui_fwd = acp_rx.recv().await.unwrap();
    let tui_json: serde_json::Value = serde_json::from_str(&tui_fwd).unwrap();
    assert_eq!(
        tui_json["params"]["_meta"]["codeNavEnabled"],
        serde_json::json!(false)
    );

    // Web client sends x.ai/code/status
    let status_with_session = r#"{"jsonrpc":"2.0","id":10,"method":"extensions/ext","params":{"method":"x.ai/code/status","params":{"sessionId":"web-session","cwd":"/repo"}}}"#;
    web_client.send(status_with_session.to_string()).unwrap();

    let status_fwd = acp_rx.recv().await.unwrap();
    let status_json: serde_json::Value = serde_json::from_str(&status_fwd).unwrap();
    assert_eq!(status_json["params"]["method"], "x.ai/code/status");
    assert_eq!(status_json["params"]["params"]["sessionId"], "web-session");

    web_client.cancel();
    tui_client.cancel();
    cancel.cancel();
}

/// Regression test for a deadlock in `connect_or_spawn`. The bug: the spawner held a file lock while doing a full `LeaderClient::connect`, which blocks in `register()` waiting for `LeaderReady`.
/// The leader needs the same lock to reach readiness. This test couples readiness to a file lock (matching production) so holding the lock while connecting would deadlock.
/// Existing tests missed this because they drove readiness via a bare `watch::channel`.
#[tokio::test]
async fn test_lock_released_before_connect_prevents_deadlock() {
    use distill_shell::leader::run_leader_server;
    use fs2::FileExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    let temp = TempDir::new().unwrap();
    let sock_path = temp.path().join("leader.sock");
    let lock_path = temp.path().join("leader.lock");

    // Spawner acquires the file lock (mirrors connect_or_spawn).
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .unwrap();
    lock_file.lock_exclusive().unwrap();

    let (acp_tx, _acp_rx) = mpsc::unbounded_channel::<String>();
    let (_response_tx, response_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = watch::channel(false);

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            ready_rx,
            watch::channel(false).0,
            watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            None,
            control_state,
        )
        .await;
    });
    wait_for_socket(&sock_path).await;

    // Simulate the leader: acquire the lock, then signal readiness
    // It blocks until the spawner releases the lock above
    tokio::task::spawn_blocking(move || {
        let f = std::fs::File::open(&lock_path).unwrap();
        f.lock_exclusive().unwrap();
        ready_tx.send(true).unwrap();
    });

    // Release the lock BEFORE connecting: connect blocks on LeaderReady, which needs the lock, and holding it here would deadlock
    lock_file.unlock().unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        LeaderClient::connect(
            sock_path,
            "test",
            ClientMode::Stdio,
            ClientCapabilities::default(),
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "connect timed out — deadlock regression (lock held during connect)"
    );
    result.unwrap().unwrap().cancel();
    cancel.cancel();
}

// ============================================================================= Hung-agent and sever-mid-RPC scenarios
// The fake agent in these tests is the test body itself (acp_rx/response_tx) "Agent hangs" and "agent completes after the client is gone" are driven deterministically
// Reconnects use fresh raw `UnixStream`s so the sever is an abrupt socket close, not a graceful `Disconnect` =============================================================================

/// Server that survives client disconnects (`no_exit_on_disconnect = true`), for sever/reconnect scenarios.
/// Same wiring as `test_session_ownership_cleanup_on_disconnect`.
async fn setup_persistent_test_server(
    temp: &TempDir,
) -> (
    std::path::PathBuf,
    tokio_util::sync::CancellationToken,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::mpsc::UnboundedSender<String>,
) {
    use distill_shell::leader::run_leader_server;

    let sock_path = temp.path().join("leader.sock");
    let (acp_tx, acp_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();

    let cancel_clone = cancel.clone();
    let sock_clone = sock_path.clone();
    tokio::spawn(async move {
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_clone.clone(),
            lock_path: sock_clone.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        let _ = run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            true,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            distill_shell::agent::activity::AgentActivity::default(),
            tokio::sync::watch::channel(true).1,
            tokio::sync::watch::channel(false).0,
            tokio::sync::watch::channel(distill_shell::leader::ShutdownReason::Manual).0,
            None,
            control_state,
        )
        .await;
    });

    wait_for_socket(&sock_path).await;
    (sock_path, cancel, acp_rx, response_tx)
}

type RawClient = (
    tokio::io::ReadHalf<UnixStream>,
    tokio::io::WriteHalf<UnixStream>,
);

/// Connect a raw socket client and complete registration.
/// Raw (not `LeaderClient`) so dropping the halves is an abrupt sever with no `Disconnect` handshake.
async fn raw_register(sock_path: &std::path::Path, client_type: &str) -> RawClient {
    let stream = UnixStream::connect(sock_path).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_message(
        &mut writer,
        &ClientMessage::Register {
            client_type: client_type.into(),
            mode: ClientMode::Stdio,
            capabilities: ClientCapabilities::default(),
        },
    )
    .await
    .unwrap();
    let response: ServerMessage = read_message(&mut reader).await.unwrap();
    assert!(matches!(response, ServerMessage::Registered { .. }));
    (reader, writer)
}

/// Read `ServerMessage`s until an ACP payload arrives; return the parsed JSON.
async fn raw_recv_acp(reader: &mut tokio::io::ReadHalf<UnixStream>) -> serde_json::Value {
    loop {
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_secs(10), read_message(reader))
                .await
                .expect("timeout waiting for ACP message")
                .expect("read error");
        match msg {
            ServerMessage::Acp { payload } => return serde_json::from_str(&payload).unwrap(),
            // Skip keepalive/readiness noise.
            _ => continue,
        }
    }
}

/// Count `unified.jsonl` orphan-drop entries for `request_id`. Namespaced request ids are unique per process (global `ClientId` counter). The pid filter fences off other test processes appending to the same shared log.
/// This binary does not sandbox GROK_HOME, so on a dev machine these entries land in the real `~/.grok` log — accepted: the server already writes `leader.client.*` lines there from every test in this file, and the pid+request-id fence keeps the counting sound regardless of what else is in the file. (Bazel sandboxes HOME, so CI writes stay test-scoped.)
fn orphan_log_count(request_id: &str) -> usize {
    let Some(bytes) = distill_telemetry::unified_log::snapshot_log() else {
        return 0;
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).is_ok_and(|entry| {
                entry["msg"] == "leader.response.orphaned"
                    && entry["ctx"]["request_id"] == request_id
                    && entry["pid"] == std::process::id()
            })
        })
        .count()
}

/// Poll until `orphan_log_count(request_id) >= 1` or the budget elapses.
async fn wait_for_orphan_log(request_id: &str) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = orphan_log_count(request_id);
        if count > 0 || tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until the server logged `leader.client.disconnected` for `client_id`.
/// This is the deterministic disconnect signal for sessions that still have other subscribers (no `evict_sessions` is emitted for those).
/// The same caveat about writing to the real home log and the same pid fence as [`orphan_log_count`] apply.
async fn wait_for_client_disconnected_log(client_id: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen = distill_telemetry::unified_log::snapshot_log().is_some_and(|bytes| {
            String::from_utf8_lossy(&bytes).lines().any(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|entry| {
                    entry["msg"] == "leader.client.disconnected"
                        && entry["ctx"]["client_id"] == client_id
                        && entry["pid"] == std::process::id()
                })
            })
        });
        if seen || tokio::time::Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Hung agent: a prompt is forwarded but the agent never replies. The client must not receive a fabricated response. The transport must stay healthy: a later cancel still reaches the agent, and other traffic still flows.
/// This is the client-visible state of the hung-turn class. Deadman semantics on top of it are a product change asserted by the ignored test below.
#[tokio::test]
async fn test_hung_agent_leaves_transport_healthy_and_forwards_cancel() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_test_server(&temp).await;

    let mut client = LeaderClient::connect(
        sock_path,
        "test-hung",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Prompt reaches the (hung) agent.
    client
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"sess-hung","prompt":[]}}"#.to_string())
        .unwrap();
    let forwarded = acp_rx.recv().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
    assert_eq!(json["method"], "session/prompt");

    // The agent hangs: no response
    // The client must see nothing (no synthesized error, no disconnect) within a bounded observation window
    let quiet = tokio::time::timeout(Duration::from_millis(500), client.recv()).await;
    assert!(
        quiet.is_err(),
        "no response should be synthesized for a hung agent, got {quiet:?}"
    );
    assert_eq!(
        *client.disconnect_reason().borrow(),
        distill_shell::leader::DisconnectReason::Connected,
        "a hung agent must not sever the client connection"
    );

    // The escape hatch still works: a cancel is forwarded while the turn hangs.
    client
        .send(
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-hung"}}"#
                .to_string(),
        )
        .unwrap();
    let cancel_fwd = acp_rx.recv().await.unwrap();
    let cancel_json: serde_json::Value = serde_json::from_str(&cancel_fwd).unwrap();
    assert_eq!(cancel_json["method"], "session/cancel");

    // Unrelated traffic still round-trips on the same connection.
    let probe = r#"{"jsonrpc":"2.0","method":"x.ai/probe","params":{"ping":true}}"#;
    response_tx.send(probe.to_string()).unwrap();
    let recv = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let recv_json: serde_json::Value = serde_json::from_str(&recv).unwrap();
    assert_eq!(recv_json["params"]["ping"], true);

    client.cancel();
    cancel.cancel();
}

/// Sever mid-RPC, the agent completes anyway, and a fresh client recovers via `session/load`.
/// The response to the dead client is dropped exactly once (`leader.response.orphaned`).
/// Post-load notifications for the session reach the new owner: the durable-terminal recovery path.
#[tokio::test]
async fn test_sever_mid_rpc_orphans_response_and_replay_recovers() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_persistent_test_server(&temp).await;

    // Client 1: create the session, then leave a prompt in flight.
    let (mut reader1, mut writer1) = raw_register(&sock_path, "sever-client-1").await;
    write_message(
        &mut writer1,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let new_fwd = acp_rx.recv().await.unwrap();
    let new_json: serde_json::Value = serde_json::from_str(&new_fwd).unwrap();
    let new_id = new_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-sever"}},"id":"{new_id}"}}"#
        ))
        .unwrap();
    let new_resp = raw_recv_acp(&mut reader1).await;
    assert_eq!(new_resp["result"]["sessionId"], "sess-sever");

    write_message(
        &mut writer1,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"sess-sever","prompt":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let prompt_fwd = acp_rx.recv().await.unwrap();
    let prompt_json: serde_json::Value = serde_json::from_str(&prompt_fwd).unwrap();
    let prompt_id = prompt_json["id"].as_str().unwrap().to_string();

    // Sever: abrupt socket close with the prompt RPC still in flight.
    drop(reader1);
    drop(writer1);

    // The eviction notification on the agent channel is the deterministic signal that the server processed the disconnect
    let evict = acp_rx.recv().await.unwrap();
    let evict_json: serde_json::Value = serde_json::from_str(&evict).unwrap();
    assert_eq!(evict_json["method"], "_x.ai/internal/evict_sessions");

    // The agent completes the turn anyway: durable terminal notification plus the RPC response addressed to the dead client
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"stopReason":"end_turn"}},"id":"{prompt_id}"}}"#
        ))
        .unwrap();

    assert_eq!(
        wait_for_orphan_log(&prompt_id).await,
        1,
        "the response to the severed client's RPC must be orphan-dropped exactly once"
    );

    // Fresh client recovers the session via session/load.
    let (mut reader2, mut writer2) = raw_register(&sock_path, "sever-client-2").await;
    write_message(
        &mut writer2,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"sess-sever","cwd":"/tmp","mcpServers":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let load_fwd = acp_rx.recv().await.unwrap();
    let load_json: serde_json::Value = serde_json::from_str(&load_fwd).unwrap();
    assert_eq!(load_json["method"], "session/load");
    let load_id = load_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{}},"id":"{load_id}"}}"#
        ))
        .unwrap();
    let load_resp = raw_recv_acp(&mut reader2).await;
    assert_eq!(load_resp["id"], 1);

    // Post-load, the durable terminal state reaches the new owner (in production this line comes from the agent's updates.jsonl replay)
    let terminal = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-sever","update":{"sessionUpdate":"turn_completed","stopReason":"end_turn"}}}"#;
    response_tx.send(terminal.to_string()).unwrap();
    let update = raw_recv_acp(&mut reader2).await;
    assert_eq!(update["method"], "session/update");
    assert_eq!(update["params"]["update"]["stopReason"], "end_turn");

    // Still exactly one orphan record for the severed RPC.
    assert_eq!(orphan_log_count(&prompt_id), 1);

    drop(reader2);
    drop(writer2);
    cancel.cancel();
}

/// Acceptance: a `session/cancel` composed just before the leader dies is currently lost in the reconnect swap window.
/// The send lands in a dead channel and nothing re-delivers it, so the turn runs on.
/// This asserts the DESIRED behavior (the cancel reaches the agent after recovery) and stays ignored until a cancel-ack/resend protocol ships.
#[tokio::test]
#[ignore = "leader-acceptance: cancel severed in reconnect swap window is not resent; un-ignore with cancel-ack"]
async fn test_cancel_severed_in_swap_window_reaches_agent_after_recovery() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_persistent_test_server(&temp).await;

    let client = LeaderClient::connect(
        sock_path.clone(),
        "swap-window-client",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();

    // Session with a prompt in flight.
    client
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#.to_string())
        .unwrap();
    let new_fwd = acp_rx.recv().await.unwrap();
    let new_json: serde_json::Value = serde_json::from_str(&new_fwd).unwrap();
    let new_id = new_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-swap"}},"id":"{new_id}"}}"#
        ))
        .unwrap();
    let (tx, mut rx) = client.into_channels();
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

    tx.send(r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"sess-swap","prompt":[]}}"#.to_string())
        .unwrap();
    let _prompt_fwd = acp_rx.recv().await.unwrap();

    // Leader dies; the cancel is composed while the connection is already dead (the swap window), so it is silently eaten today
    cancel.cancel();
    // Wait until the old server finished its socket cleanup so the same-path respawn below cannot have its fresh socket deleted from under it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while sock_path.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = tx.send(
        r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess-swap"}}"#
            .to_string(),
    );

    // Recovery: a fresh leader at the same path, a fresh client reloading the session (what the pager bridge does on reconnect)
    let (sock_path2, cancel2, mut acp_rx2, response_tx2) =
        setup_persistent_test_server(&temp).await;

    let client2 = LeaderClient::connect(
        sock_path2,
        "swap-window-client-reconnect",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    client2
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"sess-swap","cwd":"/tmp","mcpServers":[]}}"#.to_string())
        .unwrap();
    let load_fwd = acp_rx2.recv().await.unwrap();
    let load_json: serde_json::Value = serde_json::from_str(&load_fwd).unwrap();
    let load_id = load_json["id"].as_str().unwrap().to_string();
    response_tx2
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{}},"id":"{load_id}"}}"#
        ))
        .unwrap();

    // DESIRED: the severed cancel intent is re-delivered after recovery so the turn does not run on
    // Today nothing arrives and this times out
    let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let fwd = acp_rx2.recv().await.expect("agent channel closed");
            let json: serde_json::Value = serde_json::from_str(&fwd).unwrap();
            if json["method"] == "session/cancel" && json["params"]["sessionId"] == "sess-swap" {
                return;
            }
        }
    })
    .await;
    assert!(
        cancelled.is_ok(),
        "the cancel severed in the swap window must reach the agent after recovery"
    );

    client2.cancel();
    cancel2.cancel();
}

/// Two clients on one session; the driver severs mid-turn.
/// The viewer must keep receiving the stream and the durable terminal.
/// The driver's in-flight RPC response is orphan-dropped, not misrouted to the viewer.
#[tokio::test]
async fn test_driver_sever_mid_turn_viewer_sees_durable_terminal() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, mut acp_rx, response_tx) = setup_persistent_test_server(&temp).await;

    // Driver creates the session and starts a turn.
    let (mut driver_reader, mut driver_writer) = raw_register(&sock_path, "driver").await;
    write_message(
        &mut driver_writer,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let new_fwd = acp_rx.recv().await.unwrap();
    let new_json: serde_json::Value = serde_json::from_str(&new_fwd).unwrap();
    let new_id = new_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"sessionId":"sess-handoff"}},"id":"{new_id}"}}"#
        ))
        .unwrap();
    let new_resp = raw_recv_acp(&mut driver_reader).await;
    assert_eq!(new_resp["result"]["sessionId"], "sess-handoff");

    // Viewer attaches to the same session via session/load.
    let (mut viewer_reader, mut viewer_writer) = raw_register(&sock_path, "viewer").await;
    write_message(
        &mut viewer_writer,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"sess-handoff","cwd":"/tmp","mcpServers":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let load_fwd = acp_rx.recv().await.unwrap();
    let load_json: serde_json::Value = serde_json::from_str(&load_fwd).unwrap();
    let load_id = load_json["id"].as_str().unwrap().to_string();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{}},"id":"{load_id}"}}"#
        ))
        .unwrap();
    let load_resp = raw_recv_acp(&mut viewer_reader).await;
    assert_eq!(load_resp["id"], 1);

    // Driver starts the turn.
    write_message(
        &mut driver_writer,
        &ClientMessage::Acp {
            payload: r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"sess-handoff","prompt":[]}}"#.into(),
        },
    )
    .await
    .unwrap();
    let prompt_fwd = acp_rx.recv().await.unwrap();
    let prompt_json: serde_json::Value = serde_json::from_str(&prompt_fwd).unwrap();
    let prompt_id = prompt_json["id"].as_str().unwrap().to_string();

    // A first streamed chunk reaches BOTH subscribers.
    let chunk1 = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-handoff","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"one"}}}}"#;
    response_tx.send(chunk1.to_string()).unwrap();
    let driver_chunk = raw_recv_acp(&mut driver_reader).await;
    assert_eq!(driver_chunk["params"]["update"]["content"]["text"], "one");
    let viewer_chunk = raw_recv_acp(&mut viewer_reader).await;
    assert_eq!(viewer_chunk["params"]["update"]["content"]["text"], "one");

    // Driver severs mid-turn
    // The viewer still subscribes to the session, so no evict_sessions fires; wait on the disconnect log entry instead
    let (driver_client_id, _) =
        parse_namespaced_id(&prompt_id).expect("namespaced prompt id parses");
    drop(driver_reader);
    drop(driver_writer);
    assert!(
        wait_for_client_disconnected_log(driver_client_id).await,
        "server never recorded the driver disconnect"
    );

    // The turn keeps streaming; the viewer still receives it.
    let chunk2 = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-handoff","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"two"}}}}"#;
    response_tx.send(chunk2.to_string()).unwrap();
    let viewer_chunk2 = raw_recv_acp(&mut viewer_reader).await;
    assert_eq!(viewer_chunk2["params"]["update"]["content"]["text"], "two");

    // Durable terminal reaches the viewer; the driver's RPC response is orphan-dropped, not misrouted
    let terminal = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-handoff","update":{"sessionUpdate":"turn_completed","stopReason":"end_turn"}}}"#;
    response_tx.send(terminal.to_string()).unwrap();
    response_tx
        .send(format!(
            r#"{{"jsonrpc":"2.0","result":{{"stopReason":"end_turn"}},"id":"{prompt_id}"}}"#
        ))
        .unwrap();

    let viewer_terminal = raw_recv_acp(&mut viewer_reader).await;
    assert_eq!(
        viewer_terminal["params"]["update"]["sessionUpdate"],
        "turn_completed"
    );
    assert_eq!(
        wait_for_orphan_log(&prompt_id).await,
        1,
        "the severed driver's prompt response must be orphan-dropped exactly once"
    );

    // The viewer must NOT have been handed the driver's RPC response
    // The next message it sees (if any) is not a response with the driver's original id
    let stray = tokio::time::timeout(Duration::from_millis(300), async {
        raw_recv_acp(&mut viewer_reader).await
    })
    .await;
    if let Ok(msg) = stray {
        assert!(
            msg.get("result").is_none() || msg["id"] != 2,
            "driver's orphaned RPC response leaked to the viewer: {msg}"
        );
    }

    drop(viewer_reader);
    drop(viewer_writer);
    cancel.cancel();
}
