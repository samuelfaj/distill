use indexmap::IndexMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::*;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::theme::Theme;
use crate::views::dashboard::chrome::render_actions_row;
use crate::views::dashboard::state::{
    ActionsFocus, DashboardRowId, DashboardState, Filter, Focusable, RowState, SectionKey,
};
use crate::views::dashboard::test_support::{buf_to_text, header_test_row};

fn render_actions(state: &mut DashboardState, width: u16, workspace: bool) {
    let registry = crate::actions::ActionRegistry::defaults();
    state.clear_chrome_hit_areas();
    let area = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(area);
    render_actions_row(
        &mut buf,
        area,
        &Theme::groknight(),
        state,
        &registry,
        workspace,
    );
}

fn press(state: &mut DashboardState, code: crossterm::event::KeyCode) {
    let registry = crate::actions::ActionRegistry::defaults();
    let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
    let _ = state.handle_input(&event, &registry);
}

fn paint_board(
    rows: &[DashboardRow],
    state: &mut DashboardState,
    width: u16,
    height: u16,
) -> String {
    paint_board_with(
        rows,
        state,
        width,
        height,
        Grouping::State,
        &IndexMap::new(),
    )
}

fn paint_board_with(
    rows: &[DashboardRow],
    state: &mut DashboardState,
    width: u16,
    height: u16,
    grouping: Grouping,
    agents: &IndexMap<AgentId, AgentView>,
) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    render_board(
        &mut buf,
        area,
        &Theme::groknight(),
        rows,
        grouping,
        state,
        agents,
    );
    buf_to_text(&buf)
}

fn paint_dashboard(
    state: &mut DashboardState,
    agents: &mut IndexMap<AgentId, AgentView>,
    width: u16,
    height: u16,
) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let registry = crate::actions::ActionRegistry::defaults();
    let _ = super::super::render::render_dashboard(
        &mut buf,
        area,
        state,
        agents,
        &registry,
        None,
        &[],
        false,
        crate::views::dashboard::WorkspaceRowInputs::default(),
        None,
        false,
        None,
        None,
    );
    buf_to_text(&buf)
}

fn ask_question_agent(question: &str) -> AgentView {
    use crate::views::prompt_widget::StashedPrompt;
    use crate::views::question_view::QuestionViewState;
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};
    let mut agent = crate::test_util::make_agent_view(Some("ask-session"), "/tmp/ask");
    agent.display_name = Some("ask-one".into());
    agent.question_view = Some(QuestionViewState::new(
        "ask-call".into(),
        vec![Question {
            question: question.to_string(),
            options: vec![QuestionOption {
                label: "us-east".into(),
                description: String::new(),
                preview: None,
                id: None,
            }],
            multi_select: None,
            id: None,
        }],
        StashedPrompt::default(),
    ));
    agent
}

#[test]
fn board_button_sits_next_to_open_previous_and_toggles() {
    let mut state = DashboardState::new();
    state.list_focused = true;
    state.cwd_has_git_ancestor = true;
    render_actions(&mut state, 120, true);
    let text_area = {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        let registry = crate::actions::ActionRegistry::defaults();
        render_actions_row(
            &mut buf,
            area,
            &Theme::groknight(),
            &mut state,
            &registry,
            true,
        );
        buf_to_text(&buf)
    };
    assert!(text_area.contains("Board"), "got {text_area:?}");
    assert!(text_area.contains("Open Previous"));
    let hit = state
        .board_view_button_hit
        .rect
        .expect("Board button painted");
    let click = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    let registry = crate::actions::ActionRegistry::defaults();
    assert!(matches!(
        state.handle_input(&click, &registry),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert!(state.board_view);
    render_actions(&mut state, 120, true);
    let area = Rect::new(0, 0, 120, 1);
    let mut buf = Buffer::empty(area);
    render_actions_row(
        &mut buf,
        area,
        &Theme::groknight(),
        &mut state,
        &registry,
        true,
    );
    assert!(buf_to_text(&buf).contains("Row view"));
}

#[test]
fn board_button_keyboard_toggle_and_enter() {
    let mut state = DashboardState::new();
    state.list_focused = true;
    state.cwd_has_git_ancestor = true;
    render_actions(&mut state, 120, true);
    press(&mut state, crossterm::event::KeyCode::Right);
    assert_eq!(state.actions_focus, Some(ActionsFocus::BoardView));
    press(&mut state, crossterm::event::KeyCode::Enter);
    assert!(state.board_view);
    press(&mut state, crossterm::event::KeyCode::Enter);
    assert!(!state.board_view);
}

#[test]
fn board_buckets_needs_input_then_working_then_settled() {
    let rows = vec![
        header_test_row(1, RowState::Idle, "idle-one"),
        header_test_row(2, RowState::Working, "work-one"),
        header_test_row(3, RowState::NeedsInput, "ask-one"),
        header_test_row(4, RowState::Completed, "done-one"),
        header_test_row(5, RowState::Failed, "fail-one"),
    ];
    let mut state = DashboardState::new();
    state.board_view = true;
    state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(3)));
    let text = paint_board(&rows, &mut state, 80, 24);
    let ask = text.find("Awaiting").expect("awaiting bucket");
    let work = text.find("Working").expect("working bucket");
    let idle = text.find("Idle").expect("idle settled");
    assert!(ask < work, "Needs input before Working");
    assert!(work < idle, "Working before settled");
    assert!(text.contains("ask-one"));
    assert!(!text.to_lowercase().contains("completed idle"));
}

