//! Regression tests for dock-focused keyboard and mouse input.

use super::test_fixtures::make_agent;
use super::{AgentPane, AgentView};
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn cache_stop_button(agent: &mut AgentView) {
    agent.cache_dock_stop_button();
}

fn insert_running_subagent(agent: &mut AgentView, child_session_id: &str) {
    let info = super::test_fixtures::running_subagent_info(child_session_id);
    agent
        .subagent_sessions
        .insert(child_session_id.to_string(), info);
    agent.insert_test_child(
        child_session_id.to_string(),
        Box::new(super::test_fixtures::make_agent()),
    );
}

fn insert_finished_subagent(agent: &mut AgentView, child_session_id: &str) {
    insert_running_subagent(agent, child_session_id);
    let info = agent.subagent_sessions.get_mut(child_session_id).unwrap();
    info.set_finished_for_test(true);
    info.attempt.status = Some(std::sync::Arc::from("completed"));
}

fn dock_with_subagents(running: &[&str], finished: &[&str]) -> AgentView {
    let mut agent = make_agent();
    for id in running {
        insert_running_subagent(&mut agent, id);
    }
    for id in finished {
        insert_finished_subagent(&mut agent, id);
    }
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    agent
}

fn dock_subagent_ids(agent: &AgentView) -> Vec<String> {
    agent
        .dock_subagent_rows()
        .into_iter()
        .map(|(child, _, _)| child)
        .collect()
}

fn insert_running_monitor(agent: &mut AgentView, task_id: &str) {
    insert_running_task(agent, task_id);
    if let Some(task) = agent.session.bg_tasks.get_mut(task_id) {
        task.is_monitor = true;
        task.command = "tail -f log".into();
    }
}

fn insert_running_task(agent: &mut AgentView, task_id: &str) {
    agent.session.bg_tasks.insert(
        task_id.into(),
        crate::app::agent::BgTaskState {
            task_id: task_id.into(),
            tool_call_id: format!("call-{task_id}"),
            command: "sleep 5".into(),
            description: None,
            cwd: "/tmp".into(),
            output_file: "/tmp/out".into(),
            status: crate::app::agent::BgTaskStatus::Running,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor: false,
            restored_from_replay: false,
        },
    );
}

fn dock_with_task() -> AgentView {
    let mut agent = make_agent();
    insert_running_task(&mut agent, "bg-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    agent
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn ctrl_q_while_dock_focused_is_unchanged() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "Ctrl+Q must bubble to global Quit, got {outcome:?}"
    );
    assert_eq!(agent.active_pane, AgentPane::Dock);
}

#[test]
fn ctrl_x_while_dock_focused_is_unchanged() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "Ctrl+X must not take the kill arm, got {outcome:?}"
    );
}

#[test]
fn unmodified_q_unfocuses_dock() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Scrollback);
}

#[test]
fn unmodified_x_kills_selected_task() {
    let mut agent = dock_with_task();
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, 0)
            )
        })
        .expect("expanded Tasks section has a row");
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillBgTask(id)) if id == "bg-1"
    ));
}

#[test]
fn hidden_dock_does_not_navigate_or_kill() {
    let mut agent = dock_with_task();
    agent.dock_shown = false;
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert_eq!(agent.dock_cursor, 0);

    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(
        !matches!(outcome, InputOutcome::Action(_)),
        "x on a hidden dock must not kill, got {outcome:?}"
    );
}

#[test]
fn toggle_tasks_does_not_open_hidden_pane_when_dock_on_but_empty() {
    let mut agent = make_agent();
    agent.dock_on = true;
    agent.dock_shown = false;
    agent.tasks.overlay.visible = false;
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert!(!agent.tasks.overlay.visible);
    assert_ne!(agent.active_pane, AgentPane::Tasks);
}

#[test]
fn toggle_tasks_still_toggles_legacy_pane_when_dock_off() {
    let mut agent = make_agent();
    agent.dock_on = false;
    agent.dock_shown = false;
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.tasks.overlay.visible);
    assert_eq!(agent.active_pane, AgentPane::Tasks);
}

fn ctrl_g(agent: &mut AgentView) -> InputOutcome {
    agent.handle_input(
        &Event::Key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        &ActionRegistry::defaults(),
    )
}

#[test]
fn ctrl_g_hides_and_shows_dock_from_prompt() {
    let mut agent = dock_with_task();
    agent.active_pane = AgentPane::Prompt;
    assert!(matches!(ctrl_g(&mut agent), InputOutcome::Changed));
    assert!(agent.dock_hidden);
    assert_eq!(agent.active_pane, AgentPane::Prompt);
    assert!(!agent.tasks.overlay.visible);
    assert!(matches!(ctrl_g(&mut agent), InputOutcome::Changed));
    assert!(!agent.dock_hidden);
    assert_eq!(agent.active_pane, AgentPane::Prompt);
}

#[test]
fn ctrl_g_hides_shown_dock_when_dock_focused() {
    let mut agent = dock_with_task();
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert!(matches!(ctrl_g(&mut agent), InputOutcome::Changed));
    assert!(agent.dock_hidden);
    assert_eq!(agent.active_pane, AgentPane::Scrollback);
}

