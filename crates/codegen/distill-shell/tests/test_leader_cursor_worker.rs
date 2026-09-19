// Modified for Distill by Samuel Fajreldines, 2026.
//! Worker-door control commands over a real leader IPC server. No hub
//! connection is made: every start here fails validation before the bridge is dialed.
#![cfg(unix)]
use distill_shell::cpu_profile::ControlErrorCode;
use distill_shell::leader::{
    ClientCapabilities, ClientMode, ControlCommand, ControlPayload, CursorWorkerStartArgs,
    LeaderClient, ServerHandle, spawn_leader_server,
};
use std::time::Duration;
use tempfile::TempDir;
async fn wait_for_socket(sock_path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(sock_path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Timeout waiting for socket to become available");
}
async fn connect(temp: &TempDir) -> (LeaderClient, ServerHandle) {
    let sock_path = temp.path().join("leader-cursor-worker.sock");
    let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
    wait_for_socket(&sock_path).await;
    let client = LeaderClient::connect(
        sock_path,
        "cursor-worker-test",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .unwrap();
    (client, handle)
}
mod compiled_out {
    use super::*;
    #[tokio::test]
    async fn every_command_reports_not_compiled_in_and_capability_is_false() {
        let temp = TempDir::new().unwrap();
        let (client, handle) = connect(&temp).await;
        let caps = client
            .registration()
            .leader_capabilities
            .clone()
            .expect("capabilities");
        assert!(!caps.cursor_worker);
        for command in [
            ControlCommand::CursorWorkerStatus,
            ControlCommand::CursorWorkerStop,
            ControlCommand::CursorWorkerStart(CursorWorkerStartArgs {
                name: None,
                worker_dirs: vec!["/tmp".to_owned()],
                max_agents: None,
            }),
        ] {
            let error = client.send_control(command).await.unwrap().unwrap_err();
            assert_eq!(ControlErrorCode::InternalError, error.code);
            assert_eq!(
                "cursor worker support is not compiled into this leader",
                error.message
            );
        }
        let info = client
            .send_control(ControlCommand::GetLeaderInfo)
            .await
            .unwrap()
            .unwrap();
        let ControlPayload::LeaderInfo { cursor_worker, .. } = info else {
            panic!("expected leader info, got {info:?}");
        };
        let summary = cursor_worker.expect("the stub still reports a summary");
        assert_eq!(("none", 0), (summary.state.as_str(), summary.claims));
        client.cancel();
        handle.cancel.cancel();
    }
}