#[test]
fn board_space_and_click_expand_card() {
    let rows = vec![header_test_row(1, RowState::Working, "work-one")];
    let mut state = DashboardState::new();
    state.board_view = true;
    state.list_focused = true;
    state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(1)));
    let _ = paint_board(&rows, &mut state, 80, 12);
    press(&mut state, crossterm::event::KeyCode::Char(' '));
    assert!(state.board_card_expanded(&DashboardRowId::TopLevel(crate::app::agent::AgentId(1))));
    let hit = state.row_rects[0].1;
    let click = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    let registry = crate::actions::ActionRegistry::defaults();
    assert!(matches!(
        state.handle_input(&click, &registry),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert!(!state.board_card_expanded(&DashboardRowId::TopLevel(crate::app::agent::AgentId(1))));
}

#[test]
fn board_status_change_keeps_selection() {
    let mut rows = vec![
        header_test_row(1, RowState::Working, "alpha"),
        header_test_row(2, RowState::NeedsInput, "beta"),
    ];
    let mut state = DashboardState::new();
    state.board_view = true;
    let id = DashboardRowId::TopLevel(crate::app::agent::AgentId(1));
    state.focus_row(id.clone());
    let _ = paint_board(&rows, &mut state, 80, 16);
    rows[0].state = RowState::Idle;
    state.reanchor_selection(&rows);
    assert_eq!(state.selected.as_ref(), Some(&id));
}

#[test]
fn board_narrow_is_one_column_wide_is_multi() {
    assert_eq!(board_columns(40), 1);
    assert_eq!(board_columns(80), 2);
    assert_eq!(board_columns(120), 3);
    let rows = vec![
        header_test_row(1, RowState::Working, "a"),
        header_test_row(2, RowState::Working, "b"),
    ];
    let mut narrow = DashboardState::new();
    let _ = paint_board(&rows, &mut narrow, 50, 16);
    let cols: Vec<u16> = narrow.board_slots.iter().map(|(_, (c, _))| *c).collect();
    assert_eq!(cols, vec![0, 0]);
    let mut wide = DashboardState::new();
    let _ = paint_board(&rows, &mut wide, 120, 16);
    let cols: Vec<u16> = wide.board_slots.iter().map(|(_, (c, _))| *c).collect();
    assert_eq!(cols, vec![0, 1]);
}

#[test]
fn board_filter_empty_uses_no_cards() {
    let rows: Vec<DashboardRow> = Vec::new();
    let mut state = DashboardState::new();
    state.board_view = true;
    let text = paint_board(&rows, &mut state, 80, 8);
    assert!(state.row_rects.is_empty());
    assert!(!text.contains("Awaiting"));
}

#[test]
fn board_enter_still_attaches() {
    let mut state = DashboardState::new();
    state.board_view = true;
    state.list_focused = true;
    state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(1)));
    let registry = crate::actions::ActionRegistry::defaults();
    let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.handle_input(&event, &registry),
        crate::app::app_view::InputOutcome::Action(crate::app::actions::Action::DashboardAttach(_))
    ));
}

#[test]
fn board_pinned_directory_order_still_needs_input_first() {
    let mut idle = header_test_row(1, RowState::Idle, "idle-one");
    idle.pinned = true;
    idle.cwd_display = "/tmp/a".into();
    let mut ask = header_test_row(2, RowState::NeedsInput, "ask-one");
    ask.cwd_display = "/tmp/b".into();
    let mut work = header_test_row(3, RowState::Working, "work-one");
    work.cwd_display = "/tmp/a".into();
    let rows = vec![idle, ask, work];
    let mut state = DashboardState::new();
    state.board_view = true;
    state.grouping = Grouping::Directory;
    let text = paint_board_with(
        &rows,
        &mut state,
        80,
        24,
        Grouping::Directory,
        &IndexMap::new(),
    );
    let ask_at = text.find("Awaiting").expect("awaiting bucket");
    let work_at = text.find("Working").expect("working bucket");
    let idle_at = text.find("Idle").expect("idle bucket");
    assert!(ask_at < work_at, "Needs input before Working, got {text}");
    assert!(work_at < idle_at, "Working before Idle, got {text}");
    assert!(!text.contains("Pinned"));
    assert_eq!(state.grouping, Grouping::Directory);
}

