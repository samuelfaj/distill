// Modified for Distill by Samuel Fajreldines, 2026.
//! Pure conversation-shape helpers, kept crate-neutral so both the session
//! layer (`distill-shell`) and the `ChatStateActor` can share one definition
//! of "align the leading System message with a prompt".

use std::sync::Arc;

use distill_sampling_types::conversation::ConversationItem;

/// Equal after trimming trailing `\n`/`\r` from both sides.
/// Attach idempotency: a stored head that differs only by a trailing newline is already matching.
/// Interior and leading whitespace are significant.
pub fn canonical_system_prompt_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches(['\n', '\r']) == b.trim_end_matches(['\n', '\r'])
}

/// Replace the leading `System` message with `prompt`, or insert one. Returns whether it changed.
/// A head already equal modulo trailing newlines is left untouched (KV-cache-friendly idempotency).
/// Shared by cold-load pre-apply and the atomic actor head swap.
#[must_use]
pub fn replace_or_insert_system_head(
    conversation: &mut Vec<ConversationItem>,
    prompt: &str,
) -> bool {
    match conversation.first_mut() {
        Some(ConversationItem::System(sys)) => {
            if canonical_system_prompt_eq(sys.content.as_ref(), prompt) {
                return false;
            }
            sys.content = Arc::from(prompt);
            true
        }
        _ => {
            conversation.insert(0, ConversationItem::system(prompt));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_prompt(conversation: &[ConversationItem]) -> Option<&str> {
        conversation.first().and_then(|item| match item {
            ConversationItem::System(s) => Some(s.content.as_ref()),
            _ => None,
        })
    }

    #[test]
    fn canonical_system_prompt_eq_ignores_trailing_newlines() {
        assert!(canonical_system_prompt_eq("hello\n", "hello"));
        assert!(canonical_system_prompt_eq("hello\r\n", "hello"));
        assert!(!canonical_system_prompt_eq("hello", "world"));
    }

    #[test]
    fn canonical_system_prompt_eq_respects_interior_and_leading_whitespace() {
        assert!(canonical_system_prompt_eq("a\nb\n", "a\nb"));
        assert!(!canonical_system_prompt_eq("a\nb", "ab"));
        assert!(!canonical_system_prompt_eq(" hello", "hello"));
    }

    #[test]
    fn replace_or_insert_system_head_replaces_stored_head() {
        let mut history = vec![
            ConversationItem::system("default system prompt"),
            ConversationItem::user("hi"),
        ];
        assert!(replace_or_insert_system_head(
            &mut history,
            "client override"
        ));
        assert_eq!(system_prompt(&history), Some("client override"));
        assert_eq!(history.len(), 2, "must not wipe user turns");
    }

    #[test]
    fn replace_or_insert_system_head_noop_when_unchanged() {
        let mut history = vec![
            ConversationItem::system("same prompt"),
            ConversationItem::user("hi"),
        ];
        assert!(!replace_or_insert_system_head(
            &mut history,
            "same prompt\n"
        ));
    }

    #[test]
    fn replace_or_insert_system_head_inserts_when_first_is_not_system() {
        let mut history = vec![ConversationItem::user("hi")];
        assert!(replace_or_insert_system_head(
            &mut history,
            "client override"
        ));
        assert_eq!(system_prompt(&history), Some("client override"));
        assert_eq!(history.len(), 2, "inserts at head, keeps existing turns");
    }

    #[test]
    fn replace_or_insert_system_head_inserts_into_empty() {
        let mut history: Vec<ConversationItem> = vec![];
        assert!(replace_or_insert_system_head(
            &mut history,
            "client override"
        ));
        assert_eq!(system_prompt(&history), Some("client override"));
    }
}
