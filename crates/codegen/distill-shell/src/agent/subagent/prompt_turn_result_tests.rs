use super::*;

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("result test wait timed out")
}

fn prompt_turn_ok(
    completion_kind: PromptCompletionKind,
    structured_output: Option<Result<serde_json::Value, String>>,
) -> crate::session::commands::PromptTurnOk {
    crate::session::commands::PromptTurnOk {
        stop_reason: agent_client_protocol::StopReason::EndTurn,
        total_tokens: 0,
        turn_snapshot: None,
        completion_kind,
        structured_output,
        usage: None,
        tool_overrides: None,
    }
}

fn reduce(
    turn_result: Result<PromptTurnResult, tokio::sync::oneshot::error::RecvError>,
    mode: PromptTurnResultMode,
    final_text: &str,
    was_cancelled: bool,
) -> PromptTurnResultOutput {
    let summary = || "summary".to_string();
    let max_turns = |limit| format!("max turns: {limit}");
    reduce_prompt_turn_result(PromptTurnResultInput {
        result: SubagentResult::default(),
        turn_result,
        mode,
        final_text: final_text.to_string(),
        was_cancelled,
        summaries: PromptTurnResultSummaries {
            success: &summary,
            max_turns: &max_turns,
            cancelled: &summary,
        },
        result_tokens: 7,
    })
}

/// The truncation contract: a MaxTokens child marks every non-Ok surface —
/// the free-text note, the error suffixes — while a validated document stays
/// raw JSON and a non-truncated turn carries no note anywhere.
#[test]
fn max_tokens_turn_marks_every_non_ok_surface() {
    let truncated_turn = |completion, structured| {
        let mut ok = prompt_turn_ok(completion, structured);
        ok.stop_reason = agent_client_protocol::StopReason::MaxTokens;
        ok
    };

    // Free-text success: note appended.
    let out = reduce(
        Ok(Ok(truncated_turn(PromptCompletionKind::Completed, None))),
        schema_free_initial(),
        "cut report",
        false,
    );
    assert!(out.result.success);
    assert!(
        out.result
            .output
            .contains("truncated by the output token limit"),
        "free-text output must carry the note: {}",
        out.result.output
    );

    // Structured error arms: suffix stated.
    let out = reduce(
        Ok(Ok(truncated_turn(
            PromptCompletionKind::Completed,
            Some(Err("bad shape".to_string())),
        ))),
        schema_initial(),
        "cut report",
        false,
    );
    assert!(
        out.result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("truncated by the output token limit")),
        "structured error must state the truncation: {:?}",
        out.result.error
    );

    // Validated document: raw JSON by contract, no note.
    let out = reduce(
        Ok(Ok(truncated_turn(
            PromptCompletionKind::Completed,
            Some(Ok(serde_json::json!({"a": 1}))),
        ))),
        schema_initial(),
        "cut report",
        false,
    );
    assert_eq!(out.result.output.as_ref(), "{\"a\":1}");

    // Non-truncated free-text turn: no note.
    let out = reduce(
        Ok(Ok(prompt_turn_ok(PromptCompletionKind::Completed, None))),
        schema_free_initial(),
        "whole report",
        false,
    );
    assert_eq!(out.result.output.as_ref(), "whole report");
}

fn schema_initial() -> PromptTurnResultMode {
    PromptTurnResultMode::Initial {
        requires_structured_output: true,
    }
}

fn schema_free_initial() -> PromptTurnResultMode {
    PromptTurnResultMode::Initial {
        requires_structured_output: false,
    }
}

