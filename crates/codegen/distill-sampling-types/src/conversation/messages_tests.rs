use super::test_support::*;
use super::*;
use crate::conversation::messages::ANCHOR_ROUNDS;

fn messages_test_request(reasoning_effort: Option<crate::ReasoningEffort>) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::user("Hello")],
        model: Some("test-model".to_string()),
        reasoning_effort,
        ..Default::default()
    }
}

#[test]
fn json_schema_and_reasoning_effort_are_orthogonal_in_output_config() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "x": { "type": "string" } },
        "required": ["x"]
    });
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("go")])
        .with_json_schema(schema);
    req.reasoning_effort = Some(crate::ReasoningEffort::High);

    let msgs = build_messages_request(&req);
    let oc = msgs.output_config.expect("output_config present");
    assert_eq!(oc.effort.as_deref(), Some("high"));
    assert!(oc.format.is_some());
    assert!(
        msgs.thinking.is_some(),
        "thinking set when effort is present"
    );
}

#[test]
fn test_messages_request_wire_format_for_supported_variants() {
    for (variant, expected) in [
        (crate::ReasoningEffort::Low, "low"),
        (crate::ReasoningEffort::Medium, "medium"),
        (crate::ReasoningEffort::High, "high"),
        (crate::ReasoningEffort::Xhigh, "xhigh"),
        (crate::ReasoningEffort::Max, "max"),
    ] {
        let req = messages_test_request(Some(variant));
        let msgs = build_messages_request(&req);
        let json = serde_json::to_value(&msgs).unwrap();
        assert_eq!(
            json.pointer("/output_config/effort")
                .and_then(|v| v.as_str()),
            Some(expected),
            "{variant:?} should map to output_config.effort={expected:?}; got: {json:#}",
        );
        assert_eq!(
            json.pointer("/thinking/type").and_then(|v| v.as_str()),
            Some("adaptive"),
            "{variant:?} should auto-pair thinking.type=adaptive; got: {json:#}",
        );
    }
}

#[test]
fn test_messages_request_omits_output_config_when_no_supported_effort() {
    let none_or_unsupported = [
        None,
        Some(crate::ReasoningEffort::None),
        Some(crate::ReasoningEffort::Minimal),
    ];
    for input in none_or_unsupported {
        let req = messages_test_request(input);
        let msgs = build_messages_request(&req);
        assert!(
            msgs.output_config.is_none(),
            "input {input:?} must not produce output_config",
        );
        assert!(
            msgs.thinking.is_none(),
            "input {input:?} must not auto-pair thinking",
        );
    }
}

#[test]
fn test_messages_request_thinking_carries_summarized_display() {
    let req = ConversationRequest {
        reasoning_effort: Some(crate::ReasoningEffort::High),
        ..ConversationRequest::from_items(vec![ConversationItem::user("hi")])
            .with_model("messages-compatible-model")
    };
    let msg = build_messages_request(&req);
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(
        json.pointer("/thinking/type").and_then(|v| v.as_str()),
        Some("adaptive"),
        "thinking.type should be 'adaptive'; got: {json:#}",
    );
    assert_eq!(
        json.pointer("/thinking/display").and_then(|v| v.as_str()),
        Some("summarized"),
        "thinking.display must be 'summarized' so 4.7+ surfaces thinking content; got: {json:#}",
    );
}

#[test]
fn test_messages_request_omits_thinking_when_effort_unset() {
    let req = ConversationRequest::from_items(vec![ConversationItem::user("hi")])
        .with_model("messages-compatible-model");
    let msg = build_messages_request(&req);
    let json = serde_json::to_value(&msg).unwrap();
    assert!(
        json.get("thinking").is_none()
            || json
                .pointer("/thinking")
                .map(|v| v.is_null())
                .unwrap_or(false),
        "thinking must be absent when reasoning_effort is unset; got: {json:#}",
    );
    assert!(
        json.get("output_config").is_none()
            || json
                .pointer("/output_config")
                .map(|v| v.is_null())
                .unwrap_or(false),
        "output_config must be absent when reasoning_effort is unset; got: {json:#}",
    );
}

#[test]
fn test_messages_request_previous_tip_skips_a_trailing_user_run() {
    let mut items = vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::user("Fix the bug"),
    ];
    items.extend(agent_turn(0));
    items.extend(agent_turn(1));
    // The shape after a parallel batch: tool results, then followups.
    items.push(ConversationItem::user("[Image content]"));
    items.push(ConversationItem::user("<system-reminder>"));

    let json = serde_json::to_value(build_messages_request(
        &ConversationRequest::from_items(items).with_model("messages-compatible-model"),
    ))
    .unwrap();
    let Some(messages) = json.get("messages").and_then(|v| v.as_array()) else {
        panic!("expected messages array: {json:#}");
    };

    let marked: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| marker_on_last_block(m).is_some())
        .map(|(i, _)| i)
        .collect();
    let last_assistant = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .unwrap();
    let [prev_tip, tip] = marked.as_slice() else {
        panic!("tip and previous tip only: {json:#}");
    };
    assert_eq!(*tip, messages.len() - 1, "tip: {json:#}");
    assert!(
        *prev_tip < last_assistant,
        "the previous tip must sit before the last assistant turn, not inside \
             the trailing user run; got {marked:?} in {json:#}",
    );
}

