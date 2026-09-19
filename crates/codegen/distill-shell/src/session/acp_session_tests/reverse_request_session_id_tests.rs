// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression guard: every blocking reverse-request (permission, `ask_user_question`, plan-approval) must carry a non-empty `sessionId`.
//! Otherwise Tier-2 routing silently drops it (`server.rs`).
//! The invariant holds today; these tests pin it.
use distill_tools::implementations::distill::ask_user_question::{
    AskUserQuestionExtRequest, AskUserQuestionMode,
};
use distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeExtRequest;

#[test]
fn ask_user_question_request_carries_session_id() {
    let req = AskUserQuestionExtRequest {
        session_id: "sess-abc".to_string(),
        tool_call_id: "call-1".to_string(),
        questions: vec![],
        mode: AskUserQuestionMode::Default,
    };
    assert!(!req.session_id.is_empty());
    // Wire format is camelCase (`sessionId`); Tier-2 routing reads it.
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json.get("sessionId"), Some(&serde_json::json!("sess-abc")));
    assert!(
        json.get("sessionId")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    );
}

#[test]
fn exit_plan_mode_request_carries_session_id() {
    let req = ExitPlanModeExtRequest {
        session_id: "sess-xyz".to_string(),
        tool_call_id: "call-2".to_string(),
        plan_content: Some("plan".to_string()),
    };
    assert!(!req.session_id.is_empty());
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json.get("sessionId"), Some(&serde_json::json!("sess-xyz")));
    assert!(
        json.get("sessionId")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    );
}
