// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/session/ultracode/set`: capability, toggle/explicit set, effort untouched.
use agent_client_protocol as acp;

use super::{build_minimal_agent_for_tests, make_test_handle, run_local_for_bridge_test};
use crate::agent::mvp_agent::MvpAgent;
use crate::extensions::session_ultracode::{SET_METHOD, handle};

const SESSION: &str = "ultracode-sess";

fn agent_with_session() -> MvpAgent {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new(SESSION);
    let mut session = make_test_handle("test-model", false, None);
    session.info.id = sid.clone();
    session.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::Low);
    agent.insert_resident(&sid, session);
    agent
}

async fn set(agent: &MvpAgent, params: serde_json::Value) -> Result<serde_json::Value, acp::Error> {
    let request =
        acp::ExtRequest::new(SET_METHOD, serde_json::value::to_raw_value(&params).unwrap().into());
    let response = handle(agent, &request).await?;
    Ok(serde_json::from_str(response.0.get()).unwrap())
}

fn flag(agent: &MvpAgent) -> bool {
    let sid = acp::SessionId::new(SESSION);
    agent.resident_handle(&sid).unwrap().ultracode.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn initialize_advertises_session_ultracode() {
    let capabilities = crate::agent::mvp_agent::acp_agent::x_ai_capabilities();
    assert_eq!(capabilities["sessionUltracode"], serde_json::json!(true));
}

/// Omitted `enabled` toggles; explicit values set. The flag is the only thing that changes, never effort.
#[test]
fn ultracode_set_toggles_and_sets_without_touching_effort() {
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        let sid = acp::SessionId::new(SESSION);
        assert!(!flag(&agent));

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION })).await.unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": true }));
        assert!(flag(&agent));

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION, "enabled": true }))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": true }), "explicit on is idempotent");

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION })).await.unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": false }));
        assert!(!flag(&agent));

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION, "enabled": false }))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": false }));

        let handle = agent.resident_handle(&sid).unwrap();
        assert_eq!(
            handle.reasoning_effort,
            Some(distill_sampling_types::ReasoningEffort::Low),
            "toggling Ultracode must not change effort"
        );
    });
}

#[test]
fn ultracode_set_unknown_session_is_not_found() {
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        let err = set(&agent, serde_json::json!({ "sessionId": "nope", "enabled": true }))
            .await
            .unwrap_err();
        assert_eq!(err.code, acp::Error::resource_not_found(None).code);
    });
}
