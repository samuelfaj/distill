//! The typed record of everything the agent sent the client, in arrival order.
//! Round trips are recorded once answered, so a held request appears when the client releases it.

use agent_client_protocol as acp;
use serde_json::Value;
use tokio::sync::watch;

/// One message from the agent. Requests carry the reply the client's policy gave.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// A `session/update` notification.
    SessionUpdate(acp::SessionNotification),
    /// An extension notification such as `x.ai/session_notification`.
    ExtNotification { method: String, params: Value },
    /// A `session/request_permission` round trip.
    PermissionRequest {
        request: acp::RequestPermissionRequest,
        outcome: acp::RequestPermissionOutcome,
    },
    /// An extension request round trip such as `x.ai/ask_user_question`.
    ExtRequest {
        method: String,
        params: Value,
        reply: Value,
    },
}

/// The recorder the connection handler appends to. Accessors hand out point in time snapshots.
#[derive(Default)]
pub(crate) struct Transcript {
    /// The watch is both the lock over the entries and what wakes `wait_until`; each wait subscribes its own
    /// receiver.
    entries: watch::Sender<Vec<TranscriptEntry>>,
}

impl Transcript {
    pub(crate) fn record(&self, entry: TranscriptEntry) {
        self.entries.send_modify(|entries| entries.push(entry));
    }

    pub(crate) fn entries(&self) -> Vec<TranscriptEntry> {
        self.entries.borrow().clone()
    }

    /// Resolves once `is_done` accepts the entries recorded so far, which it sees whole again after each new
    /// entry, including entries recorded before the call.
    pub(crate) async fn wait_until(&self, is_done: impl Fn(&[TranscriptEntry]) -> bool) {
        self.entries
            .subscribe()
            .wait_for(|entries| is_done(entries))
            .await
            .expect("the transcript owns the sender for the whole wait");
    }

    /// The agent's message so far: every `agent_message_chunk` text block, joined.
    pub(crate) fn agent_text(&self) -> String {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::SessionUpdate(acp::SessionNotification {
                    update:
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                            content: acp::ContentBlock::Text(text),
                            ..
                        }),
                    ..
                }) => Some(text.text.as_str()),
                TranscriptEntry::SessionUpdate(_)
                | TranscriptEntry::ExtNotification { .. }
                | TranscriptEntry::PermissionRequest { .. }
                | TranscriptEntry::ExtRequest { .. } => None,
            })
            .collect()
    }

    pub(crate) fn session_update_count(&self) -> usize {
        self.entries
            .borrow()
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::SessionUpdate(_)))
            .count()
    }

    pub(crate) fn ext_notification_count(&self, method: &str) -> usize {
        self.entries
            .borrow()
            .iter()
            .filter(|entry| {
                matches!(entry, TranscriptEntry::ExtNotification { method: received, .. } if received == method)
            })
            .count()
    }
}

#[cfg(test)]
#[path = "acp_transcript_tests.rs"]
mod tests;