#[test]
fn left_collapses_and_right_l_expand_the_selected_header() {
    let mut agent = dock_with_task();
    assert!(agent.dock_tasks_expanded);
    assert!(
        matches!(
            agent.dock_items().get(agent.dock_cursor),
            Some(crate::views::dock::DockItem::Header(
                crate::views::dock::Section::Tasks
            ))
        ),
        "cursor starts on the Tasks header"
    );

    let outcome = agent.handle_dock_key(&key(KeyCode::Left, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_tasks_expanded);

    for code in [KeyCode::Right, KeyCode::Char('l')] {
        agent.dock_tasks_expanded = false;
        let outcome = agent.handle_dock_key(&key(code, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "{code:?} must expand, got {outcome:?}"
        );
        assert!(
            agent.dock_tasks_expanded,
            "{code:?} left the section closed"
        );
    }
}

#[test]
fn dock_h_toggles_show_done() {
    let mut agent = dock_with_subagents(&["child-running"], &["child-done"]);
    assert!(!agent.tasks.show_done());
    assert_eq!(dock_subagent_ids(&agent), ["child-running"]);

    assert!(matches!(
        agent.handle_dock_key(&key(KeyCode::Char('h'), KeyModifiers::NONE)),
        InputOutcome::Changed
    ));
    assert!(agent.tasks.show_done());
    assert!(agent.dock_subagents_expanded);
    assert_eq!(dock_subagent_ids(&agent), ["child-running", "child-done"]);
    let rows = agent.dock_subagent_rows();
    let done = rows.get(1).expect("finished child");
    assert_eq!(done.2.activity.as_deref(), Some("completed"));
    assert!(!done.2.spinning);

    assert!(matches!(
        agent.handle_dock_key(&key(KeyCode::Char('h'), KeyModifiers::NONE)),
        InputOutcome::Changed
    ));
    assert!(!agent.tasks.show_done());
    assert_eq!(dock_subagent_ids(&agent), ["child-running"]);
}

#[test]
fn finished_only_subagents_do_not_paint_a_done_summary() {
    let mut agent = dock_with_subagents(&[], &["child-done"]);
    assert!(agent.dock_subagent_rows().is_empty());
    assert!(!agent.dock_items().iter().any(|item| {
        matches!(
            item,
            crate::views::dock::DockItem::Header(crate::views::dock::Section::Subagents)
                | crate::views::dock::DockItem::Row(crate::views::dock::Section::Subagents, _)
        )
    }));

    insert_running_task(&mut agent, "bg-1");
    assert!(agent.dock_subagent_rows().is_empty());
    assert!(matches!(
        agent.handle_dock_key(&key(KeyCode::Char('h'), KeyModifiers::NONE)),
        InputOutcome::Changed
    ));
    assert_eq!(dock_subagent_ids(&agent), ["child-done"]);
}

#[test]
fn left_right_on_a_row_do_not_toggle_the_section() {
    let mut agent = dock_with_task();
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, 0)
            )
        })
        .expect("task row");
    let outcome = agent.handle_dock_key(&key(KeyCode::Left, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert!(agent.dock_tasks_expanded);
}

/// More background tasks than the dock can paint, so the Tasks section keeps a
/// `show N more` row.
fn agent_with_task_overflow() -> AgentView {
    let mut agent = dock_with_task();
    for i in 2..=12 {
        insert_running_task(&mut agent, &format!("bg-{i}"));
    }
    agent
}

#[test]
fn enter_on_show_more_gives_the_section_the_spare_rows() {
    let mut agent = agent_with_task_overflow();
    for child in ["child-1", "child-2"] {
        insert_running_subagent(&mut agent, child);
    }
    // The reveal ceiling is half the space above the prompt, so the view needs
    // a scrollback to measure.
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    let before = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row");

    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert!(agent.dock_tasks_show_all);
    let after = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    assert!(
        after > before,
        "revealing must widen the section: {before} -> {after}"
    );
    assert!(
        !agent.dock_subagents_show_all && !agent.dock_watchers_show_all,
        "revealing Tasks must not leave another section raised"
    );
    let items = agent.dock_items();
    assert!(
        items.contains(&crate::views::dock::DockItem::Header(
            crate::views::dock::Section::Subagents
        )),
        "the other section keeps its header: {items:?}"
    );
    assert!(
        items.len() > crate::views::dock::MAX_DOCK_ROWS as usize,
        "the dock grows past its resting height for the opened section: {items:?}"
    );
}

#[test]
fn clicking_hover_stop_kills_subagent_but_clicking_row_opens_it() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let item = crate::views::dock::DockItem::Row(crate::views::dock::Section::Subagents, 0);
    let row_y = agent.pane_areas.dock.y + 1;
    let stop_col = agent.pane_areas.dock.right() - 1;
    assert!(matches!(
        agent.handle_mouse(&mouse(MouseEventKind::Moved, stop_col, row_y)),
        InputOutcome::Changed
    ));
    assert_eq!(agent.dock_hovered, Some(item));
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop_col,
        row_y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillSubagent(id)) if id == "sa-child-1"
    ));
    assert!(agent.active_subagent.is_none());

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        agent.pane_areas.dock.x + 5,
        row_y,
    ));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_subagent.as_deref(), Some("child-1"));
}

#[test]
fn clicking_hover_stop_kills_task() {
    let mut agent = dock_with_task();
    let item = crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, 0);
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let row_y = agent.pane_areas.dock.y + 1;
    let stop_col = agent.pane_areas.dock.right() - 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, stop_col, row_y));
    assert_eq!(agent.dock_hovered, Some(item));
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop_col,
        row_y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillBgTask(id)) if id == "bg-1"
    ));
}

#[test]
fn clicking_hover_stop_kills_monitor() {
    let mut agent = make_agent();
    let task = crate::app::agent::BgTaskState {
        task_id: "monitor-1".into(),
        tool_call_id: "call-monitor-1".into(),
        command: "tail -f log".into(),
        description: None,
        cwd: "/tmp".into(),
        output_file: "/tmp/monitor-out".into(),
        status: crate::app::agent::BgTaskStatus::Running,
        start_time: std::time::SystemTime::now(),
        end_time: None,
        exit_code: None,
        signal: None,
        stdout: String::new(),
        stdout_line_count: 0,
        truncated: false,
        pending_kill: false,
        kill_requested_at: None,
        scrollback_entry_id: None,
        is_monitor: true,
        restored_from_replay: false,
    };
    agent.session.bg_tasks.insert(task.task_id.clone(), task);
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let item = crate::views::dock::DockItem::Row(crate::views::dock::Section::Watchers, 0);
    let row_y = agent.pane_areas.dock.y + 1;
    let stop_col = agent.pane_areas.dock.right() - 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, stop_col, row_y));
    assert_eq!(agent.dock_hovered, Some(item));
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop_col,
        row_y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillBgTask(id)) if id == "monitor-1"
    ));
}

#[test]
fn clicking_hover_stop_cancels_scheduled_loop() {
    let mut agent = make_agent();
    agent.session.scheduled_tasks.insert(
        "loop-1".into(),
        crate::app::agent::ScheduledTaskInfo {
            task_id: "loop-1".into(),
            prompt: "check CI".into(),
            human_schedule: "every 5m".into(),
            created_at: std::time::Instant::now(),
            next_fire_at: None,
            tag: "loop".into(),
            last_subagent_id: None,
        },
    );
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let item = crate::views::dock::DockItem::Row(crate::views::dock::Section::Watchers, 0);
    let y = agent.pane_areas.dock.y + 1;
    let x = agent.pane_areas.dock.right() - 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, x, y));
    assert_eq!(agent.dock_hovered, Some(item));
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::CancelScheduledTask(id)) if id == "loop-1"
    ));
}

#[test]
fn dock_loop_meta_includes_cadence_and_next_trigger() {
    let mut agent = make_agent();
    let next = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    agent.session.scheduled_tasks.insert(
        "loop-1".into(),
        crate::app::agent::ScheduledTaskInfo {
            task_id: "loop-1".into(),
            prompt: "check CI".into(),
            human_schedule: "every 30 minutes".into(),
            created_at: std::time::Instant::now(),
            next_fire_at: Some(next),
            tag: "loop".into(),
            last_subagent_id: None,
        },
    );
    let rows = agent.dock_watcher_rows();
    let meta = &rows.first().expect("loop row").1.meta;
    assert!(
        meta.starts_with("every 30 minutes (next in ") && !meta.contains("due now"),
        "{meta}"
    );
}