#[test]
fn board_expanded_question_uses_peek_fields_and_wraps() {
    let mut agents = IndexMap::new();
    agents.insert(AgentId(0), ask_question_agent("Which deployment region?"));
    let mut idle = crate::test_util::make_agent_view(Some("idle-session"), "/tmp/idle");
    idle.display_name = Some("idle-one".into());
    agents.insert(AgentId(1), idle);
    let mut state = DashboardState::new();
    state.board_view = true;
    state.grouping = Grouping::Directory;
    state.pinned.insert(DashboardRowId::TopLevel(AgentId(1)));
    let ask_id = DashboardRowId::TopLevel(AgentId(0));
    state.focus_row(ask_id.clone());
    state.expanded_board.insert(ask_id);
    let text = paint_dashboard(&mut state, &mut agents, 100, 28);
    assert!(
        text.contains("Which deployment region?"),
        "expanded card must show real question, got {text}"
    );
    assert!(
        !text.contains("Pending: question") || text.contains("Which deployment region?"),
        "generic pending must not replace the question, got {text}"
    );
    let ask_at = text.find("Awaiting").expect("awaiting bucket");
    let idle_at = text.find("Idle").expect("idle bucket");
    assert!(
        ask_at < idle_at,
        "NeedsInput before pinned Idle, got {text}"
    );
    assert_eq!(state.grouping, Grouping::Directory);
}

#[test]
fn board_expanded_wraps_long_activity_and_latest() {
    let pad = "run-cargo-test-all-features ";
    let activity = format!("{}CARGO_TAILWORD", pad.repeat(6));
    let latest = format!(
        "{}REPLY_TAILWORD",
        "the-latest-assistant-response ".repeat(6)
    );
    let mut row = header_test_row(1, RowState::Working, "work-one");
    row.activity = Some(activity);
    row.secondary_line = Some(latest);
    let rows = vec![row];
    let mut collapsed = DashboardState::new();
    collapsed.board_view = true;
    let collapsed_text = paint_board(&rows, &mut collapsed, 40, 16);
    assert!(
        !collapsed_text.contains("CARGO_TAILWORD"),
        "collapsed preview must truncate, got {collapsed_text}"
    );
    assert!(
        !collapsed_text.contains("REPLY_TAILWORD"),
        "collapsed preview must truncate latest, got {collapsed_text}"
    );
    let mut expanded = DashboardState::new();
    expanded.board_view = true;
    let id = DashboardRowId::TopLevel(AgentId(1));
    expanded.expanded_board.insert(id);
    let expanded_text = paint_board(&rows, &mut expanded, 40, 16);
    assert!(
        expanded_text.contains("CARGO_TAILWORD"),
        "expanded wrap must reveal activity tail, got {expanded_text}"
    );
    assert!(
        expanded_text.contains("REPLY_TAILWORD"),
        "expanded wrap must reveal latest tail, got {expanded_text}"
    );
}

#[test]
fn board_clip_offset_scrolls_expanded_details() {
    let mut row = header_test_row(1, RowState::Working, "clip-card");
    row.activity = Some("L2_ALPHA\nL3_BRAVO\nL4_CHARLIE\nL5_DELTA\nL6_ECHO\nL7_FOXTROT".into());
    let rows = vec![row];
    let id = DashboardRowId::TopLevel(AgentId(1));
    let mut state = DashboardState::new();
    state.board_view = true;
    state.filter = Filter::State(RowState::Working);
    state.expanded_board.insert(id);
    state.manual_scroll_active = true;
    state.viewport_offset = 0;
    let top = paint_board(&rows, &mut state, 40, 4);
    assert!(top.contains("clip-card"), "offset 0 shows title, got {top}");
    assert!(
        !top.contains("L7_FOXTROT"),
        "offset 0 must not show last detail, got {top}"
    );
    state.viewport_offset = 4;
    let bottom = paint_board(&rows, &mut state, 40, 4);
    assert!(
        !bottom.contains("clip-card"),
        "clipped title must leave viewport, got {bottom}"
    );
    assert!(
        bottom.contains("L7_FOXTROT"),
        "scroll must reveal last detail line, got {bottom}"
    );
}

