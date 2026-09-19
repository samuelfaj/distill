use super::*;

#[test]
fn summary_round_trips_generated_title_through_json() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.generated_title = Some("Refactor auth middleware".into());
    summary.worktree_label = Some("auth-refactor".into());

    let json = serde_json::to_string(&summary).unwrap();
    let deserialized: Summary = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized.generated_title.as_deref(),
        Some("Refactor auth middleware")
    );
    assert_eq!(
        deserialized.worktree_label.as_deref(),
        Some("auth-refactor")
    );
}

#[test]
fn summary_deserializes_without_new_fields_backward_compat() {
    let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model"
        }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert!(summary.generated_title.is_none());
    assert!(summary.worktree_label.is_none());
    assert_eq!(summary.session_summary, "first prompt text");
}

#[test]
fn summary_skips_none_generated_title_in_json() {
    let summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains("generated_title"));
    assert!(!json.contains("worktree_label"));
}

#[test]
fn summary_includes_generated_title_when_set() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.generated_title = Some("Fix K8s deployment".into());
    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("generated_title"));
    assert!(json.contains("Fix K8s deployment"));
}

#[test]
fn summary_deserializes_with_all_fields_present() {
    let json = r#"{
            "info": { "id": "full-session", "cwd": "/tmp" },
            "session_summary": "first prompt",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 10,
            "num_chat_messages": 5,
            "current_model_id": "test-model",
            "head_branch": "feature/xyz",
            "git_root_dir": "/home/user/myrepo",
            "generated_title": "Implement XYZ feature",
            "worktree_label": "xyz-feature"
        }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert_eq!(
        summary.generated_title.as_deref(),
        Some("Implement XYZ feature")
    );
    assert_eq!(summary.worktree_label.as_deref(), Some("xyz-feature"));
    assert_eq!(summary.head_branch.as_deref(), Some("feature/xyz"));
    assert_eq!(summary.git_root_dir.as_deref(), Some("/home/user/myrepo"));
}

// ── display_title direct tests ──────────────────────────────────────

#[test]
fn display_title_returns_generated_title_when_set() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.generated_title = Some("Refactor auth layer".into());
    assert_eq!(summary.display_title(), "Refactor auth layer");
}

#[test]
fn display_title_falls_back_on_empty_generated_title() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.session_summary = "first prompt fallback".into();
    summary.generated_title = Some(String::new());
    assert_eq!(summary.display_title(), "first prompt fallback");
}

#[test]
fn display_title_falls_back_on_none_generated_title() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.session_summary = "session summary fallback".into();
    summary.generated_title = None;
    assert_eq!(summary.display_title(), "session summary fallback");
}

// ── title_is_manual / manual_title_opt ──────────────────────────────

#[test]
fn title_is_manual_round_trips_through_json() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.generated_title = Some("Manual Title".into());
    summary.title_is_manual = true;

    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("title_is_manual"));
    let deserialized: Summary = serde_json::from_str(&json).unwrap();

    assert!(deserialized.title_is_manual);
    assert_eq!(
        deserialized.manual_title_opt().as_deref(),
        Some("Manual Title")
    );
}

#[test]
fn title_is_manual_defaults_false_and_skips_when_unset() {
    // Old summary.json without the field: default false, so pre-existing renames show no border title until renamed again
    let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model",
            "generated_title": "Old Rename"
        }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert!(!summary.title_is_manual);
    assert!(summary.manual_title_opt().is_none());
    assert_eq!(summary.display_title_opt().as_deref(), Some("Old Rename"));

    // And false is omitted on write, keeping old files byte-stable.
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains("title_is_manual"));
}

#[test]
fn manual_title_opt_none_for_auto_generated_title() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.generated_title = Some("Auto Title".into());

    assert!(summary.manual_title_opt().is_none());
    assert_eq!(summary.display_title_opt().as_deref(), Some("Auto Title"));
}

/// A stale `title_is_manual` over a blank `generated_title` must not relabel the `session_summary` display fallback as manual.
/// An old client could write that combination before the ext boundary rejected blank renames.
#[test]
fn manual_title_opt_ignores_stale_flag_over_blank_generated_title() {
    let mut summary = Summary::new(
        &Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    summary.session_summary = "auto first-prompt summary".into();
    summary.generated_title = Some("   ".into());
    summary.title_is_manual = true;

    assert!(summary.manual_title_opt().is_none());
    assert_eq!(
        summary.display_title_opt().as_deref(),
        Some("auto first-prompt summary")
    );
}