#[test]
fn pending_kill_subagent_has_no_clickable_stop() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent
        .subagent_sessions
        .get_mut("child-1")
        .expect("subagent")
        .attempt
        .pending_kill = true;
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let right = agent.pane_areas.dock.right() - 1;
    let row = agent.pane_areas.dock.y + 1;

    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, right, row));
    cache_stop_button(&mut agent);
    assert!(agent.dock_stop_button.is_none());
    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), right, row));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_subagent.as_deref(), Some("child-1"));
}

#[test]
fn stop_click_uses_current_row_when_rows_change_after_hover() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    insert_running_subagent(&mut agent, "child-2");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 3);
    let x = agent.pane_areas.dock.right() - 1;
    let y = agent.pane_areas.dock.y + 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, x, y));
    assert_eq!(
        agent.dock_hovered,
        Some(crate::views::dock::DockItem::Row(
            crate::views::dock::Section::Subagents,
            0
        ))
    );
    agent.subagent_sessions.remove("child-1");
    agent.subagent_views.remove("child-1");
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillSubagent(id)) if id == "sa-child-2"
    ));
}

#[test]
fn stale_stop_click_does_not_kill_the_replacement_row() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    insert_running_subagent(&mut agent, "child-2");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 3);
    let x = agent.pane_areas.dock.right() - 1;
    let y = agent.pane_areas.dock.y + 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, x, y));
    cache_stop_button(&mut agent);
    assert_eq!(
        agent.dock_stop_button.as_ref().map(|hit| hit.id.clone()),
        Some(crate::app::agent_view::DockKillId::Subagent(
            "sa-child-1".into()
        ))
    );
    agent.subagent_sessions.remove("child-1");
    agent.subagent_views.remove("child-1");

    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    assert!(
        matches!(outcome, InputOutcome::Changed),
        "stale painted kill must not dispatch, got {outcome:?}"
    );
}

#[test]
fn stop_click_targets_the_row_under_the_pointer() {
    let mut agent = make_agent();
    for child in ["child-1", "child-2", "child-3", "child-4"] {
        insert_running_subagent(&mut agent, child);
    }
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, crate::views::dock::MAX_DOCK_ROWS);
    let y = agent.pane_areas.dock.y + 2;
    let x = agent.pane_areas.dock.right() - 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, x, y));
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillSubagent(id)) if id == "sa-child-2"
    ));
}

#[test]
fn every_visible_stop_column_dispatches_and_adjacent_click_opens_row() {
    let stop_width = crate::views::dock::Section::Subagents.kill_label().width() as u16;
    for width in [stop_width, 80] {
        let mut agent = make_agent();
        insert_running_subagent(&mut agent, "child-1");
        agent.dock_shown = true;
        agent.dock_on = true;
        agent.pane_areas.dock = Rect::new(2, 4, width, 2);
        let y = agent.pane_areas.dock.y + 1;
        let _ = agent.handle_mouse(&mouse(
            MouseEventKind::Moved,
            agent.pane_areas.dock.right().saturating_sub(1),
            y,
        ));
        cache_stop_button(&mut agent);
        let stop = agent.dock_stop_button.as_ref().expect("stop").rect;
        for x in stop.x..stop.right() {
            let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
            assert!(matches!(
                outcome,
                InputOutcome::Action(Action::KillSubagent(ref id)) if id == "sa-child-1"
            ));
            cache_stop_button(&mut agent);
        }

        if stop.x > agent.pane_areas.dock.x {
            let outcome = agent.handle_mouse(&mouse(
                MouseEventKind::Down(MouseButton::Left),
                stop.x - 1,
                y,
            ));
            assert!(matches!(outcome, InputOutcome::Changed));
            assert_eq!(agent.active_subagent.as_deref(), Some("child-1"));
        }
    }
}

#[test]
fn occluded_stop_click_does_not_fall_through_to_row_activation() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(2, 4, 78, 2);
    let y = agent.pane_areas.dock.y + 1;
    let _ = agent.handle_mouse(&mouse(
        MouseEventKind::Moved,
        agent.pane_areas.dock.right() - 1,
        y,
    ));
    cache_stop_button(&mut agent);
    let stop = agent.dock_stop_button.as_ref().expect("stop").rect;
    agent.frame_occluder_rects.push(stop);

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop.x,
        stop.y,
    ));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.active_subagent.is_none());
}

#[test]
fn click_on_show_more_widens_the_section_in_place() {
    let mut agent = agent_with_task_overflow();
    for child in ["child-1", "child-2"] {
        insert_running_subagent(&mut agent, child);
    }
    // The reveal ceiling is half the space above the prompt, so the view needs
    // a scrollback to measure.
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    let before = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    let more_row = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row") as u16;
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);

    let outcome = agent.handle_mouse(&MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: agent.pane_areas.dock.y + more_row,
        modifiers: KeyModifiers::NONE,
    });

    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert!(agent.dock_tasks_show_all);
    assert!(
        agent
            .dock_layout()
            .visible_rows(crate::views::dock::Section::Tasks)
            > before
    );
}

#[test]
fn stop_click_after_a_relayout_kills_the_row_now_under_the_pointer() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    agent.active_pane = AgentPane::Prompt;
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    let items = agent.dock_items();
    let more_row = items
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row") as u16;
    let task_row = more_row - 1;
    let row_y = agent.pane_areas.dock.y + task_row;
    let stop_col = agent.pane_areas.dock.right() - 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, stop_col, row_y));
    cache_stop_button(&mut agent);

    // Revealing the rest of the section re-lays out the dock under that cell.
    let _ = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        5,
        agent.pane_areas.dock.y + more_row,
    ));
    assert!(agent.dock_tasks_show_all);
    // Revealing re-lays the dock out, so find where a task row sits now and aim
    // the second click there: the cached kill target must follow the pointer.
    let dock = agent.pane_areas.dock;
    let (row_y, index) = (0..dock.height)
        .find_map(|offset| match agent.dock_item_at(dock, dock.y + offset) {
            Some(crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, index)) => {
                Some((dock.y + offset, index))
            }
            _ => None,
        })
        .expect("a task row sits under the pointer after the relayout");
    let expected = agent
        .dock_task_rows()
        .get(index)
        .unwrap_or_else(|| panic!("missing index"))
        .0
        .clone();

    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, stop_col, row_y));
    cache_stop_button(&mut agent);
    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop_col,
        row_y,
    ));
    assert!(
        matches!(outcome, InputOutcome::Action(Action::KillBgTask(ref id)) if *id == expected),
        "the click must kill the row now under the pointer, got {outcome:?}"
    );
}