#[test]
fn test_messages_request_cache_breakpoint_marks_an_image_tip() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::User(UserItem {
            content: vec![
                ContentPart::Text {
                    text: "what is in this screenshot".into(),
                },
                ContentPart::Image {
                    url: "data:image/png;base64,iVBOR".into(),
                },
            ],
            ..Default::default()
        }),
    ])
    .with_model("messages-compatible-model");

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    let Some(first_msg) = json
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|m| m.first())
    else {
        panic!("expected first message: {json:#}");
    };
    let Some(blocks) = first_msg.get("content").and_then(|c| c.as_array()) else {
        panic!("expected content array: {json:#}");
    };

    assert_eq!(
        blocks
            .last()
            .and_then(|b| b.get("type"))
            .and_then(|t| t.as_str()),
        Some("image")
    );
    assert_eq!(
        marker_on_last_block(first_msg),
        Some("ephemeral"),
        "{json:#}",
    );
    assert!(
        blocks
            .first()
            .is_some_and(|b| b.get("cache_control").is_none()),
        "{json:#}"
    );
}

#[test]
fn test_messages_request_cache_breakpoint_skips_thinking() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Fix the bug"),
        ConversationItem::Reasoning(synthesized_reasoning_item("weighing options")),
        ConversationItem::assistant("Fixed it."),
    ])
    .with_model("messages-compatible-model");

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    let Some(asst) = json
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|m| m.get(1))
    else {
        panic!("expected assistant message: {json:#}");
    };
    let Some(blocks) = asst.get("content").and_then(|c| c.as_array()) else {
        panic!("expected content array: {json:#}");
    };

    let thinking = blocks
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"))
        .expect("reasoning should emit a thinking block");
    assert!(thinking.get("cache_control").is_none(), "{json:#}");
    assert_eq!(marker_on_last_block(asst), Some("ephemeral"), "{json:#}",);
}

#[test]
fn test_btw_cross_api_messages_no_regressions() {
    let items = btw_prepare_items(btw_mid_turn_conversation());
    let req = ConversationRequest::from_items(items);
    let msg = build_messages_request(&req);
    let json = serde_json::to_value(&msg).unwrap();

    let messages = json.get("messages").unwrap().as_array().unwrap();

    // No thinking blocks anywhere.
    for (i, m) in messages.iter().enumerate() {
        if let Some(content) = m.get("content").and_then(|c| c.as_array()) {
            for block in content {
                assert_ne!(
                    block.get("type").and_then(|t| t.as_str()),
                    Some("thinking"),
                    "messages[{i}] must not contain thinking blocks",
                );
            }
        }
    }

    // Last assistant message must not have unanswered tool_use.
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .expect("should have an assistant message");
    if let Some(content) = last_assistant.get("content").and_then(|c| c.as_array()) {
        for block in content {
            assert_ne!(
                block.get("type").and_then(|t| t.as_str()),
                Some("tool_use"),
                "last assistant in btw request must not have unanswered tool_use",
            );
        }
    }

    // Top-level thinking must be absent (no reasoning_effort set).
    assert!(
        json.get("thinking").is_none() || json.pointer("/thinking").is_some_and(|v| v.is_null()),
        "top-level thinking must be absent; got: {json:#}",
    );

    assert!(
        json.get("temperature").is_none()
            || json.pointer("/temperature").is_some_and(|v| v.is_null()),
        "temperature must be absent so proxy defaults can apply; got: {json:#}",
    );

    // The completed tool pair (call_1) must survive.
    let has_tool_use_call_1 = messages.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && b.get("id").and_then(|id| id.as_str()) == Some("call_1")
                })
            })
    });
    assert!(
        has_tool_use_call_1,
        "completed tool_use call_1 must survive"
    );

    let has_tool_result_call_1 = messages.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && b.get("tool_use_id").and_then(|id| id.as_str()) == Some("call_1")
                })
            })
    });
    assert!(
        has_tool_result_call_1,
        "completed tool_result for call_1 must survive"
    );
}

#[test]
fn test_tool_result_with_images_to_anthropic() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Read this"),
        ConversationItem::Assistant(AssistantItem {
            content: String::new().into(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".to_string(),
                arguments: "{}".into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::tool_result_with_images(
            "call_1",
            "Read image file: photo.png",
            vec![ContentPart::Image {
                url: "data:image/png;base64,iVBOR".into(),
            }],
        ),
    ]);

    let messages_req = build_messages_request(&req);

    // Find the user message that contains the tool result (the Messages API wraps tool results in user messages)
    let tool_result_msg = messages_req
        .messages
        .iter()
        .find(|m| {
            if let crate::messages::MessageContent::Blocks(blocks) = &m.content {
                blocks
                    .iter()
                    .any(|b| matches!(b, crate::messages::ContentBlock::ToolResult { .. }))
            } else {
                false
            }
        })
        .expect("Expected a message with ToolResult block");

    let crate::messages::MessageContent::Blocks(blocks) = &tool_result_msg.content else {
        panic!("Expected Blocks");
    };
    let tool_result_block = blocks
        .iter()
        .find_map(|b| {
            if let crate::messages::ContentBlock::ToolResult { content, .. } = b {
                Some(content)
            } else {
                None
            }
        })
        .unwrap();

    let crate::messages::ToolResultContent::Blocks(inner) = tool_result_block else {
        panic!("Expected ToolResultContent::Blocks, got Text");
    };
    let [t0, t1] = inner.as_slice() else {
        panic!("expected two inner blocks: {inner:?}");
    };
    assert!(
        matches!(t0, crate::messages::ContentBlock::Text { text, .. } if text == "Read image file: photo.png")
    );
    assert!(
        matches!(t1, crate::messages::ContentBlock::Image { source: crate::messages::ImageSource::Base64 { media_type, data }, .. } if media_type == "image/png" && data == "iVBOR")
    );
}

