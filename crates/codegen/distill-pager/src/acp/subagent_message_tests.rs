// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageOutput;
use distill_tools::tool_taxonomy::{CanonicalToolMeta, TOOL_META_KEY, ToolIdentity};
use distill_tools::types::output::ToolOutput;
use distill_tools::types::tool::{ToolKind, ToolNamespace};

use super::*;
use crate::acp::meta::NotificationMeta;
use crate::acp::tracker::AcpUpdateTracker;
use crate::scrollback::blocks::tool::{SentMessagePresentation, SentMessageTarget, ToolCallBlock};
use crate::scrollback::state::ScrollbackState;

const LABEL: &str = "Explore \u{201c}find callers\u{201d}";
/// Distinct from every subagent id below: the registry is keyed by the tool's id, not the child session.
const CHILD_SID: &str = "child-sid";

fn raw_output(output: SendSubagentMessageOutput) -> serde_json::Value {
    serde_json::to_value(ToolOutput::SendSubagentMessage(output)).expect("serialize tool output")
}

fn canonical_meta(name: &str, kind: ToolKind) -> acp::Meta {
    canonical_meta_with_version(name, kind, TOOL_META_VERSION)
}

fn canonical_meta_with_version(name: &str, kind: ToolKind, version: u32) -> acp::Meta {
    let identity = ToolIdentity {
        tool_kind: kind,
        namespace: ToolNamespace::Distill,
        presentation_name: kind.presentation_name(),
        read_only: false,
    };
    let mut meta = CanonicalToolMeta::new(name, &identity, None);
    meta.version = version;
    let mut object = serde_json::Map::new();
    object.insert(
        TOOL_META_KEY.to_owned(),
        serde_json::to_value(meta).expect("serialize canonical metadata"),
    );
    object
}

fn call_with_id(
    id: &str,
    status: acp::ToolCallStatus,
    input: Option<serde_json::Value>,
    output: Option<SendSubagentMessageOutput>,
) -> acp::ToolCall {
    call_with_identity(id, SEND_SUBAGENT_MESSAGE_TOOL_NAME, status, input, output)
}

fn call_with_identity(
    id: &str,
    wire_name: &str,
    status: acp::ToolCallStatus,
    input: Option<serde_json::Value>,
    output: Option<SendSubagentMessageOutput>,
) -> acp::ToolCall {
    acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id.to_owned())), wire_name)
        .kind(acp::ToolKind::Other)
        .status(status)
        .raw_input(input)
        .raw_output(output.map(raw_output))
        .meta(Some(canonical_meta(
            wire_name,
            ToolKind::ActiveAgentMessage,
        )))
}

fn call(
    status: acp::ToolCallStatus,
    input: Option<serde_json::Value>,
    output: Option<SendSubagentMessageOutput>,
) -> acp::ToolCall {
    call_with_id("send-message", status, input, output)
}

fn with_content(mut call: acp::ToolCall, content: &str) -> acp::ToolCall {
    call.content = vec![acp::ToolCallContent::Content(acp::Content::new(
        acp::ContentBlock::Text(acp::TextContent::new(content)),
    ))];
    call
}

fn input(subagent_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"subagent_id": subagent_id, "text": text})
}

fn block(tool_call: &acp::ToolCall) -> SentMessageToolCallBlock {
    block_with(&SubagentLabelRegistry::default(), tool_call)
}

fn block_with(
    labels: &SubagentLabelRegistry,
    tool_call: &acp::ToolCall,
) -> SentMessageToolCallBlock {
    let RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)) = to_block(tool_call, labels)
    else {
        panic!("expected dedicated sent-message block");
    };
    block
}

fn labels_for(subagent_id: &str) -> SubagentLabelRegistry {
    let mut labels = SubagentLabelRegistry::default();
    labels.record(subagent_id, LABEL, CHILD_SID);
    labels
}

/// The input as it reads through an empty registry: raw id, default delivery.
fn steer_input(subagent_id: &str, text: &str) -> Option<SentMessageInput> {
    unresolved_input(subagent_id, Some(SentMessageDelivery::Steer), text)
}

fn unresolved_input(
    subagent_id: &str,
    delivery: Option<SentMessageDelivery>,
    text: &str,
) -> Option<SentMessageInput> {
    Some(SentMessageInput {
        target: SentMessageTarget::Unresolved {
            subagent_id: subagent_id.to_owned(),
        },
        delivery,
        text: text.to_owned(),
    })
}