#[test]
fn collapsing_a_show_all_section_resets_it_to_preview() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent.dock_cursor = 0;

    let outcome = agent.handle_dock_key(&key(KeyCode::Left, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_tasks_expanded);
    assert!(!agent.dock_tasks_show_all);

    let outcome = agent.handle_dock_key(&key(KeyCode::Right, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.dock_tasks_expanded);
    assert_eq!(
        agent.dock_items().len(),
        crate::views::dock::MAX_DOCK_ROWS as usize,
        "re-expanding fills the dock again, no further"
    );
    assert_eq!(
        agent.dock_items().last(),
        Some(&crate::views::dock::DockItem::RevealRemaining(
            crate::views::dock::Section::Tasks
        ))
    );
}

#[test]
fn wheel_scrolls_the_section_under_the_pointer_and_clicks_follow_it() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    let dock = agent.pane_areas.dock;
    let first = agent
        .dock_item_at(dock, dock.y + 1)
        .expect("first task row");

    agent.handle_scroll(2, 5, dock.y + 1);

    let scrolled = agent
        .dock_item_at(dock, dock.y + 1)
        .expect("first task row");
    assert_ne!(scrolled, first, "the wheel scrolls the section's rows");
    assert_eq!(
        agent
            .dock_items()
            .first()
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::Header(crate::views::dock::Section::Tasks),
        "the header stays on the dock's first row"
    );

    let clicked = agent.dock_item_at(dock, dock.y + 2).expect("second row");
    let outcome = agent.handle_mouse(&MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: dock.y + 2,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        clicked
    );
}

#[test]
fn walking_down_reaches_the_show_more_row_and_enter_opens_it() {
    let mut agent = agent_with_task_overflow();
    agent.dock_shown = true;
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 24, 80, crate::views::dock::MAX_DOCK_ROWS);
    agent.active_pane = AgentPane::Dock;
    let items = agent.dock_items();
    let reveal = items
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row");
    let hidden_before = agent
        .dock_layout()
        .hidden_rows(crate::views::dock::Section::Tasks);

    // Walking down lands on `show N more` without renumbering it on the way.
    agent.dock_cursor = 0;
    for _ in 0..reveal {
        agent.handle_dock_key(&key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            agent
                .dock_layout()
                .hidden_rows(crate::views::dock::Section::Tasks),
            hidden_before,
            "the count must not move while the cursor walks past it"
        );
    }
    assert_eq!(agent.dock_cursor, reveal);
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
    );

    // Enter there opens the section.
    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.dock_tasks_show_all);
    assert!(
        agent
            .dock_layout()
            .visible_rows(crate::views::dock::Section::Tasks)
            > items.len() - reveal,
        "the section opened"
    );
}

#[test]
fn a_crowded_dock_keeps_every_section_header_on_screen() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.dock_tasks_show_all = true;

    let items = agent.dock_items();
    assert!(
        items.len() <= crate::views::dock::MAX_DOCK_ROWS as usize,
        "the dock never asks for more rows than it can paint: {items:?}"
    );
    for section in [
        crate::views::dock::Section::Subagents,
        crate::views::dock::Section::Tasks,
    ] {
        assert!(
            items.contains(&crate::views::dock::DockItem::Header(section)),
            "{section:?} lost its header: {items:?}"
        );
    }
    assert!(
        items.contains(&crate::views::dock::DockItem::RevealRemaining(
            crate::views::dock::Section::Tasks
        )),
        "the rows that do not fit stay behind a show-more row: {items:?}"
    );
}

#[test]
fn hover_follows_the_pointer_after_the_dock_relayouts() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    let x = 5;
    let y = agent.pane_areas.dock.y + 1;
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, x, y));
    assert_eq!(
        agent.dock_hovered,
        Some(
            agent
                .dock_items()
                .get(1)
                .copied()
                .unwrap_or_else(|| panic!("missing index"))
        )
    );

    agent.dock_tasks_show_all = true;
    let dock = agent.pane_areas.dock;
    agent.sync_dock_hover_from_pointer(dock);
    assert_eq!(
        agent.dock_hovered,
        Some(
            agent
                .dock_items()
                .get(1)
                .copied()
                .unwrap_or_else(|| panic!("missing index"))
        )
    );
}

#[test]
fn hover_sync_hit_tests_against_the_current_frame_dock_rect() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent.last_mouse_pos = (5, 5);
    // The previous frame's rect sat lower on screen; the pointer at row 5 is
    // above it and would hit-test to nothing.
    agent.pane_areas.dock = Rect::new(0, 12, 80, 3);
    // This frame the dock moved up and grew (e.g. a queue-from-prompt pin), so
    // row 5 is dock-local row 1. Hover must follow the current rect, not the
    // stale pane area.
    let current = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    agent.sync_dock_hover_from_pointer(current);
    assert_eq!(
        agent.dock_hovered,
        Some(
            agent
                .dock_items()
                .get(1)
                .copied()
                .unwrap_or_else(|| panic!("missing index"))
        )
    );

    let stale = agent.pane_areas.dock;
    agent.sync_dock_hover_from_pointer(stale);
    assert_eq!(agent.dock_hovered, None);
}

#[test]
fn tab_cycles_scrollback_to_dock_to_prompt() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    insert_running_task(&mut agent, "bg-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.vim_mode = true;
    agent.active_pane = AgentPane::Scrollback;
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            *item == crate::views::dock::DockItem::Header(crate::views::dock::Section::Tasks)
        })
        .expect("tasks");
    let registry = ActionRegistry::defaults();

    let outcome = agent.handle_scrollback_key(&key(KeyCode::Tab, KeyModifiers::NONE), &registry);
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::Header(crate::views::dock::Section::Subagents)
    );

    let outcome = agent.handle_dock_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Action(Action::FocusPrompt)));
}

#[test]
fn tab_from_dock_is_one_stop() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.dock_queued_expanded = false;
    agent.active_pane = AgentPane::Dock;
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            *item == crate::views::dock::DockItem::Header(crate::views::dock::Section::Queued)
        })
        .expect("queued");

    let outcome = agent.handle_dock_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Action(Action::FocusPrompt)));

    let outcome = agent.handle_dock_key(&key(KeyCode::BackTab, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Scrollback);
}

#[test]
fn j_reaches_the_queued_header() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.dock_queued_expanded = false;
    agent.active_pane = AgentPane::Dock;
    agent.dock_cursor = 0;
    for _ in 0..agent.dock_items().len() {
        agent.handle_dock_key(&key(KeyCode::Char('j'), KeyModifiers::NONE));
    }
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::Header(crate::views::dock::Section::Queued)
    );
}

#[test]
fn clicking_selected_stop_kills_without_hover() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    // Two rows would go entirely to headers; this test is about the loop row's
    // open behaviour, so give the dock its resting height.
    agent.pane_areas.dock = Rect::new(2, 4, 78, crate::views::dock::MAX_DOCK_ROWS);
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Subagents, 0)
            )
        })
        .expect("subagent row");
    agent.dock_hovered = None;
    let row_y = agent.pane_areas.dock.y + 1;
    cache_stop_button(&mut agent);

    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        agent.pane_areas.dock.right() - 1,
        row_y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillSubagent(id)) if id == "sa-child-1"
    ));
}

