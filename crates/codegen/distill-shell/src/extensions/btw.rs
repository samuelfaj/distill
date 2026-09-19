//! `x.ai/btw` extension handler: dispatch a side question to the active session via `SessionCommand::SideQuestion` and return the answer.

use agent_client_protocol as acp;
use tokio::sync::oneshot;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::{SessionCommand, SideQuestionError};

/// Same bound as the pager. A non-pager client must not grow the side-question payload without a limit.
const SIDE_QUESTION_IMAGE_CAP: usize = 50_000_000;

fn estimated_decoded_len(data: &str) -> usize {
    let b64 = data.rsplit(',').next().unwrap_or(data);
    let padding = b64.bytes().rev().take_while(|byte| *byte == b'=').count();
    (b64.len().saturating_mul(3) / 4).saturating_sub(padding)
}

pub fn cap_side_question_images(images: Vec<acp::ImageContent>) -> (Vec<acp::ImageContent>, usize) {
    cap_side_question_images_to(images, SIDE_QUESTION_IMAGE_CAP)
}

fn cap_side_question_images_to(
    images: Vec<acp::ImageContent>,
    cap: usize,
) -> (Vec<acp::ImageContent>, usize) {
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut omitted = 0usize;
    for image in images {
        let size = estimated_decoded_len(&image.data);
        if size > cap || total.saturating_add(size) > cap {
            omitted += 1;
            continue;
        }
        total += size;
        kept.push(image);
    }
    (kept, omitted)
}

pub fn side_question_omit_notice(omitted: usize) -> String {
    format!("{omitted} attached image(s) were not included (over the 50MB side-question limit).")
}

/// Handle `x.ai/btw`, a side question that doesn't interrupt the current turn.
pub(super) async fn handle_btw(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BtwRequest {
        session_id: String,
        question: String,
        /// Optional text + image blocks. Absent = legacy text-only wire.
        #[serde(default)]
        content: Vec<acp::ContentBlock>,
    }

    let req: BtwRequest = parse_params(args)?;
    let sid: acp::SessionId = req.session_id.clone().into();
    let session_handle = agent.resident_handle(&sid);
    let Some(session) = session_handle else {
        return Err(
            acp::Error::invalid_params().data(format!("session not found: {}", req.session_id))
        );
    };
    let (text_override, images) = super::content::split_content(req.content);
    let (images, omitted) = cap_side_question_images(images);
    let mut question = text_override.unwrap_or(req.question);
    if omitted > 0 {
        question.push_str("\n\n");
        question.push_str(&side_question_omit_notice(omitted));
    }
    let (tx, rx) = oneshot::channel();
    let _ = session.cmd_tx.send(SessionCommand::SideQuestion {
        question,
        images,
        respond_to: tx,
    });
    let result = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    match result {
        Ok(answer) => super::to_ext_response(Ok(serde_json::json!({
            "answer": answer,
        }))),
        // Model errors take the canonical mapping: overload gets its short display copy there
        // Rate limits keep the typed code and upgrade copy, and auth failures map to auth_required
        Err(SideQuestionError::Sampling(e)) => {
            Err(crate::sampling::error::map_sampling_err_to_acp(e))
        }
        // Non-model failures are already readable sentences. Set `message` and leave `data` unset.
        // `Display` appends JSON-encoded `data`, so `internal_error().data(e)` rendered as `Internal error: "…"`
        // That made capacity failures look like client bugs in the TUI
        Err(e) => Err(acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            e.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_question_cap_keeps_one_small_image_and_drops_the_rest() {
        let small = acp::ImageContent::new("aGVsbG8=", "image/png");
        let huge = acp::ImageContent::new("A".repeat(32), "image/png");
        let (kept, omitted) = cap_side_question_images_to(vec![small, huge], 8);
        assert_eq!(kept.len(), 1);
        assert_eq!(omitted, 1);
        assert!(side_question_omit_notice(omitted).contains("not included"));
    }

    #[test]
    fn estimated_decoded_len_does_not_underflow_on_padding() {
        assert_eq!(estimated_decoded_len(""), 0);
        assert_eq!(estimated_decoded_len("="), 0);
        assert_eq!(estimated_decoded_len("=="), 0);
        assert_eq!(estimated_decoded_len("===="), 0);
        assert_eq!(estimated_decoded_len("aGVsbG8="), 5);
    }
}
