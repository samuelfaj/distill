use serde::{Deserialize, Serialize};

/// Content-block `_meta` key for per-prompt display texts when several
/// follow-ups were combined (length ≥ 2). Empty / absent = not combined.
pub const COMBINED_DISPLAY_TEXTS_META: &str = "combinedDisplayTexts";

/// Per-item queue metadata the session actor attaches to user-originated inputs; synthetic
/// inputs (auto-wake, nudges) carry none and never appear in the visible queue. Held in
/// actor state, never serialized itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntryMeta {
    /// Stable id, reusing the prompt's unique `prompt_id`.
    pub id: String,
    /// Monotonic, bumped on each in-place edit; an edit against a stale version is a no-op.
    pub version: u64,
    /// Enqueuing client identifier (attribution); never overwritten by edits.
    pub owner: Option<String>,
    /// Most recent editor's client identifier, replaced on every in-place edit.
    pub last_editor: Option<String>,
    /// Display kind label; client-cosmetic kinds resolve to their send-intent before enqueue.
    pub kind: String,
    /// Plain prompt text for the shared queue display.
    pub text: String,
    /// Per-prompt display texts when combine merged several follow-ups (len ≥ 2).
    pub combined_texts: Option<Vec<String>>,
}

/// One queue row on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntryWire {
    pub id: String,
    #[serde(default)]
    pub version: u64,
    /// Omitted from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Mirrors [`QueueEntryMeta::last_editor`]; omitted from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_editor: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    /// See [`QueueEntryMeta::combined_texts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combined_texts: Option<Vec<String>>,
    /// 0-based position among queued, not-yet-running prompts.
    #[serde(default)]
    pub position: usize,
}

/// Broadcast payload for the `x.ai/queue/changed` notification.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueChanged {
    /// The session this queue belongs to; drives per-session fan-out routing.
    pub session_id: String,
    #[serde(default)]
    pub entries: Vec<QueueEntryWire>,
    /// The prompt the actor is currently draining, `None` when no turn runs. The correlation
    /// signal a subscriber uses to adopt `current_prompt_id` for notification routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_prompt_id: Option<String>,
    /// Display text for the running prompt. Carried explicitly because the
    /// running row is omitted from [`Self::entries`]; clients use this for the
    /// turn-start user block without relying on a stale local mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_text: Option<String>,
    /// Kind for the running prompt (`"prompt"` / `"bash"` / …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_kind: Option<String>,
    /// Per-prompt display texts when the running turn was combined (len ≥ 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_combined_texts: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_changed_full_round_trip() {
        let original = QueueChanged {
            session_id: "sess-42".into(),
            entries: vec![
                QueueEntryWire {
                    id: "p1".into(),
                    version: 3,
                    owner: Some("alice".into()),
                    last_editor: Some("bob".into()),
                    kind: "prompt".into(),
                    text: "fix the bug".into(),
                    position: 0,
                    combined_texts: None,
                },
                QueueEntryWire {
                    id: "p2".into(),
                    version: 0,
                    owner: None,
                    last_editor: None,
                    kind: "bash".into(),
                    text: "ls -la".into(),
                    position: 1,
                    combined_texts: None,
                },
            ],
            running_prompt_id: Some("p0".into()),

            running_text: None,
            running_kind: None,
            running_combined_texts: None,
        };
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(
            json.get("sessionId").and_then(|v| v.as_str()),
            Some("sess-42")
        );
        let Some(entries) = json.get("entries").and_then(|v| v.as_array()) else {
            panic!("expected entries array: {json}");
        };
        let Some(first) = entries.first() else {
            panic!("expected first entry: {entries:?}");
        };
        assert_eq!(
            first.get("lastEditor").and_then(|v| v.as_str()),
            Some("bob")
        );
        assert_eq!(
            json.get("runningPromptId").and_then(|v| v.as_str()),
            Some("p0")
        );
        let Some(second) = entries.get(1) else {
            panic!("expected second entry: {entries:?}");
        };
        assert!(second.get("owner").is_none());
        assert!(second.get("lastEditor").is_none());
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, original);
    }

    /// Pins the exact wire JSON; a key rename here breaks deployed clients.
    #[test]
    fn queue_changed_golden_wire_json() {
        let payload = QueueChanged {
            session_id: "s1".into(),
            entries: vec![QueueEntryWire {
                id: "p1".into(),
                version: 2,
                owner: Some("alice".into()),
                last_editor: Some("bob".into()),
                kind: "prompt".into(),
                text: "hi".into(),
                position: 0,
                combined_texts: None,
            }],
            running_prompt_id: Some("p0".into()),

            running_text: None,
            running_kind: None,
            running_combined_texts: None,
        };
        let expected = serde_json::json!({
            "sessionId": "s1",
            "entries": [{
                "id": "p1",
                "version": 2,
                "owner": "alice",
                "lastEditor": "bob",
                "kind": "prompt",
                "text": "hi",
                "position": 0
            }],
            "runningPromptId": "p0"
        });
        assert_eq!(serde_json::to_value(&payload).unwrap(), expected);
    }

    /// A broadcast without sessionId must fail to parse, not apply under the wrong key.
    #[test]
    fn queue_changed_requires_session_id() {
        let missing = serde_json::json!({ "entries": [] });
        assert!(serde_json::from_value::<QueueChanged>(missing).is_err());
    }

    #[test]
    fn sparse_payload_deserializes_with_defaults() {
        let sparse = serde_json::json!({
            "sessionId": "s1",
            "entries": [{"id": "p1"}]
        });
        let parsed: QueueChanged = serde_json::from_value(sparse).unwrap();
        let Some(entry) = parsed.entries.first() else {
            panic!("expected one entry: {:?}", parsed.entries);
        };
        assert_eq!(entry.version, 0);
        assert_eq!(entry.kind, "");
        assert_eq!(entry.text, "");
        assert_eq!(entry.position, 0);
        assert!(entry.owner.is_none());
        assert!(parsed.running_prompt_id.is_none());
    }

    #[test]
    fn extra_unknown_fields_ignored() {
        let json = serde_json::json!({
            "sessionId": "s1",
            "entries": [],
            "runningPromptId": null,
            "futureField": "should be ignored"
        });
        let parsed: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.session_id, "s1");
    }

    #[test]
    fn running_combined_texts_round_trip() {
        let original = QueueChanged {
            session_id: "s1".into(),
            entries: vec![],
            running_prompt_id: Some("p0".into()),
            running_text: Some("a\n\nb".into()),
            running_kind: Some("prompt".into()),
            running_combined_texts: Some(vec!["a".into(), "b".into()]),
        };
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(
            json.get("runningCombinedTexts"),
            Some(&serde_json::json!(["a", "b"]))
        );
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, original);
    }
}
