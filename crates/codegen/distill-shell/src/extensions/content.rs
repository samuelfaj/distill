//! Shared split of optional client `content` blocks.
//!
//! Interject and `/btw` both accept a text override plus image blocks. The
//! rule lives here so a side question does not inherit interjection-only behavior.

use agent_client_protocol as acp;

/// Text block (when present and non-empty) is the client's rewritten text and wins over the raw string param.
/// Image blocks are returned separately. Absent or empty content is the legacy text-only path.
pub fn split_content(content: Vec<acp::ContentBlock>) -> (Option<String>, Vec<acp::ImageContent>) {
    let text_override = content.iter().find_map(|block| match block {
        acp::ContentBlock::Text(text) if !text.text.trim().is_empty() => Some(text.text.clone()),
        _ => None,
    });
    (text_override, crate::session::image_blocks(content))
}