#[test]
fn tab_from_scrollback_skips_dock_when_hidden() {
    let mut agent = dock_with_task();
    agent.vim_mode = true;
    agent.dock_shown = false;
    agent.active_pane = AgentPane::Scrollback;
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_scrollback_key(&key(KeyCode::Tab, KeyModifiers::NONE), &registry);
    assert!(
        matches!(outcome, InputOutcome::Action(Action::FocusPrompt)),
        "hidden dock stays out of the Tab cycle, got {outcome:?}"
    );
}

#[test]
fn enter_on_header_still_toggles() {
    let mut agent = dock_with_task();
    assert!(agent.dock_tasks_expanded);
    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_tasks_expanded);
    assert_eq!(agent.dock_enter_label(), Some("expand"));
}

fn header_row(agent: &AgentView, section: crate::views::dock::Section) -> u16 {
    agent
        .dock_items()
        .iter()
        .position(|item| *item == crate::views::dock::DockItem::Header(section))
        .expect("section header") as u16
}

fn painted_header_bg(
    agent: &AgentView,
    section: crate::views::dock::Section,
) -> Option<ratatui::style::Color> {
    let theme = crate::theme::Theme::tokyonight();
    let data = agent.dock_snapshot();
    let area = Rect::new(0, 0, 80, crate::views::dock::desired_height(&data));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    crate::views::dock::render(&mut buf, area, &theme, &data);
    buf.cell((0, header_row(agent, section))).map(|c| c.bg)
}

#[test]
fn clicking_a_section_header_clears_selection_after_collapse() {
    let mut agent = dock_with_task();
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_queued_expanded = true;
    agent.active_pane = AgentPane::Prompt;
    agent.pane_areas.dock = Rect::new(0, 4, 80, 8);
    let queued = crate::views::dock::Section::Queued;
    let y = agent.pane_areas.dock.y + header_row(&agent, queued);

    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, y));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_queued_expanded);
    assert_eq!(agent.active_pane, AgentPane::Prompt);
    assert!(!agent.dock_snapshot().focused);

    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, 5, 0));
    let theme = crate::theme::Theme::tokyonight();
    assert_ne!(painted_header_bg(&agent, queued), Some(theme.bg_highlight));
    assert_ne!(
        painted_header_bg(&agent, queued),
        Some(theme.row_hover_bg())
    );

    let y = agent.pane_areas.dock.y + header_row(&agent, queued);
    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, y));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.dock_queued_expanded);
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert!(agent.dock_snapshot().focused);
}

#[test]
fn clicking_a_focused_section_header_returns_to_prompt() {
    let mut agent = dock_with_task();
    agent.pane_areas.dock = Rect::new(0, 4, 80, 8);
    let tasks = crate::views::dock::Section::Tasks;
    let y = agent.pane_areas.dock.y + header_row(&agent, tasks);

    assert_eq!(agent.active_pane, AgentPane::Dock);
    let outcome = agent.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, y));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_tasks_expanded);
    assert_eq!(agent.active_pane, AgentPane::Prompt);
    assert!(!agent.dock_snapshot().focused);
    let _ = agent.handle_mouse(&mouse(MouseEventKind::Moved, 5, 0));
    assert_ne!(
        painted_header_bg(&agent, tasks),
        Some(crate::theme::Theme::tokyonight().bg_highlight)
    );
}

#[test]
fn keyboard_collapse_keeps_dock_focus_on_the_header() {
    let mut agent = dock_with_task();
    assert!(agent.dock_tasks_expanded);
    let outcome = agent.handle_dock_key(&key(KeyCode::Left, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!agent.dock_tasks_expanded);
    assert_eq!(agent.active_pane, AgentPane::Dock);
    assert!(agent.dock_snapshot().focused);
    assert_eq!(
        painted_header_bg(&agent, crate::views::dock::Section::Tasks),
        Some(crate::theme::Theme::tokyonight().bg_highlight)
    );
}

#[test]
fn a_crowded_dock_still_shows_the_queued_header_and_its_body() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    let data = crate::views::dock::DockData {
        tasks: agent
            .dock_task_rows()
            .into_iter()
            .map(|(_, row)| row)
            .collect(),
        queued: 1,
        tasks_expanded: true,
        tasks_show_all: true,
        queue_body_rows: 2,
        ..Default::default()
    };
    let items = crate::views::dock::visible_items(&data);
    assert_eq!(
        items.last(),
        Some(&crate::views::dock::DockItem::Header(
            crate::views::dock::Section::Queued
        )),
        "a long Tasks section cannot push the Queued header out: {items:?}"
    );

    let area = Rect::new(0, 4, 80, crate::views::dock::desired_height(&data));
    let body = crate::views::dock::queue_body_rect(area, &data);
    assert!(body.height > 0, "the queue body keeps a row of its own");
    assert_eq!(
        body.y,
        area.y + items.len() as u16,
        "the body starts below the last painted row"
    );
    assert_eq!(body.bottom(), area.bottom());
}

#[test]
fn toggle_queue_does_not_open_hidden_pane_when_dock_on_but_empty() {
    let mut agent = make_agent();
    agent.dock_on = true;
    agent.dock_shown = false;
    agent.queue.overlay.visible = false;
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char(';'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "empty dock must not toggle the suppressed queue pane, got {outcome:?}"
    );
    assert!(!agent.queue.overlay.visible);
}

#[test]
fn enter_and_click_open_a_task_row() {
    let mut agent = dock_with_task();
    // Two rows would go entirely to headers; this test is about the loop row's
    // open behaviour, so give the dock its resting height.
    agent.pane_areas.dock = Rect::new(2, 4, 78, crate::views::dock::MAX_DOCK_ROWS);
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, 0)
            )
        })
        .expect("task row");

    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(
        agent
            .block_viewer
            .as_ref()
            .and_then(|v| v.bg_task_id.as_deref()),
        Some("bg-1")
    );
    assert_eq!(agent.active_pane, AgentPane::Scrollback);

    agent.block_viewer = None;
    agent.active_pane = AgentPane::Dock;
    let y = agent.pane_areas.dock.y + 1;
    let _ = agent.handle_mouse(&mouse(
        MouseEventKind::Moved,
        agent.pane_areas.dock.right() - 1,
        y,
    ));
    cache_stop_button(&mut agent);
    let stop = agent.dock_stop_button.as_ref().expect("stop").rect;
    assert!(stop.x > agent.pane_areas.dock.x);
    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        stop.x - 1,
        y,
    ));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(
        agent
            .block_viewer
            .as_ref()
            .and_then(|v| v.bg_task_id.as_deref()),
        Some("bg-1")
    );
}