#[test]
fn upgrade_legacy_reasoning_singular_anthropic_no_id() {
    // Messages streaming sets id = "" (see stream/messages.rs:340).
    // The upgrader must still emit a sibling carrying text and signature
    let raw = serde_json::json!({
        "type": "assistant",
        "content": "answer",
        "reasoning": {
            "text": "Let me think about this...",
            "encrypted": "signature-bytes-here",
            "id": ""
        },
        "model_id": "messages-compatible-model"
    });
    let mut seen = std::collections::HashSet::new();
    let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
    assert_eq!(siblings.len(), 1);
    let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
        panic!("expected Reasoning sibling: {siblings:?}");
    };
    assert_eq!(r.id, "");
    assert_eq!(r.encrypted_content.as_deref(), Some("signature-bytes-here"));
}

#[test]
fn test_messages_request_cache_breakpoint_marks_project_instructions_end() {
    // A child's first request: siblings share system, tools and AGENTS.md up to this mark.
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::project_instructions("AGENTS.md rules"),
        ConversationItem::user("task"),
    ])
    .with_model("messages-compatible-model");

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    let Some(messages) = json.get("messages").and_then(|v| v.as_array()) else {
        panic!("expected messages array: {json:#}");
    };
    let Some(agents) = messages.first() else {
        panic!("expected AGENTS.md message: {json:#}");
    };
    assert_eq!(marker_on_last_block(agents), Some("ephemeral"), "{json:#}");
    assert_eq!(count_cache_control(&json), 3, "{json:#}");
}

#[test]
fn test_messages_request_cache_breakpoint_keeps_fourth_slot_free() {
    // A gateway with automatic caching adds one breakpoint and five are rejected,
    // so AGENTS.md is not marked once system, tip and previous turn use three.
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::project_instructions("AGENTS.md rules"),
        ConversationItem::user("task"),
        ConversationItem::assistant("working"),
        ConversationItem::user("follow up"),
    ])
    .with_model("messages-compatible-model");

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    assert_eq!(count_cache_control(&json), 3, "{json:#}");
}

#[test]
fn test_messages_request_cache_breakpoint_skips_lone_project_instructions() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::project_instructions("AGENTS.md rules"),
    ])
    .with_model("messages-compatible-model");

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    // System plus the tip, which is the lone project-instructions message.
    assert_eq!(count_cache_control(&json), 2, "{json:#}");
}

/// A one-shot side call is never resent, and its system prompt is not read by
/// another call before it expires (an initial title, a memory capture whose
/// system prompt carries the capture's own note). Any breakpoint would only
/// pay the cache-write premium, so none is sent.
#[test]
fn a_one_shot_request_writes_no_breakpoint() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("Extract durable observations."),
        ConversationItem::user("transcript"),
        ConversationItem::assistant("working"),
        ConversationItem::user("follow up"),
    ])
    .with_model("messages-compatible-model")
    .one_shot();

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    assert_eq!(count_cache_control(&json), 0, "{json:#}");
    assert_eq!(
        json.get("system").and_then(|v| v.as_str()),
        Some("Extract durable observations."),
        "an unmarked lone system prompt goes as plain text: {json:#}",
    );
}

/// The goal evaluator runs every round behind the same system prompt and
/// goal: those keep their breakpoints so the next round reads them, while the
/// round message, new every time, stays unmarked.
#[test]
fn a_one_shot_request_with_a_shared_prefix_keeps_system_and_leading_message() {
    let mut req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are the evaluator."),
        ConversationItem::project_instructions("the goal"),
        ConversationItem::user("this round"),
    ])
    .with_model("messages-compatible-model")
    .one_shot();
    req.shared_prefix = true;

    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    let Some(messages) = json.get("messages").and_then(|v| v.as_array()) else {
        panic!("expected messages array: {json:#}");
    };
    assert_eq!(
        json.pointer("/system/0/cache_control/type")
            .and_then(|v| v.as_str()),
        Some("ephemeral"),
        "{json:#}",
    );
    assert_eq!(
        marker_on_last_block(&messages[0]),
        Some("ephemeral"),
        "{json:#}"
    );
    assert_eq!(marker_on_last_block(&messages[1]), None, "tip: {json:#}");
    assert_eq!(count_cache_control(&json), 2, "{json:#}");
}

fn rounds_history(rounds: usize) -> Vec<ConversationItem> {
    let mut items = vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::user("Fix the bug"),
    ];
    for n in 0..rounds {
        items.extend(agent_turn(n));
    }
    items
}

