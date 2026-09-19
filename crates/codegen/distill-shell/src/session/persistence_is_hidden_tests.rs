use super::*;

fn summary_with_kind(kind: Option<&str>) -> Summary {
    Summary {
        session_kind: kind.map(String::from),
        hidden: None,
        ..Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap()
    }
}

#[test]
fn summary_round_trips_and_defaults_reasoning_effort() {
    let mut s = summary_with_kind(None);
    s.reasoning_effort = None;
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        !json.contains("reasoning_effort"),
        "a None effort must not be serialized"
    );
    let back: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reasoning_effort, None);

    s.reasoning_effort = Some(ReasoningEffort::Xhigh);
    let json = serde_json::to_string(&s).unwrap();
    let back: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Xhigh));
}

#[test]
fn hidden_for_all_subagent_kinds() {
    for kind in ["subagent", "subagent_fork", "subagent_resume"] {
        assert!(
            summary_with_kind(Some(kind)).is_hidden(),
            "{kind} should be hidden"
        );
    }
}

#[test]
fn not_hidden_for_regular_sessions() {
    assert!(!summary_with_kind(None).is_hidden());
    assert!(!summary_with_kind(Some("fork")).is_hidden());
    assert!(!summary_with_kind(Some("worktree")).is_hidden());
}

#[test]
fn headless_is_listable_but_flagged() {
    let headless = summary_with_kind(Some("headless"));
    assert!(!headless.is_hidden(), "headless must stay listable");
    assert!(headless.is_headless());

    assert!(!summary_with_kind(None).is_headless());
    assert!(!summary_with_kind(Some("fork")).is_headless());
    assert!(!summary_with_kind(Some("subagent")).is_headless());
}

#[test]
fn explicit_hidden_overrides_session_kind() {
    let mut s = summary_with_kind(Some("subagent"));
    s.hidden = Some(false);
    assert!(!s.is_hidden(), "explicit hidden=false overrides kind");

    let mut s = summary_with_kind(None);
    s.hidden = Some(true);
    assert!(s.is_hidden(), "explicit hidden=true overrides kind");
}

#[test]
fn unused_husk_includes_worktree_stamped_empty() {
    let mut s = summary_with_kind(Some("worktree"));
    s.worktree_label = Some("fix-bug".into());
    assert!(
        s.is_unused_optimistic_husk(),
        "0-message untitled worktree stamp is still a husk"
    );
}

#[test]
fn unused_husk_excludes_fork_and_titled_or_used() {
    assert!(!summary_with_kind(Some("fork")).is_unused_optimistic_husk());

    let mut worktree_fork = summary_with_kind(Some("worktree"));
    worktree_fork.worktree_label = Some("fix-bug".into());
    worktree_fork.parent_session_id = Some("parent-session".into());
    worktree_fork.forked_at = Some(Utc::now());
    assert!(
        !worktree_fork.is_unused_optimistic_husk(),
        "worktree fork provenance must stay visible"
    );

    let mut titled = summary_with_kind(Some("worktree"));
    titled.generated_title = Some("named".into());
    assert!(!titled.is_unused_optimistic_husk());

    let mut used = summary_with_kind(None);
    used.num_messages = 1;
    assert!(!used.is_unused_optimistic_husk());
}
