// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::ConversationItem;

/// Select a recent window of the conversation history for the flush model.
/// Starts with the last `recent_message_count` messages, then expands backward to the nearest `User` message.
/// The window therefore always starts on a user boundary and may be larger than `recent_message_count`.
pub fn select_flush_window(
    messages: Vec<ConversationItem>,
    recent_message_count: usize,
) -> Vec<ConversationItem> {
    let messages: Vec<_> = messages
        .into_iter()
        .filter(|item| !matches!(item, ConversationItem::System(_)))
        .collect();

    let total = messages.len();
    let mut start = total.saturating_sub(recent_message_count);
    while start > 0
        && !messages
            .get(start)
            .is_some_and(|m| matches!(m, ConversationItem::User(_)))
    {
        start -= 1;
    }
    messages.into_iter().skip(start).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_flush_window_expands_to_user_boundary() {
        let mut messages = vec![ConversationItem::user("early question")];
        for i in 0..20 {
            messages.push(ConversationItem::assistant(format!("response {i}")));
        }

        let window = select_flush_window(messages, 20);

        assert_eq!(window.len(), 21);
        assert!(matches!(window.first(), Some(ConversationItem::User(_))));
    }

    #[test]
    fn test_select_flush_window_filters_system_messages() {
        let messages = vec![
            ConversationItem::system("you are helpful"),
            ConversationItem::user("hi"),
            ConversationItem::assistant("hello"),
        ];

        let window = select_flush_window(messages, 20);

        assert!(
            window
                .iter()
                .all(|item| !matches!(item, ConversationItem::System(_)))
        );
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn test_select_flush_window_short_conversation() {
        let messages = vec![
            ConversationItem::user("hi"),
            ConversationItem::assistant("hello"),
        ];

        let window = select_flush_window(messages, 20);

        assert_eq!(window.len(), 2);
        assert!(matches!(window.first(), Some(ConversationItem::User(_))));
    }

    #[test]
    fn flush_window_preserves_agent_provenance_until_request_projection() {
        let human = ConversationItem::user("human request");
        let agent = ConversationItem::agent_message("agent context");
        let window = select_flush_window(vec![human.clone(), agent], 20);
        let projected = distill_chat_state::compaction_utils::ModelRequestHistory::from_raw(window)
            .into_items();

        assert_eq!(
            projected.first().map(|i| i.text_content()),
            Some(human.text_content())
        );
        assert_eq!(
            projected.get(1).map(|i| i.text_content()),
            Some(format!(
                "{}\nagent context",
                distill_chat_state::compaction_utils::AGENT_MESSAGE_MODEL_LABEL
            ))
        );
    }
}
