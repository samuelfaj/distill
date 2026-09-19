use super::*;

/// For payloads with no `remote` rows: reaching the resolver is a test failure.
fn parse(payload: Value) -> Vec<SessionPickerEntry> {
    parse_session_picker_entries_with(payload, LocalPresence::Relabel, |ids| {
        panic!("no local lookup expected for {ids:?}")
    })
    .unwrap()
}

fn row(id: &str, source: &str) -> Value {
    serde_json::json!({
        "sessionId": id,
        "cwd": "/Users/me/xai",
        "summary": id,
        "source": source,
        "updatedAt": chrono::Utc::now().to_rfc3339(),
    })
}

fn conversation_row(id: &str) -> Value {
    serde_json::json!({
        "sessionId": id,
        "cwd": "",
        "summary": "chat",
        "source": "conversation",
        "_meta": { "x.ai/session": { "kind": "chat" } }
    })
}

#[test]
fn picker_keeps_conversation_with_empty_cwd_and_missing_updated_at() {
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "conv_abc",
            "cwd": "",
            "summary": "Compare GPU vendors",
            "source": "conversation",
            "_meta": { "x.ai/session": { "kind": "chat" } }
        }]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 1, "conversation must not vanish");
    let Some(entry) = entries.first() else {
        panic!("expected conversation: {entries:?}");
    };
    assert_eq!(entry.id, "conv_abc");
    assert_eq!(entry.cwd, "");
    assert_eq!(entry.source, "conversation");
}

#[test]
fn picker_keeps_old_conversation_past_cutoff() {
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "conv_old",
            "cwd": "",
            "summary": "Ancient chat",
            "source": "conversation",
            "updatedAt": "2020-01-01T00:00:00Z",
            "_meta": { "x.ai/session": { "kind": "chat" } }
        }]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 1, "old conversation must still render");
    assert_eq!(
        entries.first().map(|e| e.source.as_str()),
        Some("conversation")
    );
}

#[test]
fn picker_drops_local_with_missing_updated_at() {
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "local_no_ts",
            "cwd": "/Users/me/xai",
            "summary": "no timestamp",
            "source": "local"
        }]
    });
    let entries = parse(payload);
    assert!(
        entries.is_empty(),
        "local rows still require a parseable updatedAt"
    );
}

#[test]
fn picker_keeps_untitled_conversation_as_untitled() {
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "conv_untitled",
            "cwd": "",
            "summary": "",
            "source": "conversation",
            "updatedAt": "2026-07-01T00:00:00Z",
            "_meta": { "x.ai/session": { "kind": "chat" } }
        }]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 1, "untitled conversation must not vanish");
    let Some(entry) = entries.first() else {
        panic!("expected untitled conversation: {entries:?}");
    };
    assert_eq!(entry.summary, "Untitled");
    assert_eq!(entry.source, "conversation");
}

#[test]
fn picker_parses_last_recap_and_last_turn_summary() {
    // Use a fresh timestamp so the row is inside the 30-day list cutoff.
    let recent = chrono::Utc::now().to_rfc3339();
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "s_recap",
            "cwd": "/Users/me/xai",
            "summary": "Auth refactor",
            "source": "local",
            "updatedAt": recent,
            "lastTurnSummary": "Wired retries into billing",
            "lastRecap": "Where we left off: auth refactor across the API"
        }]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 1);
    let Some(entry) = entries.first() else {
        panic!("expected recap entry: {entries:?}");
    };
    assert_eq!(
        entry.last_turn_summary.as_deref(),
        Some("Wired retries into billing")
    );
    assert_eq!(
        entry.last_recap.as_deref(),
        Some("Where we left off: auth refactor across the API")
    );
}