fn named_input(delivery: SentMessageDelivery, text: &str) -> Option<SentMessageInput> {
    Some(SentMessageInput {
        target: SentMessageTarget::Named {
            label: Arc::from(LABEL),
            child_session_id: Arc::from(CHILD_SID),
        },
        delivery: Some(delivery),
        text: text.to_owned(),
    })
}

fn sent_block(scrollback: &ScrollbackState) -> &SentMessageToolCallBlock {
    let RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)) =
        &scrollback.get(0).expect("message entry").block
    else {
        panic!("expected dedicated sent-message block");
    };
    block
}

#[test]
fn direct_and_enveloped_wire_inputs_preserve_exact_arguments() {
    for raw in [
        input("sub-123", "follow up"),
        serde_json::json!({
            "variant": "SendSubagentMessage",
            "subagent_id": "sub-123",
            "text": "follow up",
        }),
    ] {
        let block = block(&call(
            acp::ToolCallStatus::Completed,
            Some(raw),
            Some(SendSubagentMessageOutput::Accepted {
                message_id: "message-1".into(),
            }),
        ));
        assert_eq!(block.input, steer_input("sub-123", "follow up"));
    }
}

/// An explicit `delivery` wins over the legacy `queue` flag, as it does in the tool.
#[test]
fn wire_delivery_and_registry_select_the_verb_and_target() {
    let labels = labels_for("sub-123");
    for (raw, expected_input, expected_header) in [
        (
            serde_json::json!({
                "subagent_id": "sub-123",
                "text": "follow up",
                "delivery": "interject",
                "queue": true,
            }),
            named_input(SentMessageDelivery::Interject, "follow up"),
            format!("Message interjected to {LABEL}"),
        ),
        (
            serde_json::json!({
                "subagent_id": "sub-123",
                "text": "follow up",
                "queue": true,
            }),
            named_input(SentMessageDelivery::Queue, "follow up"),
            format!("Message queued for {LABEL}"),
        ),
        (
            input("sub-123", "follow up"),
            named_input(SentMessageDelivery::Steer, "follow up"),
            format!("Message sent to {LABEL}"),
        ),
        (
            input("other-01", "follow up"),
            steer_input("other-01", "follow up"),
            "Message sent to subagent other-01".to_owned(),
        ),
    ] {
        let block = block_with(
            &labels,
            &call(
                acp::ToolCallStatus::Completed,
                Some(raw),
                Some(SendSubagentMessageOutput::Accepted {
                    message_id: "message-1".into(),
                }),
            ),
        );
        let header = block.header_text();
        assert_eq!((expected_input, expected_header), (block.input, header));
    }
}

#[test]
fn unknown_delivery_value_still_renders_id_and_text() {
    for delivery in [
        serde_json::json!("interrupt_and_send"),
        serde_json::json!(7),
    ] {
        let block = block(&call(
            acp::ToolCallStatus::Completed,
            Some(serde_json::json!({
                "subagent_id": "sub-123",
                "text": "follow up",
                "delivery": delivery,
            })),
            Some(SendSubagentMessageOutput::Accepted {
                message_id: "message-1".into(),
            }),
        ));
        let header = block.header_text();
        assert_eq!(
            (
                unresolved_input("sub-123", None, "follow up"),
                "Message sent to subagent sub-123".to_owned()
            ),
            (block.input, header),
            "{delivery}",
        );
    }
}

/// The alias is matched on the untrimmed wire value, as the shell matches it, before any registry lookup.
#[test]
fn parent_literal_is_never_resolved() {
    let labels = labels_for("parent");
    for (wire_id, expected_input) in [
        (
            "parent",
            Some(SentMessageInput {
                target: SentMessageTarget::Parent,
                delivery: Some(SentMessageDelivery::Steer),
                text: "follow up".to_owned(),
            }),
        ),
        ("Parent", steer_input("Parent", "follow up")),
        // Trimmed to `parent`, which is an id this registry happens to know
        (
            " parent ",
            named_input(SentMessageDelivery::Steer, "follow up"),
        ),
    ] {
        let block = block_with(
            &labels,
            &call(
                acp::ToolCallStatus::Completed,
                Some(input(wire_id, "follow up")),
                Some(SendSubagentMessageOutput::Accepted {
                    message_id: "message-1".into(),
                }),
            ),
        );
        assert_eq!(expected_input, block.input, "{wire_id:?}");
    }
}

