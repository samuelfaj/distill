//! `--include-partial-messages` streaming framing.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn messages_partial_deltas_emitted_when_enabled() {
    let mut r = messages(true);
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(start) = out.iter().find(|m| event_type(m) == Some("message_start")) else {
        panic!("message_start: {out:?}");
    };
    assert!(
        start
            .pointer("/event/message/model")
            .is_some_and(Value::is_string)
    );
    assert!(
        start
            .pointer("/event/message/usage")
            .is_some_and(Value::is_object)
    );
    let Some(block_start) = out
        .iter()
        .find(|m| event_type(m) == Some("content_block_start"))
    else {
        panic!("content_block_start: {out:?}");
    };
    assert_eq!(
        json_str(block_start, "/event/content_block/type"),
        Some("text")
    );
    assert_eq!(json_str(block_start, "/event/content_block/text"), Some(""));
    let delta = stream_delta(&out);
    assert_eq!(
        delta.pointer("/event/index").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(delta_type(delta), Some("text_delta"));
    assert_eq!(json_str(delta, "/event/delta/text"), Some("hi"));
}

#[test]
fn messages_partial_framing_closes_with_stop_reason_and_usage() {
    let mut r = messages(true);
    r.reduce(StreamEvent::AgentMessage("hi".into()));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: Some(ResponseUsage {
            input_tokens: 3,
            output_tokens: 7,
            ..Default::default()
        }),
        signature: None,
        stop_sequence: None,
    });
    let out = r.reduce(StreamEvent::AgentMessage("more".into()));
    let Some(delta) = out.iter().find(|m| event_type(m) == Some("message_delta")) else {
        panic!("message_delta closes the prior message: {out:?}");
    };
    assert_eq!(
        json_str(delta, "/event/delta/stop_reason"),
        Some("end_turn")
    );
    assert_eq!(
        delta
            .pointer("/event/usage/output_tokens")
            .and_then(Value::as_u64),
        Some(7)
    );
    assert_eq!(
        delta
            .pointer("/event/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(3)
    );
    assert!(out.iter().any(|m| event_type(m) == Some("message_stop")));
}

#[test]
fn messages_partial_tool_use_framed() {
    let mut r = messages(true);
    r.reduce(StreamEvent::AgentMessage("run".into()));
    let out = r.reduce(StreamEvent::ToolCall(tool_call_ev()));
    let Some(start) = out.iter().find(|m| {
        event_type(m) == Some("content_block_start")
            && json_str(m, "/event/content_block/type") == Some("tool_use")
    }) else {
        panic!("tool_use content_block_start: {out:?}");
    };
    assert_eq!(json_str(start, "/event/content_block/name"), Some("bash"));
    assert!(
        out.iter()
            .any(|m| delta_type(m) == Some("input_json_delta"))
    );
}

#[test]
fn messages_partial_tool_flush_without_pending_agrees_on_stop_reason() {
    let mut r = messages(true);
    r.reduce(StreamEvent::AgentMessage("searching".into()));
    r.reduce(StreamEvent::ToolCall(tool_call_ev()));
    let out = r.reduce(StreamEvent::ToolCallUpdate(tool_update(
        "completed",
        json!("done"),
    )));
    let Some(delta) = out.iter().find(|m| event_type(m) == Some("message_delta")) else {
        panic!("message_delta: {out:?}");
    };
    let Some(assistant) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("frame: {out:?}");
    };
    assert_eq!(
        json_str(delta, "/event/delta/stop_reason"),
        Some("tool_use")
    );
    assert_eq!(
        json_str(assistant, "/message/stop_reason"),
        Some("tool_use")
    );
}

#[test]
fn messages_partial_delta_index_tracks_block() {
    let mut r = messages(true);
    let t = r.reduce(StreamEvent::AgentThought("mull".into()));
    assert_eq!(
        stream_delta(&t)
            .pointer("/event/index")
            .and_then(Value::as_u64),
        Some(0)
    );
    let x = r.reduce(StreamEvent::AgentMessage("hi".into()));
    assert_eq!(
        stream_delta(&x)
            .pointer("/event/index")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        x.iter()
            .any(|m| event_type(m) == Some("content_block_stop"))
    );
}

