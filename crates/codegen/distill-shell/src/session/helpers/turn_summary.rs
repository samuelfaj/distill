// Modified for Distill by Samuel Fajreldines, 2026.
//! After each turn the shell generates an ultra-short one-line summary of the agent's reply for that turn (not a meta activity log).
//! The dashboard row shows it as its secondary line.
//! Like recap, it is display-only and never mutates the conversation.
//! Its auxiliary source is only the last real user turn and the visible
//! assistant reply; tools, reasoning, and older turns are deliberately absent.

use crate::sampling::ConversationItem;
use crate::session::helpers::{chat::floor_char_boundary, session_summary};

/// The instruction targets 5-12 words; this only guards against runaway output.
/// Rows truncate to width on render.
pub(crate) const TURN_SUMMARY_MAX_CHARS: usize = 200;

/// Max characters of the user message quoted in the instruction as the last-turn anchor.
const ANCHOR_MAX_CHARS: usize = 120;
const DISPLAY_USER_MAX_BYTES: usize = 1_200;
const DISPLAY_ASSISTANT_MAX_BYTES: usize = 2_600;
const DISPLAY_PAYLOAD_MAX_BYTES: usize = 4_000;

/// The conversation contains user-role turns the user never wrote (reminders, injected context).
/// Angle brackets are dropped so the quote cannot close the instruction's reminder tag.
/// `None` when no real user message with text exists (caller should skip generation).
pub(crate) fn last_user_anchor(conversation: &[ConversationItem]) -> Option<String> {
    let text = conversation.iter().rev().find_map(|item| match item {
        ConversationItem::User(u) if u.synthetic_reason.is_human() => {
            let text = item.text_content();
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    })?;
    let mut anchor: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| *c != '<' && *c != '>')
        .collect();
    if anchor.len() > ANCHOR_MAX_CHARS {
        let cut = floor_char_boundary(&anchor, ANCHOR_MAX_CHARS);
        anchor.truncate(cut);
        anchor = anchor.trim_end().to_string();
        anchor.push('\u{2026}');
    }
    Some(anchor)
}

/// Build the bounded source for the dashboard summary. Only the most recent
/// real user message and the latest visible assistant reply participate; tool
/// results and reasoning siblings are not display content.
pub(crate) fn last_turn_display_source(conversation: &[ConversationItem]) -> Option<String> {
    let user_index = conversation
        .iter()
        .rposition(ConversationItem::is_human_user_turn)?;
    conversation
        .iter()
        .skip(user_index + 1)
        .rev()
        .find_map(|item| {
            let ConversationItem::Assistant(assistant) = item else {
                return None;
            };
            (!assistant.content.trim().is_empty()).then(|| {
                session_summary::bounded_display_text(
                    assistant.content.as_ref(),
                    DISPLAY_ASSISTANT_MAX_BYTES,
                )
            })
        })?
}

pub(crate) fn last_turn_display_payload(conversation: &[ConversationItem]) -> Option<String> {
    let user_index = conversation
        .iter()
        .rposition(ConversationItem::is_human_user_turn)?;
    let user = session_summary::bounded_display_text(
        &conversation[user_index].text_content(),
        DISPLAY_USER_MAX_BYTES,
    )?;
    let assistant = last_turn_display_source(conversation)?;
    let payload = format!("LAST USER TURN:\n{user}\nASSISTANT REPLY:\n{assistant}");
    (payload.len() <= DISPLAY_PAYLOAD_MAX_BYTES).then_some(payload)
}

/// Same single-user-message design as recap (`recap_instruction`): all directions live in one reminder-wrapped turn.
/// The conversation prefix is then reused verbatim, so the prompt cache stays warm.
/// Few-shots must stay synthetic: never embed real eval/session content.
pub(crate) fn turn_summary_instruction(tag: &str, anchor: &str) -> String {
    format!(
        "<{tag}>Write an ultra-short dashboard line that captures the AGENT'S REPLY for the \
         last turn only — everything after the user message beginning: \"{anchor}\". \
         Focus on what the assistant concluded, answered, recommended, or delivered — not a \
         meta description of the turn (avoid \"Explained…\", \"Answered…\", \"Greeted…\", \
         \"Reviewed…\"). User-role messages wrapped in reminder tags like this one are \
         injected context, not the user.\n\n\
         Output ONLY the fragment: 5-12 words, plain text, glanceable on a status row. \
         Prefer the payload: answer, finding, change, or decision needed. \
         Do NOT call any tools — respond with plain text only.\n\n\
         Synthetic examples (style only — adapt to THIS turn, do not copy):\n\
         `queue_worker` shutdown race fixed; suite green\n\
         Payment retries: exp backoff in `billing/retry.rs`, 5× on 429\n\
         Retry backoff wired into `billing/retry.rs`; tests pending\n\
         Need decision: keep or drop `sqlx` cache before refactor\n\
         Black — matches the terminal aesthetic\n\n\
         Bad (never):\n\
         - Lead with Explained / Answered / Greeted / Reviewed / Confirmed / Flagged / Summarized\n\
         - Labels, quotes, bullets, markdown, code fences, multi-sentence dumps\n\
         - Filler like \"no code changes\" or \"awaiting task\" unless that is the whole point\n\
         - Summarize earlier turns or the whole session\n\
         - Call tools or invent content not in the agent's reply</{tag}>"
    )
}