#[test]
fn wire_id_is_trimmed_and_a_blank_or_missing_id_still_renders() {
    for (raw, subagent_id) in [
        (input("ag1.deadbeefcafe", "follow up"), "ag1.deadbeefcafe"),
        (input("  sub-1234\n", "follow up"), "sub-1234"),
        (input("   ", "follow up"), ""),
        (serde_json::json!({"text": "follow up"}), ""),
    ] {
        let block = block(&call(
            acp::ToolCallStatus::Completed,
            Some(raw),
            Some(SendSubagentMessageOutput::Accepted {
                message_id: "message-1".into(),
            }),
        ));
        assert_eq!(
            steer_input(subagent_id, "follow up"),
            block.input,
            "{subagent_id:?}"
        );
    }
}

#[test]
fn wire_input_is_preserved_without_admission_revalidation() {
    let oversize = "x".repeat(
        distill_tools::implementations::distill::task::types::MAX_ACTIVE_AGENT_MESSAGE_BYTES + 1,
    );
    for (subagent_id, text) in [("null", "hello"), ("sub-123", ""), ("sub-123", &oversize)] {
        let block = block(&call(
            acp::ToolCallStatus::Failed,
            Some(input(subagent_id, text)),
            Some(SendSubagentMessageOutput::Limit {
                max_bytes: 32 * 1024,
                observed_bytes: text.len(),
            }),
        ));
        assert_eq!(block.input, steer_input(subagent_id, text));
        assert!(matches!(
            block.presentation,
            SentMessagePresentation::Rejected { .. }
        ));
    }
}

#[test]
fn typed_outcomes_map_exhaustively_to_delivery_presentations() {
    use SendSubagentMessageOutput as Output;

    for (outcome, expected) in [
        (
            Output::Accepted {
                message_id: String::new(),
            },
            "Message sent to subagent sub-123",
        ),
        (
            Output::NotFoundOrNotOwned,
            "Message rejected \u{b7} subagent sub-123",
        ),
        (
            Output::NotActiveOrFinalizing,
            "Message rejected \u{b7} subagent sub-123",
        ),
        (
            Output::Saturated { max_in_flight: 8 },
            "Message rejected \u{b7} subagent sub-123",
        ),
        (
            Output::QuotaExceeded {
                kind: distill_tools::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
                limit: 4,
            },
            "Message rejected \u{b7} subagent sub-123",
        ),
        (Output::Unsupported, "Message rejected \u{b7} subagent sub-123"),
        (
            Output::Limit {
                max_bytes: 8,
                observed_bytes: 9,
            },
            "Message rejected \u{b7} subagent sub-123",
        ),
        (
            Output::AdmissionUncertain,
            "Message unconfirmed \u{b7} subagent sub-123",
        ),
        (
            Output::NotAcceptedBeforeDeadline,
            "Message rejected \u{b7} subagent sub-123",
        ),
        (Output::ChannelClosed, "Message rejected \u{b7} subagent sub-123"),
    ] {
        let block = block(&call(
            acp::ToolCallStatus::Failed,
            Some(input("sub-123", "follow up")),
            Some(outcome),
        ));
        assert_eq!(block.header_text(), expected);
    }
}

#[test]
fn pending_and_terminal_without_typed_output_use_conservative_fallbacks() {
    for output in [
        None,
        Some(SendSubagentMessageOutput::Accepted {
            message_id: "premature".into(),
        }),
    ] {
        let pending = block(&call(
            acp::ToolCallStatus::Pending,
            Some(input("sub-123", "follow up")),
            output,
        ));
        assert_eq!(pending.presentation, SentMessagePresentation::Sending);
    }

    for (id, call, expected_reason) in [
        (
            "malformed",
            with_content(
                call_with_id(
                    "malformed",
                    acp::ToolCallStatus::Failed,
                    Some(serde_json::json!({"subagent_id": 7, "text": "hello"})),
                    None,
                ),
                "Permission denied before execution",
            ),
            "Permission denied before execution",
        ),
        (
            "unavailable",
            call_with_id("unavailable", acp::ToolCallStatus::Completed, None, None),
            "Message was not accepted or delivery details are unavailable.",
        ),
    ] {
        let fallback = block(&call);
        assert_eq!(fallback.input, None, "{id}");
        assert_eq!(
            fallback.presentation,
            SentMessagePresentation::Rejected {
                reason: expected_reason.into(),
            },
            "{id}",
        );
    }
}