#[test]
fn board_slots_cover_offscreen_and_stop_at_section_rows() {
    let rows = vec![
        header_test_row(1, RowState::NeedsInput, "ask-one"),
        header_test_row(2, RowState::Working, "work-one"),
        header_test_row(3, RowState::Idle, "idle-one"),
        header_test_row(4, RowState::Idle, "idle-two"),
        header_test_row(5, RowState::Idle, "idle-three"),
    ];
    let mut state = DashboardState::new();
    state.board_view = true;
    state
        .collapsed_sections
        .insert(SectionKey::State(RowState::Working));
    let ask = DashboardRowId::TopLevel(AgentId(1));
    let idle = DashboardRowId::TopLevel(AgentId(3));
    state.focus_row(ask.clone());
    let _ = paint_board(&rows, &mut state, 40, 6);
    let ids: Vec<_> = state.board_slots.iter().map(|(id, _)| id.clone()).collect();
    assert!(ids.contains(&ask));
    assert!(ids.contains(&idle));
    assert!(ids.contains(&DashboardRowId::TopLevel(AgentId(5))));
    assert!(!ids.contains(&DashboardRowId::TopLevel(AgentId(2))));
    let ask_row = state
        .board_slots
        .iter()
        .find(|(id, _)| *id == ask)
        .map(|(_, (_, r))| *r)
        .expect("ask slot");
    let idle_row = state
        .board_slots
        .iter()
        .find(|(id, _)| *id == idle)
        .map(|(_, (_, r))| *r)
        .expect("idle slot");
    assert!(
        idle_row > ask_row + 1,
        "collapsed Working header must occupy a grid row, slots={:?}",
        state.board_slots
    );
    state.focus_row(ask.clone());
    assert!(
        board_neighbour_vertical(&state, 1).is_none(),
        "Down from Awaiting must stop on intervening header"
    );
    let first_idle = state
        .board_slots
        .iter()
        .find(|(id, _)| {
            rows.iter()
                .any(|row| row.id == *id && row.state == RowState::Idle)
        })
        .unwrap()
        .0
        .clone();
    state.focus_row(first_idle);
    assert!(
        board_neighbour_vertical(&state, -1).is_none(),
        "Up from Idle must stop on intervening header"
    );
    let order = board_focusables(
        &rows,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    assert!(
        order
            .iter()
            .any(|f| matches!(f, Focusable::Section(SectionKey::State(RowState::Working)))),
        "board_focusables must expose collapsed Working header, got {order:?}"
    );
}

#[test]
fn board_selected_agent_reveals_its_new_collapsed_bucket() {
    let id = DashboardRowId::TopLevel(AgentId(1));
    let mut state = DashboardState::new();
    state.board_view = true;
    state.grouping = Grouping::Directory;
    state.focus_row(id.clone());
    state
        .collapsed_sections
        .insert(SectionKey::State(RowState::Idle));
    let rows = vec![header_test_row(1, RowState::Idle, "selected-agent")];
    state.reanchor_selection(&rows);
    assert_eq!(state.selected, Some(id));
    assert!(
        !state
            .collapsed_sections
            .contains(&SectionKey::State(RowState::Idle))
    );
    assert_eq!(state.grouping, Grouping::Directory);
}

#[test]
fn board_control_stays_reachable_in_both_modes_at_narrow_widths() {
    for width in [40, 44, 56, 60, 64] {
        for armed in [false, true] {
            for board in [false, true] {
                let mut state = DashboardState::new();
                state.cwd_has_git_ancestor = true;
                state.dispatch_worktree = armed;
                state.board_view = board;
                state.list_focused = true;
                render_actions(&mut state, width, true);
                assert!(
                    state.board_view_button_hit.rect.is_some(),
                    "width={width} armed={armed} board={board}"
                );
                state.actions_focus = Some(ActionsFocus::BoardView);
                press(&mut state, crossterm::event::KeyCode::Enter);
                assert_eq!(state.board_view, !board);
            }
        }
    }
}

#[test]
fn board_expansion_shows_multi_select_question() {
    let mut agent = ask_question_agent("Which services should deploy?");
    agent.question_view.as_mut().unwrap().questions[0].multi_select = Some(true);
    let mut agents = IndexMap::new();
    agents.insert(AgentId(0), agent);
    let mut state = DashboardState::new();
    state.board_view = true;
    let id = DashboardRowId::TopLevel(AgentId(0));
    state.focus_row(id.clone());
    state.expanded_board.insert(id);
    let text = paint_dashboard(&mut state, &mut agents, 100, 28);
    assert!(text.contains("Which services should deploy?"), "{text}");
}

#[test]
fn board_toggle_preserves_unsent_peek_reply() {
    let mut state = DashboardState::new();
    state.peek_reply.set_text("unsent reply");
    state.toggle_board_view();
    assert!(!state.board_view);
    assert_eq!(state.peek_reply.text(), "unsent reply");
}
