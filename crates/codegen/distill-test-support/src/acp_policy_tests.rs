use agent_client_protocol as acp;
use serde_json::json;

use super::{PermissionDecision, QuestionDecision, Reply, RequestPolicy};
use crate::acp_ask_user_question::AskUserQuestionRequest;

fn selected(option_id: &'static str) -> acp::RequestPermissionOutcome {
    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
}

fn permission_request(options: Vec<acp::PermissionOption>) -> acp::RequestPermissionRequest {
    acp::RequestPermissionRequest::new(
        "s1",
        acp::ToolCallUpdate::new("tc1", acp::ToolCallUpdateFields::default()),
        options,
    )
}

#[test]
fn request_policy_answers_the_nth_request_with_its_exception_and_the_rest_with_the_default() {
    let policy = RequestPolicy::new(PermissionDecision::Allow)
        .with_nth(2, PermissionDecision::Deny)
        .with_nth(4, PermissionDecision::Cancel);

    let decisions: Vec<PermissionDecision> = (1..=5).map(|n| policy.decision_for(n)).collect();

    assert_eq!(
        vec![
            PermissionDecision::Allow,
            PermissionDecision::Deny,
            PermissionDecision::Allow,
            PermissionDecision::Cancel,
            PermissionDecision::Allow,
        ],
        decisions
    );
}

#[test]
fn permission_decision_selects_the_offered_option_of_its_kind() {
    let request = permission_request(vec![
        acp::PermissionOption::new("allow-once", "Yes", acp::PermissionOptionKind::AllowOnce),
        acp::PermissionOption::new(
            "allow-always",
            "Always",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new("reject-once", "No", acp::PermissionOptionKind::RejectOnce),
    ]);
    let cases = [
        (
            PermissionDecision::Allow,
            Reply::Now(selected("allow-once")),
        ),
        (
            PermissionDecision::AllowAlways,
            Reply::Now(selected("allow-always")),
        ),
        (
            PermissionDecision::Deny,
            Reply::Now(selected("reject-once")),
        ),
        (
            PermissionDecision::Cancel,
            Reply::Now(acp::RequestPermissionOutcome::Cancelled),
        ),
        (
            PermissionDecision::HoldUntilCancel,
            Reply::AfterCancel {
                value: acp::RequestPermissionOutcome::Cancelled,
                session_id: acp::SessionId::new("s1"),
            },
        ),
    ];

    for (decision, expected) in cases {
        assert_eq!(expected, decision.reply(&request), "{decision:?}");
    }
}

#[test]
fn permission_decision_cancels_when_the_agent_offers_no_option_of_its_kind() {
    let only_allow_once = permission_request(vec![acp::PermissionOption::new(
        "allow-once",
        "Yes",
        acp::PermissionOptionKind::AllowOnce,
    )]);

    assert_eq!(
        Reply::Now(acp::RequestPermissionOutcome::Cancelled),
        PermissionDecision::Deny.reply(&only_allow_once)
    );
}

#[test]
fn question_accept_selects_the_first_option_of_every_question_keyed_by_question_text() {
    let request: AskUserQuestionRequest = serde_json::from_value(json!({
        "sessionId": "s1",
        "toolCallId": "tc1",
        "questions": [
            { "question": "Which database?", "options": [
                { "label": "Redis", "description": "" },
                { "label": "Postgres", "description": "" }
            ] },
            { "question": "Which framework?", "options": [
                { "label": "React", "description": "" }
            ] },
            { "question": "Anything else?", "options": [] }
        ],
        "mode": "default"
    }))
    .expect("question request");

    assert_eq!(
        Reply::Now(json!({
            "outcome": "accepted",
            "answers": { "Which database?": ["Redis"], "Which framework?": ["React"] }
        })),
        QuestionDecision::Accept.reply(&request)
    );
}
