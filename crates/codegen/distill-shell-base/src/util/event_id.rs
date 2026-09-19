//! Event ID generation for session notifications.
//!
//! Provides a globally unique event ID format `{session_id}-{counter}` that the relay uses for deduplication.
//! The counter is monotonically increasing across the entire agent process, so event IDs are always comparable.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared across all sessions so event IDs increase monotonically.
static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generates a unique event ID for correlation across agent/relay/client.
///
/// Format: `{session_id}-{counter}`; the relay compares event IDs numerically by extracting the counter suffix.
pub fn generate_event_id(session_id: &str) -> String {
    let count = EVENT_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}-{}", session_id, count)
}

/// Stamp `_meta.eventId` and `agentTimestampMs` onto a notification's meta unless an `eventId` is already present, preserving other meta fields.
/// The reconnect cursor (`session/load` `_meta.cursor`) can only bound the replay tail when each persisted line is identifiable.
/// The same id must go out on the live broadcast so clients advance their cursor to ids that exist on disk. Broadcast-only notifications are deliberately left unstamped. A cursor pointing at an id absent from `updates.jsonl` never resolves and forces a full replay on every reconnect.
pub fn ensure_event_id_meta(
    session_id: &str,
    meta: &mut Option<serde_json::Map<String, serde_json::Value>>,
) {
    if meta
        .as_ref()
        .and_then(|m| m.get("eventId"))
        .is_some_and(|v| !v.is_null())
    {
        return;
    }
    let event_id = generate_event_id(session_id);
    let timestamp_ms = chrono::Utc::now().timestamp_millis();
    let obj = meta.get_or_insert_with(serde_json::Map::new);
    obj.insert("eventId".into(), event_id.into());
    obj.entry("agentTimestampMs")
        .or_insert_with(|| timestamp_ms.into());
}

/// Raise the global event counter so the next generated id is at least `next`. The counter is process-global and starts at 0 on every launch.
/// Client dedup (`acp::meta::NotificationMeta::event_seq`) relies on `eventId` increasing over a session's whole history, not just one process.
/// On `--resume` (or any reload into a fresh process) the replayed transcript carries the original process's high counters. Without re-seeding, this process would mint lower ids for new live events. Uses `fetch_max`, so it only ever raises the counter and is safe to call from concurrently-loading sessions.
pub fn ensure_event_counter_at_least(next: u64) {
    EVENT_COUNTER.fetch_max(next, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_event_id_format() {
        let id = generate_event_id("test-session-123");
        assert!(id.starts_with("test-session-123-"));
        let _counter: u64 = id.rsplit('-').next().unwrap().parse().unwrap();
    }

    #[test]
    fn ensure_event_counter_at_least_only_raises() {
        // Re-seeding to a high floor makes the next id continue past it; this is what keeps `--resume` from minting ids below the replayed maximum
        // The floor is very high so concurrent tests (which only ever raise the shared counter via fetch_add/fetch_max) cannot push it back down
        ensure_event_counter_at_least(5_000_000);
        let counter1: u64 = generate_event_id("sess")
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            counter1 >= 5_000_000,
            "next id must be at/above the seeded floor, got {counter1}"
        );

        // A lower floor is a no-op (fetch_max never decreases the counter).
        ensure_event_counter_at_least(1);
        let counter2: u64 = generate_event_id("sess")
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            counter2 > counter1,
            "a lower floor must not reset the counter: {counter2} !> {counter1}"
        );
    }

    #[test]
    fn ensure_event_id_meta_stamps_none_and_merges_existing() {
        // None meta: a fresh object with eventId and timestamp is created
        let mut meta = None;
        ensure_event_id_meta("sess-x", &mut meta);
        let obj = meta.as_ref().unwrap();
        assert!(
            obj.get("eventId")
                .and_then(|v| v.as_str())
                .is_some_and(|id| id.starts_with("sess-x-"))
        );
        assert!(obj.get("agentTimestampMs").is_some_and(|v| v.is_i64()));

        // Existing meta without eventId: fields are merged, not replaced.
        let mut meta = serde_json::json!({ "custom": true }).as_object().cloned();
        ensure_event_id_meta("sess-x", &mut meta);
        let obj = meta.as_ref().unwrap();
        assert_eq!(obj.get("custom"), Some(&serde_json::json!(true)));
        assert!(obj.contains_key("eventId"));
    }

    #[test]
    fn ensure_event_id_meta_keeps_existing_id() {
        // An already-stamped id (e.g. the emit site stamped before the persist chokepoint re-checks) must survive.
        // The persisted line has to match the live broadcast copy
        let mut meta = serde_json::json!({ "eventId": "sess-x-42" })
            .as_object()
            .cloned();
        ensure_event_id_meta("sess-x", &mut meta);
        assert_eq!(
            meta.as_ref().and_then(|m| m.get("eventId")),
            Some(&serde_json::json!("sess-x-42"))
        );
    }

    #[test]
    fn test_generate_event_id_incrementing() {
        let id1 = generate_event_id("session-a");
        let id2 = generate_event_id("session-b");
        let id3 = generate_event_id("session-a");

        let counter1: u64 = id1.rsplit('-').next().unwrap().parse().unwrap();
        let counter2: u64 = id2.rsplit('-').next().unwrap().parse().unwrap();
        let counter3: u64 = id3.rsplit('-').next().unwrap().parse().unwrap();

        // Counters increase monotonically across sessions
        assert!(counter2 > counter1);
        assert!(counter3 > counter2);
    }
}
