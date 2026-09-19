use serde_json::{Value, json};

use super::{
    ToolCallTurn, cut_reply_events, looping_reply_events, responses_api_tool_call_events,
    stream_error_events,
};
use crate::failure::{CUT_REPLY, DOOM_LOOP_TRIGGER, ErrorPosition, LOOPING_REPLY, StreamError};
use crate::inference_request::InferenceEndpoint;
use crate::scripted::SseEvent;

fn parsed(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

#[test]
fn responses_tool_call_opens_the_item_before_its_arguments_and_closes_it_before_completed() {
    let events = responses_api_tool_call_events(ToolCallTurn {
        call_id: "call_mock_1",
        name: "Read",
        arguments: "{\"path\":\"a\"}",
        model: "m",
    });
    let frames: Vec<Value> = events
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    let types: Vec<&str> = frames
        .iter()
        .map(|frame| frame.pointer("/type").unwrap().as_str().unwrap())
        .collect();
    let sequence: Vec<u64> = frames
        .iter()
        .map(|frame| frame.pointer("/sequence_number").unwrap().as_u64().unwrap())
        .collect();
    let completed_call = frames
        .get(5)
        .unwrap()
        .pointer("/response/output/0")
        .unwrap();
    assert_eq!(
        (
            vec![
                "response.created",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ],
            vec![0, 1, 2, 3, 4, 5],
            Some(""),
            Some("{\"path\":\"a\"}"),
            (
                Some("function_call"),
                Some("call_mock_1"),
                Some("Read"),
                Some("{\"path\":\"a\"}"),
            ),
        ),
        (
            types,
            sequence,
            frames
                .get(1)
                .unwrap()
                .pointer("/item/arguments")
                .unwrap()
                .as_str(),
            frames
                .get(3)
                .unwrap()
                .pointer("/arguments")
                .unwrap()
                .as_str(),
            (
                completed_call.pointer("/type").unwrap().as_str(),
                completed_call.pointer("/call_id").unwrap().as_str(),
                completed_call.pointer("/name").unwrap().as_str(),
                completed_call.pointer("/arguments").unwrap().as_str(),
            ),
        )
    );
}

#[test]
fn cut_reply_stops_at_the_output_limit_on_each_format() {
    let cases: [(InferenceEndpoint, &str, Value); 3] = [
        (
            InferenceEndpoint::ChatCompletions,
            "/0/choices/0/finish_reason",
            json!("length"),
        ),
        (
            InferenceEndpoint::Responses,
            "/2/response/incomplete_details/reason",
            json!("max_output_tokens"),
        ),
        (
            InferenceEndpoint::Messages,
            "/4/delta/stop_reason",
            json!("max_tokens"),
        ),
    ];
    for (endpoint, pointer, expected) in cases {
        let frames = Value::Array(parsed(&cut_reply_events(endpoint, CUT_REPLY, "m")));
        assert_eq!(
            (Some(&expected), true),
            (
                frames.pointer(pointer),
                frames.to_string().contains(CUT_REPLY)
            ),
            "{endpoint:?}"
        );
    }
}

#[test]
fn stream_error_takes_each_formats_shape_and_follows_the_opening_frame_midway() {
    let cases: [(InferenceEndpoint, &str); 3] = [
        (InferenceEndpoint::ChatCompletions, "/1/error/message"),
        (InferenceEndpoint::Responses, "/1/message"),
        (InferenceEndpoint::Messages, "/1/error/message"),
    ];
    let midway = StreamError::new().with_position(ErrorPosition::Midway);
    for (endpoint, pointer) in cases {
        let frames = Value::Array(parsed(&stream_error_events(endpoint, &midway, "m")));
        assert_eq!(
            (2, Some(&json!(StreamError::DEFAULT_MESSAGE))),
            (frames.as_array().unwrap().len(), frames.pointer(pointer)),
            "{endpoint:?}"
        );
    }
}

#[test]
fn stream_error_at_the_first_position_is_the_only_frame() {
    let frames = parsed(&stream_error_events(
        InferenceEndpoint::ChatCompletions,
        &StreamError::new().with_message("at capacity"),
        "m",
    ));
    assert_eq!(
        vec![
            json!({ "error": { "message": "at capacity", "type": "server_error", "code": null } })
        ],
        frames
    );
}

#[test]
fn looping_reply_carries_the_detectors_report_only_on_responses_when_asked() {
    let cases: [(InferenceEndpoint, bool, Option<Value>); 3] = [
        (
            InferenceEndpoint::Responses,
            true,
            Some(json!([DOOM_LOOP_TRIGGER])),
        ),
        (InferenceEndpoint::Responses, false, None),
        (InferenceEndpoint::ChatCompletions, true, None),
    ];
    let streamed_text = |frames: &[Value]| -> String {
        frames
            .iter()
            .filter_map(|frame| {
                frame
                    .pointer("/choices/0/delta/content")
                    .or_else(|| {
                        frame.pointer("/delta").filter(|_| {
                            frame.get("type").and_then(Value::as_str)
                                == Some("response.output_text.delta")
                        })
                    })
                    .or_else(|| frame.pointer("/delta/text"))
                    .and_then(Value::as_str)
            })
            .collect()
    };
    for (endpoint, reported, expected) in cases {
        let events = looping_reply_events(endpoint, "m", reported);
        let report = events
            .iter()
            .find(|event| event.event.as_deref() == Some("response.doom_loop_check"))
            .and_then(|event| {
                serde_json::from_str::<Value>(&event.data)
                    .ok()?
                    .pointer("/doom_loop_check/triggers")
                    .cloned()
            });
        assert_eq!(
            (expected, LOOPING_REPLY.to_owned()),
            (report, streamed_text(&parsed(&events))),
            "{endpoint:?} reported {reported}"
        );
    }
}

#[test]
fn responses_looping_report_renumbers_to_a_unique_increasing_sequence() {
    let events = looping_reply_events(InferenceEndpoint::Responses, "m", true);
    let frames = parsed(&events);
    let sequence: Vec<u64> = frames
        .iter()
        .map(|frame| frame.pointer("/sequence_number").unwrap().as_u64().unwrap())
        .collect();
    let typed_at = |kind: &str| {
        frames
            .iter()
            .position(|frame| frame.get("type").and_then(Value::as_str) == Some(kind))
    };

    assert_eq!(
        (vec![0, 1, 2, 3, 4, 5, 6], true),
        (
            sequence,
            typed_at("response.doom_loop_check") < typed_at("response.output_text.delta")
        )
    );
}
