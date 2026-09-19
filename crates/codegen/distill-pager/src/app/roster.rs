// Modified for Distill by Samuel Fajreldines, 2026.
//! Mirror types for the leader "session roster" wire format.
//!
//! The leader process hosts session actors and exposes a roster API the pager consumes in leader mode (FleetView dashboard):
//!
//! - Request/response `x.ai/sessions/list` parses into [`RosterListResponse`].
//! - Broadcast notification `x.ai/sessions/changed` parses into [`RosterChanged`].
//!
//! These structs mirror the producer-side wire format (camelCase JSON, snake_case activity enum).
//! They are deserialize-only: the pager never produces them.

use serde::Deserialize;

/// Coarse activity state for a roster entry. Wire format is snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterActivity {
    Working,
    Idle,
    NeedsInput,
    Dormant,
    Completed,
    Dead,
}

/// Origin of a roster entry (local leader vs. a remote host). We don't render origin yet, but must parse it without failing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RosterOrigin {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub host: Option<String>,
}

/// A single session in the leader roster.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub session_id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub is_worktree: bool,
    /// Set only on rows the leader synthesizes for remote claims; those are not loadable
    /// sessions. `None` for rows a local agent owns.
    #[serde(default)]
    pub session_kind: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub yolo: bool,
    pub activity: RosterActivity,
    /// Ultra-short summary of the session's most recent turn, shown as the row's secondary line.
    #[serde(default)]
    pub last_turn_summary: Option<String>,
    #[serde(default)]
    pub resident: bool,
    #[serde(default)]
    pub last_change_unix_ms: i64,
    #[serde(default)]
    pub origin: RosterOrigin,
}

/// Response to `x.ai/sessions/list`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RosterListResponse {
    #[serde(default)]
    pub sessions: Vec<RosterEntry>,
}

/// Broadcast payload for `x.ai/sessions/changed`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RosterChanged {
    #[serde(default)]
    pub upserted: Vec<RosterEntry>,
    #[serde(default)]
    pub removed: Vec<String>,
}