fn anchored(items: Vec<ConversationItem>) -> crate::messages::MessagesRequest {
    build_messages_request_with(
        &ConversationRequest::from_items(items).with_model("messages-compatible-model"),
        MessagesCacheOptions {
            anchor: true,
            extended_ttl: false,
            system_messages: false,
            deferred_tools: false,
        },
    )
}

fn marked_message_indices(request: &crate::messages::MessagesRequest) -> Vec<usize> {
    let json = serde_json::to_value(request).unwrap();
    json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| count_cache_control(m) > 0)
        .map(|(i, _)| i)
        .collect()
}

/// An edit older than the previous tip (a pruned directive, a cleared result)
/// would otherwise re-bill everything after the system prompt. The anchor is
/// only worth its slot if it names a position an earlier request already
/// wrote: the tip of that request, i.e. its last message. It must also hold
/// still between moves, or each request would write a new entry for it.
#[test]
fn the_anchor_sits_on_a_former_tip_and_moves_every_ten_rounds() {
    let mut previous_anchor = None;
    for rounds in 2..45 {
        let request = anchored(rounds_history(rounds));
        assert!(count_cache_control(&serde_json::to_value(&request).unwrap()) <= 4);
        let marked = marked_message_indices(&request);
        // system + anchor + previous tip + tip; the anchor is the oldest message mark.
        let anchor = marked[0];
        let round = (rounds - 2) / ANCHOR_ROUNDS * ANCHOR_ROUNDS;
        // The request that produced assistant round `round` ended right before it.
        let earlier = build_messages_request(
            &ConversationRequest::from_items(rounds_history(round))
                .with_model("messages-compatible-model"),
        );
        assert_eq!(anchor, earlier.messages.len() - 1, "rounds={rounds}");
        assert!(
            serde_json::to_value(&earlier.messages[anchor]).unwrap()["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .get("cache_control")
                .is_some(),
            "the earlier request wrote an entry exactly there (rounds={rounds})"
        );
        if let Some(previous) = previous_anchor
            && previous != anchor
        {
            assert_eq!(
                (rounds - 2) % ANCHOR_ROUNDS,
                0,
                "moves only on a boundary: rounds={rounds}"
            );
        }
        previous_anchor = Some(anchor);
    }
}

/// Without the option (a gateway that may add automatic caching, which takes
/// a slot itself) the fourth slot stays free, and a one-shot request still
/// marks nothing.
#[test]
fn the_anchor_needs_its_option_and_never_marks_a_one_shot() {
    let plain = build_messages_request(
        &ConversationRequest::from_items(rounds_history(30))
            .with_model("messages-compatible-model"),
    );
    assert_eq!(
        count_cache_control(&serde_json::to_value(&plain).unwrap()),
        3
    );

    let one_shot = build_messages_request_with(
        &ConversationRequest::from_items(rounds_history(30))
            .with_model("messages-compatible-model")
            .one_shot(),
        MessagesCacheOptions {
            anchor: true,
            extended_ttl: true,
            system_messages: false,
            deferred_tools: false,
        },
    );
    assert_eq!(
        count_cache_control(&serde_json::to_value(&one_shot).unwrap()),
        0
    );
}

fn ttl_values(request: &crate::messages::MessagesRequest) -> Vec<Option<String>> {
    fn walk(value: &serde_json::Value, out: &mut Vec<Option<String>>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(cc) = map.get("cache_control") {
                    out.push(cc.get("ttl").and_then(|v| v.as_str()).map(str::to_owned));
                }
                map.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&serde_json::to_value(request).unwrap(), &mut out);
    out
}

/// A round that is about to block on a long wait outlives the five-minute
/// entry, so every breakpoint gets the hour (one lifetime for all keeps the
/// API's longer-before-shorter rule). The lifetime costs 2x to write against
/// 1.25x, so it is sent only when the caller expects the wait and the client
/// knows the endpoint takes it; otherwise the wire keeps no `ttl` key.
#[test]
fn a_long_wait_marks_every_breakpoint_one_hour_only_where_the_endpoint_takes_it() {
    let mut req =
        ConversationRequest::from_items(rounds_history(25)).with_model("messages-compatible-model");
    req.long_cache_ttl = true;
    let options = MessagesCacheOptions {
        anchor: true,
        extended_ttl: true,
        system_messages: false,
        deferred_tools: false,
    };
    let long = build_messages_request_with(&req, options);
    assert_eq!(ttl_values(&long), vec![Some("1h".to_owned()); 4]);

    let unsupported = build_messages_request_with(
        &req,
        MessagesCacheOptions {
            extended_ttl: false,
            ..options
        },
    );
    assert!(ttl_values(&unsupported).iter().all(Option::is_none));
    assert!(
        !serde_json::to_string(&unsupported)
            .unwrap()
            .contains("\"ttl\"")
    );

    req.long_cache_ttl = false;
    assert!(
        ttl_values(&build_messages_request_with(&req, options))
            .iter()
            .all(Option::is_none)
    );

    // The rejection fallback strips the lifetime and keeps every breakpoint.
    let mut stripped = long.clone();
    assert_eq!(stripped.set_cache_ttl(None), 4);
    assert_eq!(ttl_values(&stripped), vec![None; 4]);
}

fn per_message_effort_history() -> Vec<ConversationItem> {
    let mut items = vec![
        ConversationItem::system("You are a helpful assistant."),
        ConversationItem::user("Fix the bug"),
    ];
    items.extend(agent_turn(0));
    items
}

fn per_message_effort_request(
    model: &str,
    items: Vec<ConversationItem>,
    effort: crate::ReasoningEffort,
) -> serde_json::Value {
    let mut req = ConversationRequest::from_items(items).with_model(model);
    req.reasoning_effort = Some(effort);
    serde_json::to_value(build_messages_request(&req)).unwrap()
}

fn marker_json(effort: &str) -> serde_json::Value {
    serde_json::json!({"role": "system", "content": [], "output_config": {"effort": effort}})
}

// A top-level effort change restarts the prompt cache, so a supported model carries it only in a marker the API applies to this request.
#[test]
fn per_message_effort_marker_follows_the_last_assistant_message() {
    let json = per_message_effort_request(
        "claude-opus-5-5",
        per_message_effort_history(),
        crate::ReasoningEffort::Low,
    );
    assert!(json.pointer("/output_config").is_none(), "{json:#}");
    assert_eq!(
        json.pointer("/thinking/type").and_then(|v| v.as_str()),
        Some("adaptive"),
        "{json:#}"
    );
    let messages = json["messages"].as_array().unwrap();
    let last_assistant = messages
        .iter()
        .rposition(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(messages[last_assistant + 1], marker_json("low"), "{json:#}");
    assert_eq!(messages.last().unwrap()["role"], "user", "{json:#}");
    assert_eq!(last_assistant + 3, messages.len(), "{json:#}");
}

// The effort must live only in the marker; any other difference would move the cached prefix when the effort changes.
#[test]
fn per_message_effort_is_the_only_difference_between_efforts() {
    let strip_marker = |mut json: serde_json::Value| {
        let messages = json["messages"].as_array_mut().unwrap();
        let at = messages.iter().position(|m| m["role"] == "system").unwrap();
        messages.remove(at);
        json
    };
    let high = per_message_effort_request(
        "claude-opus-5-5",
        per_message_effort_history(),
        crate::ReasoningEffort::High,
    );
    let low = per_message_effort_request(
        "claude-opus-5-5",
        per_message_effort_history(),
        crate::ReasoningEffort::Low,
    );
    assert_ne!(high, low);
    assert_eq!(strip_marker(high), strip_marker(low));
}

// With no assistant message yet, the marker leads the conversation so the first request still carries its effort.
#[test]
fn per_message_effort_marker_leads_the_first_request() {
    let json = per_message_effort_request(
        "claude-opus-5-5",
        vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Hello"),
        ],
        crate::ReasoningEffort::High,
    );
    let messages = json["messages"].as_array().unwrap();
    assert_eq!(messages[0], marker_json("high"), "{json:#}");
    assert_eq!(messages[1]["role"], "user", "{json:#}");
}

// Other models reject the system-role message, so they keep the top-level effort.
#[test]
fn unsupported_model_keeps_top_level_effort() {
    let json = per_message_effort_request(
        "claude-sonnet-5",
        per_message_effort_history(),
        crate::ReasoningEffort::Low,
    );
    assert_eq!(
        json.pointer("/output_config/effort")
            .and_then(|v| v.as_str()),
        Some("low"),
        "{json:#}"
    );
    assert!(
        json["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system"),
        "{json:#}"
    );
}

// A wrong match would send the beta marker to a model that rejects it, or skip it on a snapshot id that accepts it.
#[test]
fn supports_per_message_effort_matches_exact_ids_and_dated_snapshots() {
    use super::messages::supports_per_message_effort;
    assert!(supports_per_message_effort("claude-opus-5-5"));
    assert!(supports_per_message_effort("claude-opus-5-5-20260101"));
    assert!(supports_per_message_effort("claude-haiku-5-5"));
    assert!(!supports_per_message_effort("claude-opus-5-1"));
    assert!(!supports_per_message_effort("claude-sonnet-5"));
    assert!(!supports_per_message_effort("claude-opus-5-5-2026"));
    assert!(!supports_per_message_effort("claude-haiku-4-5-20251001"));
    assert!(!supports_per_message_effort("anthropic/claude-opus-5.5"));
}

// The schema is not an effort, so it stays top-level while the effort moves to the marker.
#[test]
fn per_message_effort_keeps_json_schema_top_level() {
    let schema = serde_json::json!({"type": "object"});
    let mut req = ConversationRequest::from_items(per_message_effort_history())
        .with_model("claude-opus-5-5")
        .with_json_schema(schema);
    req.reasoning_effort = Some(crate::ReasoningEffort::Low);
    let json = serde_json::to_value(build_messages_request(&req)).unwrap();
    assert!(json.pointer("/output_config/format").is_some(), "{json:#}");
    assert!(json.pointer("/output_config/effort").is_none(), "{json:#}");
    assert!(
        json["messages"]
            .as_array()
            .unwrap()
            .contains(&marker_json("low")),
        "{json:#}"
    );
}

const SYSTEM_MESSAGES: MessagesCacheOptions = MessagesCacheOptions {
    anchor: true,
    extended_ttl: false,
    system_messages: true,
    deferred_tools: false,
};

/// Two agent rounds and a closing answer under the opening prompt `v1`, then `tail`.
fn finished_turn_then(tail: Vec<ConversationItem>) -> Vec<ConversationItem> {
    let mut items = vec![
        ConversationItem::system("v1"),
        ConversationItem::user("Fix the bug"),
    ];
    items.extend(agent_turn(0));
    items.extend(agent_turn(1));
    items.push(ConversationItem::assistant("Fixed."));
    items.extend(tail);
    items
}

fn messages_json(
    model: &str,
    items: Vec<ConversationItem>,
    options: MessagesCacheOptions,
) -> serde_json::Value {
    serde_json::to_value(build_messages_request_with(
        &ConversationRequest::from_items(items).with_model(model),
        options,
    ))
    .unwrap()
}

fn without_cache_control(mut value: serde_json::Value) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut value);
    value
}

