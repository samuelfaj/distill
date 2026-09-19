use std::pin::pin;

use agent_client_protocol as acp;
use futures_util::FutureExt as _;
use serde_json::json;

use super::{Transcript, TranscriptEntry};
use crate::acp_ask_user_question::ASK_USER_QUESTION_METHOD;

fn agent_text_chunk(text: &str) -> TranscriptEntry {
    TranscriptEntry::SessionUpdate(acp::SessionNotification::new(
        "s1",
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    ))
}

#[test]
fn agent_text_joins_only_agent_message_text_chunks_in_arrival_order() {
    let transcript = Transcript::default();
    transcript.record(agent_text_chunk("Hello"));
    transcript.record(TranscriptEntry::SessionUpdate(
        acp::SessionNotification::new(
            "s1",
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new("thinking"),
            ))),
        ),
    ));
    transcript.record(agent_text_chunk(""));
    transcript.record(agent_text_chunk(", world"));

    assert_eq!("Hello, world", transcript.agent_text());
}

#[test]
fn session_update_count_excludes_extension_traffic_and_round_trips() {
    let transcript = Transcript::default();
    transcript.record(agent_text_chunk("a"));
    transcript.record(TranscriptEntry::ExtNotification {
        method: "x.ai/session_notification".to_owned(),
        params: json!({}),
    });
    transcript.record(TranscriptEntry::ExtRequest {
        method: ASK_USER_QUESTION_METHOD.to_owned(),
        params: json!({}),
        reply: json!({ "outcome": "cancelled" }),
    });
    transcript.record(TranscriptEntry::PermissionRequest {
        request: acp::RequestPermissionRequest::new(
            "s1",
            acp::ToolCallUpdate::new("tc1", acp::ToolCallUpdateFields::default()),
            vec![],
        ),
        outcome: acp::RequestPermissionOutcome::Cancelled,
    });
    transcript.record(agent_text_chunk("b"));

    assert_eq!(2, transcript.session_update_count());
}

#[tokio::test]
async fn wait_until_resolves_once_the_recorded_entries_satisfy_the_predicate() {
    let transcript = Transcript::default();
    let mut waiting = pin!(transcript.wait_until(|entries| {
        entries
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::ExtNotification { .. }))
    }));
    transcript.record(agent_text_chunk("a"));

    assert_eq!(None, waiting.as_mut().now_or_never());

    transcript.record(TranscriptEntry::ExtNotification {
        method: "x.ai/session_notification".to_owned(),
        params: json!({}),
    });

    assert_eq!(Some(()), waiting.now_or_never());
}