#[test]
fn click_opens_a_linked_loop_and_ignores_an_unlinked_one() {
    let mut agent = make_agent();
    insert_running_subagent(&mut agent, "child-1");
    agent.session.scheduled_tasks.insert(
        "loop-1".into(),
        crate::app::agent::ScheduledTaskInfo {
            task_id: "loop-1".into(),
            prompt: "check CI".into(),
            human_schedule: "every 5m".into(),
            created_at: std::time::Instant::now(),
            next_fire_at: None,
            tag: "loop".into(),
            last_subagent_id: Some("sa-child-1".into()),
        },
    );
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    // Two rows would go entirely to headers; this test is about the loop row's
    // open behaviour, so give the dock its resting height.
    agent.pane_areas.dock = Rect::new(2, 4, 78, crate::views::dock::MAX_DOCK_ROWS);
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Watchers, 0)
            )
        })
        .expect("loop row");
    assert!(
        agent
            .dock_watcher_rows()
            .first()
            .unwrap_or_else(|| panic!("missing index"))
            .1
            .openable,
        "linked loop must paint the view control"
    );

    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_subagent.as_deref(), Some("child-1"));

    agent.active_subagent = None;
    agent.active_pane = AgentPane::Dock;
    agent
        .session
        .scheduled_tasks
        .get_mut("loop-1")
        .expect("loop")
        .last_subagent_id = None;
    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert!(agent.active_subagent.is_none());
}

#[test]
fn an_unfocused_wheel_scrolls_rows_without_moving_the_cursor() {
    let mut agent = agent_with_task_overflow();
    agent.active_pane = AgentPane::Prompt;
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    agent.dock_cursor = 0;
    let dock = agent.pane_areas.dock;
    let first = agent
        .dock_item_at(dock, dock.y + 1)
        .expect("first task row");

    agent.handle_scroll(2, 5, dock.y + 1);

    assert_eq!(agent.dock_cursor, 0, "the wheel does not move the cursor");
    assert_ne!(
        agent
            .dock_item_at(dock, dock.y + 1)
            .expect("first task row"),
        first,
        "an unfocused dock still scrolls under the pointer"
    );
}

#[test]
fn a_collapsed_queue_still_keeps_its_header() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent.dock_queued_expanded = false;
    agent.active_pane = AgentPane::Prompt;
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    let data = crate::views::dock::DockData {
        tasks: agent
            .dock_task_rows()
            .into_iter()
            .map(|(_, row)| row)
            .collect(),
        queued: 1,
        tasks_expanded: true,
        tasks_show_all: true,
        queue_body_rows: 0,
        ..Default::default()
    };
    let items = crate::views::dock::visible_items(&data);
    assert!(
        items.contains(&crate::views::dock::DockItem::Header(
            crate::views::dock::Section::Queued
        )),
        "{items:?}"
    );
    assert_eq!(
        crate::views::dock::queue_body_rect(
            Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS),
            &data
        ),
        Rect::default(),
        "a collapsed queue paints no body"
    );
}

#[test]
fn clamp_dock_overflow_resets_show_all_when_a_section_shrinks() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent.dock_cursor = 20;
    agent.clamp_dock_overflow();
    assert!(agent.dock_tasks_show_all);
    assert!(agent.dock_cursor < agent.dock_items().len());

    agent
        .session
        .bg_tasks
        .retain(|id, _| id == "bg-1" || id == "bg-2");
    agent.clamp_dock_overflow();
    assert!(!agent.dock_tasks_show_all);
    let n = agent.dock_items().len();
    assert!(n > 0);
    assert!(agent.dock_cursor < n);
}

#[test]
fn reconcile_before_paint_resets_show_all_after_a_background_shrink() {
    let mut agent = agent_with_task_overflow();
    agent.dock_on = true;
    agent.dock_tasks_show_all = true;
    // A background completion shrinks the section below the overflow threshold,
    // with no dock key pressed since. The pre-paint reconciliation (not the
    // paint pass) must drop show-all so a later refill re-enters the preview.
    agent
        .session
        .bg_tasks
        .retain(|id, _| id == "bg-1" || id == "bg-2");
    agent.reconcile_dock_before_paint();
    assert!(!agent.dock_tasks_show_all);

    // With the dock off, reconciliation is a no-op.
    let mut agent = agent_with_task_overflow();
    agent.dock_on = false;
    agent.dock_tasks_show_all = true;
    agent
        .session
        .bg_tasks
        .retain(|id, _| id == "bg-1" || id == "bg-2");
    agent.reconcile_dock_before_paint();
    assert!(agent.dock_tasks_show_all);
}

#[test]
fn reveal_aims_the_cursor_when_the_last_painted_dock_is_the_resting_cap() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    let first_hidden = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    assert!(
        !agent
            .dock_items()
            .contains(&crate::views::dock::DockItem::Row(
                crate::views::dock::Section::Tasks,
                first_hidden
            )),
        "the first hidden row is past the last-painted resting cap"
    );
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row");

    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.dock_tasks_show_all);
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, first_hidden),
        "the cursor must follow the uncovered row, not stay on show-more because dock_items() was still the resting cap"
    );
}

#[test]
fn reveal_after_scroll_aims_the_cursor_at_the_first_hidden_row() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    let visible = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    assert!(
        agent.scroll_dock_section(crate::views::dock::Section::Tasks, 3),
        "test needs a section that can scroll"
    );
    let layout = agent.dock_layout();
    let offset = layout.row_offset(crate::views::dock::Section::Tasks);
    assert!(offset > 0, "the section must have scrolled");
    let first_hidden = offset + layout.visible_rows(crate::views::dock::Section::Tasks);
    assert_ne!(
        first_hidden, visible,
        "after a scroll the first hidden index is not the visible-row count"
    );
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row");

    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.dock_tasks_show_all);
    assert_eq!(
        agent
            .dock_items()
            .get(agent.dock_cursor)
            .copied()
            .unwrap_or_else(|| panic!("missing index")),
        crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, first_hidden),
        "the cursor must follow the first row the reveal uncovers"
    );
}

#[test]
fn revealing_a_second_section_leaves_the_first_open() {
    let mut agent = agent_with_task_overflow();
    for i in 1..=8 {
        insert_running_monitor(&mut agent, &format!("mon-{i}"));
    }
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 4, 80, 16);

    let reveal = |agent: &mut AgentView, section| {
        agent.dock_cursor = agent
            .dock_items()
            .iter()
            .position(|item| *item == crate::views::dock::DockItem::RevealRemaining(section))
            .unwrap_or_else(|| panic!("{section:?} overflow row"));
        agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    };

    let tasks_at_rest = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    reveal(&mut agent, crate::views::dock::Section::Tasks);
    let tasks_open = agent
        .dock_layout()
        .visible_rows(crate::views::dock::Section::Tasks);
    assert!(
        tasks_open > tasks_at_rest,
        "Tasks opened: {tasks_at_rest} -> {tasks_open}"
    );

    reveal(&mut agent, crate::views::dock::Section::Watchers);

    assert!(
        agent.dock_tasks_show_all && agent.dock_watchers_show_all,
        "opening Watchers must not close Tasks"
    );
    assert_eq!(
        agent
            .dock_layout()
            .visible_rows(crate::views::dock::Section::Tasks),
        tasks_open,
        "and must not take back the rows Tasks was already showing"
    );
    assert!(
        agent
            .dock_layout()
            .visible_rows(crate::views::dock::Section::Watchers)
            > 1,
        "while Watchers opens too"
    );
}