#[test]
fn messages_partial_thinking_then_text_defers_signature_to_frame() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::AgentThought("mull".into())));
    out.extend(r.reduce(StreamEvent::AgentMessage("hi".into())));
    out.extend(r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_real".into()),
        stop_reason: Some("end_turn".into()),
        usage: None,
        signature: Some("sig-xyz".into()),
        stop_sequence: None,
    }));
    assert!(!out.iter().any(|m| delta_type(m) == Some("signature_delta")));
    let Some(start) = out.iter().find(|m| event_type(m) == Some("message_start")) else {
        panic!("message_start: {out:?}");
    };
    assert_eq!(json_str(start, "/event/message/id"), Some("msg_0"));
    let frame = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    assert_eq!(json_str(&frame, "/message/id"), Some("msg_real"));
    assert_eq!(
        json_str(&frame, "/message/content/0/signature"),
        Some("sig-xyz")
    );
}

#[test]
fn messages_partial_response_started_emits_real_id_and_input_usage() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::ResponseStarted {
        message_id: Some("msg_real".into()),
        model: Some("grok-4".into()),
        input_tokens: 42,
        cache_read_input_tokens: 100,
        cache_creation_input_tokens: 20,
    }));
    out.extend(r.reduce(StreamEvent::AgentThought("mull".into())));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-xyz".into()),
    }));
    out.extend(r.reduce(StreamEvent::AgentMessage("hi".into())));
    out.extend(r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_real".into()),
        stop_reason: Some("end_turn".into()),
        usage: None,
        signature: Some("sig-xyz".into()),
        stop_sequence: None,
    }));

    let Some(start) = out.iter().find(|m| event_type(m) == Some("message_start")) else {
        panic!("message_start: {out:?}");
    };
    assert_eq!(json_str(start, "/event/message/id"), Some("msg_real"));
    assert_eq!(
        start
            .pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(42)
    );
    assert_eq!(
        start
            .pointer("/event/message/usage/cache_read_input_tokens")
            .and_then(Value::as_u64),
        Some(100)
    );
    assert_eq!(
        start
            .pointer("/event/message/usage/cache_creation_input_tokens")
            .and_then(Value::as_u64),
        Some(20)
    );
    assert_eq!(
        start
            .pointer("/event/message/usage/output_tokens")
            .and_then(Value::as_u64),
        Some(0)
    );

    let Some(sig) = out
        .iter()
        .position(|m| delta_type(m) == Some("signature_delta"))
    else {
        panic!("signature_delta emitted in order: {out:?}");
    };
    let Some(sig_msg) = out.get(sig) else {
        panic!("signature_delta at {sig}: {out:?}");
    };
    assert_eq!(json_str(sig_msg, "/event/delta/signature"), Some("sig-xyz"));
    let Some(stop) = out
        .iter()
        .position(|m| event_type(m) == Some("content_block_stop"))
    else {
        panic!("content_block_stop: {out:?}");
    };
    assert!(sig < stop, "signature_delta precedes content_block_stop");

    let frame = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    assert_eq!(json_str(&frame, "/message/id"), Some("msg_real"));
    assert_eq!(
        json_str(&frame, "/message/content/0/signature"),
        Some("sig-xyz")
    );
}