#[test]
fn recognition_uses_canonical_kind_and_exact_metadata_absent_legacy_title() {
    let aliased = acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("aliased")),
        "relay_to_subagent",
    )
    .meta(Some(canonical_meta(
        "relay_to_subagent",
        ToolKind::ActiveAgentMessage,
    )));
    assert!(is_tool(&aliased));

    let contradictory = acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("contradictory")),
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
    )
    .meta(Some(canonical_meta(
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
        ToolKind::Read,
    )));
    assert!(!is_tool(&contradictory));

    let mut malformed = serde_json::Map::new();
    malformed.insert(
        TOOL_META_KEY.to_owned(),
        serde_json::json!({"kind": "active_agent_message"}),
    );
    let malformed_metadata = acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("malformed")),
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
    )
    .meta(Some(malformed));
    assert!(!is_tool(&malformed_metadata));

    let unsupported_version = acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("unsupported-version")),
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
    )
    .meta(Some(canonical_meta_with_version(
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
        ToolKind::ActiveAgentMessage,
        TOOL_META_VERSION + 1,
    )));
    assert!(!is_tool(&unsupported_version));

    let legacy = acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("legacy")),
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
    );
    assert!(is_tool(&legacy));
    assert!(!is_tool(&acp::ToolCall::new(
        acp::ToolCallId::new(Arc::from("lookalike")),
        "prefix_send_subagent_message",
    )));
}

#[test]
fn legacy_serialized_outputs_replay_with_current_truthful_classification() {
    for (id, legacy_output, expected) in [
        (
            "legacy-accepted",
            serde_json::json!({
                "type": "SendAgentMessage",
                "outcome": "accepted",
                "message_id": "message-1",
            }),
            SentMessagePresentation::Sent,
        ),
        (
            "legacy-channel-closed",
            serde_json::json!({
                "type": "SendAgentMessage",
                "outcome": "channel_closed",
            }),
            SentMessagePresentation::Rejected {
                reason: SendSubagentMessageOutput::ChannelClosed.to_string(),
            },
        ),
    ] {
        let mut call = call_with_id(
            id,
            acp::ToolCallStatus::Completed,
            Some(serde_json::json!({
                "variant": "SendAgentMessage",
                "subagent_id": "sub-123",
                "text": "follow up",
            })),
            None,
        );
        call.raw_output = Some(legacy_output);
        let block = block(&call);
        assert_eq!(block.presentation, expected, "{id}");
        assert_eq!(block.input, steer_input("sub-123", "follow up"), "{id}");
    }
}

#[test]
fn pager_does_not_route_unsupported_canonical_metadata_version() {
    let mut call = call_with_id(
        "unsupported-version",
        acp::ToolCallStatus::Completed,
        Some(input("sub-123", "follow up")),
        Some(SendSubagentMessageOutput::Accepted {
            message_id: "message-1".into(),
        }),
    );
    call.meta = Some(canonical_meta_with_version(
        SEND_SUBAGENT_MESSAGE_TOOL_NAME,
        ToolKind::ActiveAgentMessage,
        TOOL_META_VERSION + 1,
    ));
    let mut tracker = AcpUpdateTracker::new();
    let mut scrollback = ScrollbackState::new();
    tracker.handle_update(
        acp::SessionUpdate::ToolCall(call),
        &NotificationMeta::default(),
        &mut scrollback,
    );

    assert!(matches!(
        &scrollback.get(0).expect("unsupported metadata entry").block,
        RenderBlock::ToolCall(ToolCallBlock::Other(_))
    ));
}

#[test]
fn pager_routes_exact_metadata_absent_legacy_title_to_the_dedicated_block() {
    let mut legacy = call_with_id(
        "legacy-terminal",
        acp::ToolCallStatus::Completed,
        Some(input("sub-123", "follow up")),
        Some(SendSubagentMessageOutput::Accepted {
            message_id: "message-1".into(),
        }),
    );
    legacy.meta = None;
    let mut tracker = AcpUpdateTracker::new();
    let mut scrollback = ScrollbackState::new();
    tracker.handle_update(
        acp::SessionUpdate::ToolCall(legacy),
        &NotificationMeta::default(),
        &mut scrollback,
    );

    assert!(matches!(
        &scrollback.get(0).expect("legacy terminal entry").block,
        RenderBlock::ToolCall(ToolCallBlock::SentMessage(_))
    ));
}

