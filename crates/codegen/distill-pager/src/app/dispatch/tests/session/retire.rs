// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::app::actions::WorkspaceMutationFailure;
use crate::app::agent::{AgentCommand, AgentState};
use crate::app::effects::CompactError;
use crate::app::workspace_test_fixtures::{new_member, temp_store};
use crate::views::dashboard::DashboardRowId;
use crate::views::prompt_widget::StashedPrompt;
use crate::views::question_view::QuestionViewState;

fn compact_ok(app: &mut AppView, id: AgentId) -> Vec<Effect> {
    dispatch_task_result(
        TaskResult::CompactComplete {
            agent_id: id,
            result: Ok(()),
        },
        app,
    )
}

fn compact_err(app: &mut AppView, id: AgentId, cancelled: bool) -> Vec<Effect> {
    dispatch_task_result(
        TaskResult::CompactComplete {
            agent_id: id,
            result: Err(CompactError {
                cancelled,
                message: if cancelled {
                    "compact cancelled".into()
                } else {
                    "disk failed".into()
                },
            }),
        },
        app,
    )
}

#[test]
fn retire_compacts_then_closes_and_keeps_history() {
    let mut app = three_agent_app();
    let id = AgentId(1);
    app.active_view = ActiveView::Agent(id);
    app.agents.get_mut(&id).unwrap().is_worktree = true;
    let effects = dispatch(Action::RetireSession, &mut app);
    assert!(
        matches!(
            effects.first(),
            Some(Effect::Compact {
                agent_id,
                user_context: None,
                ..
            }) if *agent_id == id
        ),
        "{effects:?}"
    );
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_some());
    let effects = compact_ok(&mut app, id);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::DeleteSession { .. })),
        "history must stay: {effects:?}"
    );
    assert!(!app.agents.contains_key(&id));
    assert!(app.agents.contains_key(&AgentId(0)));
}

#[test]
fn retire_sole_agent_goes_to_welcome_and_keeps_history() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let _ = dispatch(Action::RetireSession, &mut app);
    let effects = compact_ok(&mut app, id);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::DeleteSession { .. })),
        "history must stay: {effects:?}"
    );
    assert!(!app.agents.contains_key(&id));
    assert!(matches!(app.active_view, ActiveView::Welcome));
}

#[test]
fn ordinary_close_still_refuses_sole_agent() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch_sessions_confirm_close(&mut app, id);
    assert!(effects.is_empty());
    assert!(app.agents.contains_key(&id));
}

#[test]
fn retire_cancel_or_save_failure_leaves_agent_open() {
    let mut app = three_agent_app();
    let id = AgentId(1);
    app.active_view = ActiveView::Agent(id);
    let _ = dispatch(Action::RetireSession, &mut app);
    let _ = compact_err(&mut app, id, false);
    assert!(app.agents.contains_key(&id));
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_none());

    let _ = dispatch(Action::RetireSession, &mut app);
    let _ = compact_err(&mut app, id, true);
    assert!(app.agents.contains_key(&id));
}

#[test]
fn retire_rejects_busy_queued_replay_pending_bg_and_scheduled() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_none());

    app.agents.get_mut(&id).unwrap().session.state = AgentState::Idle;
    enqueue_local(&mut app, id, "later");
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());

    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .pending_prompts
        .clear();
    app.agents.get_mut(&id).unwrap().session.loading_replay = true;
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    app.agents.get_mut(&id).unwrap().session.loading_replay = false;

    app.agents.get_mut(&id).unwrap().question_view = Some(QuestionViewState::new(
        "q-1".into(),
        vec![],
        StashedPrompt::default(),
    ));
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    app.agents.get_mut(&id).unwrap().question_view = None;

    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .bg_tasks
        .insert("bg-1".into(), make_bg_task("bg-1"));
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    app.agents.get_mut(&id).unwrap().session.bg_tasks.clear();

    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .scheduled_tasks
        .insert(
            "loop-1".into(),
            crate::app::agent::ScheduledTaskInfo {
                task_id: "loop-1".into(),
                prompt: "keep going".into(),
                human_schedule: "every 5m".into(),
                created_at: std::time::Instant::now(),
                next_fire_at: None,
                tag: "loop".into(),
                last_subagent_id: None,
            },
        );
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .scheduled_tasks
        .clear();

    app.agents.get_mut(&id).unwrap().prompt.set_text("keep me");
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_none());
    assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "keep me");
}

#[test]
fn queued_work_during_compact_prevents_close() {
    let mut app = three_agent_app();
    let id = AgentId(1);
    app.active_view = ActiveView::Agent(id);
    let _ = dispatch(Action::RetireSession, &mut app);
    enqueue_local(&mut app, id, "after compact");
    let _ = compact_ok(&mut app, id);
    assert!(
        app.agents.contains_key(&id),
        "new queue during compact must not close"
    );
}