#[test]
fn reveal_does_not_shrink_a_floor_taller_dock_assignment() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    for i in 1..=4 {
        insert_running_monitor(&mut agent, &format!("mon-{i}"));
    }
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_subagents_expanded = true;
    agent.dock_tasks_expanded = true;
    agent.dock_watchers_expanded = true;
    agent.dock_queued_expanded = true;
    // Floor-taller assignment (11) on a short scrollback: half of 8+11 is 9.
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 8);
    agent.pane_areas.dock = Rect::new(0, 8, 80, 11);
    let assigned = agent.pane_areas.dock.height;
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("tasks overflow");

    let _ = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(agent.dock_tasks_show_all);
    assert!(
        agent.pane_areas.dock.height >= assigned,
        "reveal must not shrink a floor-taller assignment ({assigned} -> {})",
        agent.pane_areas.dock.height
    );
}

#[test]
fn reveal_on_a_typical_terminal_does_not_shrink_the_dock() {
    let mut agent = agent_with_task_overflow();
    // above_prompt = 16 → dock_max_rows stays 8. Floor for twelve tasks is 3.
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 8);
    agent.pane_areas.dock = Rect::new(0, 8, 80, crate::views::dock::MAX_DOCK_ROWS);
    let assigned = agent.pane_areas.dock.height;
    let before = crate::views::dock::desired_height(&agent.dock_snapshot());
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row");

    let _ = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(agent.dock_tasks_show_all);
    assert!(
        agent.pane_areas.dock.height >= assigned,
        "reveal must not shrink the last-painted band ({assigned} -> {})",
        agent.pane_areas.dock.height
    );
    assert!(
        crate::views::dock::desired_height(&agent.dock_snapshot()) >= before,
        "the next layout pass must not ask for fewer rows than the resting band"
    );
}

#[test]
fn a_reveal_may_take_everything_above_the_prompt_but_the_scrollback_floor() {
    let mut agent = agent_with_task_overflow();
    for i in 1..=20 {
        insert_running_subagent(&mut agent, &format!("child-{i}"));
    }
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 24, 80, 8);
    let above_prompt = 24 + 8;
    let floor = crate::views::agent::SCROLLBACK_MIN_ROWS;

    agent.dock_subagents_show_all = true;

    let asked = crate::views::dock::desired_height(&agent.dock_snapshot());
    assert!(
        asked > crate::views::dock::MAX_DOCK_ROWS,
        "an opened section grows past the resting height: {asked}"
    );
    assert!(
        asked <= above_prompt - floor,
        "but never past what the scrollback can spare: {asked} of {}",
        above_prompt - floor
    );
}

#[test]
fn revealed_scroll_follows_the_assigned_height_not_the_raised_request() {
    let mut agent = agent_with_task_overflow();
    agent.dock_tasks_show_all = true;
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    // This frame assigned the resting band. The reveal request is larger
    // (half of scrollback + dock). Scroll must still hide rows past 8.
    agent.pane_areas.dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);

    let assigned = crate::views::dock::DockLayout::with_cap(
        &agent.dock_counts(),
        crate::views::dock::MAX_DOCK_ROWS as usize,
    );
    let layout = agent.dock_layout();
    assert_eq!(
        layout.rows(),
        assigned.rows(),
        "a revealed section squeezed to the resting band must scroll that band, not the raised request"
    );
    assert!(
        layout.rows_below(crate::views::dock::Section::Tasks) > 0,
        "twelve tasks still overflow an 8-row assignment"
    );
    assert!(
        agent.scroll_dock_section(crate::views::dock::Section::Tasks, 1),
        "wheel/keyboard must still reach rows the raised request would have called on-screen"
    );
}

#[test]
fn dock_item_at_budgets_against_the_passed_rect_height() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_shown = true;
    agent.dock_on = true;
    // Prior frame was tall; this frame assigned 4. Hit-testing the live rect
    // must cap to 4 headers, not the stale 14-row item list.
    agent.pane_areas.dock = Rect::new(0, 4, 80, 14);
    let squeezed = Rect::new(0, 4, 80, 4);
    let hit: Vec<_> = (0..4)
        .filter_map(|i| agent.dock_item_at(squeezed, squeezed.y + i))
        .collect();
    let expected = crate::views::dock::DockLayout::with_cap(&agent.dock_counts(), 4)
        .rows()
        .to_vec();
    assert_eq!(
        hit, expected,
        "the passed rect height, not pane_areas.dock, must budget the hit-test: {hit:?}"
    );
    assert!(
        agent.dock_layout().rows().len() > hit.len(),
        "the stale tall pane_areas layout must not be what the squeezed rect hit-tests"
    );
}

#[test]
fn a_squeezed_dock_keeps_trailing_headers_reachable() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.pane_areas.dock = Rect::new(0, 4, 80, 4);

    let items = agent.dock_items();
    for section in [
        crate::views::dock::Section::Subagents,
        crate::views::dock::Section::Tasks,
        crate::views::dock::Section::Queued,
    ] {
        assert!(
            items.contains(&crate::views::dock::DockItem::Header(section)),
            "{section:?} must stay reachable when the frame assigns 4 rows: {items:?}"
        );
    }
    assert_eq!(
        items.len(),
        4,
        "a 4-row assignment spends its rows on headers: {items:?}"
    );
}

#[test]
fn wheel_over_a_band_scrolls_it_without_changing_how_many_rows_it_shows() {
    let mut agent = agent_with_task_overflow();
    insert_running_subagent(&mut agent, "child-1");
    for i in 1..=4 {
        insert_running_monitor(&mut agent, &format!("watch-{i}"));
    }
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    let dock = Rect::new(0, 4, 80, crate::views::dock::MAX_DOCK_ROWS);
    agent.pane_areas.dock = dock;

    let layout = agent.dock_layout();
    let shown = layout.visible_rows(crate::views::dock::Section::Tasks);
    assert!(
        shown >= 1,
        "a crowded resting dock still shows a row under the Tasks header"
    );
    assert!(
        layout.rows_below(crate::views::dock::Section::Tasks) > 0,
        "the reveal label must have a hidden count"
    );
    let reveal_row = layout
        .rows()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("Tasks show-more row");

    agent.handle_scroll(3, dock.x + 5, dock.y + reveal_row as u16);

    assert_eq!(
        agent
            .dock_layout()
            .visible_rows(crate::views::dock::Section::Tasks),
        shown,
        "scrolling changes which rows a band shows, never how many"
    );
}