#[test]
fn messages_partial_response_started_ids_do_not_leak_across_responses() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::ResponseStarted {
        message_id: Some("msg_real".into()),
        model: None,
        input_tokens: 9,
        cache_read_input_tokens: 5,
        cache_creation_input_tokens: 0,
    }));
    out.extend(r.reduce(StreamEvent::AgentMessage("one".into())));
    out.extend(r.reduce(response_completed("msg_real", "end_turn")));
    out.extend(r.reduce(StreamEvent::AgentMessage("two".into())));
    let starts: Vec<&Value> = out
        .iter()
        .filter(|m| event_type(m) == Some("message_start"))
        .collect();
    let [first, second] = starts.as_slice() else {
        panic!("expected two message_start events: {out:?}");
    };
    assert_eq!(json_str(first, "/event/message/id"), Some("msg_real"));
    assert_eq!(
        first
            .pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(9)
    );
    assert_eq!(
        first
            .pointer("/event/message/usage/cache_read_input_tokens")
            .and_then(Value::as_u64),
        Some(5)
    );
    assert_eq!(json_str(second, "/event/message/id"), Some("msg_0"));
    assert_eq!(
        second
            .pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        second
            .pointer("/event/message/usage/cache_read_input_tokens")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        second
            .pointer("/event/message/usage/cache_creation_input_tokens")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn messages_partial_thinking_terminal_emits_signature_delta() {
    let mut r = messages(true);
    r.reduce(StreamEvent::AgentThought("mull".into()));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: None,
        signature: Some("sig-term".into()),
        stop_sequence: None,
    });
    let out = r.reduce(StreamEvent::AgentMessage("answer".into()));
    let Some(sig) = out
        .iter()
        .position(|m| delta_type(m) == Some("signature_delta"))
    else {
        panic!("signature_delta emitted: {out:?}");
    };
    let Some(sig_msg) = out.get(sig) else {
        panic!("signature_delta at {sig}: {out:?}");
    };
    assert_eq!(
        json_str(sig_msg, "/event/delta/signature"),
        Some("sig-term")
    );
    let Some(stop) = out
        .iter()
        .position(|m| event_type(m) == Some("content_block_stop"))
    else {
        panic!("content_block_stop: {out:?}");
    };
    assert!(sig < stop, "signature_delta precedes content_block_stop");
}

#[test]
fn messages_partial_message_start_ids_are_unique() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::AgentMessage("one".into())));
    out.extend(r.reduce(response_completed("msg_a", "end_turn")));
    out.extend(r.reduce(StreamEvent::AgentMessage("two".into())));
    let ids: Vec<&str> = out
        .iter()
        .filter(|m| event_type(m) == Some("message_start"))
        .map(|m| json_str(m, "/event/message/id").expect("message_start id"))
        .collect();
    assert_eq!(ids, vec!["msg_0", "msg_1"]);
}

#[test]
fn messages_partial_signature_only_thinking_block_emits_framing() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", Some("grok-4"), 5)));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-only".into()),
    }));
    out.extend(r.reduce(StreamEvent::AgentMessage("answer".into())));
    out.extend(r.reduce(response_completed("msg_a", "end_turn")));
    out.extend(r.finish(&end_turn()));
    let Some(cb_start) = out.iter().position(|m| {
        event_type(m) == Some("content_block_start")
            && json_str(m, "/event/content_block/type") == Some("thinking")
    }) else {
        panic!("thinking content_block_start: {out:?}");
    };
    let Some(sig) = out
        .iter()
        .position(|m| delta_type(m) == Some("signature_delta"))
    else {
        panic!("signature_delta: {out:?}");
    };
    let Some(start_msg) = out.get(cb_start) else {
        panic!("content_block_start at {cb_start}: {out:?}");
    };
    let Some(sig_msg) = out.get(sig) else {
        panic!("signature_delta at {sig}: {out:?}");
    };
    assert_eq!(
        start_msg.pointer("/event/index").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        json_str(sig_msg, "/event/delta/signature"),
        Some("sig-only")
    );
    assert!(
        cb_start < sig,
        "content_block_start precedes signature_delta"
    );
    let Some(frame) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("frame: {out:?}");
    };
    let Some(blocks) = frame.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {frame:?}");
    };
    let [thinking, text, ..] = blocks.as_slice() else {
        panic!("expected thinking then text: {blocks:?}");
    };
    assert_eq!(json_str(thinking, "/type"), Some("thinking"));
    assert_eq!(json_str(thinking, "/signature"), Some("sig-only"));
    assert_eq!(json_str(text, "/type"), Some("text"));
    assert_eq!(json_str(text, "/text"), Some("answer"));
}

#[test]
fn messages_partial_per_block_signature_deltas() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::AgentThought("first think".into())));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-1".into()),
    }));
    out.extend(r.reduce(StreamEvent::AgentMessage("interlude".into())));
    out.extend(r.reduce(StreamEvent::AgentThought("second think".into())));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-2".into()),
    }));
    out.extend(r.finish(&end_turn()));
    let sigs: Vec<&str> = out
        .iter()
        .filter(|m| delta_type(m) == Some("signature_delta"))
        .map(|m| json_str(m, "/event/delta/signature").expect("signature_delta signature"))
        .collect();
    assert_eq!(sigs, vec!["sig-1", "sig-2"], "each block keeps its own sig");
}