#[test]
fn picker_parses_session_kind() {
    let recent = chrono::Utc::now().to_rfc3339();
    let payload = serde_json::json!({
        "sessions": [
            {
                "sessionId": "s_headless",
                "cwd": "/Users/me/xai",
                "summary": "Classify clip",
                "source": "local",
                "updatedAt": recent,
                "sessionKind": "headless"
            },
            {
                "sessionId": "s_plain",
                "cwd": "/Users/me/xai",
                "summary": "Interactive work",
                "source": "local",
                "updatedAt": recent
            }
        ]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 2);
    let [headless, plain] = entries.as_slice() else {
        panic!("expected two session-kind entries: {entries:?}");
    };
    assert_eq!(headless.session_kind.as_deref(), Some("headless"));
    assert_eq!(plain.session_kind, None);
}

#[test]
fn picker_relabels_remote_rows_with_one_batched_lookup() {
    let payload = serde_json::json!({
        "sessions": [
            row("remote_on_disk", "remote"),
            row("remote_elsewhere", "remote"),
            row("already_local", "local"),
            row("on_both", "both"),
            row("remote_missing", "remote"),
        ]
    });

    let entries =
        parse_session_picker_entries_with(payload, LocalPresence::Relabel, |remote_ids| {
            assert_eq!(
                remote_ids,
                ["remote_on_disk", "remote_elsewhere", "remote_missing"]
            );
            Ok(["remote_on_disk", "remote_elsewhere"]
                .map(str::to_owned)
                .into_iter()
                .collect())
        })
        .unwrap();

    let labels: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.source.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            ("remote_on_disk", "local"),
            ("remote_elsewhere", "local"),
            ("already_local", "local"),
            ("on_both", "both"),
            ("remote_missing", "remote"),
        ]
    );
}

#[test]
fn session_list_response_unwraps_result_envelope_or_takes_bare_payload() {
    let bare = r#"{"sessions":[{"sessionId":"a"}]}"#;
    let wrapped = r#"{"result":{"sessions":[{"sessionId":"a"}]}}"#;
    let expected = serde_json::json!({"sessions": [{"sessionId": "a"}]});

    assert_eq!(read_session_list_response(bare).unwrap(), expected);
    assert_eq!(read_session_list_response(wrapped).unwrap(), expected);
    assert_eq!(
        read_session_list_response(r#"{"result":null}"#).unwrap(),
        serde_json::Value::Null
    );
    assert_eq!(
        read_session_list_response("not json").unwrap(),
        serde_json::Value::Null
    );
}

#[test]
fn session_list_response_surfaces_error_envelope() {
    assert_eq!(
        read_session_list_response(r#"{"error":"agent unavailable"}"#),
        Err("agent unavailable".to_owned())
    );
    assert_eq!(
        read_session_list_response(r#"{"error":{"code":-32000}}"#),
        Err("unknown error".to_owned())
    );
}

/// A grok.com chat and a Build session can carry the same id; resolving the Build id must not pull the chat off the conversation load path.
#[test]
fn picker_relabel_leaves_conversation_row_sharing_a_remote_id() {
    let recent = chrono::Utc::now().to_rfc3339();
    let payload = serde_json::json!({
        "sessions": [
            {
                "sessionId": "shared_id",
                "cwd": "/Users/me/xai",
                "summary": "build session",
                "source": "remote",
                "updatedAt": recent,
            },
            {
                "sessionId": "shared_id",
                "cwd": "",
                "summary": "chat",
                "source": "conversation",
                "_meta": { "x.ai/session": { "kind": "chat" } }
            }
        ]
    });
    let entries =
        parse_session_picker_entries_with(payload, LocalPresence::Relabel, |remote_ids| {
            assert_eq!(remote_ids, ["shared_id"]);
            Ok(std::iter::once("shared_id".to_owned()).collect())
        })
        .unwrap();
    let labels: Vec<&str> = entries.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(labels, ["local", "conversation"]);
}

#[test]
fn picker_skips_local_lookup_when_no_remote_rows() {
    let recent = chrono::Utc::now().to_rfc3339();
    let payload = serde_json::json!({
        "sessions": [
            {
                "sessionId": "local_only",
                "cwd": "/Users/me/xai",
                "summary": "local",
                "source": "local",
                "updatedAt": recent,
            },
            {
                "sessionId": "conv",
                "cwd": "",
                "summary": "chat",
                "source": "conversation",
                "_meta": { "x.ai/session": { "kind": "chat" } }
            }
        ]
    });
    let entries = parse(payload);
    assert_eq!(entries.len(), 2);
}

#[test]
fn picker_still_drops_build_row_with_empty_summary() {
    let payload = serde_json::json!({
        "sessions": [{
            "sessionId": "local_empty",
            "cwd": "/nonexistent/effects-test",
            "summary": "",
            "source": "local",
            "updatedAt": chrono::Utc::now().to_rfc3339()
        }]
    });
    let entries = parse(payload);
    assert!(entries.is_empty(), "empty-summary Build rows stay dropped");
}

#[test]
fn session_list_partial_parses_reasons() {
    let payload = |reason: &str| {
        serde_json::json!({
            "sessions": [],
            "_meta": { "x.ai/partial": { "conversations": true, "reason": reason } }
        })
    };
    assert_eq!(
        parse_session_list_partial(&payload("no_oauth")),
        Some(ConversationsPartial::NoOauth)
    );
    assert_eq!(
        parse_session_list_partial(&payload("timeout")),
        Some(ConversationsPartial::Timeout)
    );
    assert_eq!(
        parse_session_list_partial(&payload("error")),
        Some(ConversationsPartial::Error)
    );
    assert_eq!(
        parse_session_list_partial(&payload("something_new")),
        Some(ConversationsPartial::Error)
    );
}

#[test]
fn session_list_partial_absent_for_healthy_or_meta_less_responses() {
    let healthy = serde_json::json!({
        "sessions": [],
        "_meta": { "x.ai/partial": { "conversations": false } }
    });
    assert_eq!(parse_session_list_partial(&healthy), None);
    let legacy = serde_json::json!({ "sessions": [] });
    assert_eq!(parse_session_list_partial(&legacy), None);
}

#[test]
fn dashboard_presence_resolves_every_candidate_in_one_call() {
    let payload = serde_json::json!({
        "sessions": [
            row("on_disk_local", "local"),
            row("on_disk_remote", "remote"),
            row("on_disk_both", "both"),
            row("gone_local", "local"),
            row("foreign", "cursor"),
            conversation_row("conv"),
        ]
    });

    let entries = parse_session_picker_entries_with(payload, LocalPresence::Require, |ids| {
        assert_eq!(
            ids,
            ["on_disk_local", "on_disk_remote", "on_disk_both", "gone_local"],
            "conversation and foreign rows are dropped before the walk; every other row is a candidate"
        );
        Ok(["on_disk_local", "on_disk_remote", "on_disk_both"]
            .map(str::to_owned)
            .into_iter()
            .collect())
    })
    .unwrap();

    let labels: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.source.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            ("on_disk_local", "local"),
            ("on_disk_remote", "local"),
            ("on_disk_both", "local"),
        ]
    );
}

#[test]
fn dashboard_presence_skips_the_walk_when_no_row_can_be_local() {
    let payload = serde_json::json!({
        "sessions": [row("foreign", "cursor"), conversation_row("conv")]
    });

    let entries = parse_session_picker_entries_with(payload, LocalPresence::Require, |ids| {
        panic!("no local lookup expected for {ids:?}")
    })
    .unwrap();

    assert!(entries.is_empty());
}

#[test]
fn dashboard_presence_surfaces_resolution_failure() {
    let payload = serde_json::json!({ "sessions": [row("local", "local")] });

    let error = parse_session_picker_entries_with(payload, LocalPresence::Require, |_| {
        Err("disk failed".to_owned())
    })
    .unwrap_err();

    assert!(error.contains("disk failed"), "{error}");
}

#[test]
fn relabel_presence_keeps_shell_labels_when_resolution_fails() {
    let payload = serde_json::json!({ "sessions": [row("remote_row", "remote")] });

    let entries = parse_session_picker_entries_with(payload, LocalPresence::Relabel, |_| {
        Err("disk failed".to_owned())
    })
    .unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries.first().map(|e| e.source.as_str()), Some("remote"));
}

