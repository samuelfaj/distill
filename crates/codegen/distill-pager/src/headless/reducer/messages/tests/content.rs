//! Text/thinking/signature blocks and assistant-frame boundaries (default mode).

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn messages_groups_thinking_and_coalesced_text() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentThought("mulling".into()));
    r.reduce(StreamEvent::AgentMessage("Hello ".into()));
    r.reduce(StreamEvent::AgentMessage("world".into()));
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant message");
    assert_eq!(msg_type(&msg), Some("assistant"));
    assert_eq!(json_str(&msg, "/message/stop_reason"), Some("end_turn"));
    assert_eq!(json_str(&msg, "/session_id"), Some("sess-1"));
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [thinking, text, ..] = blocks.as_slice() else {
        panic!("expected thinking then text: {blocks:?}");
    };
    assert_eq!(json_str(thinking, "/type"), Some("thinking"));
    assert_eq!(json_str(thinking, "/thinking"), Some("mulling"));
    assert_eq!(json_str(text, "/type"), Some("text"));
    assert_eq!(json_str(text, "/text"), Some("Hello world"));
}

#[test]
fn messages_response_completed_stamps_assistant_frame() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentThought("plan".into()));
    r.reduce(StreamEvent::AgentMessage("hi".into()));
    assert!(
        r.reduce(StreamEvent::ResponseCompleted {
            message_id: Some("msg_real".into()),
            stop_reason: Some("end_turn".into()),
            usage: Some(ResponseUsage {
                input_tokens: 12,
                output_tokens: 7,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 0,
                ..Default::default()
            }),
            signature: Some("sig-abc".into()),
            stop_sequence: None,
        })
        .is_empty()
    );
    let msg = r.flush_assistant(Some("stop")).expect("assistant message");
    assert_eq!(json_str(&msg, "/message/id"), Some("msg_real"));
    assert_eq!(json_str(&msg, "/message/stop_reason"), Some("end_turn"));
    assert_eq!(
        msg.pointer("/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(12)
    );
    assert_eq!(
        msg.pointer("/message/usage/output_tokens")
            .and_then(Value::as_u64),
        Some(7)
    );
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let Some(thinking) = blocks.first() else {
        panic!("expected thinking block: {blocks:?}");
    };
    assert_eq!(json_str(thinking, "/type"), Some("thinking"));
    assert_eq!(json_str(thinking, "/signature"), Some("sig-abc"));
}

#[test]
fn messages_multiple_thinking_blocks_stamp_signature_on_last_only() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentThought("first think".into()));
    r.reduce(StreamEvent::AgentMessage("interlude".into()));
    r.reduce(StreamEvent::AgentThought("second think".into()));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: None,
        signature: Some("sig-final".into()),
        stop_sequence: None,
    });
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant message");
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [first, text, second, ..] = blocks.as_slice() else {
        panic!("expected thinking, text, thinking: {blocks:?}");
    };
    assert_eq!(json_str(first, "/type"), Some("thinking"));
    assert_eq!(json_str(first, "/thinking"), Some("first think"));
    assert_eq!(json_str(first, "/signature"), Some(""));
    assert_eq!(json_str(text, "/type"), Some("text"));
    assert_eq!(json_str(second, "/type"), Some("thinking"));
    assert_eq!(json_str(second, "/thinking"), Some("second think"));
    assert_eq!(json_str(second, "/signature"), Some("sig-final"));
}

#[test]
fn messages_response_completed_consumed_per_response() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentMessage("call".into()));
    r.reduce(response_completed("msg_a", "tool_use"));
    r.reduce(StreamEvent::ToolCallUpdate(tool_update(
        "in_progress",
        Value::Null,
    )));
    let out = r.reduce(StreamEvent::ToolCallUpdate(tool_update(
        "completed",
        json!("done"),
    )));
    let Some(assistant) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("expected assistant frame: {out:?}");
    };
    assert_eq!(json_str(assistant, "/message/id"), Some("msg_a"));
    assert_eq!(
        json_str(assistant, "/message/stop_reason"),
        Some("tool_use")
    );
    r.reduce(StreamEvent::AgentMessage("next".into()));
    let msg = r.flush_assistant(Some("end_turn")).expect("assistant");
    assert_eq!(json_str(&msg, "/message/id"), Some("msg_0"));
    assert_eq!(json_str(&msg, "/message/stop_reason"), Some("end_turn"));
}

