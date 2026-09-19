use super::{
    HistoryToolCall, InferenceEndpoint, InferenceRequest, InferenceRequestKind,
    first_system_message, history_tool_calls, last_user_message, tool_results,
};
use crate::conversation::ConversationTracker;
use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[test]
fn explicit_request_headers_override_the_tool_count_heuristic() {
    let conversations = ConversationTracker::default();
    let body = json!({
        "messages": [{ "role": "user", "content": "title" }],
        "tools": [
            { "type": "function", "function": { "name": "read_file" } },
            { "type": "function", "function": { "name": "write" } }
        ]
    });
    let cases: [(&[(&str, &str)], InferenceRequestKind); 3] = [
        (
            &[("x-grok-req-id", "title-request")],
            InferenceRequestKind::Auxiliary,
        ),
        (
            &[("x-grok-req-id", "title-request"), ("x-grok-turn-idx", "1")],
            InferenceRequestKind::Foreground,
        ),
        (
            &[("x-grok-req-id", ""), ("x-grok-turn-idx", "")],
            InferenceRequestKind::Foreground,
        ),
    ];
    for (pairs, expected) in cases {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, value.parse().unwrap());
        }
        let request = InferenceRequest::new(
            &conversations,
            InferenceEndpoint::ChatCompletions,
            &headers,
            &body,
        );
        assert_eq!(expected, request.kind(), "{pairs:?}");
    }
}
#[test]
fn first_system_message_reads_every_request_format() {
    let cases: [(Value, Option<&str>); 6] = [
        (
            json!({ "messages": [{ "role": "system", "content": "chat system" }] }),
            Some("chat system"),
        ),
        (
            json!({ "messages": [{ "role": "system", "content": [
                { "type": "text", "text": "part one" }, { "type": "text", "text": "part two" }
            ] }] }),
            Some("part one\npart two"),
        ),
        (
            json!({ "system": "messages system", "messages": [{ "role": "user", "content": "hi" }] }),
            Some("messages system"),
        ),
        (
            json!({ "instructions": "responses system", "input": [{ "role": "user", "content": "hi" }] }),
            Some("responses system"),
        ),
        (
            json!({ "input": [{ "role": "developer", "content": [{ "type": "input_text", "text": "dev" }] }] }),
            Some("dev"),
        ),
        (
            json!({ "input": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
    ];
    for (body, expected) in cases {
        assert_eq!(
            expected.map(str::to_owned),
            first_system_message(&body),
            "{body}"
        );
    }
}
#[test]
fn last_user_message_reads_every_request_format() {
    let cases: [(Value, Option<&str>); 4] = [
        (
            json!({ "messages": [
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "reply" },
                { "role": "user", "content": "last" }
            ] }),
            Some("last"),
        ),
        (
            json!({ "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "typed" }] }] }),
            Some("typed"),
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "body" },
                { "type": "text", "text": "after the result" }
            ] }] }),
            Some("after the result"),
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "body" }
            ] }] }),
            None,
        ),
    ];
    for (body, expected) in cases {
        assert_eq!(
            expected.map(str::to_owned),
            last_user_message(&body),
            "{body}"
        );
    }
}
#[test]
fn tool_results_reads_every_request_format() {
    let cases: [Value; 3] = [
        json!({ "messages": [
            { "role": "tool", "tool_call_id": "call_mock_1_1", "content": "file body" }
        ] }),
        json!({ "input": [
            { "type": "function_call_output", "call_id": "call_mock_1_1", "output": "file body" }
        ] }),
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "call_mock_1_1", "content": [
                { "type": "text", "text": "file body" }
            ] }
        ] }] }),
    ];
    for body in cases {
        assert_eq!(
            BTreeMap::from([("call_mock_1_1".to_owned(), "file body".to_owned())]),
            tool_results(&body),
            "{body}"
        );
    }
}
#[test]
fn history_tool_calls_reads_every_request_format_with_arguments_parsed() {
    let cases: [Value; 3] = [
        json!({ "messages": [{ "role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"target_file\":\"a.rs\"}" }
        }] }] }),
        json!({ "input": [{
            "type": "function_call", "call_id": "call_1",
            "name": "read_file", "arguments": "{\"target_file\":\"a.rs\"}"
        }] }),
        json!({ "messages": [{ "role": "assistant", "content": [{
            "type": "tool_use", "id": "call_1",
            "name": "read_file", "input": { "target_file": "a.rs" }
        }] }] }),
    ];
    for body in cases {
        assert_eq!(
            vec![HistoryToolCall {
                name: "read_file".to_owned(),
                arguments: json!({ "target_file": "a.rs" }),
            }],
            history_tool_calls(&body),
            "{body}"
        );
    }
}