/// A mid-session prompt change (plan mode, `/memory`, a new worker) used to rewrite the top-level
/// system prompt, re-billing every cached token after it. On a model that takes system-role
/// messages the opening prompt stays byte for byte and the new prompt follows the cached history,
/// after the user turn (the only place the API takes one), so only the update is new input.
#[test]
fn a_system_prompt_update_follows_the_cached_history_and_leaves_the_prefix_alone() {
    let before = messages_json(
        "claude-opus-5-5",
        finished_turn_then(vec![ConversationItem::user("Now add a test")]),
        SYSTEM_MESSAGES,
    );
    let after = messages_json(
        "claude-opus-5-5",
        finished_turn_then(vec![
            ConversationItem::system_prompt_update("v2"),
            ConversationItem::user("Now add a test"),
        ]),
        SYSTEM_MESSAGES,
    );
    assert_eq!(after["system"], before["system"], "{after:#}");
    let before_messages = before["messages"].as_array().unwrap();
    let after_messages = after["messages"].as_array().unwrap();
    assert_eq!(after_messages.len(), before_messages.len() + 1, "{after:#}");
    assert_eq!(
        &after_messages[..before_messages.len()],
        before_messages.as_slice(),
        "every message before the update keeps its bytes and breakpoints"
    );
    let update = after_messages.last().unwrap();
    assert_eq!(update["role"], "system", "{after:#}");
    assert_eq!(
        update["content"],
        format!("{SYSTEM_PROMPT_UPDATE_PREAMBLE}\n\nv2"),
        "the model is told the update replaces the earlier prompt"
    );
    assert_eq!(
        count_cache_control(update),
        0,
        "no breakpoint on a system message"
    );

    // The next round keeps the update where it was, so it is cached from then on.
    let next = messages_json(
        "claude-opus-5-5",
        finished_turn_then(vec![
            ConversationItem::system_prompt_update("v2"),
            ConversationItem::user("Now add a test"),
            ConversationItem::assistant("Added."),
            ConversationItem::user("Thanks"),
        ]),
        SYSTEM_MESSAGES,
    );
    let next_messages = without_cache_control(next["messages"].clone());
    assert_eq!(
        next_messages.as_array().unwrap()[..after_messages.len()],
        without_cache_control(after["messages"].clone())
            .as_array()
            .unwrap()[..],
        "{next:#}"
    );
    assert_eq!(next_messages[after_messages.len()]["role"], "assistant");
}