#[test]
fn messages_signature_only_thinking_block_kept_in_frame() {
    let mut r = messages(false);
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-only".into()),
    });
    r.reduce(StreamEvent::AgentMessage("answer".into()));
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [thinking, text, ..] = blocks.as_slice() else {
        panic!("expected thinking then text: {blocks:?}");
    };
    assert_eq!(json_str(thinking, "/type"), Some("thinking"));
    assert_eq!(json_str(thinking, "/thinking"), Some(""));
    assert_eq!(json_str(thinking, "/signature"), Some("sig-only"));
    assert_eq!(json_str(text, "/type"), Some("text"));
    assert_eq!(json_str(text, "/text"), Some("answer"));
}

#[test]
fn messages_pure_signature_only_response_emits_thinking_block() {
    let mut r = messages(false);
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-only".into()),
    });
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [thinking] = blocks.as_slice() else {
        panic!("expected one thinking block: {blocks:?}");
    };
    assert_eq!(json_str(thinking, "/type"), Some("thinking"));
    assert_eq!(json_str(thinking, "/thinking"), Some(""));
    assert_eq!(json_str(thinking, "/signature"), Some("sig-only"));
}

#[test]
fn messages_no_spurious_empty_thinking_block() {
    let mut r = messages(false);
    assert!(r.flush_assistant(Some("end_turn")).is_none());
}

#[test]
fn messages_per_response_model_reflects_mid_session_switch() {
    let mut r = messages(false);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", Some("grok-4"), 5)));
    out.extend(r.reduce(StreamEvent::AgentMessage("from A".into())));
    out.extend(r.reduce(response_completed("msg_a", "end_turn")));
    out.extend(r.reduce(response_started("msg_b", Some("grok-4-fast"), 6)));
    out.extend(r.reduce(StreamEvent::AgentMessage("from B".into())));
    out.extend(r.reduce(response_completed("msg_b", "end_turn")));
    out.extend(r.finish(&end_turn()));
    let frames: Vec<&Value> = out
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let [a, b] = frames.as_slice() else {
        panic!("one frame per response: {out:?}");
    };
    assert_eq!(json_str(a, "/message/id"), Some("msg_a"));
    assert_eq!(json_str(a, "/message/model"), Some("grok-4"));
    assert_eq!(json_str(b, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(b, "/message/model"), Some("grok-4-fast"));
}

#[test]
fn messages_per_block_thinking_signatures_kept() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentThought("first think".into()));
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-1".into()),
    });
    r.reduce(StreamEvent::AgentMessage("interlude".into()));
    r.reduce(StreamEvent::AgentThought("second think".into()));
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-2".into()),
    });
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: None,
        signature: Some("sig-2".into()),
        stop_sequence: None,
    });
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant message");
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [first, text, second, ..] = blocks.as_slice() else {
        panic!("expected thinking, text, thinking: {blocks:?}");
    };
    assert_eq!(json_str(first, "/type"), Some("thinking"));
    assert_eq!(json_str(first, "/thinking"), Some("first think"));
    assert_eq!(json_str(first, "/signature"), Some("sig-1"));
    assert_eq!(json_str(text, "/type"), Some("text"));
    assert_eq!(json_str(second, "/type"), Some("thinking"));
    assert_eq!(json_str(second, "/thinking"), Some("second think"));
    assert_eq!(json_str(second, "/signature"), Some("sig-2"));
}

