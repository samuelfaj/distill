use super::*;

#[test]
fn summary_round_trips_head_fields_through_json() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.head_commit = Some("abc123def456".into());
    summary.head_branch = Some("main".into());

    let json = serde_json::to_string(&summary).unwrap();
    let deserialized: Summary = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.head_commit.as_deref(), Some("abc123def456"));
    assert_eq!(deserialized.head_branch.as_deref(), Some("main"));
}

#[test]
fn summary_deserializes_without_head_fields_backward_compat() {
    let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert!(summary.agent_id.is_none());
    assert!(summary.attempt_id.is_none());
    assert!(summary.head_commit.is_none());
    assert!(summary.head_branch.is_none());
}

#[test]
fn session_identity_start_resume_and_fork() {
    let started = next_session_identity(None, false, (0x11, 0x22));
    assert_eq!(started.agent_id, "ag1.11");
    assert_eq!(started.attempt_id, "at1.22");

    let resumed = next_session_identity(Some(&started.agent_id), false, (0x33, 0x44));
    assert_eq!(resumed.agent_id, started.agent_id);
    assert_eq!(resumed.attempt_id, "at1.44");

    let forked = next_session_identity(Some(&resumed.agent_id), true, (0x55, 0x66));
    assert_eq!(forked.agent_id, "ag1.55");
    assert_ne!(forked.agent_id, resumed.agent_id);
    assert_eq!(forked.attempt_id, "at1.66");

    let invalid_legacy = next_session_identity(Some("legacy-session-id"), false, (0x77, 0x88));
    assert_eq!(invalid_legacy.agent_id, "ag1.77");
    assert_eq!(invalid_legacy.attempt_id, "at1.88");
}

#[test]
fn summary_relocation_metadata_is_backward_compatible() {
    let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert_eq!(summary.cwd_generation, 0);
    assert!(summary.previous_cwd.is_none());
    assert!(summary.pending_cwd_switch_reminder.is_none());
    assert_eq!(summary.cwd_switch_bookkeeping_generation, 0);

    let serialized = serde_json::to_value(summary).unwrap();
    for field in [
        "cwd_generation",
        "previous_cwd",
        "pending_cwd_switch_reminder",
        "cwd_switch_bookkeeping_generation",
    ] {
        assert!(serialized.get(field).is_none());
    }
}

#[test]
fn summary_relocation_metadata_round_trips() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/new".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.cwd_generation = 2;
    summary.previous_cwd = Some("/old".into());
    summary.pending_cwd_switch_reminder = Some(PendingCwdSwitchReminder {
        cwd_generation: 2,
        previous_cwd: "/old".into(),
        destination_cwd: "/new".into(),
        content: "moved".into(),
        destination_project_instructions: Some("target rules".into()),
    });

    let serialized = serde_json::to_value(&summary).unwrap();
    assert_eq!(
        serialized
            .get("pending_cwd_switch_reminder")
            .and_then(|v| v.get("destination_cwd")),
        Some(&serde_json::json!("/new"))
    );
    assert!(
        serialized
            .get("pending_cwd_switch_reminder")
            .and_then(|v| v.get("cwd"))
            .is_none()
    );
    let back: Summary = serde_json::from_value(serialized).unwrap();
    assert_eq!(back.cwd_generation, 2);
    assert_eq!(back.previous_cwd.as_deref(), Some("/old"));
    assert_eq!(
        back.pending_cwd_switch_reminder,
        summary.pending_cwd_switch_reminder
    );
    assert_eq!(back.info.cwd, "/new");

    let pending: PendingCwdSwitchReminder = serde_json::from_value(serde_json::json!({
        "cwd_generation": 2,
        "previous_cwd": "/old",
        "cwd": "/new",
        "content": "moved"
    }))
    .unwrap();
    assert_eq!(pending.destination_cwd, "/new");
}

#[test]
fn summary_skips_none_head_fields_in_serialized_json() {
    let summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    // In a non-git directory the fields will be None.
    let json = serde_json::to_string(&summary).unwrap();
    // The test cwd may be inside a git repo, so assert skip_serializing_if omission only when the field is None
    if summary.head_commit.is_none() {
        assert!(!json.contains("head_commit"));
    }
    if summary.head_branch.is_none() {
        assert!(!json.contains("head_branch"));
    }
}