/// Without the option (another endpoint, or after the API refused one), on a model without
/// system-role messages, or when the update has no user turn to follow, the request is exactly the
/// one rewriting the head sent: the latest update is the top-level prompt and nothing else moves.
#[test]
fn without_system_messages_the_latest_update_is_the_top_level_prompt() {
    let items = finished_turn_then(vec![
        ConversationItem::system_prompt_update("v2"),
        ConversationItem::user("Now add a test"),
        ConversationItem::assistant("Added."),
        ConversationItem::system_prompt_update("v3"),
        ConversationItem::user("Thanks"),
    ]);
    let mut rewritten = items.clone();
    assert!(fold_system_prompt_updates(&mut rewritten));
    assert_eq!(current_system_prompt(&rewritten), Some("v3"));
    for (model, options) in [
        ("claude-opus-5-5", MessagesCacheOptions::default()),
        ("claude-sonnet-5", SYSTEM_MESSAGES),
    ] {
        let folded = messages_json(model, items.clone(), options);
        assert_eq!(
            folded,
            messages_json(model, rewritten.clone(), options),
            "{model}"
        );
        assert!(
            folded["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["role"] != "system"),
            "{folded:#}"
        );
    }

    // A request that ends on the assistant leaves the update nowhere to sit.
    let prefill = finished_turn_then(vec![ConversationItem::system_prompt_update("v2")]);
    let mut prefill_rewritten = prefill.clone();
    assert!(fold_system_prompt_updates(&mut prefill_rewritten));
    assert_eq!(
        messages_json("claude-opus-5-5", prefill, SYSTEM_MESSAGES),
        messages_json("claude-opus-5-5", prefill_rewritten, SYSTEM_MESSAGES)
    );
}

/// The API rejects a system message between a `tool_use` and its `tool_result`, so an update
/// stored inside a tool round goes after the results, never right after the assistant.
#[test]
fn a_system_prompt_update_never_splits_a_tool_round() {
    let mut items = vec![
        ConversationItem::system("v1"),
        ConversationItem::user("Fix the bug"),
    ];
    let mut round = agent_turn(0);
    round.insert(1, ConversationItem::system_prompt_update("v2"));
    items.extend(round);
    let json = messages_json("claude-opus-5-5", items, SYSTEM_MESSAGES);
    let messages = json["messages"].as_array().unwrap();
    let at = messages
        .iter()
        .position(|m| m["role"] == "system")
        .expect("the update is sent in history");
    assert_eq!(at + 1, messages.len(), "{json:#}");
    assert_eq!(messages[at - 1]["role"], "user", "{json:#}");
    assert_eq!(
        messages[at - 1]["content"][0]["type"],
        "tool_result",
        "{json:#}"
    );
}

/// A trailing update must not push the effort back to the top level, which would restart the cache.
#[test]
fn per_message_effort_survives_a_trailing_system_prompt_update() {
    let mut req = ConversationRequest::from_items(finished_turn_then(vec![
        ConversationItem::user("Now add a test"),
        ConversationItem::system_prompt_update("v2"),
    ]))
    .with_model("claude-opus-5-5");
    req.reasoning_effort = Some(crate::ReasoningEffort::Low);
    let json = serde_json::to_value(build_messages_request_with(&req, SYSTEM_MESSAGES)).unwrap();
    assert!(json.pointer("/output_config/effort").is_none(), "{json:#}");
    let messages = json["messages"].as_array().unwrap();
    assert!(messages.contains(&marker_json("low")), "{json:#}");
    assert_eq!(messages.last().unwrap()["role"], "system", "{json:#}");
}

// A wrong match sends a system-role message to a model that rejects it (Claude Sonnet 5), or rewrites the prompt where one was free.
#[test]
fn supports_system_messages_matches_the_documented_models() {
    for model in [
        "claude-opus-5-5",
        "claude-opus-5-5-20260101",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5",
        "claude-fable-5-1",
        "claude-mythos-5",
        "claude-mythos-5-1",
    ] {
        assert!(supports_system_messages(model), "{model}");
    }
    for model in [
        "claude-sonnet-5",
        "claude-sonnet-5-20260101",
        "claude-opus-4-7",
        "claude-haiku-4-5-20251001",
        "anthropic/claude-opus-5.5",
    ] {
        assert!(!supports_system_messages(model), "{model}");
    }
}

const DEFERRED_TOOLS: MessagesCacheOptions = MessagesCacheOptions {
    anchor: true,
    extended_ttl: false,
    system_messages: true,
    deferred_tools: true,
};

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: Some(format!("{name} tool")),
        parameters: serde_json::json!({"type": "object"}),
    }
}

