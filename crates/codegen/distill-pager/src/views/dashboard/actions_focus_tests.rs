//! Keyboard and click interaction on the actions row: the `←`/`→` walk, Enter-as-click, Esc, and the focus fallback.
//! Paint and layout coverage for the row stays in `chrome_tests.rs`; these tests only render it to learn which items exist.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::theme::Theme;
use crate::views::dashboard::actions_focus::ActionsFocus;
use crate::views::dashboard::chrome::render_actions_row;
use crate::views::dashboard::state::DashboardState;

/// Paint the actions row on its own, the way `render_dashboard` does (per-frame hit-area reset included).
fn render_actions_only(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &mut DashboardState,
    workspace_dashboard_enabled: bool,
) {
    let registry = crate::actions::ActionRegistry::defaults();
    state.clear_chrome_hit_areas();
    render_actions_row(
        buf,
        area,
        theme,
        state,
        &registry,
        workspace_dashboard_enabled,
    );
}

/// Feed one unmodified key to the dashboard.
fn press(
    state: &mut DashboardState,
    code: crossterm::event::KeyCode,
) -> crate::app::app_view::InputOutcome {
    let registry = crate::actions::ActionRegistry::defaults();
    let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
    state.handle_input(&event, &registry)
}

/// A state with the list focused and the actions row painted at `width` cells, so navigation knows which items exist.
fn painted_actions_state(width: u16, workspace_dashboard_enabled: bool) -> DashboardState {
    let mut state = DashboardState::new();
    state.list_focused = true;
    state.cwd_has_git_ancestor = true;
    let area = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(area);
    render_actions_only(
        &mut buf,
        area,
        &Theme::groknight(),
        &mut state,
        workspace_dashboard_enabled,
    );
    state
}

/// ←/→ walk the actions row in visual order and stop at both ends: `+ New Agent` → `Open Previous` → `Worktree`, no wrap.
#[test]
fn arrows_walk_actions_row_in_visual_order_without_wrapping() {
    use crossterm::event::KeyCode::{Left, Right};
    let mut state = painted_actions_state(120, true);
    assert_eq!(state.actions_focus, Some(ActionsFocus::NewAgent));

    assert!(matches!(
        press(&mut state, Left),
        crate::app::app_view::InputOutcome::Unchanged
    ));
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::NewAgent),
        "left end stops"
    );

    assert!(matches!(
        press(&mut state, Right),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::OpenPrevious));
    assert!(matches!(
        press(&mut state, Right),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::Worktree));
    assert!(matches!(
        press(&mut state, Right),
        crate::app::app_view::InputOutcome::Unchanged
    ));
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::Worktree),
        "right end stops"
    );

    assert!(matches!(
        press(&mut state, Left),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::OpenPrevious));
    assert!(matches!(
        press(&mut state, Left),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::NewAgent));
}

/// Items the last frame did not paint are skipped: without the workspace dashboard there is no `Open Previous`, so Right lands on
/// `Worktree` directly; on a row too narrow for any right-hand item, Right does nothing.
#[test]
fn arrows_skip_actions_items_that_were_not_painted() {
    use crossterm::event::KeyCode::Right;
    let mut v1 = painted_actions_state(120, false);
    assert!(v1.open_session_button_hit.rect.is_none());
    press(&mut v1, Right);
    assert_eq!(v1.actions_focus, Some(ActionsFocus::Worktree));

    let mut narrow = painted_actions_state(20, true);
    assert!(narrow.worktree_toggle_hit.rect.is_none());
    assert!(matches!(
        press(&mut narrow, Right),
        crate::app::app_view::InputOutcome::Unchanged
    ));
    assert_eq!(narrow.actions_focus, Some(ActionsFocus::NewAgent));
}

#[test]
fn arrows_walk_actions_row_from_empty_input_but_not_over_a_draft() {
    use crossterm::event::KeyCode::{Char, Left, Right};
    let mut state = painted_actions_state(120, true);
    state.list_focused = false;

    assert!(matches!(
        press(&mut state, Right),
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::OpenPrevious));
    press(&mut state, Left);
    assert_eq!(state.actions_focus, Some(ActionsFocus::NewAgent));

    crate::appearance::cache::set_vim_mode(true);
    press(&mut state, Char('l'));
    crate::appearance::cache::set_vim_mode(false);
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::NewAgent),
        "a letter types into the empty input instead of walking the row"
    );
    assert_eq!(state.dispatch.text(), "l");

    state.dispatch.set_text("a typed draft");
    press(&mut state, Right);
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::NewAgent),
        "with a draft the arrows move the caret, not the cursor"
    );

    state.dispatch.set_text("");
    state.enter_search_mode();
    press(&mut state, Right);
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::NewAgent),
        "in search mode the buffer is a query even when empty"
    );
}