#[test]
fn retire_tracks_session_identity_and_target() {
    let mut app = three_agent_app();
    let target = AgentId(1);
    app.active_view = ActiveView::Agent(target);
    let _ = dispatch(Action::RetireSession, &mut app);
    app.active_view = ActiveView::Agent(AgentId(0));
    app.agents.get_mut(&target).unwrap().session.session_id =
        Some("other-session".to_string().into());
    let _ = compact_ok(&mut app, target);
    assert!(
        app.agents.contains_key(&target),
        "identity change must abort retire"
    );

    app.agents.get_mut(&target).unwrap().session.session_id =
        Some("placeholder".to_string().into());
    app.active_view = ActiveView::Agent(target);
    let _ = dispatch(Action::RetireSession, &mut app);
    app.active_view = ActiveView::Agent(AgentId(0));
    let _ = compact_ok(&mut app, target);
    assert!(
        !app.agents.contains_key(&target),
        "retire must close the original target even after view switch"
    );
    assert!(app.agents.contains_key(&AgentId(0)));
}

#[test]
fn dashboard_retire_hotpath_uses_selected_row() {
    let mut app = three_agent_app();
    mark_agent_nonempty(&mut app, AgentId(0));
    mark_agent_nonempty(&mut app, AgentId(1));
    ensure_dashboard_state(&mut app);
    app.active_view = ActiveView::AgentDashboard;
    app.dashboard
        .as_mut()
        .unwrap()
        .focus_row(DashboardRowId::TopLevel(AgentId(1)));
    let effects = dispatch(Action::RetireSession, &mut app);
    assert!(matches!(
        effects.first(),
        Some(Effect::Compact { agent_id, .. }) if *agent_id == AgentId(1)
    ));
    let _ = compact_ok(&mut app, AgentId(1));
    assert!(!app.agents.contains_key(&AgentId(1)));
}

#[test]
fn retire_cancel_then_ok_complete_leaves_agent_open() {
    let mut app = three_agent_app();
    let id = AgentId(1);
    app.active_view = ActiveView::Agent(id);
    let _ = dispatch(Action::RetireSession, &mut app);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::CommandCancelling {
        command: AgentCommand::Compact,
    };
    let _ = compact_ok(&mut app, id);
    assert!(
        app.agents.contains_key(&id),
        "late CompactComplete Ok after cancel must not close"
    );
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_none());
    assert!(
        app.agents
            .get(&id)
            .unwrap()
            .retire_after_workspace
            .is_none()
    );
}

fn ready_workspace_retire_app(session_id: &str) -> (tempfile::TempDir, AppView) {
    let (temp, mut store) = temp_store();
    store
        .insert_member(new_member(session_id, "Saved"))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let mut app = test_app_with_agent();
    app.workspace_dashboard_enabled = true;
    app.workspace_membership.set_ready_for_test(store, snapshot);
    ensure_dashboard_state(&mut app);
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id =
        Some(session_id.to_string().into());
    app.active_view = ActiveView::Agent(AgentId(0));
    (temp, app)
}

fn take_remove_write(
    effects: Vec<Effect>,
) -> (
    distill_dashboard_store::WorkspaceStore,
    Vec<distill_dashboard_store::MemberKey>,
) {
    let mut effects = effects;
    let Some(idx) = effects.iter().position(|effect| {
        matches!(
            effect,
            Effect::WriteWorkspace {
                mutation: WorkspaceMutation::Remove(_),
                ..
            }
        )
    }) else {
        panic!("expected workspace remove write, got {effects:?}");
    };
    let Effect::WriteWorkspace {
        store,
        mutation: WorkspaceMutation::Remove(keys),
    } = effects.remove(idx)
    else {
        unreachable!();
    };
    (store, keys)
}

#[test]
fn workspace_retire_waits_for_commit_then_closes() {
    let (_temp, mut app) = ready_workspace_retire_app("saved");
    let id = AgentId(0);
    let _ = dispatch(Action::RetireSession, &mut app);
    let effects = compact_ok(&mut app, id);
    assert!(
        app.agents.contains_key(&id),
        "must not drop before membership write commits"
    );
    assert!(
        app.agents
            .get(&id)
            .unwrap()
            .retire_after_workspace
            .is_some()
    );
    let (mut store, keys) = take_remove_write(effects);
    for key in &keys {
        let _ = store.remove_member(key);
    }
    let snapshot = store.snapshot().unwrap();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorkspaceWriteCompleted {
            store,
            completion: WorkspaceWriteCompletion::Remove {
                keys,
                snapshot: Ok(snapshot),
                failures: vec![],
            },
        }),
        &mut app,
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::DeleteSession { .. })),
        "history must stay: {effects:?}"
    );
    assert!(!app.agents.contains_key(&id));
    assert!(
        app.workspace_membership
            .snapshot()
            .is_some_and(|snapshot| snapshot.members.is_empty())
    );
}