#[test]
fn messages_partial_empty_response_still_frames_message() {
    let mut r = messages(true);
    r.reduce(response_started("msg_empty", Some("grok-4"), 5));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_empty".into()),
        stop_reason: Some("end_turn".into()),
        usage: Some(ResponseUsage {
            input_tokens: 5,
            output_tokens: 0,
            ..Default::default()
        }),
        signature: None,
        stop_sequence: None,
    });
    let out = r.finish(&end_turn());
    let Some(start) = out.iter().find(|m| event_type(m) == Some("message_start")) else {
        panic!("message_start for the empty response: {out:?}");
    };
    assert!(out.iter().any(|m| event_type(m) == Some("message_delta")));
    assert!(out.iter().any(|m| event_type(m) == Some("message_stop")));
    assert!(
        !out.iter()
            .any(|m| event_type(m).is_some_and(|t| t.starts_with("content_block"))),
        "no content_block_* events: {out:?}"
    );
    assert!(
        out.iter().all(|m| msg_type(m) != Some("assistant")),
        "{out:?}"
    );
    assert_eq!(json_str(start, "/event/message/id"), Some("msg_empty"));
    assert_eq!(
        start
            .pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(5)
    );
}

#[test]
fn messages_partial_empty_then_real_response_do_not_cross_attribute() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", None, 11)));
    out.extend(r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: Some(ResponseUsage {
            input_tokens: 11,
            output_tokens: 0,
            ..Default::default()
        }),
        signature: None,
        stop_sequence: None,
    }));
    out.extend(r.reduce(response_started("msg_b", None, 22)));
    out.extend(r.reduce(StreamEvent::AgentMessage("real".into())));
    let starts: Vec<&Value> = out
        .iter()
        .filter(|m| event_type(m) == Some("message_start"))
        .collect();
    let [a, b] = starts.as_slice() else {
        panic!("one envelope per response: {out:?}");
    };
    assert_eq!(json_str(a, "/event/message/id"), Some("msg_a"));
    assert_eq!(
        a.pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(11)
    );
    assert_eq!(json_str(b, "/event/message/id"), Some("msg_b"));
    assert_eq!(
        b.pointer("/event/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(22)
    );
}

#[test]
fn messages_partial_message_delta_carries_stop_sequence() {
    let mut r = messages(true);
    r.reduce(StreamEvent::AgentMessage("answer".into()));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_seq".into()),
        stop_reason: Some("stop_sequence".into()),
        usage: None,
        signature: None,
        stop_sequence: Some("<END>".into()),
    });
    let out = r.reduce(StreamEvent::AgentMessage("more".into()));
    let Some(delta) = out.iter().find(|m| event_type(m) == Some("message_delta")) else {
        panic!("message_delta closes the prior message: {out:?}");
    };
    assert_eq!(
        json_str(delta, "/event/delta/stop_reason"),
        Some("stop_sequence")
    );
    assert_eq!(json_str(delta, "/event/delta/stop_sequence"), Some("<END>"));
    let Some(assistant) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("assistant frame: {out:?}");
    };
    assert_eq!(json_str(assistant, "/message/stop_sequence"), Some("<END>"));
    if let Some(start) = out.iter().find(|m| event_type(m) == Some("message_start")) {
        assert!(
            start
                .pointer("/event/message/stop_sequence")
                .is_none_or(Value::is_null)
        );
    }
}

#[test]
fn messages_partial_consecutive_signature_blocks_keep_own_signature() {
    let mut r = messages(true);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::AgentThought("first".into())));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-1".into()),
    }));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-2".into()),
    }));
    out.extend(r.finish(&end_turn()));
    let sigs: Vec<&str> = out
        .iter()
        .filter(|m| delta_type(m) == Some("signature_delta"))
        .map(|m| json_str(m, "/event/delta/signature").expect("signature_delta signature"))
        .collect();
    assert_eq!(sigs, vec!["sig-1", "sig-2"], "{out:?}");
    let Some(frame) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("assistant frame: {out:?}");
    };
    let Some(blocks) = frame.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {frame:?}");
    };
    let [first, second] = blocks.as_slice() else {
        panic!("expected two thinking blocks: {blocks:?}");
    };
    assert_eq!(json_str(first, "/signature"), Some("sig-1"));
    assert_eq!(json_str(second, "/signature"), Some("sig-2"));
}
