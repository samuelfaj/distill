//! One tool call turn, with no reasoning and no text, in each endpoint's format, and the shapes a
//! failure answers with, built over the frame that opens a reply in each format.

use serde_json::json;

use crate::failure::{DOOM_LOOP_TRIGGER, ErrorPosition, LOOPING_REPLY, StreamError};
use crate::inference_request::InferenceEndpoint;
use crate::scripted::SseEvent;
use crate::sse::{
    chat_completion_deltas, chat_completion_script_exact, chat_completion_script_from_deltas,
    doom_loop_check_frame, insert_before_type, messages_api_script, renumber_sequence_numbers,
    responses_api_script_exact,
};

/// The chunk that opens a Chat Completions reply: the assistant role and no content.
fn chat_completion_role_chunk(model: &str) -> SseEvent {
    SseEvent::data(
        json!({
            "id": "chatcmpl-test", "object": "chat.completion.chunk",
            "created": 1234567890, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        })
        .to_string(),
    )
}

/// The `response.created` frame that opens a Responses API reply, numbered 0.
fn responses_api_created_frame(model: &str) -> SseEvent {
    SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_test", "object": "response", "created_at": 1234567890,
                "model": model, "status": "in_progress", "output": []
            }
        })
        .to_string(),
    )
}

/// The `message_start` frame that opens a Messages API reply.
fn messages_api_message_start(model: &str) -> SseEvent {
    SseEvent::data(
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_test", "type": "message", "role": "assistant",
                "content": [], "model": model, "stop_reason": null,
                "usage": {
                    "input_tokens": 10, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                }
            }
        })
        .to_string(),
    )
}

/// `text` stopped at the output token limit: `finish_reason: "length"`, `response.incomplete` with
/// `max_output_tokens`, or `stop_reason: "max_tokens"`.
pub(crate) fn cut_reply_events(
    endpoint: InferenceEndpoint,
    text: &str,
    model: &str,
) -> Vec<SseEvent> {
    match endpoint {
        InferenceEndpoint::ChatCompletions => {
            chat_completion_script_from_deltas(&chat_completion_deltas(text), model, "length")
        }
        InferenceEndpoint::Responses => responses_api_cut_script(text, model),
        InferenceEndpoint::Messages => messages_api_script(text, model, "max_tokens"),
    }
}

/// A Responses reply cut by the output token limit: one text delta, then `response.incomplete`.
fn responses_api_cut_script(text: &str, model: &str) -> Vec<SseEvent> {
    vec![
        responses_api_created_frame(model),
        SseEvent::data(
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 1,
                "item_id": "item_test",
                "output_index": 0,
                "content_index": 0,
                "delta": text
            })
            .to_string(),
        ),
        SseEvent::data(
            json!({
                "type": "response.incomplete",
                "sequence_number": 2,
                "response": {
                    "id": "resp_test", "object": "response", "created_at": 1234567890,
                    "model": model, "status": "incomplete",
                    "incomplete_details": { "reason": "max_output_tokens" },
                    "output": [{
                        "type": "message", "id": "msg_test", "role": "assistant", "status": "incomplete",
                        "content": [{ "type": "output_text", "text": text, "annotations": [] }]
                    }],
                    "usage": {
                        "input_tokens": 10, "output_tokens": 3, "total_tokens": 13,
                        "input_tokens_details": { "cached_tokens": 0 },
                        "output_tokens_details": { "reasoning_tokens": 0 }
                    }
                }
            })
            .to_string(),
        ),
        SseEvent::data("[DONE]"),
    ]
}

/// The error event alone, or after the frame that opens a reply when the position is `Midway`.
pub(crate) fn stream_error_events(
    endpoint: InferenceEndpoint,
    stream_error: &StreamError,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = match stream_error.position {
        ErrorPosition::First => Vec::new(),
        ErrorPosition::Midway => vec![match endpoint {
            InferenceEndpoint::ChatCompletions => chat_completion_role_chunk(model),
            InferenceEndpoint::Responses => responses_api_created_frame(model),
            InferenceEndpoint::Messages => messages_api_message_start(model),
        }],
    };
    events.push(stream_error_event(
        endpoint,
        &stream_error.message,
        events.len(),
    ));
    events
}

/// One error event in each format's shape: a Chat Completions `error` data frame, a Responses
/// `error` event numbered after prior frames, or a Messages `overloaded_error`.
fn stream_error_event(
    endpoint: InferenceEndpoint,
    message: &str,
    sequence_number: usize,
) -> SseEvent {
    match endpoint {
        InferenceEndpoint::ChatCompletions => SseEvent::data(
            json!({ "error": { "message": message, "type": "server_error", "code": null } })
                .to_string(),
        ),
        InferenceEndpoint::Responses => SseEvent::data(
            json!({
                "type": "error", "code": null, "message": message, "param": null,
                "sequence_number": sequence_number
            })
            .to_string(),
        ),
        InferenceEndpoint::Messages => SseEvent::with_event(
            "error",
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": message } })
                .to_string(),
        ),
    }
}

