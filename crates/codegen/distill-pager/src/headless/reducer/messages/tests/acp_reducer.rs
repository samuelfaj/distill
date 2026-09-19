use super::*;
use pretty_assertions::assert_eq;

#[test]
fn acp_reducer_maps_agent_message_to_text() {
    let mut r = AcpReducer;
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(first) = out.first() else {
        panic!("expected an ACP line: {out:?}");
    };
    assert_eq!(first, &json!({"type": "text", "data": "hi"}));
}

#[test]
fn acp_reducer_maps_tool_call_to_native_shape() {
    let mut r = AcpReducer;
    let out = r.reduce(StreamEvent::ToolCall(tool_call_ev()));
    let Some(first) = out.first() else {
        panic!("expected an ACP line: {out:?}");
    };
    assert_eq!(
        first,
        &json!({
            "type": "tool_call",
            "toolCallId": "t1",
            "title": "Bash",
            "kind": "execute",
            "status": "in_progress",
            "toolName": "bash",
            "rawInput": {"command": "ls"},
            "content": [],
            "locations": [],
        })
    );
}

#[test]
fn acp_reducer_maps_tool_call_update_to_native_shape() {
    let mut r = AcpReducer;
    let out = r.reduce(StreamEvent::ToolCallUpdate(tool_update(
        "completed",
        json!({"ok": true}),
    )));
    let Some(first) = out.first() else {
        panic!("expected an ACP line: {out:?}");
    };
    assert_eq!(
        first,
        &json!({
            "type": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [],
            "rawOutput": {"ok": true},
            "locations": [],
        })
    );
}

#[test]
fn acp_response_completed_emits_usage_line() {
    let mut r = AcpReducer;
    let out = r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_1".into()),
        stop_reason: Some("tool_use".into()),
        usage: Some(ResponseUsage {
            input_tokens: 5,
            output_tokens: 2,
            ..Default::default()
        }),
        signature: Some("sig".into()),
        stop_sequence: None,
    });
    let Some(usage) = out.first() else {
        panic!("expected a usage line: {out:?}");
    };
    assert_eq!(msg_type(usage), Some("usage"));
    assert_eq!(json_str(usage, "/messageId"), Some("msg_1"));
    assert_eq!(json_str(usage, "/stopReason"), Some("tool_use"));
    assert_eq!(
        usage.pointer("/usage/input_tokens").and_then(Value::as_u64),
        Some(5)
    );
    assert_eq!(json_str(usage, "/signature"), Some("sig"));
}

#[test]
fn acp_finish_emits_end_line_with_usage_and_structured_output() {
    let mut r = AcpReducer;
    let aggregate = json!({
        "inputTokens": 5,
        "outputTokens": 2,
        "totalTokens": 7,
        "numTurns": 1,
    });
    let out = r.finish(&TurnEnd {
        stop_reason: "end_turn",
        session_id: "sess-1",
        request_id: "req-1",
        usage: Some(&aggregate),
        structured_output: Some(Ok(json!({"name": "alice"}))),
        result_text: "",
        duration_ms: 0,
    });
    let Some(end) = out.last() else {
        panic!("expected an end line: {out:?}");
    };
    assert_eq!(msg_type(end), Some("end"));
    assert_eq!(json_str(end, "/stopReason"), Some("end_turn"));
    assert_eq!(json_str(end, "/sessionId"), Some("sess-1"));
    assert_eq!(json_str(end, "/requestId"), Some("req-1"));
    assert_eq!(json_str(end, "/structuredOutput/name"), Some("alice"));
    assert!(end.get("usage").is_some_and(Value::is_object));
}