/// Enter on a right-hand item is a click on it, draft or no draft; only `+ New Agent` sends a typed draft.
#[test]
fn enter_acts_like_a_click_on_the_focused_actions_item() {
    use crate::app::actions::Action;
    use crate::app::app_view::InputOutcome;
    use crossterm::event::KeyCode::Enter;
    let mut state = painted_actions_state(120, true);
    state.dispatch.set_text("a typed draft");

    state.focus_action(ActionsFocus::OpenPrevious);
    assert!(matches!(
        press(&mut state, Enter),
        InputOutcome::Action(Action::ShowSessionPicker)
    ));
    state.focus_action(ActionsFocus::Worktree);
    assert!(matches!(
        press(&mut state, Enter),
        InputOutcome::Action(Action::DashboardToggleWorktree)
    ));
    state.focus_action(ActionsFocus::NewAgent);
    assert!(
        matches!(
            press(&mut state, Enter),
            InputOutcome::Action(Action::DashboardDispatch { attach: false, .. })
        ),
        "+ New Agent with a draft dispatches it and stays on the dashboard",
    );
    // The list-pane footer offers `Ctrl+S:send+open` for this case, so the chord must reach the dispatcher from the list pane too
    let ctrl_s = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    let registry = crate::actions::ActionRegistry::defaults();
    assert!(
        matches!(
            state.handle_input(&ctrl_s, &registry),
            InputOutcome::Action(Action::DashboardDispatch { attach: true, .. })
        ),
        "Ctrl+S with a draft on + New Agent, list focused, sends and opens",
    );
}

/// Esc from a right-hand item steps back to `+ New Agent`; only from there does a second Esc leave the dashboard.
#[test]
fn esc_from_right_hand_item_returns_to_new_agent_before_exiting() {
    use crate::app::actions::Action;
    use crate::app::app_view::InputOutcome;
    use crossterm::event::KeyCode::Esc;
    let mut state = painted_actions_state(120, true);
    state.focus_action(ActionsFocus::Worktree);
    assert!(matches!(press(&mut state, Esc), InputOutcome::Changed));
    assert_eq!(state.actions_focus, Some(ActionsFocus::NewAgent));
    assert!(matches!(
        press(&mut state, Esc),
        InputOutcome::Action(Action::ExitDashboard)
    ));
}

/// A focused `Worktree` toggle paints in the focus colour, and a focused item the row drops on resize falls back to `+ New Agent`
/// in the same frame.
#[test]
fn focused_worktree_toggle_is_green_and_falls_back_when_dropped() {
    let theme = Theme::groknight();
    let mut state = painted_actions_state(120, true);
    state.focus_action(ActionsFocus::Worktree);
    let area = Rect::new(0, 0, 120, 1);
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut state, true);
    let worktree = state.worktree_toggle_hit.rect.expect("painted");
    assert_eq!(
        buf.cell((worktree.x, worktree.y)).map(|c| c.fg),
        Some(theme.accent_success)
    );

    // Shrink below the width the toggle needs: focus must land on `+ New Agent`, painted focused in this frame
    let narrow = Rect::new(0, 0, 20, 1);
    let mut buf2 = Buffer::empty(narrow);
    render_actions_only(&mut buf2, narrow, &theme, &mut state, true);
    assert!(state.worktree_toggle_hit.rect.is_none());
    assert_eq!(state.actions_focus, Some(ActionsFocus::NewAgent));
    assert_eq!(buf2.cell((0, 0)).map(|c| c.fg), Some(theme.accent_success));
}

/// Regression: a click on a right-hand item while the dispatch input has focus must not leave Enter dead.
/// The click moves the cursor onto the item and an empty Enter — from the input pane, where a fresh dashboard starts — acts on
/// that item, exactly as the footer promises; the input keeps focus so a draft in progress isn't interrupted.
#[test]
fn click_then_empty_enter_from_input_pane_acts_on_the_clicked_item() {
    use crate::app::actions::Action;
    use crate::app::app_view::InputOutcome;
    let registry = crate::actions::ActionRegistry::defaults();
    let mut state = painted_actions_state(120, true);
    state.list_focused = false;
    let click_at = |rect: Rect| {
        crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    };

    let worktree = state.worktree_toggle_hit.rect.expect("painted");
    assert!(matches!(
        state.handle_input(&click_at(worktree), &registry),
        InputOutcome::Action(Action::DashboardToggleWorktree)
    ));
    assert_eq!(state.actions_focus, Some(ActionsFocus::Worktree));
    assert!(
        !state.list_focused,
        "a toggle click leaves the input pane focused"
    );
    assert_eq!(state.focused_action_label(), Some("enable worktree"));
    assert!(
        matches!(
            press(&mut state, crossterm::event::KeyCode::Enter),
            InputOutcome::Action(Action::DashboardToggleWorktree)
        ),
        "empty Enter from the input pane acts on the clicked item",
    );
    // Ctrl+S ("send and open") shares the empty-draft path, so it must act on the item too rather than go dead
    let ctrl_s = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(
        matches!(
            state.handle_input(&ctrl_s, &registry),
            InputOutcome::Action(Action::DashboardToggleWorktree)
        ),
        "empty Ctrl+S from the input pane acts on the clicked item",
    );

    // Back on `+ New Agent`, the same empty Enter creates, as before
    state.focus_action(ActionsFocus::NewAgent);
    assert!(matches!(
        press(&mut state, crossterm::event::KeyCode::Enter),
        InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail)
    ));
}
