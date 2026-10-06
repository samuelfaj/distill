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

/// Replace the leading `System` message with `prompt`, or insert one, and drop every system prompt
/// update (older views of the prompt the head now carries). Returns whether it changed.
/// A head already equal modulo trailing newlines is left untouched (KV-cache-friendly idempotency).
/// Shared by cold-load pre-apply and the atomic actor head swap.
#[must_use]
pub fn replace_or_insert_system_head(
    conversation: &mut Vec<ConversationItem>,
    prompt: &str,
) -> bool {
    let len = conversation.len();
    conversation.retain(|item| !item.is_system_prompt_update());
    let dropped_updates = conversation.len() != len;
    match conversation.first_mut() {
        Some(ConversationItem::System(sys)) => {
            if canonical_system_prompt_eq(sys.content.as_ref(), prompt) {
                return dropped_updates;
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

/// Keeps `prompt` the system prompt in effect after a rewrite (a rewind) that may have cut the
/// update carrying it: it then becomes the head, as [`replace_or_insert_system_head`] sets it.
/// Returns whether it changed.
#[must_use]
pub fn keep_system_prompt(conversation: &mut Vec<ConversationItem>, prompt: &str) -> bool {
    if distill_sampling_types::current_system_prompt(conversation)
        .is_some_and(|current| canonical_system_prompt_eq(current, prompt))
    {
        return false;
    }
    replace_or_insert_system_head(conversation, prompt)
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

    /// A head set from outside (a client override, a model switch) is the whole prompt: an update
    /// left after it would replace it again, since the last one is the prompt in effect.
    #[test]
    fn replace_or_insert_system_head_drops_system_prompt_updates() {
        let mut history = vec![
            ConversationItem::system("v1"),
            ConversationItem::user("hi"),
            ConversationItem::assistant("yo"),
            ConversationItem::system_prompt_update("v2"),
            ConversationItem::user("again"),
        ];
        assert!(replace_or_insert_system_head(&mut history, "v1"));
        assert_eq!(history.len(), 4);
        assert!(
            !history
                .iter()
                .any(ConversationItem::is_system_prompt_update)
        );
        assert_eq!(
            distill_sampling_types::current_system_prompt(&history),
            Some("v1")
        );
    }

    /// A rewind that cuts the update carrying the prompt in effect must not revert the session
    /// to an older prompt while its mode, memory and worker stay current.
    #[test]
    fn keep_system_prompt_restores_a_cut_update_into_the_head() {
        let mut history = vec![
            ConversationItem::system("v1"),
            ConversationItem::user("hi"),
            ConversationItem::assistant("yo"),
            ConversationItem::system_prompt_update("v2"),
        ];
        assert!(!keep_system_prompt(&mut history, "v2"), "still in effect");
        history.truncate(3);
        assert!(keep_system_prompt(&mut history, "v2"));
        assert_eq!(system_prompt(&history), Some("v2"));
        assert_eq!(history.len(), 3);
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