#[test]
fn messages_assistant_frame_carries_stop_sequence() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentMessage("answer".into()));
    r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_seq".into()),
        stop_reason: Some("stop_sequence".into()),
        usage: None,
        signature: None,
        stop_sequence: Some("<END>".into()),
    });
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    assert_eq!(
        json_str(&msg, "/message/stop_reason"),
        Some("stop_sequence")
    );
    assert_eq!(json_str(&msg, "/message/stop_sequence"), Some("<END>"));
}

#[test]
fn messages_consecutive_text_responses_split_into_frames() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentMessage("first".into()));
    r.reduce(response_completed("msg_a", "end_turn"));
    let out = r.reduce(StreamEvent::AgentMessage("second".into()));
    let Some(a) = out.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("frame A flushed on new content: {out:?}");
    };
    assert_eq!(json_str(a, "/message/id"), Some("msg_a"));
    assert_eq!(json_str(a, "/message/content/0/text"), Some("first"));
    r.reduce(response_completed("msg_b", "end_turn"));
    let out2 = r.finish(&turn_end("end_turn", "second"));
    let Some(b) = out2.iter().find(|m| msg_type(m) == Some("assistant")) else {
        panic!("frame B flushed at finish: {out2:?}");
    };
    assert_eq!(json_str(b, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(b, "/message/content/0/text"), Some("second"));
}

#[test]
fn messages_duplicate_response_started_does_not_merge_content() {
    let mut r = messages(false);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", Some("grok-4"), 5)));
    out.extend(r.reduce(StreamEvent::AgentMessage("A".into())));
    out.extend(r.reduce(response_started("msg_b", Some("grok-4"), 6)));
    out.extend(r.reduce(StreamEvent::AgentMessage("B".into())));
    out.extend(r.reduce(response_completed("msg_b", "end_turn")));
    out.extend(r.finish(&end_turn()));
    let frames: Vec<&Value> = out
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let [a, b] = frames.as_slice() else {
        panic!("A flushed before B opens: {out:?}");
    };
    assert_eq!(json_str(a, "/message/id"), Some("msg_a"));
    assert_eq!(json_str(a, "/message/content/0/text"), Some("A"));
    assert_eq!(
        a.pointer("/message/content")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "A did not absorb B's content"
    );
    assert_eq!(json_str(b, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(b, "/message/content/0/text"), Some("B"));
    assert_eq!(
        b.pointer("/message/content")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "B did not absorb A's content"
    );
}

#[test]
fn messages_signature_only_restart_does_not_leak_signature() {
    let mut r = messages(false);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", None, 0)));
    out.extend(r.reduce(StreamEvent::AgentThought("mull".into())));
    out.extend(r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-a".into()),
    }));
    out.extend(r.reduce(response_started("msg_b", None, 0)));
    out.extend(r.reduce(StreamEvent::AgentMessage("B".into())));
    out.extend(r.reduce(response_completed("msg_b", "end_turn")));
    out.extend(r.finish(&end_turn()));
    let frames: Vec<&Value> = out
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let [a, b] = frames.as_slice() else {
        panic!("expected two assistant frames: {out:?}");
    };
    assert_eq!(json_str(a, "/message/id"), Some("msg_a"));
    assert_eq!(json_str(a, "/message/content/0/type"), Some("thinking"));
    assert_eq!(json_str(a, "/message/content/0/signature"), Some("sig-a"));
    assert_eq!(json_str(b, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(b, "/message/content/0/type"), Some("text"));
    let Some(b_blocks) = b.pointer("/message/content").and_then(Value::as_array) else {
        panic!("B content array: {b:?}");
    };
    assert!(
        b_blocks
            .iter()
            .all(|block| json_str(block, "/type") != Some("thinking")),
        "no thinking block leaked into B: {b:?}"
    );
}