/// [`LOOPING_REPLY`], with the detector's `response.doom_loop_check` report before the text when
/// `reported`; only the Responses format carries a report.
pub(crate) fn looping_reply_events(
    endpoint: InferenceEndpoint,
    model: &str,
    reported: bool,
) -> Vec<SseEvent> {
    let events = match endpoint {
        InferenceEndpoint::ChatCompletions => chat_completion_script_exact(LOOPING_REPLY, model),
        InferenceEndpoint::Responses => responses_api_script_exact(LOOPING_REPLY, model),
        InferenceEndpoint::Messages => messages_api_script(LOOPING_REPLY, model, "end_turn"),
    };
    if endpoint != InferenceEndpoint::Responses || !reported {
        return events;
    }
    let spliced = insert_before_type(
        events,
        doom_loop_check_frame(&[DOOM_LOOP_TRIGGER], 0),
        "response.output_text.delta",
    );
    renumber_sequence_numbers(spliced)
}

#[derive(Clone, Copy)]
pub(crate) struct ToolCallTurn<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) model: &'a str,
}

/// Chat Completions: one `tool_calls` delta, then a `finish_reason: "tool_calls"` chunk with usage.
pub(crate) fn chat_completion_tool_call_events(turn: ToolCallTurn<'_>) -> Vec<SseEvent> {
    let ToolCallTurn {
        call_id,
        name,
        arguments,
        model,
    } = turn;
    vec![
        SseEvent::data(
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments }
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        SseEvent::data(
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 20,
                    "total_tokens": 30
                }
            })
            .to_string(),
        ),
        SseEvent::data("[DONE]"),
    ]
}

/// Responses API: the `function_call` item is added before its arguments delta, its arguments are
/// whole in `function_call_arguments.done`, and the item closes before `response.completed`.
pub(crate) fn responses_api_tool_call_events(turn: ToolCallTurn<'_>) -> Vec<SseEvent> {
    let ToolCallTurn {
        call_id,
        name,
        arguments,
        model,
    } = turn;
    let item_id = format!("fc_{call_id}");
    let response = json!({
        "id": "resp_test",
        "object": "response",
        "created_at": 1234567890,
        "model": model
    });
    let call = json!({
        "type": "function_call",
        "id": item_id,
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed"
    });
    let mut in_progress = response.clone();
    if let Some(map) = in_progress.as_object_mut() {
        map.insert("status".to_owned(), json!("in_progress"));
        map.insert("output".to_owned(), json!([]));
    }
    let mut opened = call.clone();
    if let Some(map) = opened.as_object_mut() {
        map.insert("arguments".to_owned(), json!(""));
        map.insert("status".to_owned(), json!("in_progress"));
    }
    let mut completed = response;
    if let Some(map) = completed.as_object_mut() {
        map.insert("status".to_owned(), json!("completed"));
        map.insert("output".to_owned(), json!([call]));
        map.insert(
            "usage".to_owned(),
            json!({
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens_details": { "reasoning_tokens": 0 }
            }),
        );
    }
    let events = [
        json!({ "type": "response.created", "response": in_progress }),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": opened }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id,
            "output_index": 0,
            "delta": arguments
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "item_id": item_id,
            "output_index": 0,
            "name": name,
            "arguments": arguments
        }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": call }),
        json!({ "type": "response.completed", "response": completed }),
    ];
    events
        .into_iter()
        .enumerate()
        .map(|(sequence_number, mut event)| {
            if let Some(map) = event.as_object_mut() {
                map.insert("sequence_number".to_owned(), json!(sequence_number));
            }
            SseEvent::data(event.to_string())
        })
        .chain(std::iter::once(SseEvent::data("[DONE]")))
        .collect()
}

/// Messages API: one `tool_use` block, its input in one `input_json_delta`; stops with `tool_use`.
pub(crate) fn messages_api_tool_use_events(turn: ToolCallTurn<'_>) -> Vec<SseEvent> {
    let ToolCallTurn {
        call_id,
        name,
        arguments,
        model,
    } = turn;
    vec![
        messages_api_message_start(model),
        SseEvent::data(
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}
            })
            .to_string(),
        ),
        SseEvent::data(
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": arguments}
            })
            .to_string(),
        ),
        SseEvent::data(json!({"type": "content_block_stop", "index": 0}).to_string()),
        SseEvent::data(
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 5, "input_tokens": 10}
            })
            .to_string(),
        ),
        SseEvent::data(json!({"type": "message_stop"}).to_string()),
    ]
}

#[cfg(test)]
#[path = "tool_call_turn_tests.rs"]
mod tests;
