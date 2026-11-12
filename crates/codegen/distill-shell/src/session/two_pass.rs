// Modified for Distill by Samuel Fajreldines, 2026.
//! Pure builders for prefire two-pass compaction (shell-only).
//!
//! Pass1 summarizes the history as the main request sends it into NOTE₁.
//! Pass2 rewrites NOTE₁ and the tail added since into NOTE₂, the note the successor sees.
//! Sampling lives in [`super::compaction`]; this module has no I/O.

use distill_chat_state::compaction_utils::format_compact_summary_content;
use distill_sampling_types::ConversationItem;

/// Minimum char length for a closed `<summary>` block to be preferred as NOTE₁ over the full pass1 response.
const TWO_PASS_MIN_SUMMARY_BLOCK_CHARS: usize = 1000;

/// Cap on NOTE₁ text embedded in pass2.
const TWO_PASS_MAX_NOTE1_CHARS: usize = 60_000;

fn extract_summary_block(text: &str, min_chars: usize) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let open = "<summary>";
    let close = "</summary>";
    let lower = text.to_ascii_lowercase();
    let mut blocks: Vec<String> = Vec::new();
    let mut search_from = 0usize;
    let text_bytes = text.as_bytes();
    let lower_bytes = lower.as_bytes();
    while search_from < lower_bytes.len() {
        let Some(rel) = lower_bytes
            .get(search_from..)
            .and_then(|hay| find_bytes(hay, open.as_bytes()))
        else {
            break;
        };
        let start = search_from + rel + open.len();
        let Some(rel_close) = lower_bytes
            .get(start..)
            .and_then(|hay| find_bytes(hay, close.as_bytes()))
        else {
            break;
        };
        let end = start + rel_close;
        let inner = text_bytes
            .get(start..end)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("");
        blocks.push(inner.to_string());
        search_from = end + close.len();
    }
    for block in blocks.into_iter().rev() {
        let stripped = block.trim();
        if stripped.chars().count() > min_chars {
            return Some(stripped.to_string());
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Prefer a substantive `<summary>` inner for NOTE₁; otherwise the full pass1 response.
pub(crate) fn note_for_two_pass_pass2(pass1_raw: &str) -> String {
    let mut note = extract_summary_block(pass1_raw, TWO_PASS_MIN_SUMMARY_BLOCK_CHARS)
        .unwrap_or_else(|| pass1_raw.trim().to_string());
    let n = note.chars().count();
    if n > TWO_PASS_MAX_NOTE1_CHARS {
        note = note.chars().take(TWO_PASS_MAX_NOTE1_CHARS).collect();
        note.push_str("\n\n[… NOTE₁ truncated for pass2 input budget …]");
    }
    note
}

/// Pass1 sample history: `prefix` followed by the compaction instruction as a user turn.
pub(crate) fn build_two_pass_pass1_history(
    prefix: &[ConversationItem],
    compaction_prompt: &str,
) -> Vec<ConversationItem> {
    let mut history = prefix.to_vec();
    history.push(ConversationItem::user(compaction_prompt.to_string()));
    history
}

/// Pass2 sample history: the system turns from `prefix`, then the NOTE₁ carrier, the tail, and the compaction instruction.
///
/// The successor sees only the model output of *this* history (NOTE₂).
pub(crate) fn build_two_pass_pass2_history(
    prefix: &[ConversationItem],
    tail: &[ConversationItem],
    note1: &str,
    compaction_prompt: &str,
) -> Vec<ConversationItem> {
    let mut history: Vec<ConversationItem> = prefix
        .iter()
        .filter(|item| matches!(item, ConversationItem::System(_)))
        .cloned()
        .collect();
    if history.is_empty() {
        history.push(ConversationItem::system("You are a helpful assistant."));
    }

    history.push(ConversationItem::user_meta(format_compact_summary_content(
        note1,
    )));
    history.extend(tail.iter().cloned());
    history.push(ConversationItem::user(compaction_prompt));
    history
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_prefers_summary_block_and_caps_huge_raw() {
        let text = format!(
            "<summary>short</summary>\n<summary>\n{}\n</summary>",
            "x".repeat(1001)
        );
        assert_eq!(note_for_two_pass_pass2(&text), "x".repeat(1001));
        assert_eq!(note_for_two_pass_pass2("no tags"), "no tags");
        let huge = "n".repeat(TWO_PASS_MAX_NOTE1_CHARS + 500);
        let note = note_for_two_pass_pass2(&huge);
        assert!(note.chars().count() <= TWO_PASS_MAX_NOTE1_CHARS + 80);
        assert!(note.contains("truncated"));
    }
}
