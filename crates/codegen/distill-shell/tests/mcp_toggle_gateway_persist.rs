// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: a gateway-connector enable via `x.ai/mcp/toggle` must propagate a failed
//! `disabled_mcp_servers` clear as an error, like the local-server sibling (PersistFailed).

mod acp_harness;

use std::sync::Arc;

use acp_harness::{AutoApproveClient, RPC_TIMEOUT, connect_and_auth, new_session};
use agent_client_protocol::{self as acp, Agent as _};
use serde_json::json;

#[test]
fn gateway_toggle_propagates_failed_enable_persist() {
    acp_harness::run_agent_test(|cwd, _server| async move {
        let distill_home =
            std::path::PathBuf::from(std::env::var("GROK_HOME").expect("harness sets GROK_HOME"));

        let (conn, _init) = connect_and_auth(AutoApproveClient, "gateway-persist-test").await;
        let session_id = new_session(&conn, &cwd).await;

        // Unparseable config.toml: the enable write refuses to clobber it and
        // errors, so the disabled entry cannot be cleared.
        std::fs::write(distill_home.join("config.toml"), "not toml [[[").unwrap();

        let params = serde_json::value::RawValue::from_string(
            json!({
                "session_id": session_id.0.to_string(),
                "server_name": "managed_gateway:linear",
                "enabled": true,
            })
            .to_string(),
        )
        .expect("serialize mcp/toggle params");
        let err = tokio::time::timeout(
            RPC_TIMEOUT,
            conn.ext_method(acp::ExtRequest::new("x.ai/mcp/toggle", Arc::from(params))),
        )
        .await
        .expect("mcp/toggle timed out")
        .expect_err("a failed enable persist must not report ok:true");
        let detail = format!("{err:?}");
        assert!(
            detail.contains("disabled MCP server entry"),
            "error must name the failed persist, got: {detail}"
        );
    });
}