/// Parse an `x.ai/sessions/list` ext-response body into a [`RosterListResponse`].
/// The agent answers through `ExtMethodResult::success(..).to_ext_response()`, which wraps the payload as `{ "result": { "sessions": [...] } }`.
/// A bare `{ "sessions": [...] }` body (no envelope) is tolerated too.
pub fn parse_roster_list_response(body: &str) -> Option<RosterListResponse> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let payload = value.get("result").unwrap_or(&value);
    serde_json::from_value::<RosterListResponse>(payload.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a representative agent-side roster entry with every field set to a non-default value so the round-trip exercises name/case mapping.
    fn agent_entry() -> distill_shell::agent::roster::RosterEntry {
        use distill_shell::agent::roster as agent;
        agent::RosterEntry {
            session_id: "sess-abc".to_string(),
            title: Some("Fix the roster".to_string()),
            cwd: "/repo/worktree".to_string(),
            is_worktree: true,
            session_kind: None,
            model_id: Some("grok-4".to_string()),
            reasoning_effort: None,
            yolo: true,
            activity: agent::RosterActivity::Working,
            last_turn_summary: Some("Fixed the roster merge".to_string()),
            resident: true,
            last_change_unix_ms: 1_725_000_000_123,
            origin: agent::RosterOrigin::Local,
        }
    }

    /// Serialize the agent's `RosterListResponse` exactly as `handle_roster_list` does and confirm the pager recovers the session.
    /// Reproduces the production bug: a direct `from_str::<RosterListResponse>` on the enveloped body silently succeeds with an empty roster.
    /// The `naive` assertion below pins that trap; `parse_roster_list_response` must unwrap `result` first.
    #[test]
    fn roster_list_response_survives_result_envelope() {
        use distill_shell::agent::roster as agent;
        use distill_shell::session::ExtMethodResult;

        let agent_resp = agent::RosterListResponse {
            sessions: vec![agent_entry()],
        };

        // Exact wire bytes the agent emits for `x.ai/sessions/list`.
        let ext_response = ExtMethodResult::success(agent_resp)
            .to_ext_response()
            .expect("agent serializes the roster response");
        let body: &str = ext_response.0.get();
        assert!(
            body.contains("\"result\""),
            "agent wraps the payload in a `result` envelope: {body}"
        );

        // Repro of the original bug mechanism: a naive direct deserialize of the wrapped body succeeds but drops every session
        let naive: RosterListResponse =
            serde_json::from_str(body).expect("naive parse succeeds (that is the trap)");
        assert!(
            naive.sessions.is_empty(),
            "naive direct parse silently drops sessions from the envelope"
        );

        // The fixed parser unwraps `result` first and recovers the session.
        let parsed = parse_roster_list_response(body).expect("fixed parser must parse");
        assert_eq!(
            parsed.sessions.len(),
            1,
            "session must survive the `result` envelope"
        );
        let Some(e) = parsed.sessions.first() else {
            panic!("expected session: {:?}", parsed.sessions);
        };
        assert_eq!(e.session_id, "sess-abc");
        assert_eq!(e.title.as_deref(), Some("Fix the roster"));
        assert_eq!(e.cwd, "/repo/worktree");
        assert!(e.is_worktree);
        assert_eq!(e.model_id.as_deref(), Some("grok-4"));
        assert!(e.yolo);
        assert_eq!(e.activity, RosterActivity::Working);
        assert_eq!(
            e.last_turn_summary.as_deref(),
            Some("Fixed the roster merge")
        );
        assert!(e.resident);
        assert_eq!(e.last_change_unix_ms, 1_725_000_000_123);
        assert_eq!(e.origin.kind, "local");
    }

    /// A leader-synthesized Cursor worker row (`sessionKind`, no model, not resident) parses with
    /// its kind visible, so the dashboard can tell it from a loadable session.
    #[test]
    fn roster_list_response_keeps_cursor_worker_rows() {
        use distill_shell::agent::roster as agent;
        use distill_shell::session::ExtMethodResult;

        let cursor_row = agent::RosterEntry {
            session_id: "cursor-worker:bc-1".to_string(),
            title: Some("External agent bc-1".to_string()),
            cwd: "/home/u/.grok/worktrees/proj/cursor-bc-1".to_string(),
            is_worktree: true,
            session_kind: Some("cursor-worker".to_string()),
            model_id: None,
            reasoning_effort: None,
            yolo: false,
            activity: agent::RosterActivity::Idle,
            last_turn_summary: None,
            resident: false,
            last_change_unix_ms: 1_762_000_000_000,
            origin: agent::RosterOrigin::Local,
        };
        let ext_response = ExtMethodResult::success(agent::RosterListResponse {
            sessions: vec![agent_entry(), cursor_row],
        })
        .to_ext_response()
        .expect("agent serializes the roster response");

        let parsed = parse_roster_list_response(ext_response.0.get()).expect("parses");
        let [live, row] = parsed.sessions.as_slice() else {
            panic!("expected two roster rows, got {}", parsed.sessions.len());
        };
        assert_eq!(None, live.session_kind);
        assert_eq!("cursor-worker:bc-1", row.session_id);
        assert_eq!(Some("cursor-worker"), row.session_kind.as_deref());
        assert_eq!(None, row.model_id);
        assert!(!row.resident);
        assert_eq!(RosterActivity::Idle, row.activity);
    }

    /// A bare `{ "sessions": [...] }` body (no `result` envelope) must still parse; the parser tolerates both shapes.
    #[test]
    fn roster_list_response_parses_bare_body() {
        let body = r#"{"sessions":[{"sessionId":"s1","cwd":"/x","isWorktree":false,"yolo":false,"activity":"idle","resident":true,"lastChangeUnixMs":7,"origin":{"kind":"local"}}]}"#;
        let parsed = parse_roster_list_response(body).expect("bare body parses");
        assert_eq!(parsed.sessions.len(), 1);
        assert_eq!(
            parsed.sessions.first().map(|s| s.session_id.as_str()),
            Some("s1")
        );
    }

    /// Round-trip for `x.ai/sessions/changed`: serialize the agent's `RosterChanged` as `emit_roster_changed` does (bare params, no envelope).
    /// The pager's `RosterChanged` must recover `upserted`, `removed`, and the nested entry fields (camelCase).
    #[test]
    fn roster_changed_round_trips() {
        use distill_shell::agent::roster as agent;

        let agent_changed = agent::RosterChanged {
            upserted: vec![agent_entry()],
            removed: vec!["sess-gone".to_string()],
        };
        // `emit_roster_changed` serializes the bare payload (no envelope).
        let params = serde_json::to_string(&agent_changed).expect("serialize RosterChanged");

        let parsed: RosterChanged =
            serde_json::from_str(&params).expect("pager parses the broadcast params");
        assert_eq!(parsed.upserted.len(), 1, "upserted entry must survive");
        let Some(upserted) = parsed.upserted.first() else {
            panic!("expected upserted: {:?}", parsed.upserted);
        };
        assert_eq!(upserted.session_id, "sess-abc");
        assert_eq!(upserted.activity, RosterActivity::Working);
        assert_eq!(parsed.removed, vec!["sess-gone".to_string()]);
    }
}
