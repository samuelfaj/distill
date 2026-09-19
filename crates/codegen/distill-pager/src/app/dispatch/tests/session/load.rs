// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for session loading, restore, pickers, and deep search.
use super::*;
fn expect_agent(app: &AppView, id: AgentId) -> &AgentView {
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    agent
}
use crate::views::modal::ActiveModal;
use distill_shell::session::unified_list::ListScope;
/// Opening the cancel-turn picker while scrollback is focused must hand keyboard focus to the picker.
/// Otherwise up/down keys go to scrollback and the modal is only navigable via mouse.
#[test]
fn cancel_turn_picker_grabs_focus_from_scrollback() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent
            .subagent_sessions
            .insert("child-1".into(), make_test_subagent("child-1", "sa-1"));
        agent.active_pane = ActivePane::Scrollback;
    }
    let effects = dispatch(Action::CancelTurn, &mut app);
    assert!(effects.is_empty());
    assert!(expect_agent(&app, id).cancel_turn_view.is_some());
    assert_eq!(
        expect_agent(&app, id).active_pane,
        ActivePane::Prompt,
        "picker should steal focus from scrollback so keyboard navigation works"
    );
}
#[test]
fn session_loaded_with_restore_shows_summary_in_scrollback() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-restore".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-restore"),
            models: None,
            modes: None,
            code_restored: true,
            restore_summary: Some(
                "checked out abc12345, staged: true, unstaged: false, untracked: 3".into(),
            ),
            restore_degree: Some(distill_workspace::session::git::RestoreDegree::Full),
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::HydrateSessionMetaFromDisk { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    let has_restore_msg = expect_agent(&app, id)
        .scrollback
        .entries_in_range(0..expect_agent(&app, id).scrollback.len())
        .iter()
        .any(|e| matches!(&e.block, RenderBlock::System(s) if s.text.contains("Code restored")));
    assert!(has_restore_msg, "expected restore summary in scrollback");
    assert_eq!(
        expect_agent(&app, id).session.restore_degree,
        Some(distill_workspace::session::git::RestoreDegree::Full),
        "SessionLoaded must store restore_degree on the session"
    );
}
/// Hydrating an auto-generated title sets only `generated_session_title`, never `display_name` (the border title).
#[test]
fn session_title_hydration_auto_leaves_display_name_none() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("Auto Title".into(), false)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    let agent = &expect_agent(&app, id);
    assert_eq!(agent.generated_session_title.as_deref(), Some("Auto Title"));
    assert!(
        agent.display_name.is_none(),
        "auto titles must not restore the manual-rename border title"
    );
}
/// A manual title restores `display_name` (the prompt-border title), but only while the field is still empty.
/// A rename made while the disk read was in flight must never be clobbered by the stale on-disk title.
#[test]
fn session_title_hydration_manual_restores_display_name_cold_cache_only() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("Disk Title".into(), true)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    {
        let agent = &expect_agent(&app, id);
        assert_eq!(agent.display_name.as_deref(), Some("Disk Title"));
        assert_eq!(agent.generated_session_title.as_deref(), Some("Disk Title"));
    }
    app.agents.get_mut(&id).unwrap().display_name = Some("Fresh Rename".into());
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("Stale Disk Title".into(), true)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).display_name.as_deref(),
        Some("Fresh Rename"),
        "hydration is a cold-cache fallback, never an overwrite"
    );
    assert_eq!(
        expect_agent(&app, id).generated_session_title.as_deref(),
        Some("Disk Title"),
        "generated_session_title is also cold-cache; a live title must not be clobbered"
    );
}
/// A live auto title (SessionSummaryGenerated) that wins the race with the disk read must not be replaced.
/// The `/rename` ghost-prefill falls back to this field.
#[test]
fn session_title_hydration_does_not_clobber_live_generated_title() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().generated_session_title = Some("Live Auto".into());
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("Stale Disk Title".into(), false)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    let agent = &expect_agent(&app, id);
    assert_eq!(
        agent.generated_session_title.as_deref(),
        Some("Live Auto"),
        "late disk hydrate must not replace a live generated title"
    );
    assert!(
        agent.display_name.is_none(),
        "auto disk titles must not restore display_name"
    );
}
/// Whitespace-only titles from disk are ignored entirely, manual or not.
#[test]
fn session_title_hydration_ignores_blank_title() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("   ".into(), true)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    let agent = &expect_agent(&app, id);
    assert!(agent.display_name.is_none());
    assert!(agent.generated_session_title.is_none());
}
/// A title made only of C0 controls and whitespace strips to blank, so skip it rather than restoring the unsanitized string into `display_name`.
#[test]
fn session_title_hydration_skips_control_only_title() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some(("\u{1b}\u{07}\n\t".into(), true)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    let agent = &expect_agent(&app, id);
    assert!(
        agent.display_name.is_none(),
        "control-only title must not land in display_name, got {:?}",
        agent.display_name
    );
    assert!(
        agent.generated_session_title.is_none(),
        "control-only title must not land in generated_session_title, got {:?}",
        agent.generated_session_title
    );
}
/// Dirty-but-nonempty on-disk titles are stripped then capped before restore.
#[test]
fn session_title_hydration_sanitizes_and_caps_dirty_title() {
    use distill_shell::session::persistence::MAX_TITLE_SCALARS;
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    let dirty = format!(
        "ok\u{1b}]0;PWNED\u{07}{}",
        "é".repeat(MAX_TITLE_SCALARS + 5)
    );
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: Some((dirty, true)),
            last_turn_summary: None,
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    const PREFIX: &str = "ok]0;PWNED";
    let expected = format!(
        "{PREFIX}{}",
        "é".repeat(MAX_TITLE_SCALARS - PREFIX.chars().count())
    );
    let agent = &expect_agent(&app, id);
    assert_eq!(agent.display_name.as_deref(), Some(expected.as_str()));
    assert_eq!(
        agent.generated_session_title.as_deref(),
        Some(expected.as_str())
    );
}
/// The persisted last-turn summary hydrates only while the field is still empty.
/// A value already set by a live `LastTurnSummary` delivery (always newer than any disk read) must not be overwritten by the slower disk result.
#[test]
fn last_turn_summary_hydration_is_cold_cache_only() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: None,
            last_turn_summary: Some("From disk".into()),
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).last_turn_summary.as_deref(),
        Some("From disk")
    );
    app.agents
        .get_mut(&id)
        .unwrap()
        .set_last_turn_summary(Some("Live delivery".into()));
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: None,
            last_turn_summary: Some("Stale disk read".into()),
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).last_turn_summary.as_deref(),
        Some("Live delivery"),
        "hydration is a cold-cache fallback, never an overwrite"
    );
}
/// A rewind that clears `last_turn_summary` while disk hydration is in flight must not be undone by the late pre-rewind `summary.json` value.
#[test]
fn last_turn_summary_hydration_does_not_restore_after_rewind_clear() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-title".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().set_last_turn_summary(None);
    dispatch(
        Action::TaskComplete(TaskResult::SessionMetaFromDisk {
            agent_id: id,
            title: None,
            last_turn_summary: Some("Pre-rewind disk summary".into()),
            last_turn_summary_gen: 0,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).last_turn_summary,
        None,
        "stale disk hydrate must not re-apply a summary rewind already cleared"
    );
}
/// Replay can leave entries marked running: bg tasks, scheduler runs, tools cut off when the previous process died.
/// The load must sweep them when no live turn adopts them.
/// A stuck running entry holds `needs_animation()` open forever (a permanent ~30fps tick and redraw loop on an idle TUI).
#[test]
fn session_loaded_without_adoption_finishes_replayed_running_entries() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-stuck".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::tool_call("Run something", "info", true));
        agent.scrollback.set_last_running(true);
        assert!(agent.scrollback.needs_animation());
    }
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-stuck"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(
        !agent.scrollback.has_running_entries(),
        "replayed running entries must be finished when no turn is adopted"
    );
}
/// Resume into a cwd with `.git/grok-worktree-source` sets `session.is_worktree`.
#[test]
fn load_session_marks_standalone_worktree_cwd() {
    let mut app = test_app();
    let main = crate::test_util::TempGitRepo::init("main-only");
    let clone = main.standalone_clone("wt-branch");
    dispatch(
        Action::LoadSession("sess-wt".into(), Some(clone.path.clone()), false),
        &mut app,
    );
    assert!(
        expect_agent(&app, AgentId(0)).session.is_worktree,
        "resume into a standalone grok worktree must set session.is_worktree"
    );
    assert_eq!(expect_agent(&app, AgentId(0)).session.cwd, clone.path);
}
#[test]
fn load_session_plain_repo_is_not_worktree() {
    let mut app = test_app();
    let repo = crate::test_util::TempGitRepo::init("main");
    dispatch(
        Action::LoadSession("sess-plain-git".into(), Some(repo.path.clone()), false),
        &mut app,
    );
    assert!(!expect_agent(&app, AgentId(0)).session.is_worktree);
}
#[test]
fn remote_restore_marks_standalone_worktree_cwd() {
    let mut app = test_app();
    let main = crate::test_util::TempGitRepo::init("main-only");
    let clone = main.standalone_clone("wt-branch");
    app.cwd = clone.path.clone();
    let _ = dispatch_load_session_with_restore(
        &mut app,
        "remote-wt".into(),
        clone.path.display().to_string(),
    );
    assert!(expect_agent(&app, AgentId(0)).session.is_worktree);
    assert_eq!(expect_agent(&app, AgentId(0)).session.cwd, clone.path);
}
#[test]
fn remote_restore_plain_repo_is_not_worktree() {
    let mut app = test_app();
    let repo = crate::test_util::TempGitRepo::init("main");
    app.cwd = repo.path.clone();
    let _ = dispatch_load_session_with_restore(
        &mut app,
        "remote-plain".into(),
        repo.path.display().to_string(),
    );
    assert!(!expect_agent(&app, AgentId(0)).session.is_worktree);
}
/// Cross-cwd resume anchors the agent cwd to the resolved origin cwd.
#[test]
fn load_session_anchors_agent_cwd_to_resolved_session_cwd() {
    let mut app = test_app();
    let process_cwd = app.cwd.clone();
    let origin_cwd = PathBuf::from("/some/other/origin-cwd");
    assert_ne!(origin_cwd, process_cwd, "test precondition");
    dispatch(
        Action::LoadSession("sess-xcwd".into(), Some(origin_cwd.clone()), false),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, AgentId(0)).session.cwd,
        origin_cwd,
        "cross-cwd resume must anchor the agent cwd to the session's origin cwd"
    );
}
/// With no resolved cwd (`None`), the agent cwd stays the process cwd.
#[test]
fn load_session_falls_back_to_process_cwd_when_no_session_cwd() {
    let mut app = test_app();
    let process_cwd = app.cwd.clone();
    dispatch(
        Action::LoadSession("sess-samecwd".into(), None, false),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, AgentId(0)).session.cwd,
        process_cwd,
        "same-cwd resume must keep the agent cwd at the process cwd"
    );
}
/// A stale fresh-view load resolving inside an open reconnect reload window must not close the window.
/// Flipping `loading_replay` makes the replay gate drop the rest of the reconnect replay (a truncated transcript reported as a successful restore).
/// The original purge site: completing a session load (no reload window open) drops the replay transient and must purge exactly once.
#[test]
fn session_loaded_purges_replay_transient() {
    use crate::memory_release::test_support;
    test_support::install_counting_hook();
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-purge".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    let before = test_support::calls();
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-purge"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        test_support::calls(),
        before + 1,
        "load completion must purge the dropped replay transient exactly once"
    );
}
#[test]
fn session_loaded_during_open_reload_window_defers_to_window() {
    let mut app = test_app();
    dispatch(Action::LoadSession("sess-w".into(), None, false), &mut app);
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().begin_session_reload(1);
    let suppressed = distill_dashboard_store::SessionId::new("sess-w").unwrap();
    app.workspace_membership
        .suppress_for_test(suppressed.clone());
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-w"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "a load result mid-window must produce no effects (no queue drain)"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.session_reload.is_some(), "the window stays open");
    assert!(agent.session.loading_replay, "the replay gate stays open");
    assert!(
        app.workspace_membership
            .removal_suppressed_for_test(&suppressed),
        "a deferred load must not re-enable workspace adoption"
    );
}
/// Failure variant of the above: no `TurnFailed` block may be pushed into the staging state.
#[test]
fn session_load_failed_during_open_reload_window_defers_to_window() {
    let mut app = test_app();
    dispatch(Action::LoadSession("sess-w".into(), None, false), &mut app);
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().begin_session_reload(1);
    let staging_len = expect_agent(&app, id).scrollback.len();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoadFailed {
            agent_id: id,
            session_id: acp::SessionId::new("sess-w"),
            error: "boom".into(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.session_reload.is_some(), "the window stays open");
    assert!(agent.session.loading_replay);
    assert_eq!(
        agent.scrollback.len(),
        staging_len,
        "no failure block was pushed into staging"
    );
}
/// `SessionRestoreFailed` variant of the defer guard: no `TurnFailed` block may be pushed into staging and the window must stay open.
#[test]
fn session_restore_failed_during_open_reload_window_defers_to_window() {
    let mut app = test_app();
    dispatch(Action::LoadSession("sess-w".into(), None, false), &mut app);
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().begin_session_reload(1);
    let staging_len = expect_agent(&app, id).scrollback.len();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionRestoreFailed {
            agent_id: id,
            error: "boom".into(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.session_reload.is_some(), "the window stays open");
    assert!(agent.session.loading_replay);
    assert_eq!(
        agent.scrollback.len(),
        staging_len,
        "no failure block was pushed into staging"
    );
}
/// `SessionRestoreProgress` variant of the defer guard: no progress block may be pushed into staging.
#[test]
fn session_restore_progress_during_open_reload_window_defers_to_window() {
    let mut app = test_app();
    dispatch(Action::LoadSession("sess-w".into(), None, false), &mut app);
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().begin_session_reload(1);
    let staging_len = expect_agent(&app, id).scrollback.len();
    dispatch(
        Action::TaskComplete(TaskResult::SessionRestoreProgress {
            agent_id: id,
            message: "Downloading...".into(),
        }),
        &mut app,
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.session_reload.is_some(), "the window stays open");
    assert_eq!(
        agent.scrollback.len(),
        staging_len,
        "no progress block was pushed into staging"
    );
}
/// The SessionLoaded path also shows a warning banner when the server reported `code_restored: false` with a summary.
#[test]
fn session_loaded_with_restore_failure_shows_warning_banner() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-fail".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-fail"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: Some(
                "restore aborted (checkout failed); stash skipped: MERGE_HEAD present".into(),
            ),
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    let entries = expect_agent(&app, id)
        .scrollback
        .entries_in_range(0..expect_agent(&app, id).scrollback.len());
    let warn = entries.iter().find_map(|e| match &e.block {
        RenderBlock::System(s) if s.text.contains("Code restore failed") => Some(&s.text),
        _ => None,
    });
    let text = warn.expect("warning banner missing").as_str();
    assert!(
        text.starts_with('\u{26A0}'),
        "expected ⚠ prefix, got: {text}"
    );
    assert!(text.contains("MERGE_HEAD present"));
    assert!(
        !entries.iter().any(|e| matches!(
            &e.block,
            RenderBlock::System(s) if s.text.contains("Code restored")
        )),
        "success banner must not appear on failure"
    );
}
#[test]
fn session_loaded_without_restore_no_summary() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-plain".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-plain"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::HydrateSessionMetaFromDisk { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    let has_restore_msg = expect_agent(&app, id)
        .scrollback
        .entries_in_range(0..expect_agent(&app, id).scrollback.len())
        .iter()
        .any(|e| matches!(&e.block, RenderBlock::System(s) if s.text.contains("Code restored")));
    assert!(!has_restore_msg, "should not have restore summary");
}
/// A second `SessionLoaded` without a restore must reset `restore_degree` to `None`, not keep a stale `Some(Full)` from a previous load.
#[test]
fn session_loaded_without_restore_resets_restore_degree() {
    let mut app = test_app();
    dispatch(Action::LoadSession("sess-r2".into(), None, false), &mut app);
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-r2"),
            models: None,
            modes: None,
            code_restored: true,
            restore_summary: Some("checked out abc".into()),
            restore_degree: Some(distill_workspace::session::git::RestoreDegree::Full),
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).session.restore_degree,
        Some(distill_workspace::session::git::RestoreDegree::Full)
    );
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-r2"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        expect_agent(&app, id).session.restore_degree.is_none(),
        "second load without restore must clear stale degree"
    );
}
#[test]
fn session_loaded_with_flag_emits_five_fetches_and_clears_flag() {
    use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = None;
        a.pending_extensions_fetch = true;
        a.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Hooks));
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("s"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 5);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMcpsList { cache: true, .. }))
    );
    assert!(!expect_agent(&app, id).pending_extensions_fetch);
}
#[test]
fn session_restored_does_not_consume_flag_and_defers_to_load() {
    use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = None;
        a.pending_extensions_fetch = true;
        a.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Hooks));
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionRestored {
            agent_id: id,
            local_session_id: "s".to_string(),
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 0);
    assert!(expect_agent(&app, id).pending_extensions_fetch);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. }))
    );
}
#[test]
fn session_load_failed_clears_flag_no_fetches() {
    use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.pending_extensions_fetch = true;
        a.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Hooks));
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionLoadFailed {
            agent_id: id,
            session_id: acp::SessionId::new("s"),
            error: "boom".to_string(),
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 0);
    assert!(!expect_agent(&app, id).pending_extensions_fetch);
}
#[test]
fn load_session_seeds_available_commands_from_bootstrap() {
    let mut app = test_app();
    app.bootstrap_acp_commands = vec![acp::AvailableCommand::new(
        "session-info".to_string(),
        "Show session info".to_string(),
    )];
    dispatch(
        Action::LoadSession("sess-123".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    assert_eq!(expect_agent(&app, id).session.available_commands.len(), 1);
    assert_eq!(
        expect_agent(&app, id)
            .session
            .available_commands
            .first()
            .map(|c| c.name.as_str()),
        Some("session-info")
    );
    assert_eq!(
        expect_agent(&app, id).session.available_commands_generation,
        1
    );
}
/// Known session id resume always emits LoadSession, never CreateSession.
#[test]
fn resume_known_session_id_loads_not_creates() {
    let mut app = test_app();
    let effects = dispatch(
        Action::LoadSession("resume-known-id".into(), None, false),
        &mut app,
    );
    assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::LoadSession { session_id, .. } if session_id == "resume-known-id")),
            "expected LoadSession, got {effects:?}"
        );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "resume must never CreateSession"
    );
}
/// `SessionRestored` under `--chat` refuses a non-conversation local Build row (no LoadSession; agent torn down).
#[test]
fn session_restored_refuses_local_build_under_chat_mode() {
    let mut app = test_app_with_agent();
    let id = *app.agents.keys().next().unwrap();
    app.chat_mode = true;
    let cwd = app.cwd.clone();
    let session_id = format!("restored-build-{}", std::process::id());
    let sess_dir = plant_local_build_session(&cwd, &session_id);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionRestored {
            agent_id: id,
            local_session_id: session_id,
        }),
        &mut app,
    );
    let _ = std::fs::remove_dir_all(&sess_dir);
    assert!(
        effects.is_empty(),
        "SessionRestored must refuse Build under --chat, got {effects:?}"
    );
    assert!(
        !app.agents.contains_key(&id),
        "placeholder agent must be removed on refuse"
    );
}
/// The `SessionRestored` follow-up LoadSession stays `chat_kind: false` (not a picker conversation entry).
/// Sticky `--chat` with no local disk still opens as chat, so `conversation_entry` and the rename kind are Chat.
#[test]
fn session_restored_sticky_chat_sets_conversation_entry() {
    let mut app = test_app_with_agent();
    let id = *app.agents.keys().next().unwrap();
    app.chat_mode = true;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionRestored {
            agent_id: id,
            local_session_id: "restored_no_disk".into(),
        }),
        &mut app,
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::LoadSession {
            session_id,
            chat_kind: false,
            ..
        }] if session_id == "restored_no_disk"
    ));
    let agent = app.agents.get(&id).expect("agent kept");
    assert!(agent.chat_kind, "agent UI bit comes from sticky --chat");
    assert!(
        agent.conversation_entry,
        "sticky --chat restore with no local disk opens as chat (rename kind)"
    );
    assert_eq!(
        agent.rename_kind(),
        distill_shell::session::unified_list::SessionKind::Chat
    );
}
/// Completing a mid-session login restores the agent view instead of running the startup load-session flow.
#[test]
fn auth_complete_restores_view_after_mid_session_login() {
    let mut app = test_app_with_agent();
    dispatch(Action::Login, &mut app);
    let seq = authenticating_seq(&app);
    assert_eq!(app.active_view, ActiveView::Welcome);
    dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: seq,
            meta: None,
        }),
        &mut app,
    );
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    assert_eq!(app.auth_return_view, None);
    assert!(matches!(app.auth_state, AuthState::Done));
}
#[test]
fn session_loaded_drains_pending_first_prompt_to_front() {
    let mut app = fork_test_app();
    dispatch(
        Action::Fork(fork_args(Some(false), Some("first directive"))),
        &mut app,
    );
    let new_id = AgentId(1);
    app.agents
        .get_mut(&new_id)
        .unwrap()
        .session
        .enqueue_prompt("user-typed prompt".into());
    app.agents.get_mut(&new_id).unwrap().session.session_id = Some("new-fork-sid".into());
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: new_id,
            session_id: "new-fork-sid".into(),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    let queue: Vec<_> = expect_agent(&app, new_id)
        .session
        .pending_prompts
        .iter()
        .map(|p| p.text.clone())
        .collect();
    assert_eq!(queue, vec!["user-typed prompt".to_string()]);
    assert!(
        expect_agent(&app, new_id).pending_first_prompt.is_none(),
        "drained prompt must be cleared"
    );
}
#[test]
fn session_loaded_with_no_pending_first_prompt_does_not_enqueue() {
    let mut app = fork_test_app();
    let id = AgentId(0);
    let queue_before = expect_agent(&app, id).session.pending_prompts.len();
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: "test-session".into(),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).session.pending_prompts.len(),
        queue_before,
        "no enqueue when pending_first_prompt is None"
    );
}
#[test]
fn session_load_failed_clears_pending_first_prompt() {
    let mut app = fork_test_app();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().pending_first_prompt = Some("orphaned directive".into());
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoadFailed {
            agent_id: id,
            session_id: "test-session".into(),
            error: "boom".into(),
        }),
        &mut app,
    );
    assert!(
        expect_agent(&app, id).pending_first_prompt.is_none(),
        "load failure must drop the directive"
    );
}
#[test]
fn reanchor_grouped_selection_lands_on_a_row() {
    use crate::views::picker::PickerState;
    let map: Vec<Option<()>> = vec![None, Some(()), Some(())];
    let mut st = PickerState::default();
    st.selected = 9;
    reanchor_grouped_selection(&mut st, &map);
    assert_eq!(st.selected, 2);
    let mut st = PickerState::default();
    reanchor_grouped_selection(&mut st, &map);
    assert_eq!(st.selected, 1);
    let empty: Vec<Option<()>> = vec![];
    let mut st = PickerState::default();
    st.selected = 5;
    reanchor_grouped_selection(&mut st, &empty);
    assert_eq!(st.selected, 0);
}
#[test]
fn entry_title_loading_when_no_session_id() {
    use crate::views::session_title::entry_title;
    let mut app = test_app_with_agent();
    if let Some(a) = app.agents.get_mut(&AgentId(0)) {
        a.session.session_id = None;
    }
    let title = entry_title(expect_agent(&app, AgentId(0)));
    assert_eq!(title, "loading...");
}
/// Regression: SessionLoaded must clear stale running entries from replay.
/// Without the finish_turn call, Execute blocks that were InProgress when the session was last active stay orphaned as "running" forever.
#[test]
fn session_loaded_clears_stale_running_entries() {
    use crate::acp::meta::NotificationMeta;
    use std::sync::Arc;
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-stale".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    let agent = app.agents.get_mut(&id).unwrap();
    let meta = NotificationMeta::default();
    agent.session.handle_update(
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from("tc-stale")),
                "Execute `sleep 999`".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![])
            .locations(vec![]),
        ),
        &meta,
        &mut agent.scrollback,
    );
    agent.session.handle_update(
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-stale")),
            acp::ToolCallUpdateFields::new().status(Some(acp::ToolCallStatus::InProgress)),
        )),
        &meta,
        &mut agent.scrollback,
    );
    assert!(!agent.scrollback.is_empty());
    assert!(
        agent.scrollback.needs_animation(),
        "scrollback should have running entries before SessionLoaded",
    );
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-stale"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        !expect_agent(&app, id).scrollback.needs_animation(),
        "no entries should be animating after SessionLoaded",
    );
}
/// A failed `x.ai/prompt_history` fetch arrives as `PromptHistoryLoaded` with an empty list.
#[test]
fn a_restored_transcript_stays_recallable_after_a_failed_fetch() {
    let mut app = test_app();
    dispatch(
        Action::LoadSession("sess-history".into(), None, false),
        &mut app,
    );
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("first prompt"));
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("second prompt"));
    }
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoaded {
            agent_id: id,
            session_id: acp::SessionId::new("sess-history"),
            models: None,
            modes: None,
            code_restored: false,
            restore_summary: None,
            restore_degree: None,
            running_prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).session.prompt_history,
        ["second prompt", "first prompt"]
    );
    dispatch(
        Action::TaskComplete(TaskResult::PromptHistoryLoaded {
            agent_id: id,
            prompts: vec![],
        }),
        &mut app,
    );
    assert_eq!(
        expect_agent(&app, id).session.prompt_history,
        ["second prompt", "first prompt"]
    );
    assert!(!expect_agent(&app, id).session.prompt_history_loading);
}
#[test]
fn session_restore_failed_clears_prompt_history_loading() {
    let mut app = test_app();
    let effects = dispatch_load_session_with_restore(&mut app, "remote-sess".into(), "/tmp".into());
    assert!(matches!(
        effects.as_slice(),
        [Effect::RestoreAndLoadSession { .. }]
    ));
    let id = AgentId(0);
    assert!(expect_agent(&app, id).session.prompt_history_loading);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionRestoreFailed {
            agent_id: id,
            error: "boom".into(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!expect_agent(&app, id).session.prompt_history_loading);
    assert!(!expect_agent(&app, id).session.loading_replay);
}
#[test]
fn resume_focuses_existing_agent_for_open_session() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let agent_0 = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_0,
            session_id: "wt-sess-1".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    {
        let agent = app.agents.get_mut(&agent_0).unwrap();
        agent
            .subagent_sessions
            .insert("child-1".into(), make_test_subagent("child-1", "sa-1"));
        agent.active_subagent = Some("child-1".into());
    }
    dispatch(Action::NewSession, &mut app);
    let agent_1 = AgentId(1);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_1,
            session_id: "new-sess-2".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    let count_before = app.agents.len();
    let effects = dispatch(
        Action::LoadSession("wt-sess-1".into(), None, false),
        &mut app,
    );
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_0));
    assert_eq!(app.agents.len(), count_before);
    assert!(effects.is_empty());
    assert!(expect_agent(&app, agent_0).active_subagent.is_none());
    assert_eq!(
        expect_agent(&app, agent_0).session.session_id,
        Some(acp::SessionId::new("wt-sess-1"))
    );
    assert_eq!(
        expect_agent(&app, agent_1).session.session_id,
        Some(acp::SessionId::new("new-sess-2"))
    );
}
#[test]
fn resume_unknown_session_still_creates_new_agent() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: AgentId(0),
            session_id: "sess-aaa".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    let effects = dispatch(
        Action::LoadSession("sess-never-open".into(), None, false),
        &mut app,
    );
    let new_id = AgentId(1);
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == new_id));
    assert_eq!(app.agents.len(), 2);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::LoadSession {
            agent_id,
            session_id,
            ..
        }         if *agent_id == new_id && session_id == "sess-never-open"
    )));
}
/// GB-4877: resume A → /new → resume A in minimal must LoadSession.
/// /new drops A's AgentView so `focus_if_session_already_open` cannot short-circuit.
#[test]
fn minimal_resume_after_new_reloads_prior_session() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    dispatch(Action::NewSession, &mut app);
    let agent_a = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_a,
            session_id: "sess-a".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    dispatch(Action::NewSession, &mut app);
    let agent_b = AgentId(1);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_b,
            session_id: "sess-b".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    assert!(
        !app.agents.contains_key(&agent_a),
        "minimal /new must drop the prior AgentView"
    );
    assert_eq!(app.agents.len(), 1);
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_b));
    let effects = dispatch(Action::LoadSession("sess-a".into(), None, false), &mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::LoadSession { session_id, .. } if session_id == "sess-a"
        )),
        "resume of A after /new must LoadSession, got {effects:?}"
    );
}
/// `/resume` of B leaves A's AgentView. Minimal `/new` must unregister A, not only B.
#[test]
fn minimal_new_unregisters_resume_leftover() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    dispatch(Action::NewSession, &mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: AgentId(0),
            session_id: "sess-a".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    dispatch(Action::LoadSession("sess-b".into(), None, false), &mut app);
    assert!(app.agents.contains_key(&AgentId(0)));
    assert_eq!(app.agents.len(), 2);
    let effects = dispatch(Action::NewSession, &mut app);
    let unregistered: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::UnregisterActiveSession { session_id } => Some(session_id.0.as_ref()),
            _ => None,
        })
        .collect();
    assert!(
        unregistered.contains(&"sess-a"),
        "leftover resume view must UnregisterActiveSession, got {effects:?}"
    );
    assert!(
        unregistered.contains(&"sess-b"),
        "previously active session must UnregisterActiveSession, got {effects:?}"
    );
    assert!(!app.agents.contains_key(&AgentId(0)));
    assert!(!app.agents.contains_key(&AgentId(1)));
}
/// Already-open focus still applies when /resume picks the session currently on screen.
#[test]
fn minimal_resume_of_current_session_still_focuses() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    dispatch(Action::NewSession, &mut app);
    let agent_a = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_a,
            session_id: "sess-a".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    let effects = dispatch(Action::LoadSession("sess-a".into(), None, false), &mut app);
    assert!(
        effects.is_empty(),
        "resume of the visible session must stay focus-only, got {effects:?}"
    );
    assert_eq!(app.agents.len(), 1);
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_a));
}
/// A stale `attached_agent` (not equal to the visible agent) must not re-activate the overlay.
#[test]
fn resume_open_session_does_not_rearm_stale_overlay() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let agent_0 = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_0,
            session_id: "sess-a".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    dispatch(Action::NewSession, &mut app);
    let agent_1 = AgentId(1);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_1,
            session_id: "sess-b".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    ensure_dashboard_state(&mut app);
    app.active_view = ActiveView::Agent(agent_1);
    app.dashboard.as_mut().unwrap().attached_agent = Some(agent_0);
    let effects = dispatch(Action::LoadSession("sess-b".into(), None, false), &mut app);
    assert!(effects.is_empty());
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_1));
    assert_eq!(
        app.dashboard.as_ref().unwrap().attached_agent,
        Some(agent_0)
    );
}
/// Conversation resume must not focus a Build agent that shares the same id.
#[test]
fn resume_conversation_does_not_focus_build_id_collision() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let agent_0 = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_0,
            session_id: "shared-id".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    assert!(!expect_agent(&app, agent_0).chat_kind);
    let count_before = app.agents.len();
    let effects = dispatch(
        Action::LoadSession("shared-id".into(), None, true),
        &mut app,
    );
    assert_eq!(app.agents.len(), count_before + 1);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::LoadSession {
            session_id,
            chat_kind: true,
            ..
        } if session_id == "shared-id"
    )));
    assert!(!expect_agent(&app, agent_0).chat_kind);
}
#[test]
fn duplicate_load_unbind_invalidates_old_minimal_btw_response() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    dispatch(Action::NewSession, &mut app);
    let old_owner = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: old_owner,
            session_id: "shared-id".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    let request_id = match dispatch(
        Action::SendBtw {
            question: "old question".into(),
            images: Vec::new(),
        },
        &mut app,
    )
    .as_slice()
    {
        [
            Effect::SendBtw {
                minimal_request_id: Some(id),
                ..
            },
        ] => *id,
        other => panic!("expected correlated minimal /btw effect, got {other:?}"),
    };
    dispatch(
        Action::LoadSession("shared-id".into(), None, true),
        &mut app,
    );
    assert!(expect_agent(&app, old_owner).session.session_id.is_none());
    assert!(expect_agent(&app, old_owner).btw_state.is_none());
    assert!(
        expect_agent(&app, old_owner)
            .minimal_btw_lifecycle
            .is_none()
    );
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            agent_id: old_owner,
            result: Ok("old answer".into()),
            minimal_request_id: Some(request_id),
        }),
        &mut app,
    );
    assert!(expect_agent(&app, old_owner).btw_state.is_none());
    assert!(
        expect_agent(&app, old_owner)
            .minimal_btw_lifecycle
            .is_none()
    );
}
/// Under sticky `--chat`, agents stamp `chat_kind=true` even for build loads; resume with conversation-entry false must still focus the open agent.
#[test]
fn resume_under_chat_mode_focuses_despite_entry_false() {
    let mut app = test_app();
    app.chat_mode = true;
    dispatch(Action::NewSession, &mut app);
    let agent_0 = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_0,
            session_id: "chat-mode-sess".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    app.agents.get_mut(&agent_0).unwrap().chat_kind = true;
    dispatch(Action::NewSession, &mut app);
    let agent_1 = AgentId(1);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_1,
            session_id: "other".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    app.agents.get_mut(&agent_1).unwrap().chat_kind = true;
    let count_before = app.agents.len();
    let effects = dispatch(
        Action::LoadSession("chat-mode-sess".into(), None, false),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(app.agents.len(), count_before);
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_0));
}
/// Resuming the agent that `attached_agent` already points at must call `focus_row`.
#[test]
fn resume_stale_attached_target_focuses_dashboard_row() {
    use crate::views::dashboard::DashboardRowId;
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let agent_0 = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_0,
            session_id: "sess-a".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    dispatch(Action::NewSession, &mut app);
    let agent_1 = AgentId(1);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: agent_1,
            session_id: "sess-b".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    ensure_dashboard_state(&mut app);
    app.active_view = ActiveView::Agent(agent_1);
    app.dashboard.as_mut().unwrap().attached_agent = Some(agent_0);
    app.dashboard
        .as_mut()
        .unwrap()
        .focus_row(DashboardRowId::TopLevel(agent_1));
    let effects = dispatch(Action::LoadSession("sess-a".into(), None, false), &mut app);
    assert!(effects.is_empty());
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == agent_0));
    assert_eq!(
        app.dashboard.as_ref().unwrap().attached_agent,
        Some(agent_0)
    );
    assert_eq!(
        app.dashboard.as_ref().unwrap().selected,
        Some(DashboardRowId::TopLevel(agent_0))
    );
}
/// After SessionLoadFailed, retrying resume must reissue LoadSession.
#[test]
fn resume_after_load_failed_reissues_load() {
    let mut app = test_app();
    let effects = dispatch(
        Action::LoadSession("fail-then-retry".into(), None, false),
        &mut app,
    );
    let agent_0 = AgentId(0);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::LoadSession { agent_id, .. } if *agent_id == agent_0
    )));
    assert!(expect_agent(&app, agent_0).loading_placeholder_id.is_some());
    dispatch(
        Action::TaskComplete(TaskResult::SessionLoadFailed {
            agent_id: agent_0,
            session_id: acp::SessionId::new("fail-then-retry"),
            error: "transient".into(),
        }),
        &mut app,
    );
    assert!(!expect_agent(&app, agent_0).session.loading_replay);
    assert!(expect_agent(&app, agent_0).loading_placeholder_id.is_some());
    let count_before = app.agents.len();
    let effects = dispatch(
        Action::LoadSession("fail-then-retry".into(), None, false),
        &mut app,
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::LoadSession {
                agent_id,
                session_id,
                ..
            } if *agent_id != agent_0 && session_id == "fail-then-retry"
        )),
        "retry after failure must emit LoadSession for a new agent, got {effects:?}"
    );
    assert_eq!(app.agents.len(), count_before + 1);
}
#[test]
fn session_restored_clears_stale_session_id() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: AgentId(0),
            session_id: "remote-sess".into(),
            models: None,
            modes: None,
        }),
        &mut app,
    );
    dispatch(Action::NewSession, &mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionRestored {
            agent_id: AgentId(1),
            local_session_id: "remote-sess".into(),
        }),
        &mut app,
    );
    assert_eq!(expect_agent(&app, AgentId(0)).session.session_id, None);
    assert_eq!(
        expect_agent(&app, AgentId(1)).session.session_id,
        Some(acp::SessionId::new("remote-sess"))
    );
}
#[test]
fn minimal_new_session_queues_welcome_card() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let _ = dispatch(Action::NewSession, &mut app);
    assert!(
        app.minimal_state.welcome_pending,
        "a fresh minimal session should queue the welcome card"
    );
}
#[test]
fn non_minimal_new_session_does_not_queue_welcome_card() {
    let mut app = test_app();
    app.screen_mode = crate::app::ScreenMode::Inline;
    let _ = dispatch(Action::NewSession, &mut app);
    assert!(
        !app.minimal_state.welcome_pending,
        "the welcome card is minimal-only"
    );
}
/// Picking a conversation row dispatches a direct chat load, never local resolution or GCS restore.
#[test]
fn pick_conversation_row_dispatches_direct_chat_load() {
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-pick-1")]);
    let effects = dispatch(Action::PickSession(0), &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::LoadSession {
                session_id,
                session_cwd: None,
                chat_kind: true,
                ..
            }] if session_id == "conv-pick-1"
        ),
        "expected a direct chat LoadSession, got {effects:?}"
    );
}
/// Welcome-screen variant of the conversation-row pick.
#[test]
fn pick_conversation_row_from_welcome_dispatches_direct_chat_load() {
    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_conversation_entry("conv-pick-2")]);
    let effects = dispatch(Action::PickSession(0), &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::LoadSession {
                session_id,
                session_cwd: None,
                chat_kind: true,
                ..
            }] if session_id == "conv-pick-2"
        ),
        "expected a direct chat LoadSession, got {effects:?}"
    );
}
/// Canary: a remote Build row not on disk still takes the GCS-restore path.
#[test]
fn pick_remote_build_row_still_restores() {
    let mut app = test_app_with_agent();
    let id = format!("remote-only-{}", std::process::id());
    let mut e = make_picker_entry(&id, "/r");
    e.source = "remote".into();
    open_session_picker_with(&mut app, vec![e]);
    let effects = dispatch(Action::PickSession(0), &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::RestoreAndLoadSession { session_id, .. }] if *session_id == id
        ),
        "expected RestoreAndLoadSession, got {effects:?}"
    );
}
/// A content-search hit that matches a conversation row shown in the same picker also dispatches the direct chat load.
#[test]
fn pick_content_session_conversation_row_dispatches_direct_chat_load() {
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-hit-1")]);
    let effects = dispatch(
        Action::PickContentSession {
            session_id: "conv-hit-1".into(),
            cwd: String::new(),
        },
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::LoadSession {
                session_id,
                session_cwd: None,
                chat_kind: true,
                ..
            }] if session_id == "conv-hit-1"
        ),
        "expected a direct chat LoadSession, got {effects:?}"
    );
}
/// Worktree resume is refused for conversation rows (no cwd to check out); the refusal must not set the one-shot chat bit.
#[test]
fn pick_session_in_worktree_refuses_conversation_row() {
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-wt-1")]);
    let effects = dispatch(Action::PickSessionInWorktree(0), &mut app);
    assert!(effects.is_empty(), "no worktree effects, got {effects:?}");
    assert!(
        !app.deferred_startup.pending_chat,
        "refusal must not set the one-shot chat bit"
    );
    assert!(read_toast(&app).contains("worktree"));
}
/// Chat mode replaces the local FTS5 deep search with a debounced server-side list refetch; Build mode keeps the deep search untouched.
#[test]
fn chat_mode_query_change_schedules_debounced_search() {
    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_conversation_entry("conv-ds-1")]);
    app.session_picker_state.set_query("abc");
    app.chat_mode = true;
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DebounceSessionSearch { query, seq: 1, .. }] if query == "abc"
        ),
        "chat-mode query change must arm the search debounce, got {effects:?}"
    );
    assert_eq!(app.session_picker_list_seq, 1, "trigger must bump the seq");
    assert!(
        app.session_picker_content_loading,
        "arming the search must raise the in-flight indicator"
    );
    app.chat_mode = false;
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::DeepSearchSessions { .. }]),
        "Build-mode deep search unchanged, got {effects:?}"
    );
    assert_eq!(
        app.session_picker_list_seq, 1,
        "Build-mode search must not bump the list seq"
    );
}
/// A current-seq debounce expiry issues the fetch with the query; a stale one (superseded by newer typing) is dropped.
#[test]
fn chat_mode_debounce_expiry_fetches_current_and_drops_stale() {
    let mut app = test_app();
    app.chat_mode = true;
    app.session_picker_state.set_query("abc");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList { query: Some(q), seq: 1, .. }] if q == "abc"
        ),
        "current debounce expiry must fetch with the query, got {effects:?}"
    );
    app.session_picker_state.set_query("abcd");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "stale debounce expiry must not fetch, got {effects:?}"
    );
    app.session_picker_state.set_query("abcde");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    app.active_view = ActiveView::AgentDashboard;
    let live_generation = app.session_picker_generation;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: live_generation,
            query: "abcde".into(),
            seq: 3,
        }),
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList {
                host: SessionPickerHost::Welcome,
                generation,
                query: Some(q),
                seq: 3,
                ..
            }] if q == "abcde" && *generation == live_generation
        ),
        "chat-mode expiry off the welcome view must still fetch, re-stamped \
         with the same host and generation, got {effects:?}"
    );
}
/// Build mode: any query of 2 or more chars arms the deep-search debounce.
/// Abundant title matches must not suppress the content search (users read that as "content search doesn't exist").
/// Ctrl+/ still searches immediately.
#[test]
fn build_mode_query_arms_debounce_despite_title_hits_and_force_skips_it() {
    let mut app = test_app();
    assert!(!app.chat_mode);
    app.session_picker_entries = Some(vec![
        make_picker_entry("prost-1", "/r"),
        make_picker_entry("prost-2", "/r"),
        make_picker_entry("prost-3", "/r"),
    ]);
    app.session_picker_state.set_query("prost");
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DebounceSessionSearch { query, seq: 1, .. }] if query == "prost"
        ),
        "unforced query must arm the debounce even with 3+ title hits, got {effects:?}"
    );
    assert!(
        app.session_picker_content_loading,
        "arming the debounce must raise the in-flight indicator"
    );
    assert_eq!(
        app.session_picker_list_seq, 0,
        "Build-mode search must not touch the chat list seq"
    );
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeepSearchSessions { query, seq: 2, .. }] if query == "prost"
        ),
        "forced search must skip the debounce, got {effects:?}"
    );
}
/// Build mode: a sub-2-char query clears the content results and invalidates the previously armed debounce.
/// The debounce's late expiry then cannot resurrect the search.
#[test]
fn build_mode_short_query_clears_results_and_invalidates_armed_debounce() {
    let mut app = test_app();
    app.session_picker_content_results = Some(vec![]);
    app.session_picker_state.set_query("ab");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    app.session_picker_state.set_query("a");
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    assert!(
        effects.is_empty(),
        "short query emits nothing, got {effects:?}"
    );
    assert!(app.session_picker_content_results.is_none());
    assert!(!app.session_picker_content_loading);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "ab".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "expiry armed before the clear must not search, got {effects:?}"
    );
}
/// Build mode: a current-seq debounce expiry dispatches the deep search; one superseded by newer typing is dropped.
/// A current expiry with the welcome picker hidden behind another view is dropped too.
/// Build-mode deep search (unlike chat mode) requires the welcome view to be visible.
#[test]
fn build_mode_debounce_expiry_searches_current_and_drops_stale() {
    let mut app = test_app();
    app.session_picker_state.set_query("abc");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeepSearchSessions { query, seq: 1, .. }] if query == "abc"
        ),
        "current expiry must dispatch the deep search, got {effects:?}"
    );
    app.session_picker_state.set_query("abcd");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "stale expiry must be dropped, got {effects:?}"
    );
    app.session_picker_state.set_query("abcde");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    assert_eq!(app.session_picker_deep_search_seq, 3);
    app.active_view = ActiveView::AgentDashboard;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            query: "abcde".into(),
            seq: 3,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "build-mode expiry for a hidden welcome picker must not search, got {effects:?}"
    );
}
/// Modal `/resume` surface: the debounce expiry validates against the MODAL's deep-search seq (the welcome counter still sits at 0 here).
#[test]
fn build_mode_modal_debounce_expiry_validates_modal_seq() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_picker_entry("local-dm-1", "/r")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("abc");
    }
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DebounceSessionSearch { seq: 1, .. }]
        ),
        "modal query must arm the debounce, got {effects:?}"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeepSearchSessions { query, seq: 1, .. }] if query == "abc"
        ),
        "expiry must validate against the modal seq, got {effects:?}"
    );
}
/// The modal arms a debounce, then closes.
/// The dismissal bump lands on the WELCOME counter and collides with the carried modal seq (both 1 here).
/// The expiry must still be dropped because no picker surface is live.
#[test]
fn build_mode_modal_close_drops_armed_debounce_despite_seq_collision() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_picker_entry("local-cl-1", "/r")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("abc");
    }
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    assert_eq!(
        app.session_picker_deep_search_seq, 0,
        "modal arm must not touch the welcome counter"
    );
    let armed_generation = modal_picker_generation(&app);
    get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal = None;
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    assert_eq!(
        app.session_picker_deep_search_seq, 1,
        "collision precondition: welcome counter equals the armed modal seq"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::AgentModal,
            generation: armed_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "modal-armed expiry must not search after the modal closed, got {effects:?}"
    );
}
/// Ctrl+/ (forced search) skips the debounce.
/// Clearing the query immediately refetches the unfiltered list (`query: None`): restoring the recent list must not wait out a debounce.
#[test]
fn chat_mode_force_search_fetches_immediately_and_empty_query_unfilters() {
    let mut app = test_app();
    app.chat_mode = true;
    app.session_picker_state.set_query("abc");
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList { query: Some(q), seq: 1, .. }] if q == "abc"
        ),
        "forced search must fetch without debouncing, got {effects:?}"
    );
    assert!(
        app.session_picker_content_loading,
        "search fetch must raise the in-flight indicator"
    );
    app.session_picker_state.set_query("");
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList {
                query: None,
                seq: 2,
                ..
            }]
        ),
        "cleared query must refetch the unfiltered list immediately (no debounce), got {effects:?}"
    );
    assert!(
        !app.session_picker_content_loading,
        "unfiltered refetch is not a search — indicator must drop"
    );
}
/// The modal `/resume` picker's query (not the welcome picker's) drives the chat-mode search when a modal is open.
#[test]
fn chat_mode_search_reads_modal_query_first() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    app.session_picker_state.set_query("welcome-query");
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-mq-1")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("modal-query");
    }
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList { query: Some(q), .. }] if q == "modal-query"
        ),
        "modal query must win over the welcome picker's, got {effects:?}"
    );
}
/// Out-of-order list completions: only the response for the current seq lands; stale successes and failures are both dropped.
#[test]
fn stale_session_list_responses_are_dropped() {
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    app.session_picker_state.set_query("abc");
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    app.session_picker_state.set_query("abcd");
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-stale-1")],
            partial: None,
            seq: 1,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "stale list result must be dropped"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            error: "boom".into(),
            seq: 1,
            query: Some("abc".into()),
        }),
        &mut app,
    );
    assert!(
        expect_agent(&app, AgentId(0)).toast.is_none(),
        "stale list failure must not toast"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-fresh-2")],
            partial: None,
            seq: 2,
            query: Some("abcd".into()),
        }),
        &mut app,
    );
    let ids: Vec<&str> = app
        .session_picker_entries
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(ids, ["conv-fresh-2"], "current-seq result must land");
    assert_eq!(
        app.session_picker_entries_query.as_deref(),
        Some("abcd"),
        "search results must be stamped with their fetch query"
    );
    assert!(
        !app.session_picker_content_loading,
        "landing the search must drop the in-flight indicator"
    );
}
/// Modal `/resume` surface: a current-seq search response replaces the modal's entries (query-stamped, cursor re-anchored).
/// A stale one leaves them untouched.
#[test]
fn modal_search_response_lands_and_stale_is_dropped() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-old")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("hit");
        state.selected = 3;
    }
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-hit-1")],
            partial: None,
            seq: 1,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    {
        let agent = get_active_agent(&app).expect("active agent");
        let Some(ActiveModal::SessionPicker {
            entries: Some(list),
            entries_query,
            content_loading,
            state,
            ..
        }) = agent.active_modal.as_ref()
        else {
            panic!("expected SessionPicker modal with entries");
        };
        assert_eq!(
            list.first().map(|e| e.id.as_str()),
            Some("conv-hit-1"),
            "search results land in the modal"
        );
        assert_eq!(
            entries_query.as_deref(),
            Some("hit"),
            "modal entries must carry their fetch-query stamp"
        );
        assert!(!content_loading, "landing clears the modal indicator");
        assert_eq!(state.selected, 1, "cursor re-anchors onto the result row");
    }
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("hits");
    }
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-stale-m")],
            partial: None,
            seq: 1,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    let agent = get_active_agent(&app).expect("active agent");
    let Some(ActiveModal::SessionPicker {
        entries: Some(list),
        content_loading,
        ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected SessionPicker modal with entries");
    };
    assert_eq!(
        list.first().map(|e| e.id.as_str()),
        Some("conv-hit-1"),
        "stale modal response must be dropped"
    );
    assert!(
        content_loading,
        "stale response must not clear the newer search's indicator"
    );
}
/// With the modal gone the response would fall through to the WELCOME picker fields, whose search box never held the modal's query.
/// That would leave mismatched entries and a stale fetch-query stamp for the next resume view.
/// The close must bump the seq so the late response is dropped.
#[test]
fn modal_close_drops_in_flight_search_response() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-cl-1")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("hit");
    }
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let seq = app.session_picker_list_seq;
    let generation = modal_picker_generation(&app);
    get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal = None;
    let effects = dispatch(Action::SessionPickerClosed, &mut app);
    assert!(
        effects.is_empty(),
        "close is a pure invalidation, got {effects:?}"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-late-1")],
            partial: None,
            seq,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "post-close response must not land on the welcome picker"
    );
    assert!(
        app.session_picker_entries_query.is_none(),
        "no stale fetch-query stamp may leak to the welcome picker"
    );
}
/// Sibling of the close test: PICKING from the modal dismisses it too, so the same in-flight chat-mode search must be invalidated.
/// Otherwise its late response falls through to the WELCOME picker fields in `handle_session_list_loaded`'s fallback.
#[test]
fn modal_pick_drops_in_flight_search_response() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-pk-1")]);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("hit");
    }
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let seq = app.session_picker_list_seq;
    let generation = modal_picker_generation(&app);
    let effects = dispatch(Action::PickSession(0), &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::LoadSession { .. }]),
        "pick must still load the session, got {effects:?}"
    );
    assert!(
        app.session_picker_list_seq > seq,
        "pick must invalidate the in-flight search"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-late-p")],
            partial: None,
            seq,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "post-pick response must not land on the welcome picker"
    );
    assert!(
        app.session_picker_entries_query.is_none(),
        "no stale fetch-query stamp may leak to the welcome picker"
    );
}
/// Welcome-screen sibling: Esc closes the picker and must invalidate the in-flight fetch.
/// Otherwise its response repopulates `session_picker_entries` and visually resurrects the picker the user just closed.
#[test]
fn welcome_esc_drops_in_flight_fetch_response() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app();
    app.chat_mode = true;
    app.session_picker_entries = Some(vec![make_conversation_entry("conv-w-esc")]);
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let seq = app.session_picker_list_seq;
    let generation = app.session_picker_generation;
    let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let out = app.handle_input(&esc);
    assert!(
        matches!(
            out,
            crate::app::app_view::InputOutcome::Action(Action::SessionPickerClosed)
        ),
        "welcome Esc must surface SessionPickerClosed, got {out:?}"
    );
    assert!(
        app.session_picker_entries.is_none(),
        "Esc clears the welcome picker"
    );
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![make_conversation_entry("conv-late-w")],
            partial: None,
            seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "in-flight fetch must not repopulate the closed welcome picker"
    );
}
/// Build-mode sibling of the chat Esc test, pinning Esc during load.
/// The fast foreign fetch has landed (its rows hidden behind the Grok-default CTA) while the native fetch is still in flight.
/// Esc must really dismiss the picker: drop the loading flag and invalidate the fetch so its late response cannot resurrect the picker.
#[test]
fn build_welcome_esc_during_load_dismisses_without_resurrection() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app();
    assert!(!app.chat_mode);
    let _ = dispatch(Action::FetchSessionList, &mut app);
    let seq = app.session_picker_list_seq;
    let generation = app.session_picker_generation;
    assert!(app.session_picker_loading);
    let mut foreign = make_picker_entry("claude-1", "/repo");
    foreign.source = "claude".into();
    app.session_picker_entries = Some(vec![foreign]);
    let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let out = app.handle_input(&esc);
    assert!(
        matches!(
            out,
            crate::app::app_view::InputOutcome::Action(Action::SessionPickerClosed)
        ),
        "welcome Esc must surface SessionPickerClosed, got {out:?}"
    );
    assert!(
        app.session_picker_entries.is_none(),
        "Esc clears the welcome picker"
    );
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    assert!(
        !app.session_picker_loading,
        "dismissal must end the loading limbo (`show_picker` keys off it)"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("native-late", "/repo")],
            partial: None,
            seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "late native response must not resurrect the closed picker"
    );
}
/// The spinner-only loading picker (nothing landed yet) still owns Esc: it must dismiss the picker, not leave Esc dead over the menu it covers.
#[test]
fn build_welcome_esc_dismisses_spinner_only_loading_picker() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app();
    let _ = dispatch(Action::FetchSessionList, &mut app);
    assert!(app.session_picker_loading);
    assert!(app.session_picker_entries.is_none());
    let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let out = app.handle_input(&esc);
    assert!(
        matches!(
            out,
            crate::app::app_view::InputOutcome::Action(Action::SessionPickerClosed)
        ),
        "Esc on the loading picker must close it, got {out:?}"
    );
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    assert!(!app.session_picker_loading, "picker fully dismissed");
}
/// Build mode: an Only-policy modal fetch completing after close and reopen is dropped by the host and incarnation routing.
/// It cannot leak its rows or query stamp into the replacement Exclude-policy modal.
#[test]
fn build_mode_close_and_reopen_drop_opposite_policy_response() {
    use distill_shell::session::unified_list::HeadlessPolicy;
    let mut app = test_app_with_agent();
    assert!(!app.chat_mode);
    let _ = dispatch(Action::ShowSessionPicker, &mut app);
    let old_generation = modal_picker_generation(&app);
    let only_effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::AgentModal,
            generation,
            seq: only_seq,
            headless_policy: HeadlessPolicy::Only,
            ..
        },
    ] = only_effects.as_slice()
    else {
        panic!("expected modal Headless fetch, got {only_effects:?}");
    };
    assert_eq!(*generation, old_generation);
    let only_seq = *only_seq;
    get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal = None;
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    let exclude_effects = dispatch(Action::ShowSessionPicker, &mut app);
    let Some(Effect::FetchSessionList {
        host: SessionPickerHost::AgentModal,
        generation: new_generation,
        seq: exclude_seq,
        headless_policy,
        ..
    }) = exclude_effects.first()
    else {
        panic!("expected reopened picker fetch, got {exclude_effects:?}");
    };
    assert_eq!(*headless_policy, HeadlessPolicy::Exclude);
    assert!(*new_generation > old_generation);
    assert!(*exclude_seq > only_seq);
    let new_generation = *new_generation;
    let exclude_seq = *exclude_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: old_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("stale-generation", "/tmp/repo")],
            partial: None,
            seq: exclude_seq,
            query: Some("modal-only".into()),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let Some(ActiveModal::SessionPicker {
        entries,
        entries_query,
        loading,
        ..
    }) = expect_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("reopened picker missing");
    };
    assert!(entries.is_none(), "stale incarnation rows must be dropped");
    assert!(entries_query.is_none(), "stale query stamp must be dropped");
    assert!(
        *loading,
        "the replacement modal keeps waiting for its fetch"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: new_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("stale-policy", "/tmp/repo")],
            partial: None,
            seq: only_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let Some(ActiveModal::SessionPicker {
        entries, loading, ..
    }) = expect_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("reopened picker missing");
    };
    assert!(
        entries.is_none(),
        "obsolete Only-policy rows must be dropped"
    );
    assert!(
        *loading,
        "the replacement modal keeps waiting for its fetch"
    );
    assert!(app.session_picker_entries.is_none());
    assert!(app.session_picker_entries_query.is_none());
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: new_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("current", "/tmp/repo")],
            partial: None,
            seq: exclude_seq,
            query: None,
        }),
        &mut app,
    );
    let Some(ActiveModal::SessionPicker {
        entries: Some(entries),
        ..
    }) = expect_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("reopened picker missing");
    };
    assert_eq!(entries.first().map(|e| e.id.as_str()), Some("current"));
}
/// A zero-hit search is a normal outcome: the picker shows an empty list.
/// It never shows the misleading "No sessions found for this directory" toast, which would fire on every keystroke.
/// Plain fetches keep the toast.
#[test]
fn zero_hit_search_shows_empty_list_without_toast() {
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    app.session_picker_state.set_query("zzz");
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq: 1,
            query: Some("zzz".into()),
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries
            .as_ref()
            .is_some_and(|v| v.is_empty()),
        "zero-hit search keeps an empty (not None) list"
    );
    assert!(
        expect_agent(&app, AgentId(0)).toast.is_none(),
        "zero-hit search must not toast"
    );
    assert!(!app.session_picker_content_loading);
    let _ = dispatch(Action::FetchSessionList, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq: 2,
            query: None,
        }),
        &mut app,
    );
    assert!(app.session_picker_entries.is_none());
    assert!(
        read_toast(&app).contains("No sessions found"),
        "plain empty fetch keeps the generic toast"
    );
}
/// Welcome-surface twin of the modal pick test: a content-only search hit must be pickable when the entries carry a matching fetch-query stamp.
#[test]
fn welcome_server_search_hit_with_unrelated_title_is_pickable() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let mut app = test_app();
    app.chat_mode = true;
    let mut e = make_conversation_entry("conv-content-w1");
    e.summary = "Quarterly roadmap notes".into();
    app.session_picker_entries = Some(vec![e.clone()]);
    app.session_picker_state.set_query("hit");
    app.session_picker_entries_query = Some("hit".into());
    app.session_picker_state.selected = 0;
    let out = app.handle_input(&enter);
    assert!(
        matches!(
            out,
            crate::app::app_view::InputOutcome::Action(Action::PickSession(0))
        ),
        "welcome content-only search hit must be pickable, got {out:?}"
    );
    let mut app = test_app();
    app.chat_mode = true;
    app.session_picker_entries = Some(vec![e]);
    app.session_picker_state.set_query("hit");
    app.session_picker_state.selected = 0;
    let out = app.handle_input(&enter);
    assert!(
        !matches!(
            out,
            crate::app::app_view::InputOutcome::Action(Action::PickSession(_))
        ),
        "unstamped welcome entries must still be fuzzy-filtered, got {out:?}"
    );
}
/// A current-seq FAILED search clears the in-flight indicator and the fuzzy-bypass stamp, and shows the toast.
/// A stuck "Searching…" or a stale stamp on the error state would outlive the entries it described.
#[test]
fn current_seq_failed_search_clears_indicator_and_stamp() {
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    app.session_picker_entries_query = Some("old".into());
    app.session_picker_state.set_query("hit");
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(app.session_picker_content_loading);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            error: "boom".into(),
            seq: 1,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    assert!(
        !app.session_picker_content_loading,
        "failed search must drop the in-flight indicator"
    );
    assert!(
        app.session_picker_entries_query.is_none(),
        "failed search must clear the fuzzy-bypass stamp"
    );
    assert!(app.session_picker_entries.is_none());
    assert!(read_toast(&app).contains("Couldn't load sessions"));
}
/// Modal branch of the gated failure clear: a current-seq FAILED search clears the modal's indicator and stamp.
/// A plain (query-less) failure leaves the modal's deep-search spinner alone.
#[test]
fn modal_failed_search_clears_indicator_and_plain_failure_preserves_spinner() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    open_session_picker_with(&mut app, vec![make_conversation_entry("conv-mf-1")]);
    if let Some(ActiveModal::SessionPicker {
        state,
        entries_query,
        ..
    }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("hit");
        *entries_query = Some("old".into());
    }
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            error: "boom".into(),
            seq: 1,
            query: Some("hit".into()),
        }),
        &mut app,
    );
    {
        let agent = get_active_agent(&app).expect("active agent");
        let Some(ActiveModal::SessionPicker {
            entries,
            loading,
            content_loading,
            entries_query,
            ..
        }) = agent.active_modal.as_ref()
        else {
            panic!("expected SessionPicker modal");
        };
        assert!(
            !content_loading,
            "failed search must drop the modal indicator"
        );
        assert!(
            entries_query.is_none(),
            "failed search must clear the modal stamp"
        );
        assert!(entries.is_none(), "failure drops the modal entries");
        assert!(!loading);
    }
    assert!(read_toast(&app).contains("Couldn't load sessions"));
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_picker_entry("local-mf-1", "/r")]);
    let _ = dispatch(Action::FetchSessionList, &mut app);
    if let Some(ActiveModal::SessionPicker { state, .. }) = get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal
        .as_mut()
    {
        state.set_query("abc");
    }
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::DeepSearchSessions { .. }]),
        "Build-mode modal deep search armed, got {effects:?}"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            error: "boom".into(),
            seq: app.session_picker_list_seq,
            query: None,
        }),
        &mut app,
    );
    let agent = get_active_agent(&app).expect("active agent");
    let Some(ActiveModal::SessionPicker {
        content_loading, ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected SessionPicker modal");
    };
    assert!(
        content_loading,
        "plain failure must not hide the modal deep-search spinner"
    );
}
/// Build-mode canary: `content_loading` belongs to the FTS5 deep search (guarded by `deep_search_seq`).
/// A plain (query-less) list response or failure landing while the deep search runs must NOT hide its spinner.
#[test]
fn build_mode_list_response_preserves_deep_search_spinner() {
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::FetchSessionList, &mut app);
    app.session_picker_state.set_query("abc");
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::DeepSearchSessions { .. }]),
        "Build-mode deep search armed, got {effects:?}"
    );
    assert!(app.session_picker_content_loading, "deep search in flight");
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("local-1", "/r")],
            partial: None,
            seq: app.session_picker_list_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_content_loading,
        "plain list response must not hide the deep-search spinner"
    );
    assert!(app.session_picker_entries.is_some(), "entries still land");
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            error: "boom".into(),
            seq: app.session_picker_list_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_content_loading,
        "plain list failure must not hide the deep-search spinner"
    );
}
/// Build mode: rapid plain fetches advance both the shared list seq and the welcome incarnation.
/// A response must match both before it can land, in either completion order.
#[test]
fn build_mode_rapid_plain_fetches_drop_superseded_response() {
    let mut app = test_app();
    assert!(!app.chat_mode);
    let first = dispatch(Action::FetchSessionList, &mut app);
    let first_generation = app.session_picker_generation;
    let second = dispatch(Action::FetchSessionList, &mut app);
    let second_generation = app.session_picker_generation;
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::Welcome,
            generation: first_effect_generation,
            seq: first_seq,
            ..
        },
    ] = first.as_slice()
    else {
        panic!("expected first list fetch, got {first:?}");
    };
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::Welcome,
            generation: second_effect_generation,
            seq: second_seq,
            ..
        },
    ] = second.as_slice()
    else {
        panic!("expected second list fetch, got {second:?}");
    };
    assert_eq!(*first_effect_generation, first_generation);
    assert_eq!(*second_effect_generation, second_generation);
    assert!(second_generation > first_generation);
    assert!(*second_seq > *first_seq);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: first_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("build-first", "/r")],
            partial: None,
            seq: *first_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_entries.is_none(),
        "superseded plain response must be dropped"
    );
    assert!(
        app.session_picker_loading,
        "the dropped response must not clear the newer fetch's spinner"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: second_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("build-second", "/r")],
            partial: None,
            seq: *second_seq,
            query: None,
        }),
        &mut app,
    );
    assert_eq!(
        app.session_picker_entries
            .as_ref()
            .and_then(|entries| entries.first().map(|e| e.id.as_str())),
        Some("build-second"),
        "the current response must land"
    );
    assert!(!app.session_picker_loading);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: first_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("build-first-late", "/r")],
            partial: None,
            seq: *first_seq,
            query: None,
        }),
        &mut app,
    );
    assert_eq!(
        app.session_picker_entries
            .as_ref()
            .and_then(|entries| entries.first().map(|e| e.id.as_str())),
        Some("build-second"),
        "a late superseded response must not clobber the applied result"
    );
}
/// Picker incarnation generations: every fetch reallocates the welcome picker's generation.
/// A fetch with the modal open overwrites the modal's constructed 0 placeholder with a fresh allocation (distinct from the welcome one).
/// A dismissal reallocates the welcome generation again.
#[test]
fn picker_generations_reallocate_on_fetch_and_dismissal() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    assert_eq!(app.session_picker_generation, 0);
    let _ = dispatch(Action::FetchSessionList, &mut app);
    let welcome_after_fetch = app.session_picker_generation;
    assert!(welcome_after_fetch > 0, "fetch must allocate a generation");
    let _ = dispatch(Action::ShowSessionPicker, &mut app);
    let Some(&ActiveModal::SessionPicker { generation, .. }) = get_active_agent(&app)
        .expect("active agent")
        .active_modal
        .as_ref()
    else {
        panic!("expected SessionPicker modal");
    };
    assert!(
        generation > welcome_after_fetch,
        "open must overwrite the modal's 0 placeholder with a fresh allocation"
    );
    assert!(
        app.session_picker_generation > welcome_after_fetch,
        "the modal-open fetch reallocates the welcome generation too"
    );
    assert_ne!(
        generation, app.session_picker_generation,
        "generations are unique across hosts"
    );
    get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal = None;
    let welcome_before_dismiss = app.session_picker_generation;
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    assert!(
        app.session_picker_generation > welcome_before_dismiss,
        "dismissal must reallocate the welcome generation"
    );
}
/// Reopening the modal picker starts a new incarnation.
/// The first incarnation's late list response is dropped; the reopened incarnation's own response applies.
/// The test also pins the producer side: the modal fetch carries the modal host and the modal's live generation.
#[test]
fn reopened_modal_drops_prior_incarnation_list_result() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    assert!(!app.chat_mode);
    let effects = dispatch(Action::ShowSessionPicker, &mut app);
    let first_generation = modal_picker_generation(&app);
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::AgentModal,
            generation,
            seq: first_seq,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("expected modal fetch, got {effects:?}");
    };
    assert_eq!(*generation, first_generation);
    let first_seq = *first_seq;
    get_active_agent_mut(&mut app)
        .expect("active agent")
        .active_modal = None;
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    let second_effects = dispatch(Action::ShowSessionPicker, &mut app);
    let second_generation = modal_picker_generation(&app);
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::AgentModal,
            generation,
            seq: second_seq,
            ..
        },
    ] = second_effects.as_slice()
    else {
        panic!("expected reopened modal fetch, got {second_effects:?}");
    };
    assert_eq!(*generation, second_generation);
    assert!(second_generation > first_generation);
    assert!(*second_seq > first_seq);
    let second_seq = *second_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: first_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("stale-open", "/r")],
            partial: None,
            seq: first_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    {
        let agent = get_active_agent(&app).expect("active agent");
        let Some(ActiveModal::SessionPicker {
            entries, loading, ..
        }) = agent.active_modal.as_ref()
        else {
            panic!("expected SessionPicker modal");
        };
        assert!(
            entries.is_none(),
            "stale-incarnation response must not land"
        );
        assert!(loading, "reopened modal keeps waiting for its own fetch");
    }
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: second_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("fresh-open", "/r")],
            partial: None,
            seq: second_seq,
            query: None,
        }),
        &mut app,
    );
    let agent = get_active_agent(&app).expect("active agent");
    let Some(ActiveModal::SessionPicker {
        entries: Some(list),
        loading,
        ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected SessionPicker modal with entries");
    };
    assert_eq!(list.first().map(|e| e.id.as_str()), Some("fresh-open"));
    assert!(!loading);
}
/// An armed welcome debounce dies with its incarnation.
/// A dismissal drops the armed spinner flag and reallocates the generation, so the expiry emits nothing.
/// A browse refetch reallocates the generation while leaving the deep-search seq current, so its expiry is dropped on the generation alone.
#[test]
fn dismissed_welcome_picker_drops_armed_debounce_expiry() {
    let mut app = test_app();
    assert!(!app.chat_mode);
    app.session_picker_state.set_query("abc");
    let effects = dispatch(Action::TriggerDeepSearch, &mut app);
    let armed_generation = app.session_picker_generation;
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DebounceSessionSearch {
                host: SessionPickerHost::Welcome,
                generation,
                seq: 1,
                ..
            }] if *generation == armed_generation
        ),
        "welcome debounce must be stamped with the welcome host and its live generation, got {effects:?}"
    );
    assert!(app.session_picker_content_loading);
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    assert!(
        !app.session_picker_content_loading,
        "dismissal must drop the armed spinner flag"
    );
    assert!(
        app.session_picker_generation > armed_generation,
        "dismissal must reallocate the welcome generation"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: armed_generation,
            query: "abc".into(),
            seq: 1,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "dismissed picker's expiry must not search, got {effects:?}"
    );
    assert!(!app.session_picker_content_loading);
    app.session_picker_state.set_query("abcd");
    let _ = dispatch(Action::TriggerDeepSearch, &mut app);
    let rearmed_generation = app.session_picker_generation;
    let armed_seq = app.session_picker_deep_search_seq;
    let _ = dispatch(Action::FetchSessionList, &mut app);
    assert_eq!(
        app.session_picker_deep_search_seq, armed_seq,
        "the browse refetch leaves the deep-search seq current"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionSearchDebounceExpired {
            host: SessionPickerHost::Welcome,
            generation: rearmed_generation,
            query: "abcd".into(),
            seq: armed_seq,
        }),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "the superseded incarnation's expiry must not search, got {effects:?}"
    );
}
/// A welcome-issued fetch whose response completes after a modal opened on top is dropped.
/// The modal-open fetch reallocated the welcome generation, so the response can neither retarget the modal nor land in the welcome picker's fields.
#[test]
fn welcome_fetch_response_does_not_retarget_open_modal() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    assert!(!app.chat_mode);
    let effects = dispatch(Action::FetchSessionList, &mut app);
    let [
        Effect::FetchSessionList {
            host: SessionPickerHost::Welcome,
            generation: welcome_generation,
            seq: welcome_seq,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("expected welcome fetch, got {effects:?}");
    };
    let welcome_generation = *welcome_generation;
    let welcome_seq = *welcome_seq;
    let _ = dispatch(Action::ShowSessionPicker, &mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: welcome_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("welcome-late", "/r")],
            partial: None,
            seq: welcome_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    {
        let agent = get_active_agent(&app).expect("active agent");
        let Some(ActiveModal::SessionPicker {
            entries, loading, ..
        }) = agent.active_modal.as_ref()
        else {
            panic!("expected SessionPicker modal");
        };
        assert!(
            entries.is_none(),
            "welcome response must not land in the modal"
        );
        assert!(loading, "the modal keeps waiting for its own fetch");
    }
    assert!(
        app.session_picker_entries.is_none(),
        "the superseded welcome incarnation's response is dropped everywhere"
    );
}
/// The dashboard routing arm: a dashboard-host result (list or card detail) applies to the mounted surface when generation and per-kind seq match.
/// It is dropped on a generation or detail-seq mismatch, and dropped (with no welcome fallback) while the surface is unmounted.
#[test]
fn dashboard_host_results_route_to_surface_only() {
    use crate::views::session_picker_surface::SessionPickerSurface;
    let cwd = std::env::current_dir().expect("cwd");
    let planted = plant_local_build_session(&cwd, "dash-fresh");
    let mut app = test_app();
    let generation = app.alloc_picker_generation();
    let mut surface = SessionPickerSurface::new(generation);
    surface.loading = true;
    app.dashboard_session_picker = Some(surface);
    let list_result = |generation, id: &str| {
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Dashboard,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry(id, &cwd.to_string_lossy())],
            partial: None,
            seq: 0,
            query: None,
        })
    };
    let _ = dispatch(list_result(generation, "dash-fresh"), &mut app);
    {
        let surface = app.dashboard_session_picker.as_ref().expect("surface");
        assert_eq!(
            surface
                .entries
                .as_ref()
                .and_then(|e| e.first().map(|x| x.id.as_str())),
            Some("dash-fresh"),
            "matching dashboard result must apply to the surface"
        );
        assert!(!surface.loading);
    }
    let _ = dispatch(list_result(generation + 1, "dash-stale"), &mut app);
    assert_eq!(
        app.dashboard_session_picker
            .as_ref()
            .and_then(|s| s.entries.as_ref())
            .and_then(|e| e.first().map(|x| x.id.as_str())),
        Some("dash-fresh"),
        "generation mismatch must be dropped"
    );
    let detail = crate::app::app_view::CardDetail {
        turn_count: 9,
        tool_call_count: 1,
        first_prompt_preview: "first".into(),
    };
    let detail_result = |generation, seq| {
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::Dashboard,
            generation,
            source: "local".into(),
            session_id: "dash-fresh".into(),
            seq,
            detail: detail.clone(),
        })
    };
    let detail_seq = app
        .dashboard_session_picker
        .as_ref()
        .expect("surface")
        .detail_seq;
    let surface_card_detail = |app: &AppView| {
        app.dashboard_session_picker
            .as_ref()
            .and_then(|s| s.entries.as_ref())
            .and_then(|e| e.first())
            .and_then(|e| e.card_detail.as_ref())
            .map(|d| d.turn_count)
    };
    let _ = dispatch(detail_result(generation, detail_seq + 1), &mut app);
    assert_eq!(
        surface_card_detail(&app),
        None,
        "detail-seq mismatch must be dropped"
    );
    let _ = dispatch(detail_result(generation, detail_seq), &mut app);
    assert_eq!(
        surface_card_detail(&app),
        Some(9),
        "matching card detail must stamp the surface's entry"
    );
    app.dashboard_session_picker
        .as_mut()
        .expect("surface")
        .content_loading = true;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::DeepSearchResults {
            host: SessionPickerHost::Dashboard,
            generation,
            results: vec![content_hit("dash-hit")],
            seq: 0,
        }),
        &mut app,
    );
    {
        let surface = app.dashboard_session_picker.as_ref().expect("surface");
        assert_eq!(
            surface
                .content_results
                .as_ref()
                .and_then(|hits| hits.first().map(|h| h.session_id.as_str())),
            Some("dash-hit"),
            "matching deep-search results must land on the surface"
        );
        assert!(!surface.content_loading);
    }
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::Dashboard,
            generation,
            error: "boom".into(),
            seq: 0,
            query: None,
        }),
        &mut app,
    );
    {
        let surface = app.dashboard_session_picker.as_ref().expect("surface");
        assert!(
            surface.entries.is_none(),
            "the routed failure clears the surface's native rows"
        );
        assert!(!surface.loading);
    }
    assert!(app.session_picker_entries.is_none());
    assert!(!app.session_picker_loading);
    app.dashboard_session_picker = None;
    let effects = dispatch(list_result(generation, "dash-unmounted"), &mut app);
    assert!(effects.is_empty());
    assert!(app.dashboard_session_picker.is_none());
    assert!(
        app.session_picker_entries.is_none(),
        "an unmounted dashboard result must not fall through to welcome storage"
    );
    std::fs::remove_dir_all(planted).expect("remove planted session");
}
/// A fetch issued for one agent's modal cannot land on another agent's modal: the generations differ, so the late result is dropped.
/// The requesting (now background) modal stays loading; nothing routes back to a non-active modal.
#[test]
fn modal_result_does_not_cross_agents_and_background_modal_starves() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    assert!(!app.chat_mode);
    let a_effects = dispatch(Action::ShowSessionPicker, &mut app);
    let a_generation = modal_picker_generation(&app);
    let [Effect::FetchSessionList { seq: a_seq, .. }] = a_effects.as_slice() else {
        panic!("expected agent A modal fetch, got {a_effects:?}");
    };
    let a_seq = *a_seq;
    insert_placeholder_agent(&mut app, AgentId(1));
    app.active_view = ActiveView::Agent(AgentId(1));
    let _ = dispatch(Action::ShowSessionPicker, &mut app);
    let b_generation = modal_picker_generation(&app);
    assert_ne!(a_generation, b_generation);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: a_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("agent-a-late", "/r")],
            partial: None,
            seq: a_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    {
        let agent = get_active_agent(&app).expect("active agent");
        let Some(ActiveModal::SessionPicker {
            entries, loading, ..
        }) = agent.active_modal.as_ref()
        else {
            panic!("expected agent B's SessionPicker modal");
        };
        assert!(
            entries.is_none(),
            "another agent's result must not land here"
        );
        assert!(loading, "B's modal keeps waiting for its own fetch");
    }
    let Some(ActiveModal::SessionPicker {
        entries, loading, ..
    }) = expect_agent(&app, AgentId(0)).active_modal.as_ref()
    else {
        panic!("expected agent A's SessionPicker modal");
    };
    assert!(entries.is_none());
    assert!(
        loading,
        "the background modal stays loading: its result is dropped, not delivered"
    );
}
/// A welcome browse refetch clears the search box without bumping the deep-search seq.
/// The in-flight deep search is orphaned by the refetch's generation reallocation and must not repopulate the cleared results.
/// The new incarnation's own search still lands.
#[test]
fn welcome_browse_refetch_orphans_in_flight_deep_search() {
    let mut app = test_app();
    assert!(!app.chat_mode);
    app.session_picker_state.set_query("needle");
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    let search_generation = app.session_picker_generation;
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeepSearchSessions { seq: 1, .. }]
        ),
        "forced deep search must dispatch, got {effects:?}"
    );
    let _ = dispatch(Action::FetchSessionList, &mut app);
    assert_eq!(
        app.session_picker_deep_search_seq, 1,
        "the browse refetch leaves the deep-search seq untouched"
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::DeepSearchResults {
            host: SessionPickerHost::Welcome,
            generation: search_generation,
            results: vec![content_hit("stale-deep-hit")],
            seq: 1,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(
        app.session_picker_content_results.is_none(),
        "the orphaned deep search must not repopulate the cleared picker"
    );
    assert!(!app.session_picker_content_loading);
    app.session_picker_state.set_query("needle2");
    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeepSearchSessions { seq: 2, .. }]
        ),
        "re-armed deep search must dispatch, got {effects:?}"
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::DeepSearchResults {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            results: vec![content_hit("fresh-deep-hit")],
            seq: 2,
        }),
        &mut app,
    );
    assert_eq!(
        app.session_picker_content_results
            .as_ref()
            .and_then(|hits| hits.first().map(|h| h.session_id.as_str())),
        Some("fresh-deep-hit"),
        "the live incarnation's deep search must land"
    );
    assert!(!app.session_picker_content_loading);
}
/// A background modal's in-flight card detail survives welcome-picker activity.
/// Only the routed surface's detail seq moves, so the detail still applies when the user returns and its own host, generation, and seq match.
/// The modal's OWN list changes still invalidate its in-flight details.
#[test]
fn background_modal_card_detail_survives_welcome_refetch() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    assert!(!app.chat_mode);
    let modal_effects = dispatch(Action::ShowSessionPicker, &mut app);
    let modal_generation = modal_picker_generation(&app);
    let [Effect::FetchSessionList { seq: modal_seq, .. }] = modal_effects.as_slice() else {
        panic!("expected modal fetch, got {modal_effects:?}");
    };
    let modal_seq = *modal_seq;
    let modal_list = |generation, seq| {
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation,
            scope: ListScope::Cwd,
            sessions: vec![
                make_picker_entry("card-target", "/r"),
                make_picker_entry("card-other", "/r"),
            ],
            partial: None,
            seq,
            query: None,
        })
    };
    let _ = dispatch(modal_list(modal_generation, modal_seq), &mut app);
    let effects = dispatch(
        Action::ExpandSessionCard {
            source: "local".into(),
            session_id: "card-target".into(),
        },
        &mut app,
    );
    let [
        Effect::LoadCardDetail {
            host: SessionPickerHost::AgentModal,
            generation,
            session_id,
            seq,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("expected a modal-stamped card detail load, got {effects:?}");
    };
    assert_eq!(*generation, modal_generation);
    assert_eq!(session_id, "card-target");
    let detail_seq = *seq;
    app.active_view = ActiveView::Welcome;
    let welcome_effects = dispatch(Action::FetchSessionList, &mut app);
    let [
        Effect::FetchSessionList {
            seq: welcome_seq, ..
        },
    ] = welcome_effects.as_slice()
    else {
        panic!("expected welcome fetch, got {welcome_effects:?}");
    };
    let welcome_seq = *welcome_seq;
    let welcome_detail_seq = app.session_picker_detail_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![make_picker_entry("welcome-row", "/r")],
            partial: None,
            seq: welcome_seq,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.session_picker_detail_seq > welcome_detail_seq,
        "the welcome result advances the welcome detail seq"
    );
    {
        let Some(&ActiveModal::SessionPicker {
            detail_seq: modal_detail_seq,
            generation: live_modal_generation,
            ..
        }) = expect_agent(&app, AgentId(0)).active_modal.as_ref()
        else {
            panic!("agent A's modal must survive the view switch");
        };
        assert_eq!(
            modal_detail_seq, detail_seq,
            "welcome activity must not advance the modal's detail seq"
        );
        assert_eq!(live_modal_generation, modal_generation);
    }
    app.active_view = ActiveView::Agent(AgentId(0));
    let detail = crate::app::app_view::CardDetail {
        turn_count: 4,
        tool_call_count: 2,
        first_prompt_preview: "first".into(),
    };
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_generation,
            source: "local".into(),
            session_id: "card-target".into(),
            seq: detail_seq,
            detail: detail.clone(),
        }),
        &mut app,
    );
    let modal_card_detail = |app: &AppView, id: &str| {
        let Some(ActiveModal::SessionPicker {
            entries: Some(entries),
            ..
        }) = expect_agent(app, AgentId(0)).active_modal.as_ref()
        else {
            panic!("expected SessionPicker modal with entries");
        };
        entries
            .iter()
            .find(|e| e.id == id)
            .expect("entry")
            .card_detail
            .as_ref()
            .map(|d| d.turn_count)
    };
    assert_eq!(
        modal_card_detail(&app, "card-target"),
        Some(4),
        "the surviving detail must stamp the background modal's own row"
    );
    let effects = dispatch(
        Action::ExpandSessionCard {
            source: "local".into(),
            session_id: "card-other".into(),
        },
        &mut app,
    );
    let [Effect::LoadCardDetail { seq, .. }] = effects.as_slice() else {
        panic!("expected a card detail load, got {effects:?}");
    };
    let second_detail_seq = *seq;
    let _ = dispatch(
        modal_list(modal_generation, app.session_picker_list_seq),
        &mut app,
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_generation,
            source: "local".into(),
            session_id: "card-other".into(),
            seq: second_detail_seq,
            detail,
        }),
        &mut app,
    );
    assert_eq!(
        modal_card_detail(&app, "card-other"),
        None,
        "a modal-targeted list change must still invalidate the modal's own in-flight detail"
    );
}
/// Legacy fast path pinned: opening or refreshing the picker fetches with no query and invalidates any in-flight search fetch.
#[test]
fn plain_picker_fetch_carries_no_query_and_bumps_seq() {
    let mut app = test_app();
    app.chat_mode = true;
    app.session_picker_state.set_query("abc");
    let _ = dispatch(Action::ForceDeepSearch, &mut app);
    let effects = dispatch(Action::FetchSessionList, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionList {
                query: None,
                seq: 2,
                ..
            }]
        ),
        "picker fetch must be unfiltered and supersede the search, got {effects:?}"
    );
}
