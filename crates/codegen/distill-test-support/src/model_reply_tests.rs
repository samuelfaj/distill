use serde_json::{Value, json};

use super::ModelReply;
use crate::failure::StatusFailure;
use crate::inference_request::InferenceEndpoint;
use crate::scripted::ScriptedBody;
use crate::tools::PickedToolCall;

fn read_call() -> ModelReply {
    ModelReply::ToolCall {
        call_id: "call_mock_1_1".to_owned(),
        call: PickedToolCall {
            name: "read_file".to_owned(),
            arguments: json!({ "target_file": "a.rs" }),
        },
    }
}

fn sse_frames(reply: ModelReply, endpoint: InferenceEndpoint) -> Vec<Value> {
    let ScriptedBody::Sse(events) = reply.into_response(endpoint, "m").body else {
        panic!("reply renders as SSE");
    };
    events
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

#[test]
fn each_endpoint_renders_the_reply_in_its_own_format() {
    let cases: [(InferenceEndpoint, ModelReply, &str, &str); 6] = [
        (
            InferenceEndpoint::ChatCompletions,
            read_call(),
            "/0/choices/0/delta/tool_calls/0/function/name",
            "read_file",
        ),
        (
            InferenceEndpoint::Responses,
            read_call(),
            "/5/response/output/0/name",
            "read_file",
        ),
        (
            InferenceEndpoint::Messages,
            read_call(),
            "/1/content_block/name",
            "read_file",
        ),
        (
            InferenceEndpoint::ChatCompletions,
            ModelReply::Text("REPLY".to_owned()),
            "/0/choices/0/delta/content",
            "REPLY",
        ),
        (
            InferenceEndpoint::Responses,
            ModelReply::Text("REPLY".to_owned()),
            "/1/delta",
            "REPLY",
        ),
        (
            InferenceEndpoint::Messages,
            ModelReply::Text("REPLY".to_owned()),
            "/2/delta/text",
            "REPLY",
        ),
    ];
    for (endpoint, reply, pointer, expected) in cases {
        let found = Value::Array(sse_frames(reply, endpoint))
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(Some(expected.to_owned()), found, "{endpoint:?} {pointer}");
    }
}

#[test]
fn reasoning_reply_streams_the_thought_in_each_format() {
    let cases: [(InferenceEndpoint, &str, &str); 3] = [
        (
            InferenceEndpoint::ChatCompletions,
            "/0/choices/0/delta/reasoning_content",
            "THINK",
        ),
        (
            InferenceEndpoint::Responses,
            "/3/response/output/0/summary/0/text",
            "THINK",
        ),
        (InferenceEndpoint::Messages, "/2/delta/thinking", "THINK"),
    ];
    for (endpoint, pointer, expected) in cases {
        let reply = ModelReply::ReasoningReply {
            reasoning: "THINK".to_owned(),
            text: "REPLY".to_owned(),
        };
        let found = Value::Array(sse_frames(reply, endpoint))
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(Some(expected.to_owned()), found, "{endpoint:?} {pointer}");
    }
}

#[test]
fn refusal_without_a_body_serves_the_default_error_json() {
    let response = ModelReply::Refusal(StatusFailure::new(500))
        .into_response(InferenceEndpoint::ChatCompletions, "m");
    let ScriptedBody::Json(body) = &response.body else {
        panic!("a refusal without a body is served as JSON");
    };
    assert_eq!(
        (
            500,
            &json!({ "error": { "message": "mock upstream failure 500", "type": "server_error" } })
        ),
        (response.status, body)
    );
}
