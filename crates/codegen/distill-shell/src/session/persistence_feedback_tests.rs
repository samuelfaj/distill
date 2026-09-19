use super::*;
use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, FeedbackSubmission, FeedbackType, RatingType,
};

#[test]
fn test_feedback_jsonl_multi_line_roundtrip() {
    let submission = FeedbackSubmission {
        session_id: "session-abc".into(),
        user_id: None,
        client_type: ClientType::Tui,
        feedback_type: FeedbackType::RatingWithText,
        turn_number: Some(7),
        rating_type: Some(RatingType::Thumbs),
        rating_value: Some(-1),
        feedback_text: Some("could be better".into()),
        model_id: Some("grok-3-fast".into()),
        resolved_model_id: Some("grok-4.5".into()),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let entries = vec![
        LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "s1".into(),
            turn_number: Some(7),
            solicited: false,
            request_id: None,
            dismissed: false,
            submission: Some(submission),
        }),
        LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "s1".into(),
            turn_number: None,
            solicited: true,
            request_id: Some("req-456".into()),
            dismissed: true,
            submission: None,
        }),
    ];

    let jsonl: String = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
        .collect();
    let lines: Vec<serde_json::Value> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // `dismissed` is skipped when false; `requestId` and `submission` when absent.
    let shape = |line: &serde_json::Value| {
        (
            line.get("type").cloned().unwrap_or(serde_json::Value::Null),
            line.get("requestId").cloned(),
            line.get("dismissed").cloned(),
            line.get("submission").is_some(),
        )
    };
    assert_eq!(
        lines.iter().map(shape).collect::<Vec<_>>(),
        [
            (serde_json::json!("user_feedback"), None, None, true),
            (
                serde_json::json!("user_feedback"),
                Some(serde_json::json!("req-456")),
                Some(serde_json::json!(true)),
                false
            ),
        ]
    );

    let parsed: Vec<LocalFeedbackEntry> = lines
        .into_iter()
        .map(|line| serde_json::from_value(line).unwrap())
        .collect();
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        serde_json::to_value(&entries).unwrap()
    );
}
