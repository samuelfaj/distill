use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::Serialize;

use crate::types::Event;

#[derive(Serialize)]
struct EventEntry {
    ts: String,
    #[serde(flatten)]
    event: Event,
}

const EVENTS_FILE: &str = "events.jsonl";

/// Writes events to `events.jsonl`.
/// Clones share one file, and the writer is `Send + Sync` so background tasks can hold one.
#[derive(Clone)]
pub struct EventWriter {
    inner: Arc<EventWriterInner>,
}

struct EventWriterInner {
    file: Mutex<Option<File>>,
    error_logged: AtomicBool,
}

impl EventWriter {
    pub fn open(session_dir: &Path) -> Self {
        let path = session_dir.join(EVENTS_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "failed to open {EVENTS_FILE}");
                e
            })
            .ok();
        Self {
            inner: Arc::new(EventWriterInner {
                file: Mutex::new(file),
                error_logged: AtomicBool::new(false),
            }),
        }
    }

    pub fn noop() -> Self {
        Self {
            inner: Arc::new(EventWriterInner {
                file: Mutex::new(None),
                error_logged: AtomicBool::new(true), // True from the start, so this writer never warns
            }),
        }
    }

    pub fn emit(&self, event: Event) {
        let entry = EventEntry {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event,
        };
        let Ok(mut line) = serde_json::to_vec(&entry) else {
            return;
        };
        line.push(b'\n');

        let Ok(mut guard) = self.inner.file.lock() else {
            return;
        };
        if let Some(ref mut f) = *guard
            && let Err(e) = f.write_all(&line)
            && !self.inner.error_logged.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(error = %e, "{EVENTS_FILE} write failed");
        }
    }
}

impl std::fmt::Debug for EventWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventWriter").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        EVENT_SCHEMA_VERSION, Event, SessionRelationship, ToolOutcome, TurnOutcomeLabel,
    };

    fn _assert_event_writer_is_send_sync_clone()
    where
        EventWriter: Send + Sync + Clone,
    {
    }

    #[test]
    fn test_emit_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let writer = EventWriter::open(dir.path());

        writer.emit(Event::TurnStarted {
            session_id: "test-session".into(),
            turn_number: 1,
            model_id: "grok-3".into(),
            yolo_mode: false,
            conversation_message_count: 0,
            session_relationship: SessionRelationship::Primary,
            schema_version: EVENT_SCHEMA_VERSION.into(),
            redirect_kind: None,
        });
        writer.emit(Event::FirstToken);
        writer.emit(Event::ToolCompleted {
            tool_name: "bash".into(),
            duration_ms: 1500,
            outcome: ToolOutcome::Success,
            tool_call_id: "call_xyz".into(),
            source: crate::types::ToolCompletedSource::Shell,
            rewriting_hook: None,
        });
        writer.emit(Event::TurnEnded {
            outcome: TurnOutcomeLabel::Completed,
            cancellation_category: None,
            cancellation_context: None,
        });

        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        let lines: Vec<&str> = text.trim().split('\n').collect();
        assert_eq!(lines.len(), 4);

        let [l0, l1, l2, l3] = lines.as_slice() else {
            panic!("expected four event lines: {lines:?}");
        };
        let first: serde_json::Value = serde_json::from_str(l0).unwrap();
        assert_eq!(first.get("type"), Some(&serde_json::json!("turn_started")));
        assert_eq!(
            first.get("session_id"),
            Some(&serde_json::json!("test-session"))
        );
        assert!(first.get("ts").and_then(|v| v.as_str()).is_some());

        let second: serde_json::Value = serde_json::from_str(l1).unwrap();
        assert_eq!(second.get("type"), Some(&serde_json::json!("first_token")));

        let third: serde_json::Value = serde_json::from_str(l2).unwrap();
        assert_eq!(
            third.get("type"),
            Some(&serde_json::json!("tool_completed"))
        );
        assert_eq!(third.get("tool_name"), Some(&serde_json::json!("bash")));
        assert_eq!(third.get("duration_ms"), Some(&serde_json::json!(1500)));
        assert_eq!(
            third.get("tool_call_id"),
            Some(&serde_json::json!("call_xyz"))
        );
        assert!(
            third.get("source").is_none(),
            "shell ToolCompleted must omit source"
        );

        let fourth: serde_json::Value = serde_json::from_str(l3).unwrap();
        assert_eq!(fourth.get("type"), Some(&serde_json::json!("turn_ended")));
        assert_eq!(fourth.get("outcome"), Some(&serde_json::json!("completed")));
        assert!(fourth.get("cancellation_category").is_none());
    }

    #[test]
    fn cloned_writer_shares_file() {
        let dir = tempfile::tempdir().unwrap();
        let w1 = EventWriter::open(dir.path());
        let w2 = w1.clone();

        w1.emit(Event::FirstToken);
        w2.emit(Event::FirstToken);

        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        let lines: Vec<&str> = text.trim().split('\n').collect();
        assert_eq!(lines.len(), 2, "both writes should go to the same file");
    }
}