#[test]
fn session_picker_entry_maps_to_dormant_roster_row() {
    let updated = chrono::Utc::now();
    let entry = SessionPickerEntry {
        id: "sess-1".to_string(),
        summary: "Wire up dashboard".to_string(),
        updated_at: updated,
        created_at: updated,
        cwd: "/repo/app".to_string(),
        hostname: Some("box".to_string()),
        source: "local".to_string(),
        model_id: Some("grok-4".to_string()),
        num_messages: 3,
        last_active_at: Some(updated),
        branch: None,
        repo_name: "repo-app".to_string(),
        worktree_label: Some("wt".to_string()),
        last_turn_summary: Some("Fixed the parser".to_string()),
        last_recap: None,
        session_kind: None,
        card_detail: None,
    };

    let roster = session_picker_entry_to_roster(entry);
    assert_eq!(roster.session_id, "sess-1");
    assert_eq!(roster.title.as_deref(), Some("Wire up dashboard"));
    assert_eq!(roster.cwd, "/repo/app");
    assert!(roster.is_worktree, "worktree_label present → is_worktree");
    assert_eq!(roster.model_id.as_deref(), Some("grok-4"));
    assert_eq!(roster.activity, RosterActivity::Dormant);
    assert_eq!(
        roster.last_turn_summary.as_deref(),
        Some("Fixed the parser")
    );
    assert!(!roster.resident);
    assert_eq!(roster.last_change_unix_ms, updated.timestamp_millis());
    assert_eq!(roster.origin.kind, "local");
    assert_eq!(roster.origin.host.as_deref(), Some("box"));
}