#[test]
fn messages_content_before_late_response_started_flushes_first() {
    let mut r = messages(false);
    let mut out = Vec::new();
    out.extend(r.reduce(StreamEvent::AgentMessage("early".into())));
    out.extend(r.reduce(response_started("msg_b", None, 0)));
    out.extend(r.reduce(StreamEvent::AgentMessage("late".into())));
    out.extend(r.reduce(response_completed("msg_b", "end_turn")));
    out.extend(r.finish(&end_turn()));
    let frames: Vec<&Value> = out
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let [early, late] = frames.as_slice() else {
        panic!("early content is its own frame: {out:?}");
    };
    assert_eq!(json_str(early, "/message/content/0/text"), Some("early"));
    assert_eq!(json_str(early, "/message/id"), Some("msg_0"));
    assert_eq!(json_str(late, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(late, "/message/content/0/text"), Some("late"));
}

#[test]
fn messages_consecutive_signature_blocks_keep_own_signature() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentThought("first".into()));
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-1".into()),
    });
    r.reduce(StreamEvent::ReasoningCompleted {
        signature: Some("sig-2".into()),
    });
    let msg = r
        .flush_assistant(Some("end_turn"))
        .expect("assistant frame");
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        panic!("assistant content array: {msg:?}");
    };
    let [first, second] = blocks.as_slice() else {
        panic!("two thinking blocks, not collapsed: {blocks:?}");
    };
    assert_eq!(json_str(first, "/type"), Some("thinking"));
    assert_eq!(json_str(first, "/thinking"), Some("first"));
    assert_eq!(json_str(first, "/signature"), Some("sig-1"));
    assert_eq!(json_str(second, "/type"), Some("thinking"));
    assert_eq!(json_str(second, "/thinking"), Some(""));
    assert_eq!(json_str(second, "/signature"), Some("sig-2"));
}

#[test]
fn messages_compact_completed_maps_to_system_boundary() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AgentMessage("hi".into()));
    let out = r.reduce(StreamEvent::Lifecycle(Lifecycle::CompactCompleted {
        pre_tokens: 1234,
    }));
    let Some(boundary) = out.last() else {
        panic!("expected compact boundary: {out:?}");
    };
    assert_eq!(msg_type(boundary), Some("system"));
    assert_eq!(json_str(boundary, "/subtype"), Some("compact_boundary"));
    assert_eq!(
        json_str(boundary, "/compact_metadata/trigger"),
        Some("auto")
    );
    assert_eq!(
        boundary
            .pointer("/compact_metadata/pre_tokens")
            .and_then(Value::as_u64),
        Some(1234)
    );
}

#[test]
fn messages_late_response_completed_for_flushed_response_is_dropped() {
    let mut r = messages(false);
    let mut out = Vec::new();
    out.extend(r.reduce(response_started("msg_a", Some("grok-4"), 1)));
    out.extend(r.reduce(StreamEvent::AgentMessage("a-text".into())));
    out.extend(r.reduce(response_started("msg_b", Some("grok-4"), 2)));
    out.extend(r.reduce(StreamEvent::AgentMessage("b-text".into())));
    out.extend(r.reduce(StreamEvent::ResponseCompleted {
        message_id: Some("msg_a".into()),
        stop_reason: Some("end_turn".into()),
        usage: Some(ResponseUsage {
            input_tokens: 99,
            output_tokens: 99,
            ..Default::default()
        }),
        signature: None,
        stop_sequence: None,
    }));
    out.extend(r.finish(&end_turn()));
    let assistants: Vec<_> = out
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let [a, b] = assistants.as_slice() else {
        panic!("A flushed at B's start, B flushed at finish: {out:?}");
    };
    assert_eq!(json_str(a, "/message/id"), Some("msg_a"));
    assert_eq!(json_str(a, "/message/content/0/text"), Some("a-text"));
    assert_eq!(json_str(b, "/message/id"), Some("msg_b"));
    assert_eq!(json_str(b, "/message/content/0/text"), Some("b-text"));
    assert_ne!(
        b.pointer("/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(99),
        "A's late usage must not land on B"
    );
}
