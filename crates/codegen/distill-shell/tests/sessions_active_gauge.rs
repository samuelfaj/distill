// Modified for Distill by Samuel Fajreldines, 2026.
//! `sessions_active` is counted at the one spawn point shared by every way the agent hosts a session.
//! This is the sole test in its binary, so the process-global gauge sees no other session traffic and the counts can be exact.

#[allow(dead_code)]
mod acp_harness;

use acp_harness::{AutoApproveClient, RPC_TIMEOUT, connect_and_auth, new_session, run_agent_test};
use agent_client_protocol::{self as acp, Agent as _};

fn sessions_active() -> u32 {
    distill_telemetry::activity::gauge_value(distill_telemetry::activity::SESSIONS_ACTIVE_KEY)
}

#[test]
fn sessions_active_follows_hosting_exactly() {
    run_agent_test(|cwd, _mock| async move {
        assert_eq!(0, sessions_active(), "nothing hosted before session/new");
        let (conn, _init) = connect_and_auth(AutoApproveClient, "sessions-active-pin").await;
        let first = new_session(&conn, &cwd).await;
        assert_eq!(1, sessions_active(), "a hosted session counts itself");
        let second = new_session(&conn, &cwd).await;
        assert_eq!(2, sessions_active(), "a host with two sessions reads 2");

        close_and_await(&conn, second, 1).await;
        close_and_await(&conn, first, 0).await;
    });
}

async fn close_and_await(conn: &acp::ClientSideConnection, id: acp::SessionId, expect: u32) {
    tokio::time::timeout(
        RPC_TIMEOUT,
        conn.close_session(acp::CloseSessionRequest::new(id)),
    )
    .await
    .expect("session/close timed out")
    .expect("session/close failed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while sessions_active() != expect {
        assert!(
            std::time::Instant::now() < deadline,
            "closing a session must release its gauge slot (want {expect})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