#[test]
fn initial_schema_requirement_accepts_valid_structured_output() {
    let output = reduce(
        Ok(Ok(prompt_turn_ok(
            PromptCompletionKind::Completed,
            Some(Ok(serde_json::json!({"answer": 42}))),
        ))),
        schema_initial(),
        "fallback",
        false,
    );

    assert_eq!(
        (
            output.result.success,
            output.result.error.as_deref(),
            output.result.output.as_ref(),
            output.result.tokens_used,
        ),
        (true, None, r#"{"answer":42}"#, 7),
    );
}

#[test]
fn initial_schema_requirement_rejects_missing_or_invalid_output() {
    let cases = [
        (
            Some(Err::<serde_json::Value, _>("schema mismatch".to_string())),
            "structured output validation failed: schema mismatch",
        ),
        (None, "structured output requested but none produced"),
    ];

    for (structured_output, expected_error) in cases {
        let output = reduce(
            Ok(Ok(prompt_turn_ok(
                PromptCompletionKind::Completed,
                structured_output,
            ))),
            schema_initial(),
            "fallback",
            false,
        );
        assert_eq!(
            (
                output.result.success,
                output.result.error.as_deref(),
                output.result.output.as_ref(),
            ),
            (false, Some(expected_error), "fallback"),
        );
    }
}

#[test]
fn parent_followup_is_schema_free_while_initial_schema_path_is_not() {
    let initial = reduce(
        Ok(Ok(prompt_turn_ok(
            PromptCompletionKind::Completed,
            Some(Ok(serde_json::json!({"answer": 42}))),
        ))),
        schema_initial(),
        "follow-up answer",
        false,
    );
    let followup = reduce(
        Ok(Ok(prompt_turn_ok(PromptCompletionKind::Completed, None))),
        PromptTurnResultMode::ParentFollowup,
        "follow-up answer",
        false,
    );

    assert_eq!(initial.result.output.as_ref(), r#"{"answer":42}"#);
    assert_eq!(followup.result.output.as_ref(), "follow-up answer");
    assert!(followup.result.success);
}

#[test]
fn parent_followup_rejects_impossible_structured_receipt() {
    let output = reduce(
        Ok(Ok(prompt_turn_ok(
            PromptCompletionKind::Completed,
            Some(Ok(serde_json::json!({"unexpected": true}))),
        ))),
        PromptTurnResultMode::ParentFollowup,
        "follow-up text",
        false,
    );

    assert!(!output.result.success);
    assert_eq!(
        output.result.error.as_deref(),
        Some("Parent follow-up unexpectedly produced structured output")
    );
    assert_eq!(output.result.output.as_ref(), "follow-up text");
    assert!(output.result.output_usage_incomplete);
}

#[test]
fn successful_empty_followup_does_not_reuse_the_initial_output() {
    let previous = SubagentResult {
        success: true,
        output: Arc::from("initial answer"),
        ..Default::default()
    };
    let empty_summary = String::new;
    let max_turns = |_| String::new();
    let output = reduce_prompt_turn_result(PromptTurnResultInput {
        result: previous,
        turn_result: Ok(Ok(prompt_turn_ok(PromptCompletionKind::Completed, None))),
        mode: PromptTurnResultMode::ParentFollowup,
        final_text: String::new(),
        was_cancelled: false,
        summaries: PromptTurnResultSummaries {
            success: &empty_summary,
            max_turns: &max_turns,
            cancelled: &empty_summary,
        },
        result_tokens: 0,
    });

    assert!(output.result.success);
    assert_eq!(output.result.output.as_ref(), "");
}

#[test]
fn every_completion_kind_maps_to_the_terminal_contract() {
    let cases = [
        (
            PromptCompletionKind::Completed,
            (true, false, None, false, false),
        ),
        (
            PromptCompletionKind::StationarityEnded,
            (true, false, None, false, false),
        ),
        (
            PromptCompletionKind::Cancelled {
                category: None,
                context: None,
            },
            (false, true, Some("Subagent turn was cancelled"), true, true),
        ),
        (
            PromptCompletionKind::MaxTurnsReached { limit: 4 },
            (
                false,
                true,
                Some("max turns reached (limit: 4)"),
                true,
                false,
            ),
        ),
        (
            PromptCompletionKind::Rewound,
            (false, true, Some("Subagent turn was rewound"), true, true),
        ),
        (
            PromptCompletionKind::RemovedFromQueue,
            (
                false,
                true,
                Some("Subagent turn was removed before it ran"),
                true,
                false,
            ),
        ),
    ];

    for (kind, expected) in cases {
        let output = reduce(
            Ok(Ok(prompt_turn_ok(kind, None))),
            schema_free_initial(),
            "partial",
            false,
        );
        assert_eq!(
            (
                output.result.success,
                output.result.cancelled,
                output.result.error.as_deref(),
                output.result.output_usage_incomplete,
                output.cancellation_may_hide_usage,
            ),
            expected,
        );
        assert_eq!(output.result.output.as_ref(), "partial");
    }
}

#[tokio::test]
async fn session_error_and_cancelled_channel_drop_preserve_partial_text() {
    let error = serde_json::from_value::<agent_client_protocol::Error>(serde_json::json!({
        "code": -32603,
        "message": "receipt failed",
        "data": null,
    }))
    .expect("valid ACP error");
    let session_error = reduce(
        Ok(Err(error)),
        PromptTurnResultMode::ParentFollowup,
        "partial",
        false,
    );
    assert_eq!(
        (
            session_error.result.cancelled,
            session_error.result.error.as_deref(),
            session_error.result.output.as_ref(),
            session_error.result.output_usage_incomplete,
            session_error.cancellation_may_hide_usage,
        ),
        (
            false,
            Some("Session error: receipt failed"),
            "partial",
            true,
            false,
        ),
    );

    let (sender, receiver) = tokio::sync::oneshot::channel::<PromptTurnResult>();
    drop(sender);
    let channel_error = await_with_timeout(receiver).await;
    let dropped = reduce(
        channel_error,
        PromptTurnResultMode::ParentFollowup,
        "partial",
        true,
    );
    assert_eq!(
        (
            dropped.result.cancelled,
            dropped.result.error.as_deref(),
            dropped.result.output.as_ref(),
            dropped.result.output_usage_incomplete,
            dropped.cancellation_may_hide_usage,
        ),
        (true, Some("Subagent was cancelled"), "partial", true, true,),
    );
}