/// Clean the model's raw output into a one-line fragment.
/// Recap normalization (whitespace collapse, stray label/quote stripping) runs first, then the tighter [`TURN_SUMMARY_MAX_CHARS`] cap.
pub(crate) fn clean_turn_summary_text(raw: &str) -> String {
    let mut out = super::session_recap::clean_recap_text(raw);
    if out.len() > TURN_SUMMARY_MAX_CHARS {
        let cut = floor_char_boundary(&out, TURN_SUMMARY_MAX_CHARS);
        out.truncate(cut);
        out = out.trim_end().to_string();
        out.push('\u{2026}');
    }
    out
}

/// Accept only a complete, short dashboard fragment. In particular, reject
/// oversized output before the legacy cleaner can truncate it into a false
/// success.
pub(crate) fn turn_summary_display_text(raw: &str) -> Option<String> {
    let normalized = super::session_recap::clean_recap_text(raw);
    if normalized.is_empty()
        || normalized.eq_ignore_ascii_case("none")
        || normalized.len() > TURN_SUMMARY_MAX_CHARS
        || normalized.split_whitespace().count() > 12
        || normalized.starts_with("LAST USER TURN:")
        || normalized.starts_with("ASSISTANT REPLY:")
    {
        return None;
    }
    let summary = clean_turn_summary_text(&normalized);
    (!summary.is_empty() && summary.len() <= TURN_SUMMARY_MAX_CHARS).then_some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ConversationItem {
        ConversationItem::user(text.to_string())
    }

    fn synthetic_user(text: &str) -> ConversationItem {
        use distill_sampling_types::{ContentPart, SyntheticReason, UserItem};
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: std::sync::Arc::from(text),
            }],
            synthetic_reason: SyntheticReason::SystemReminder,
            ..Default::default()
        })
    }

    #[test]
    fn anchor_skips_synthetic_user_turns() {
        let conv = vec![
            ConversationItem::system("sys".to_string()),
            user("fix the parser"),
            ConversationItem::assistant("done".to_string()),
            synthetic_user("<system-reminder>injected</system-reminder>"),
        ];
        assert_eq!(last_user_anchor(&conv).as_deref(), Some("fix the parser"));
    }

    #[test]
    fn anchor_none_without_real_user_message() {
        let conv = vec![
            ConversationItem::system("sys".to_string()),
            synthetic_user("injected"),
        ];
        assert_eq!(last_user_anchor(&conv), None);
        assert_eq!(last_user_anchor(&[user("   \n ")]), None);
    }

    #[test]
    fn anchor_collapses_drops_angle_brackets_and_truncates() {
        let long = format!("review <the>   plan\n{}", "x".repeat(200));
        let anchor = last_user_anchor(&[user(&long)]).unwrap();
        assert!(anchor.starts_with("review the plan"));
        assert!(!anchor.contains('<') && !anchor.contains('>'));
        assert!(anchor.ends_with('\u{2026}'));
        assert!(anchor.chars().count() <= ANCHOR_MAX_CHARS + 1);
    }

    #[test]
    fn instruction_embeds_tag_and_anchor() {
        let text = turn_summary_instruction("system-reminder", "fix the parser");
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
        assert!(text.contains("beginning: \"fix the parser\""));
    }

    #[test]
    fn clean_normalizes_and_caps() {
        assert_eq!(
            clean_turn_summary_text("Summary: \"Fixed the\n\n  parser\""),
            "Fixed the parser"
        );
        let capped = clean_turn_summary_text(&"word ".repeat(100));
        assert!(capped.len() <= TURN_SUMMARY_MAX_CHARS + '\u{2026}'.len_utf8());
        assert!(capped.ends_with('\u{2026}'));
    }

    #[test]
    fn display_payload_uses_only_the_last_real_turn() {
        let conv = vec![
            ConversationItem::system("system".to_owned()),
            user("old objective"),
            ConversationItem::assistant("old answer"),
            user("fix the parser"),
            ConversationItem::Reasoning(distill_sampling_types::synthesized_reasoning_item(
                "private chain of thought",
            )),
            ConversationItem::tool_result("call-1", "tool catalog"),
            ConversationItem::assistant("parser fixed; tests pending"),
        ];
        let payload = last_turn_display_payload(&conv).expect("last turn has a reply");
        assert!(payload.contains("LAST USER TURN:\nfix the parser"));
        assert!(payload.contains("ASSISTANT REPLY:\nparser fixed; tests pending"));
        assert!(!payload.contains("old objective"));
        assert!(!payload.contains("private chain"));
        assert!(!payload.contains("tool catalog"));
        assert!(payload.len() <= 4_000);
    }

    #[test]
    fn display_output_rejects_truncation_and_empty_markers() {
        assert_eq!(
            turn_summary_display_text("parser fixed; tests pending"),
            Some("parser fixed; tests pending".into())
        );
        assert!(turn_summary_display_text("none").is_none());
        assert!(turn_summary_display_text(&"word ".repeat(80)).is_none());
        assert!(turn_summary_display_text("ASSISTANT REPLY: parser fixed").is_none());
    }
}