#[test]
fn revealing_a_section_leaves_the_hit_test_map_alone() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 24, 80, crate::views::dock::MAX_DOCK_ROWS);
    let before = agent.pane_areas;
    let more_row = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row") as u16;

    let _ = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        5,
        agent.pane_areas.dock.y + more_row,
    ));

    assert!(agent.dock_tasks_show_all);
    assert_eq!(
        agent.pane_areas.dock, before.dock,
        "the reveal must not rewrite the rect the mouse hit-tests against"
    );
    assert_eq!(
        agent.pane_areas.scrollback, before.scrollback,
        "nor borrow rows from the scrollback's rect"
    );
    assert!(
        agent.dock_reveal_pending,
        "it flags the reveal for the next frame instead"
    );
}

#[test]
fn a_frame_spends_the_pending_reveal() {
    let mut agent = agent_with_task_overflow();
    agent.pane_areas.scrollback = Rect::new(0, 0, 80, 24);
    agent.pane_areas.dock = Rect::new(0, 24, 80, crate::views::dock::MAX_DOCK_ROWS);
    let more_row = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::RevealRemaining(crate::views::dock::Section::Tasks)
            )
        })
        .expect("overflow row") as u16;

    let _ = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        5,
        agent.pane_areas.dock.y + more_row,
    ));
    assert!(agent.dock_reveal_pending, "the reveal is waiting for rows");
    // While pending, the layout budgets the full ask rather than the height the
    // last frame assigned, so the cursor can reach a row the reveal uncovered.
    assert!(
        agent.dock_items().len() > crate::views::dock::MAX_DOCK_ROWS as usize,
        "a pending reveal budgets its ask, not the old assignment"
    );

    assert!(agent.take_dock_row_request(), "the frame spends it");
    assert!(
        !agent.dock_reveal_pending,
        "a stale flag would budget rows nothing painted"
    );
    assert!(
        !agent.take_dock_row_request(),
        "and a frame that paints no dock leaves nothing behind"
    );
}

fn workflow_run(
    run_id: &str,
    name: &str,
    status: &str,
) -> crate::views::workflows::WorkflowRunSnapshot {
    crate::views::workflows::WorkflowRunSnapshot {
        run_id: run_id.to_owned(),
        name: name.to_owned(),
        objective: "obj".to_owned(),
        status: status.to_owned(),
        management_available: true,
        builtin: false,
        phases: Vec::new(),
        current_phase: Some("Verify".to_owned()),
        agents: vec![crate::views::workflows::WorkflowAgentRowView {
            agent_id: "a1".into(),
            label: "one".into(),
            phase: Some("Verify".into()),
            model: None,
            state: "running".into(),
            tokens_used: 0,
            duration_ms: 0,
        }],
        agent_budget: None,
        agents_used: 0,
        agents_reserved: 0,
        agents_remaining: None,
        agent_usage_incomplete: false,
        active_agents: 1,
        elapsed_ms: 5_000,
        received_at: std::time::Instant::now(),
        pause_message: None,
        result_summary: None,
    }
}

fn dock_with_workflow() -> AgentView {
    let mut agent = make_agent();
    agent
        .workflow_runs
        .push(workflow_run("wf-1", "learn-traces-2", "active"));
    agent.session.scheduled_tasks.insert(
        "loop-1".into(),
        crate::app::agent::ScheduledTaskInfo {
            task_id: "loop-1".into(),
            prompt: "check CI".into(),
            human_schedule: "every 5m".into(),
            created_at: std::time::Instant::now(),
            next_fire_at: None,
            tag: "loop".into(),
            last_subagent_id: None,
        },
    );
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    agent
}

fn workflow_row_index(agent: &AgentView) -> usize {
    agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Workflows, 0)
            )
        })
        .expect("workflow row")
}

#[test]
fn hidden_dock_keeps_the_running_workflow_status_cue() {
    let mut agent = dock_with_workflow();
    assert_eq!(agent.watchers().workflows, 1);
    assert!(agent.dock_covers_idle_cues(true));
    agent.dock_hidden = true;
    assert!(!agent.dock_covers_idle_cues(true));
    assert_eq!(agent.watchers().workflows, 1);
}

#[test]
fn running_workflow_is_a_dock_row_above_watchers() {
    let agent = dock_with_workflow();
    let items = agent.dock_items();
    assert_eq!(
        items.first(),
        Some(&crate::views::dock::DockItem::Header(
            crate::views::dock::Section::Workflows
        ))
    );
    assert_eq!(
        items.get(1),
        Some(&crate::views::dock::DockItem::Row(
            crate::views::dock::Section::Workflows,
            0
        ))
    );
    assert!(
        items.contains(&crate::views::dock::DockItem::Header(
            crate::views::dock::Section::Watchers
        )),
        "watchers stay after the workflow row: {items:?}"
    );
    let (run_id, row) = agent
        .dock_workflow_rows()
        .into_iter()
        .next()
        .expect("workflow row");
    assert_eq!(run_id, "wf-1");
    assert_eq!(row.kind, "Workflow");
    assert_eq!(row.description, "learn-traces-2");
    assert_eq!(row.activity.as_deref(), Some("Verify · 1 agent"));
    assert!(row.openable && row.killable && row.spinning);
}

#[test]
fn terminal_workflow_is_hidden_from_the_dock() {
    let mut agent = make_agent();
    agent
        .workflow_runs
        .push(workflow_run("wf-done", "old-scan", "complete"));
    agent.dock_shown = true;
    agent.dock_on = true;
    assert!(agent.dock_workflow_rows().is_empty());
    assert!(!agent.dock_items().iter().any(|item| {
        matches!(
            item,
            crate::views::dock::DockItem::Header(crate::views::dock::Section::Workflows)
                | crate::views::dock::DockItem::Row(crate::views::dock::Section::Workflows, _)
        )
    }));
}

#[test]
fn enter_on_workflow_row_opens_the_active_run() {
    let mut agent = dock_with_workflow();
    agent.dock_cursor = workflow_row_index(&agent);
    let outcome = agent.handle_dock_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.show_workflows);
    assert_eq!(agent.workflows_view.detail_run_id.as_deref(), Some("wf-1"));
    assert_eq!(
        agent.workflows_view.selected_run_id.as_deref(),
        Some("wf-1")
    );
}

#[test]
fn x_on_workflow_row_stops_the_run() {
    let mut agent = dock_with_workflow();
    agent.dock_cursor = workflow_row_index(&agent);
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::SendSlashCommandPreservingDraft(ref command))
            if command == "/workflow stop learn-traces-2"
    ));
}