/// `items` on `model`, offering `offered` and holding `deferred` back.
fn tools_json(
    model: &str,
    items: Vec<ConversationItem>,
    offered: &[&str],
    deferred: &[&str],
    options: MessagesCacheOptions,
) -> serde_json::Value {
    let req = ConversationRequest {
        tools: offered.iter().map(|name| tool_spec(name)).collect(),
        deferred_tools: deferred.iter().map(|name| tool_spec(name)).collect(),
        ..ConversationRequest::from_items(items).with_model(model)
    };
    serde_json::to_value(build_messages_request_with(&req, options)).unwrap()
}

fn without_tool_additions(mut items: Vec<ConversationItem>) -> Vec<ConversationItem> {
    items.retain(|item| !item.is_tool_addition());
    items
}

/// A tool family that joined mid-session used to change the tools array, which opens the cached
/// prefix, so the whole conversation was re-billed. Declared deferred from the first request, the
/// family joins with one `tool_addition` system message after the human turn that needed it: the
/// tools array and every earlier message keep their bytes, and the next round keeps the addition
/// where it was, so it is cached from then on.
#[test]
fn a_tool_family_joining_keeps_the_tools_array_and_the_cached_history() {
    let core = ["edit_file", "read_file"];
    let media = ["generate_image", "edit_image"];
    let ask = || ConversationItem::user("Draw an icon for the app");
    let before = tools_json(
        "claude-opus-5-5",
        finished_turn_then(vec![ask()]),
        &core,
        &media,
        DEFERRED_TOOLS,
    );
    let joined = || finished_turn_then(vec![ask(), ConversationItem::tool_addition(media)]);
    let all = ["edit_file", "read_file", "generate_image", "edit_image"];
    let after = tools_json("claude-opus-5-5", joined(), &all, &[], DEFERRED_TOOLS);
    assert_eq!(after["tools"], before["tools"], "{after:#}");
    let deferred: Vec<&str> = before["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["defer_loading"] == true)
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(deferred, ["edit_image", "generate_image"], "{before:#}");

    let before_messages = before["messages"].as_array().unwrap();
    let after_messages = after["messages"].as_array().unwrap();
    assert_eq!(after_messages.len(), before_messages.len() + 1, "{after:#}");
    assert_eq!(
        &after_messages[..before_messages.len()],
        before_messages.as_slice(),
        "every message before the addition keeps its bytes and breakpoints"
    );
    let addition = after_messages.last().unwrap();
    assert_eq!(addition["role"], "system", "{after:#}");
    assert_eq!(
        addition["content"],
        serde_json::json!([
            {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "generate_image"}},
            {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "edit_image"}},
        ])
    );
    assert_eq!(
        count_cache_control(addition),
        0,
        "no breakpoint on a system message"
    );

    let mut later = joined();
    later.push(ConversationItem::assistant("Here it is."));
    later.push(ConversationItem::user("Thanks"));
    let next = tools_json("claude-opus-5-5", later, &all, &[], DEFERRED_TOOLS);
    assert_eq!(next["tools"], before["tools"]);
    let next_messages = without_cache_control(next["messages"].clone());
    assert_eq!(
        next_messages.as_array().unwrap()[..after_messages.len()],
        without_cache_control(after["messages"].clone())
            .as_array()
            .unwrap()[..],
        "{next:#}"
    );
    assert_eq!(next_messages[after_messages.len()]["role"], "assistant");
}