#[test]
fn workspace_retire_failure_leaves_agent_and_membership() {
    let (_temp, mut app) = ready_workspace_retire_app("saved");
    let id = AgentId(0);
    let _ = dispatch(Action::RetireSession, &mut app);
    let effects = compact_ok(&mut app, id);
    let (store, keys) = take_remove_write(effects);
    let snapshot = store.snapshot().unwrap();
    let _ = dispatch(
        Action::TaskComplete(TaskResult::WorkspaceWriteCompleted {
            store,
            completion: WorkspaceWriteCompletion::Remove {
                keys,
                snapshot: Ok(snapshot),
                failures: vec![WorkspaceMutationFailure {
                    key: crate::app::workspace_test_fixtures::key("saved"),
                    error: "disk full".into(),
                    retryable: false,
                }],
            },
        }),
        &mut app,
    );
    assert!(app.agents.contains_key(&id));
    assert!(
        app.agents
            .get(&id)
            .unwrap()
            .retire_after_workspace
            .is_none()
    );
    assert!(
        app.workspace_membership
            .snapshot()
            .is_some_and(|snapshot| snapshot.members.len() == 1)
    );
}

#[test]
fn workspace_retire_absent_snapshot_is_not_a_removal_ack() {
    let (_temp, store) = temp_store();
    let mut app = test_app_with_agent();
    app.workspace_dashboard_enabled = true;
    let snapshot = store.snapshot().unwrap();
    app.workspace_membership.set_ready_for_test(store, snapshot);
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id = Some("not-yet-adopted".into());
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .retire_after_workspace = Some("not-yet-adopted".into());
    assert!(crate::app::workspace_sync::request_removal(
        &mut app,
        "not-yet-adopted",
        crate::app::workspace_membership::RemovalCause::Archive,
    ));
    let effects = crate::app::dispatch::session::modal::complete_retire_after_workspace(
        &mut app,
        &std::collections::HashSet::new(),
    );
    assert!(effects.is_empty());
    assert!(app.agents.contains_key(&AgentId(0)));
    assert!(
        app.agents
            .get(&AgentId(0))
            .unwrap()
            .retire_after_workspace
            .is_some()
    );
}

#[test]
fn retire_preserves_unsent_dashboard_reply() {
    let mut app = three_agent_app();
    let id = AgentId(1);
    ensure_dashboard_state(&mut app);
    app.active_view = ActiveView::AgentDashboard;
    let row = DashboardRowId::TopLevel(id);
    let fields = crate::views::dashboard::peek::compute_peek_fields(&row, &app.agents).unwrap();
    let dashboard = app.dashboard.as_mut().unwrap();
    dashboard.focus_row(row.clone());
    dashboard.set_peek(Some(crate::views::dashboard::peek::PeekPanelState::new(
        row, fields,
    )));
    dashboard.peek_reply.set_text("unsent reply");
    assert!(dispatch(Action::RetireSession, &mut app).is_empty());
    assert_eq!(
        app.dashboard.as_ref().unwrap().peek_reply.text(),
        "unsent reply"
    );
    assert!(app.agents.get(&id).unwrap().retire_after_compact.is_none());
}

#[test]
fn workspace_retire_keeps_work_arriving_before_archive_ack() {
    let (_temp, mut app) = ready_workspace_retire_app("saved");
    let id = AgentId(0);
    dispatch(Action::RetireSession, &mut app);
    let (mut store, keys) = take_remove_write(compact_ok(&mut app, id));
    enqueue_local(&mut app, id, "keep this new work");
    for key in &keys {
        store.remove_member(key).unwrap();
    }
    let snapshot = store.snapshot().unwrap();
    dispatch(
        Action::TaskComplete(TaskResult::WorkspaceWriteCompleted {
            store,
            completion: WorkspaceWriteCompletion::Remove {
                keys,
                snapshot: Ok(snapshot),
                failures: vec![],
            },
        }),
        &mut app,
    );
    assert!(app.agents.contains_key(&id));
    assert!(
        app.agents
            .get(&id)
            .unwrap()
            .retire_after_workspace
            .is_none()
    );
    assert!(
        !app.agents
            .get(&id)
            .unwrap()
            .session
            .pending_prompts
            .is_empty()
    );
}
