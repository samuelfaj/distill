//! `x.ai/interject` extension handler.
//!
//! Queues a mid-turn interjection into the active session's pending interjection buffer.
//! The session actor drains it at the next safe point in `process_conversation_turn`.

use agent_client_protocol as acp;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::SessionCommand;

pub const INTERJECT_METHOD: &str = "x.ai/interject";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterjectRequest {
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub interjection_id: Option<String>,
    /// Optional structured blocks (text and images) from image-capable clients.
    /// Absent means the legacy text-only wire shape (empty after default).
    #[serde(default)]
    pub content: Vec<acp::ContentBlock>,
}

/// Handle `x.ai/interject`: queue a mid-turn user interjection.
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: InterjectRequest = parse_params(args)?;
    let sid: acp::SessionId = req.session_id.clone().into();
    // An interjection racing a reconnect-replayed `session/load` (leader restart) waits for the load instead of failing
    let session_handle = agent.session_handle_waiting_for_load(&sid).await;
    let Some(session) = session_handle else {
        return Err(
            acp::Error::invalid_params().data(format!("session not found: {}", req.session_id))
        );
    };

    let (text_override, images) = super::content::split_content(req.content);
    let _ = session.cmd_tx.send(SessionCommand::Interject {
        text: text_override.unwrap_or(req.text),
        id: req.interjection_id,
        images,
    });

    super::to_ext_response(Ok(serde_json::json!({
        "status": "queued",
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy wire shape (no `content`) parses byte-identically: text-only, zero images, no text override.
    #[test]
    fn parse_without_content_is_legacy_text_only() {
        let req: InterjectRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "text": "steer left",
            "interjectionId": "i1",
        }))
        .expect("legacy params must parse");
        assert_eq!(req.text, "steer left");
        assert_eq!(req.interjection_id.as_deref(), Some("i1"));
        let (text_override, images) = super::super::content::split_content(req.content);
        assert_eq!(text_override, None);
        assert!(images.is_empty());
    }

    /// `content` with text and image blocks parses; the images are extracted.
    /// The Text block (the client's rewritten, path-stripped text) overrides the raw `text` param.
    #[test]
    fn parse_with_content_extracts_images_and_prefers_block_text() {
        let req: InterjectRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "text": "look at [Image #1: /tmp/x.png]",
            "content": [
                { "type": "text", "text": "look at [Image #1]" },
                { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
            ],
        }))
        .expect("content params must parse");
        let (text_override, images) = super::super::content::split_content(req.content);
        assert_eq!(
            text_override.as_deref(),
            Some("look at [Image #1]"),
            "rewritten block text must win over the raw text param"
        );
        assert_eq!(images.len(), 1);
        let Some(image) = images.first() else {
            panic!("expected one image: {images:?}");
        };
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "aGVsbG8=");
    }

    /// Garbage `content` fails the whole parse (strict, like other params) instead of silently dropping attachments.
    #[test]
    fn parse_with_garbage_content_is_an_error() {
        let result: Result<InterjectRequest, _> = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "text": "steer",
            "content": "not an array",
        }));
        assert!(result.is_err(), "garbage content must be rejected");
    }
}