/// Without the option (another endpoint, or after the API refused deferred tools) or on a model
/// without mid-conversation tool changes, the request is exactly the one before deferred tools:
/// the tools in effect, no deferred tool the model could not use, no addition.
#[test]
fn without_tool_changes_the_request_sends_the_tools_in_effect() {
    let items = finished_turn_then(vec![
        ConversationItem::user("Draw an icon for the app"),
        ConversationItem::tool_addition(["generate_image"]),
    ]);
    let offered = ["generate_image", "read_file"];
    for (model, options) in [
        ("claude-opus-5-5", SYSTEM_MESSAGES),
        ("claude-sonnet-5", DEFERRED_TOOLS),
        ("claude-sonnet-4-5", DEFERRED_TOOLS),
    ] {
        let sent = tools_json(model, items.clone(), &offered, &["schedule_task"], options);
        assert_eq!(
            sent,
            tools_json(
                model,
                without_tool_additions(items.clone()),
                &offered,
                &[],
                options
            ),
            "{model}"
        );
        assert!(
            !sent.to_string().contains("defer_loading")
                && !sent.to_string().contains("schedule_task"),
            "{sent:#}"
        );
    }
}

/// Where an addition has no place a system message can sit (a request that ends on the
/// assistant), or every tool would be deferred (the first one offered would rewrite the prompt's
/// head), the request sends the tools in effect instead of one the API rejects or re-bills.
#[test]
fn tool_additions_fall_back_where_they_cannot_keep_the_cache() {
    let offered = ["generate_image", "read_file"];
    let prefill = finished_turn_then(vec![ConversationItem::tool_addition(["generate_image"])]);
    assert_eq!(
        tools_json(
            "claude-opus-5-5",
            prefill.clone(),
            &offered,
            &[],
            DEFERRED_TOOLS
        ),
        tools_json(
            "claude-opus-5-5",
            without_tool_additions(prefill),
            &offered,
            &[],
            DEFERRED_TOOLS
        )
    );

    let only_joined = finished_turn_then(vec![
        ConversationItem::user("Draw an icon for the app"),
        ConversationItem::tool_addition(["generate_image"]),
    ]);
    let sent = tools_json(
        "claude-opus-5-5",
        only_joined.clone(),
        &["generate_image"],
        &[],
        DEFERRED_TOOLS,
    );
    assert_eq!(
        sent,
        tools_json(
            "claude-opus-5-5",
            without_tool_additions(only_joined),
            &["generate_image"],
            &[],
            DEFERRED_TOOLS
        )
    );
    assert!(!sent.to_string().contains("defer_loading"), "{sent:#}");
}

/// After a resume the history can name a tool the session has not judged needed again: it stays
/// deferred with no addition, so the model never sees a tool the harness would refuse to run. A
/// tool named twice is offered once, where it first joined, and an unknown name is never
/// referenced (the API rejects a reference to an undeclared tool).
#[test]
fn a_tool_addition_offers_only_tools_the_session_offers_and_each_once() {
    let items = finished_turn_then(vec![
        ConversationItem::user("Draw an icon for the app"),
        ConversationItem::tool_addition(["generate_image", "vanished_tool"]),
        ConversationItem::assistant("Done."),
        ConversationItem::user("And a banner"),
        ConversationItem::tool_addition(["generate_image", "edit_image"]),
    ]);
    let resumed = tools_json(
        "claude-opus-5-5",
        items.clone(),
        &["read_file"],
        &["edit_image", "generate_image"],
        DEFERRED_TOOLS,
    );
    assert!(
        resumed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system"),
        "{resumed:#}"
    );
    assert!(
        !resumed.to_string().contains("vanished_tool"),
        "{resumed:#}"
    );

    let joined = tools_json(
        "claude-opus-5-5",
        items,
        &["edit_image", "generate_image", "read_file"],
        &[],
        DEFERRED_TOOLS,
    );
    let additions: Vec<Vec<&str>> = joined["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| {
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|block| block["tool"]["name"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        additions,
        [vec!["generate_image"], vec!["edit_image"]],
        "{joined:#}"
    );
    assert_eq!(
        joined["tools"], resumed["tools"],
        "the same array either way"
    );
}