#[test]
fn pager_routes_canonical_kind_with_aliased_wire_name_to_the_dedicated_block() {
    let mut tracker = AcpUpdateTracker::new();
    let mut scrollback = ScrollbackState::new();
    tracker.handle_update(
        acp::SessionUpdate::ToolCall(call_with_identity(
            "aliased-terminal",
            "relay_to_subagent",
            acp::ToolCallStatus::Completed,
            Some(input("sub-123", "follow up")),
            Some(SendSubagentMessageOutput::Accepted {
                message_id: "message-1".into(),
            }),
        )),
        &NotificationMeta::default(),
        &mut scrollback,
    );

    assert!(matches!(
        &scrollback.get(0).expect("aliased terminal entry").block,
        RenderBlock::ToolCall(ToolCallBlock::SentMessage(_))
    ));
}

#[test]
fn pager_routes_every_terminal_category_to_the_dedicated_block() {
    use SendSubagentMessageOutput as Output;

    let mut tracker = AcpUpdateTracker::new();
    let mut scrollback = ScrollbackState::new();
    let outputs = [
        Output::Accepted {
            message_id: "message-1".into(),
        },
        Output::NotFoundOrNotOwned,
        Output::NotActiveOrFinalizing,
        Output::Saturated { max_in_flight: 8 },
        Output::QuotaExceeded {
            kind: distill_tools::implementations::distill::task::types::ActiveAgentMessageQuotaKind::AttemptOutbound,
            limit: 32,
        },
        Output::Unsupported,
        Output::Limit {
            max_bytes: 8,
            observed_bytes: 9,
        },
        Output::AdmissionUncertain,
        Output::NotAcceptedBeforeDeadline,
        Output::ChannelClosed,
    ];
    let expected_count = outputs.len();
    for (index, output) in outputs.into_iter().enumerate() {
        tracker.handle_update(
            acp::SessionUpdate::ToolCall(call_with_id(
                &format!("terminal-{index}"),
                acp::ToolCallStatus::Completed,
                Some(input("sub-123", "follow up")),
                Some(output),
            )),
            &NotificationMeta::default(),
            &mut scrollback,
        );
    }

    assert_eq!(scrollback.len(), expected_count);
    for index in 0..scrollback.len() {
        assert!(matches!(
            &scrollback.get(index).expect("terminal entry").block,
            RenderBlock::ToolCall(ToolCallBlock::SentMessage(_))
        ));
    }
}

#[test]
fn sent_row_uses_label_from_registry_and_survives_refinement() {
    let mut tracker = AcpUpdateTracker::new();
    *tracker.subagent_labels.borrow_mut() = labels_for("sub-123");
    let mut scrollback = ScrollbackState::new();
    // The eager Pending call carries no input yet, exactly like the shell's first ToolCall
    tracker.handle_update(
        acp::SessionUpdate::ToolCall(call(acp::ToolCallStatus::Pending, None, None)),
        &NotificationMeta::default(),
        &mut scrollback,
    );
    assert_eq!(sent_block(&scrollback).input, None);
    assert_eq!(
        sent_block(&scrollback).header_text(),
        "Message sending to subagent"
    );

    let update = |fields: acp::ToolCallUpdateFields| {
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("send-message")),
            fields,
        ))
    };
    tracker.handle_update(
        update(
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_input(Some(input("sub-123", "follow up"))),
        ),
        &NotificationMeta::default(),
        &mut scrollback,
    );
    let expected_input = named_input(SentMessageDelivery::Steer, "follow up");
    assert_eq!(sent_block(&scrollback).input, expected_input);
    assert_eq!(
        sent_block(&scrollback).header_text(),
        format!("Message sending to {LABEL}")
    );

    tracker.handle_update(
        update(
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::Completed))
                .raw_output(Some(raw_output(SendSubagentMessageOutput::Accepted {
                    message_id: "message-1".into(),
                }))),
        ),
        &NotificationMeta::default(),
        &mut scrollback,
    );
    let block = sent_block(&scrollback);
    assert_eq!(block.presentation, SentMessagePresentation::Sent);
    assert_eq!(block.input, expected_input);
    assert_eq!(block.header_text(), format!("Message sent to {LABEL}"));
    assert_eq!(Some(CHILD_SID), block.child_session_id());
}
