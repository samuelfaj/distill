//! Foreground requests with the same [`ConversationKey`] are one conversation, so a parent and its
//! subagent child are told apart and a resumed child continues its own.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::inference_request::{
    first_system_message, history_tool_calls, last_assistant_message, tool_results,
};
use crate::request_log::LogEntry;
use crate::tools::Tool;

const SYSTEM_MESSAGE_PREVIEW_CHARS: usize = 80;

/// Conversations are numbered from 1 in the order the mock first saw each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ConversationId(usize);

impl ConversationId {
    #[must_use]
    pub(crate) const fn nth(number: usize) -> Self {
        assert!(number > 0, "conversations are counted from 1");
        ConversationId(number)
    }

    pub(crate) const fn number(self) -> usize {
        self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conversation {}", self.0)
    }
}

/// What groups foreground requests into one conversation, tried in this order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationKey {
    /// Sent for every session, so a prompt re rendered mid session stays in its conversation.
    SessionId(String),
    SystemMessage(String),
    /// Every request with neither shares one conversation.
    Unkeyed,
}

impl ConversationKey {
    pub(crate) fn from_request(session_id: Option<&str>, body: &Value) -> Self {
        if let Some(session_id) = session_id {
            return ConversationKey::SessionId(session_id.to_owned());
        }
        match first_system_message(body) {
            Some(message) => ConversationKey::SystemMessage(message),
            None => ConversationKey::Unkeyed,
        }
    }
}

impl fmt::Display for ConversationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversationKey::SessionId(session_id) => write!(f, "session {session_id}"),
            ConversationKey::SystemMessage(message) => {
                let mut chars = message.chars();
                let preview: String = chars.by_ref().take(SYSTEM_MESSAGE_PREVIEW_CHARS).collect();
                let cut = if chars.next().is_some() { "..." } else { "" };
                write!(f, "system message {preview:?}{cut}")
            }
            ConversationKey::Unkeyed => f.write_str("unkeyed"),
        }
    }
}

/// A conversation as the mock saw it, read at one moment; every reader works on the requests the
/// agent sent, so the reply that ended the conversation is in none of them.
#[derive(Debug, Clone)]
pub struct ReadConversation {
    id: ConversationId,
    key: ConversationKey,
    requests: Vec<LogEntry>,
}

impl ReadConversation {
    #[must_use]
    pub fn number(&self) -> usize {
        self.id.number()
    }

    #[must_use]
    pub fn requests(&self) -> Vec<LogEntry> {
        self.requests.clone()
    }

    #[must_use]
    pub fn first_system_prompt(&self) -> Option<String> {
        self.requests.first()?.first_system_prompt()
    }

    #[must_use]
    pub fn last_reply(&self) -> Option<String> {
        last_assistant_message(self.requests.last()?.body.as_ref()?)
    }

    #[must_use]
    pub fn saw_tool_call(&self, tool: Tool) -> bool {
        self.requests
            .iter()
            .filter_map(|request| request.body.as_ref())
            .flat_map(history_tool_calls)
            .any(|call| tool.is_called_by(&call))
    }

    #[must_use]
    pub fn tool_result(&self, call_id: &str) -> Option<String> {
        self.requests
            .iter()
            .filter_map(|request| request.body.as_ref())
            .find_map(|body| tool_results(body).remove(call_id))
    }
}

impl fmt::Display for ReadConversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.id, self.key)
    }
}

#[derive(Clone, Default)]
pub(crate) struct ConversationTracker {
    keys: Arc<std::sync::Mutex<Vec<ConversationKey>>>,
}

impl ConversationTracker {
    pub(crate) fn assign(&self, key: ConversationKey) -> ConversationId {
        let mut keys = self.keys.lock().unwrap();
        let index = match keys.iter().position(|known| *known == key) {
            Some(index) => index,
            None => {
                keys.push(key);
                keys.len() - 1
            }
        };
        ConversationId::nth(index + 1)
    }

    pub(crate) fn snapshot(&self, log: &[LogEntry]) -> Vec<ReadConversation> {
        self.keys
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let id = ConversationId::nth(index + 1);
                ReadConversation {
                    id,
                    key: key.clone(),
                    requests: log
                        .iter()
                        .filter(|entry| entry.conversation == Some(id.number()))
                        .cloned()
                        .collect(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;
