use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use futures_util::FutureExt as _;
use serde_json::{Value, json};

use super::ScriptedClient;
use crate::acp_ask_user_question::ASK_USER_QUESTION_METHOD;
use crate::acp_policy::{ClientPolicy, PermissionDecision, QuestionDecision, RequestPolicy};
use crate::acp_transcript::TranscriptEntry;

fn permission_request(
    session_id: &'static str,
    tool_call_id: &'static str,
) -> acp::RequestPermissionRequest {
    acp::RequestPermissionRequest::new(
        session_id,
        acp::ToolCallUpdate::new(tool_call_id, acp::ToolCallUpdateFields::default()),
        vec![
            acp::PermissionOption::new("allow-once", "Yes", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("reject-once", "No", acp::PermissionOptionKind::RejectOnce),
        ],
    )
}

fn selected(option_id: &'static str) -> acp::RequestPermissionOutcome {
    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
}

fn ext_request(method: &'static str, params: &Value) -> acp::ExtRequest {
    acp::ExtRequest::new(
        method,
        Arc::from(serde_json::value::to_raw_value(params).expect("params serialize")),
    )
}

fn parse(response: &acp::ExtResponse) -> Value {
    serde_json::from_str(response.0.get()).expect("ext response is JSON")
}

#[tokio::test]
async fn permission_requests_are_numbered_in_arrival_order_across_the_client() {
    let client = ScriptedClient::new(ClientPolicy {
        permissions: RequestPolicy::new(PermissionDecision::Allow)
            .with_nth(2, PermissionDecision::Deny),
        questions: RequestPolicy::new(QuestionDecision::Cancel),
        ..ClientPolicy::default()
    });

    let mut outcomes = Vec::new();
    for (session_id, tool_call_id) in [("s1", "tc1"), ("s2", "tc2"), ("s1", "tc3")] {
        let response = client
            .request_permission(permission_request(session_id, tool_call_id))
            .await
            .expect("policy reply");
        outcomes.push(response.outcome);
    }

    assert_eq!(
        vec![
            selected("allow-once"),
            selected("reject-once"),
            selected("allow-once")
        ],
        outcomes
    );
}

#[tokio::test]
async fn held_permission_is_answered_and_recorded_only_once_its_session_is_released() {
    let client = ScriptedClient::new(ClientPolicy {
        permissions: RequestPolicy::new(PermissionDecision::HoldUntilCancel),
        ..ClientPolicy::default()
    });
    let session_id = acp::SessionId::new("s1");
    let mut held = client.request_permission(permission_request("s1", "tc1"));

    assert_eq!(None, held.as_mut().now_or_never());
    assert_eq!(Vec::<TranscriptEntry>::new(), client.transcript().entries());

    let (response, ()) = tokio::join!(held, client.holds().release_held_requests(&session_id));

    assert_eq!(
        acp::RequestPermissionOutcome::Cancelled,
        response.expect("policy reply").outcome
    );
    assert_eq!(
        vec![TranscriptEntry::PermissionRequest {
            request: permission_request("s1", "tc1"),
            outcome: acp::RequestPermissionOutcome::Cancelled,
        }],
        client.transcript().entries()
    );
}

#[tokio::test]
async fn question_request_gets_the_policy_reply() {
    let client = ScriptedClient::new(ClientPolicy {
        questions: RequestPolicy::new(QuestionDecision::Accept),
        ..ClientPolicy::default()
    });
    let params = json!({
        "sessionId": "s1",
        "toolCallId": "tc1",
        "questions": [{ "question": "Which?", "options": [{ "label": "A", "description": "" }] }],
        "mode": "default"
    });

    let reply = client
        .ext_method(ext_request(ASK_USER_QUESTION_METHOD, &params))
        .await
        .expect("policy reply");

    let expected_answer = json!({ "outcome": "accepted", "answers": { "Which?": ["A"] } });
    assert_eq!(expected_answer, parse(&reply));
    assert_eq!(
        vec![TranscriptEntry::ExtRequest {
            method: ASK_USER_QUESTION_METHOD.to_owned(),
            params,
            reply: expected_answer,
        }],
        client.transcript().entries()
    );
}

#[tokio::test]
async fn other_extension_requests_are_answered_null_and_recorded() {
    let client = ScriptedClient::new(ClientPolicy::default());
    let params = json!({ "sessionId": "s1", "toolCallId": "tc2" });

    let reply = client
        .ext_method(ext_request("x.ai/exit_plan_mode", &params))
        .await
        .expect("null reply");

    assert_eq!(Value::Null, parse(&reply));
    assert_eq!(
        vec![TranscriptEntry::ExtRequest {
            method: "x.ai/exit_plan_mode".to_owned(),
            params,
            reply: Value::Null,
        }],
        client.transcript().entries()
    );
}

#[tokio::test]
async fn malformed_question_params_are_rejected_as_invalid_params() {
    let client = ScriptedClient::new(ClientPolicy::default());
    let params = json!({ "sessionId": "s1", "questions": "not a list" });

    let error = client
        .ext_method(ext_request(ASK_USER_QUESTION_METHOD, &params))
        .await
        .expect_err("malformed params were answered");

    assert_eq!(acp::Error::invalid_params().code, error.code);
}

#[tokio::test]
async fn held_question_is_dismissed_once_the_session_in_its_params_is_released() {
    let client = ScriptedClient::new(ClientPolicy {
        questions: RequestPolicy::new(QuestionDecision::HoldUntilCancel),
        ..ClientPolicy::default()
    });
    let session_id = acp::SessionId::new("s1");
    let params = json!({ "sessionId": "s1", "toolCallId": "tc1", "questions": [] });
    let mut held = client.ext_method(ext_request(ASK_USER_QUESTION_METHOD, &params));

    assert!(held.as_mut().now_or_never().is_none());

    let (reply, ()) = tokio::join!(held, client.holds().release_held_requests(&session_id));

    assert_eq!(
        json!({ "outcome": "cancelled" }),
        parse(&reply.expect("policy reply"))
    );
}
