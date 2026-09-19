// Modified for Distill by Samuel Fajreldines, 2026.
//! Server-authoritative prompt queue wire types.
//!
//! Canonical definitions live in `distill_prompt_queue`.
//! This re-export keeps every existing `crate::session::prompt_queue::*` and `distill_shell::session::prompt_queue::*` path resolving without edits.

pub use distill_prompt_queue::{
    COMBINED_DISPLAY_TEXTS_META, CombineGate, QueueChanged, QueueEntryMeta, QueueEntryWire,
    TEXT_SEPARATOR, combine_prefix_len, is_combined, join_texts, stamp_combined_display_texts,
};

// Outbound method for broadcast_queue_changed. This is an ACP routing concern, not a queue concern.
pub const QUEUE_CHANGED_METHOD: &str = "x.ai/queue/changed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_changed_serializes_camel_case_with_session_id() {
        let payload = QueueChanged {
            session_id: "sess-1".to_string(),
            entries: vec![QueueEntryWire {
                id: "p1".to_string(),
                version: 0,
                owner: Some("grok-tui".to_string()),
                last_editor: None,
                kind: "prompt".to_string(),
                text: "hello".to_string(),
                position: 0,
                combined_texts: None,
            }],
            running_prompt_id: None,

            running_text: None,
            running_kind: None,
            running_combined_texts: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json.get("sessionId").and_then(|v| v.as_str()),
            Some("sess-1")
        );
        let entry0 = json.get("entries").and_then(|e| e.get(0));
        assert_eq!(
            entry0.and_then(|e| e.get("id")).and_then(|v| v.as_str()),
            Some("p1")
        );
        assert_eq!(
            entry0
                .and_then(|e| e.get("position"))
                .and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(entry0.and_then(|e| e.get("lastEditor")).is_none());
        assert!(json.get("runningPromptId").is_none());
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn queue_changed_round_trips_running_prompt_id() {
        let payload = QueueChanged {
            session_id: "sess-1".to_string(),
            entries: Vec::new(),
            running_prompt_id: Some("prompt-running".to_string()),

            running_text: None,
            running_kind: None,
            running_combined_texts: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json.get("runningPromptId").and_then(|v| v.as_str()),
            Some("prompt-running")
        );
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn queue_entry_wire_round_trips_last_editor() {
        let entry = QueueEntryWire {
            id: "p1".to_string(),
            version: 3,
            owner: Some("grok-tui".to_string()),
            last_editor: Some("grok-vscode".to_string()),
            kind: "prompt".to_string(),
            text: "hello".to_string(),
            position: 0,
            combined_texts: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            json.get("lastEditor").and_then(|v| v.as_str()),
            Some("grok-vscode")
        );
        let round: QueueEntryWire = serde_json::from_value(json).unwrap();
        assert_eq!(round, entry);
    }
}
