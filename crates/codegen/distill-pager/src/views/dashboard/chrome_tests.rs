use super::*;
use crate::views::dashboard::state::{ActionsFocus, DashboardRowId, DashboardState};
use crate::views::dashboard::test_support::{buf_to_text, header_test_row};

fn buf_cell(buf: &Buffer, x: u16, y: u16) -> &ratatui::buffer::Cell {
    buf.cell((x, y))
        .unwrap_or_else(|| panic!("missing cell ({x},{y})"))
}

/// Paint the header row on its own, the way `render_dashboard` does (per-frame hit-area reset included), with no promo CTA.
fn render_header_only(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    let registry = crate::actions::ActionRegistry::defaults();
    state.clear_chrome_hit_areas();
    render_header(buf, area, theme, rows, state, &registry, None);
}

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

/// The `+ New Agent` button paints green (`accent_success`) when focused so the cursor is obvious, and `text_secondary` otherwise.
#[test]
fn actions_new_agent_button_focused_is_green() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 120, 1);

    // Focused (default for a fresh dashboard with no row selected).
    let mut focused = DashboardState::new();
    focused.focus_new_agent_button();
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut focused, false);
    let rect = focused
        .new_agent_button_hit
        .rect
        .expect("button must render");
    assert_eq!(
        buf_cell(&buf, rect.x, rect.y).fg,
        theme.accent_success,
        "focused + New Agent must paint green (accent_success), got {:?}",
        buf_cell(&buf, rect.x, rect.y).fg,
    );

    // Unfocused (a row holds the cursor instead).
    let mut unfocused = DashboardState::new();
    unfocused.focus_row(super::super::state::DashboardRowId::TopLevel(
        crate::app::agent::AgentId(0),
    ));
    let mut buf2 = Buffer::empty(area);
    render_actions_only(&mut buf2, area, &theme, &mut unfocused, false);
    let rect2 = unfocused
        .new_agent_button_hit
        .rect
        .expect("button must render");
    assert_eq!(
        buf_cell(&buf2, rect2.x, rect2.y).fg,
        theme.text_secondary,
        "unfocused + New Agent must paint text_secondary, got {:?}",
        buf_cell(&buf2, rect2.x, rect2.y).fg,
    );
}

/// On hover the unfocused `+ New Agent` button brightens its text from `text_secondary` to `text_primary` so the mouse user sees it is clickable.
/// Only the foreground changes; the background stays `bg_base` (no fill).
/// The `hovered` flag, which the mouse-move handler flips via `HitArea::update_hover`, drives the styling.
#[test]
fn actions_new_agent_button_hover_brightens_text() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 120, 1);

    // Unfocused so the hover styling is isolated from the focus (green) styling
    let mut state = DashboardState::new();
    state.focus_row(super::super::state::DashboardRowId::TopLevel(
        crate::app::agent::AgentId(0),
    ));

    // First render populates the button's hit rect.
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut state, false);
    let rect = state.new_agent_button_hit.rect.expect("button must render");

    // Moving the mouse over the button flips hover on.
    assert!(
        state.new_agent_button_hit.update_hover(rect.x, rect.y),
        "moving the mouse over the button must flip hover on",
    );

    // Re-render with hover active: text_primary fg, background unchanged (still bg_base, no fill on hover)
    let mut buf2 = Buffer::empty(area);
    render_actions_only(&mut buf2, area, &theme, &mut state, false);
    let cell = buf_cell(&buf2, rect.x, rect.y);
    assert_eq!(
        cell.fg, theme.text_primary,
        "hovered + New Agent must use text_primary fg, got {:?}",
        cell.fg,
    );
    assert_eq!(
        cell.bg, theme.bg_base,
        "hovered + New Agent must keep bg_base (no hover fill), got {:?}",
        cell.bg,
    );

    // Moving the mouse off the button clears hover: back to the resting state (text_secondary fg, bg_base)
    assert!(
        state.new_agent_button_hit.update_hover(rect.right() + 1, 0),
        "moving the mouse off the button must flip hover off",
    );
    let mut buf3 = Buffer::empty(area);
    render_actions_only(&mut buf3, area, &theme, &mut state, false);
    let cell3 = buf_cell(&buf3, rect.x, rect.y);
    assert_eq!(
        cell3.bg, theme.bg_base,
        "non-hovered + New Agent must paint on bg_base, got {:?}",
        cell3.bg,
    );
    assert_eq!(
        cell3.fg, theme.text_secondary,
        "non-hovered + New Agent must use text_secondary fg, got {:?}",
        cell3.fg,
    );
}

/// Regression: the header renders from the dashboard's STAGED `cwd` (synced from `app.cwd` on a `/cd`), not the live process cwd.
/// A location change updates `state.cwd` immediately; the process cwd only moves later via `Effect::SetWorkingDir` (which can fail).
/// The header must follow `state.cwd` to show where dispatches will run.
#[test]
fn header_location_renders_from_staged_cwd() {
    let theme = Theme::current();
    let rows: Vec<DashboardRow> = Vec::new();
    // Wide area so the path isn't width-truncated.
    let area = Rect::new(0, 0, 200, 1);

    let mut state = DashboardState::new();
    // A distinct absolute path outside $HOME (rendered verbatim) that differs from the process cwd
    // No git cache entry, so no branch span
    state.cwd = std::path::PathBuf::from("/grok-staged-cwd-marker");

    let mut buf = Buffer::empty(area);
    render_header_only(&mut buf, area, &theme, &rows, &mut state);

    let top_row: String = (0..area.width)
        .map(|x| buf_cell(&buf, x, 0).symbol().to_string())
        .collect();
    assert!(
        top_row.contains("/grok-staged-cwd-marker"),
        "header must render the staged cwd, not the process cwd; got: {top_row:?}",
    );
}

/// The header paints the cwd in `text_secondary` and follows it with the `[Choose Ctrl+l]` picker hint.
/// The hint's `Choose` label takes the dim row-secondary colour; its key is a shade fainter still, so the path reads first.
#[test]
fn header_paints_cwd_then_choose_hint_with_design_colours() {
    // Fixed RGB palette: the colour tiers below collapse to `Reset` on the terminal theme
    let theme = Theme::groknight();
    let area = Rect::new(0, 0, 200, 1);
    let mut state = DashboardState::new();
    state.cwd = std::path::PathBuf::from("/grok-choose-hint-marker");
    let mut buf = Buffer::empty(area);
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    let text = buf_to_text(&buf);
    let registry = crate::actions::ActionRegistry::defaults();
    let key = registry
        .key_for(crate::actions::ActionId::DashboardOpenLocationPicker)
        .expect("location picker has a dashboard binding")
        .display();
    let expected = format!("/grok-choose-hint-marker [Choose {key}]");
    let start = text
        .find(&expected)
        .unwrap_or_else(|| panic!("header must read `{expected}`, got: {text:?}"));

    let cell = |offset: usize| buf_cell(&buf, (start + offset) as u16, 0);
    assert_eq!(
        cell(0).fg,
        theme.text_secondary,
        "cwd paints text_secondary"
    );
    let choose_at = "/grok-choose-hint-marker [".len();
    assert_eq!(
        cell(choose_at).fg,
        theme.gray_dim,
        "`Choose` takes the dim row-secondary colour"
    );
    let key_at = "/grok-choose-hint-marker [Choose ".len();
    // `key_hint_style` is `Theme::faint` on `bg_base`; a palette edit moves this with it.
    let key_fg = cell(key_at).fg;
    assert_eq!(
        key_fg,
        key_hint_style(&theme).fg.expect("key hint sets a fg")
    );
    let luma = |c: Color| match c {
        Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
        other => panic!("expected an RGB colour, got {other:?}"),
    };
    assert!(
        luma(theme.bg_base) < luma(key_fg) && luma(key_fg) < luma(theme.gray_dim),
        "the key must be fainter than `Choose` ({:?}) but lighter than the background ({:?}), got {key_fg:?}",
        theme.gray_dim,
        theme.bg_base,
    );
    assert_eq!(
        cell(expected.len() - 1).fg,
        theme.gray_dim,
        "the closing bracket matches `Choose`"
    );

    let hit = state.location_hit.rect.expect("location hit rect");
    assert!(
        (hit.x as usize + hit.width as usize) >= start + expected.len(),
        "the location hit rect must cover the `[Choose …]` hint, got {hit:?}",
    );
}

/// Terminal theme: `Theme::faint` is DIM with no hard colour.
#[test]
fn header_key_hint_falls_back_to_dim_on_terminal_theme() {
    let theme = Theme::terminal();
    let area = Rect::new(0, 0, 200, 1);
    let mut state = DashboardState::new();
    state.cwd = std::path::PathBuf::from("/grok-terminal-theme-marker");
    let mut buf = Buffer::empty(area);
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    let text = buf_to_text(&buf);
    let key_at = text.find("[Choose ").expect("hint painted") + "[Choose ".len();
    let key = buf_cell(&buf, key_at as u16, 0);
    assert_eq!(
        key.fg,
        Color::Reset,
        "no hard colour on the terminal palette"
    );
    assert!(
        key.modifier.contains(Modifier::DIM),
        "the key must use the DIM attribute instead, got {:?}",
        key.modifier
    );
}

/// When the header is too narrow for the cwd plus the hint, the hint is dropped before the path is cut.
#[test]
fn header_drops_choose_hint_before_truncating_path() {
    let theme = Theme::current();
    let mut state = DashboardState::new();
    state.cwd = std::path::PathBuf::from("/grok-narrow-header-marker");
    // Exactly the path width: the hint can't fit, the path must
    let area = Rect::new(0, 0, "/grok-narrow-header-marker".len() as u16, 1);
    let mut buf = Buffer::empty(area);
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    let text = buf_to_text(&buf);
    assert!(
        text.contains("/grok-narrow-header-marker") && !text.contains("Choose"),
        "path must survive intact and the hint must go, got: {text:?}",
    );
}

/// The `+ New Agent` button reads `+ New Agent in Worktree` (and the toggle `Disable Worktree`) when worktree mode is armed in a git repo.
/// It reads `+ New Agent` / `Worktree` otherwise (off, or armed outside a git repo, where the mode can't take effect).
#[test]
fn actions_labels_reflect_worktree_mode() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 120, 1);

    // Off: plain new-agent button
    let mut off = DashboardState::new();
    off.cwd_has_git_ancestor = true;
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut off, false);
    let text = buf_to_text(&buf);
    assert!(
        text.contains("+ New Agent ") && !text.contains("in Worktree") && !text.contains("Disable"),
        "worktree mode off → + New Agent / Worktree, got: {text:?}",
    );

    // Armed in a git repo: worktree labels
    let mut armed = DashboardState::new();
    armed.cwd_has_git_ancestor = true;
    armed.dispatch_worktree = true;
    let mut buf2 = Buffer::empty(area);
    render_actions_only(&mut buf2, area, &theme, &mut armed, false);
    let text2 = buf_to_text(&buf2);
    assert!(
        text2.contains("+ New Agent in Worktree") && text2.contains("Disable Worktree"),
        "worktree mode armed in a repo → worktree labels, got: {text2:?}",
    );

    // Armed but NOT a git repo: still the plain labels (mode is inert)
    let mut armed_no_git = DashboardState::new();
    armed_no_git.cwd_has_git_ancestor = false;
    armed_no_git.dispatch_worktree = true;
    let mut buf3 = Buffer::empty(area);
    render_actions_only(&mut buf3, area, &theme, &mut armed_no_git, false);
    let text3 = buf_to_text(&buf3);
    assert!(
        text3.contains("+ New Agent ")
            && !text3.contains("in Worktree")
            && !text3.contains("Disable"),
        "armed outside a repo → plain labels, got: {text3:?}",
    );
}

/// The `Worktree Ctrl+w` hint is right-aligned, shows the registry's chord, and is a click target for the same toggle.
/// Like the other two buttons, a click also moves the cursor onto it, so the focus cue and the footer follow the click.
#[test]
fn actions_worktree_hint_shows_chord_and_toggles_on_click() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 120, 1);
    let mut state = DashboardState::new();
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut state, false);
    let text = buf_to_text(&buf);
    let registry = crate::actions::ActionRegistry::defaults();
    let key = registry
        .key_for(crate::actions::ActionId::DashboardToggleWorktree)
        .expect("worktree toggle has a dashboard binding")
        .display();
    let expected = format!("Worktree {key}");
    assert!(text.contains(&expected), "got: {text:?}");
    let hit = state
        .worktree_toggle_hit
        .rect
        .expect("worktree hint hit rect");
    assert_eq!(
        hit.x + hit.width,
        area.x + area.width,
        "the hint is flush with the row's right edge"
    );

    let click = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    assert!(matches!(
        state.handle_input(&click, &registry),
        crate::app::app_view::InputOutcome::Action(
            crate::app::actions::Action::DashboardToggleWorktree
        )
    ));
    assert_eq!(
        state.actions_focus,
        Some(ActionsFocus::Worktree),
        "the click moves the cursor onto the toggle"
    );
}

/// A squeezed actions row drops the right-hand items rather than painting over `+ New Agent`, which always survives.
#[test]
fn actions_row_drops_right_items_when_narrow() {
    let theme = Theme::current();
    let mut state = DashboardState::new();
    // Room for `+ New Agent` plus a couple of cells, not for any right-hand hint
    let area = Rect::new(0, 0, 14, 1);
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut state, true);
    let text = buf_to_text(&buf);
    assert!(text.contains("+ New Agent"), "got: {text:?}");
    assert!(state.new_agent_button_hit.rect.is_some());
    assert!(state.worktree_toggle_hit.rect.is_none());
    assert!(state.open_session_button_hit.rect.is_none());
}

/// At an in-between width the row keeps `Worktree` (laid out first, flush right) and drops `Open Previous` together with its divider,
/// so no `│` is left dangling. A cursor parked on the dropped button falls back to `+ New Agent` in the same frame, painted focused.
#[test]
fn actions_row_keeps_worktree_and_drops_open_previous_with_divider() {
    let theme = Theme::groknight();
    let mut state = DashboardState::new();
    state.focus_open_session_button();
    // `+ New Agent` (11) + gap (2) + `Worktree Ctrl+w` (15) = 28 fits; adding ` │ Open Previous /resume` (24) does not
    let area = Rect::new(0, 0, 40, 1);
    let mut buf = Buffer::empty(area);
    render_actions_only(&mut buf, area, &theme, &mut state, true);
    let text = buf_to_text(&buf);
    assert!(text.contains("Worktree"), "got: {text:?}");
    assert!(
        !text.contains("Open Previous") && !text.contains('│'),
        "Open Previous and its divider must both go, got: {text:?}",
    );
    let worktree = state
        .worktree_toggle_hit
        .rect
        .expect("worktree hint painted");
    assert_eq!(worktree.x + worktree.width, area.x + area.width);
    assert!(state.open_session_button_hit.rect.is_none());
    assert!(
        state.new_agent_button_focused() && !state.open_session_button_focused(),
        "focus must fall back to + New Agent when Open Previous is dropped",
    );
    let new_agent = state.new_agent_button_hit.rect.expect("button painted");
    assert_eq!(
        buf_cell(&buf, new_agent.x, new_agent.y).fg,
        theme.accent_success,
        "the fallback must show in the same frame: + New Agent paints focused",
    );
}

/// Right-hand items follow strict right-to-left priority at every width.
/// `Open Previous` is never painted without the worktree toggle; the toggle stays flush right.
/// A wider armed label must not let a narrower item take the freed space.
#[test]
fn actions_row_right_items_follow_strict_priority_at_every_width() {
    let theme = Theme::current();
    for armed in [false, true] {
        for width in 1u16..=90 {
            let mut state = DashboardState::new();
            state.cwd_has_git_ancestor = true;
            state.dispatch_worktree = armed;
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            render_actions_only(&mut buf, area, &theme, &mut state, true);
            let ctx = format!("armed={armed} width={width}");
            assert!(
                state.new_agent_button_hit.rect.is_some(),
                "{ctx}: + New Agent"
            );
            let worktree = state.worktree_toggle_hit.rect;
            let open = state.open_session_button_hit.rect;
            if let Some(w) = worktree {
                assert_eq!(
                    w.x + w.width,
                    area.x + area.width,
                    "{ctx}: toggle flush right"
                );
            }
            assert!(
                open.is_none() || worktree.is_some(),
                "{ctx}: Open Previous must not outlive the worktree toggle",
            );
            let text = buf_to_text(&buf);
            assert_eq!(
                text.contains('│'),
                open.is_some(),
                "{ctx}: the divider appears exactly when both right-hand items do",
            );
        }
    }

    // The exact armed bands from the width table: 46-47 used to show `Open Previous` with the toggle gone
    let armed_at = |width: u16| {
        let mut state = DashboardState::new();
        state.cwd_has_git_ancestor = true;
        state.dispatch_worktree = true;
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        render_actions_only(&mut buf, area, &theme, &mut state, true);
        (
            state.worktree_toggle_hit.rect.is_some(),
            state.open_session_button_hit.rect.is_some(),
        )
    };
    assert_eq!(armed_at(45), (false, false));
    assert_eq!(armed_at(46), (false, false));
    assert_eq!(armed_at(47), (false, false));
    assert_eq!(armed_at(48), (true, false));
    assert_eq!(armed_at(71), (true, false));
    assert_eq!(armed_at(72), (true, true));
}

/// The header pairs the location label (cwd and git info) on the left with right-aligned, labelled state-count chips.
/// The glyph carries the state colour and the `{count} {label}` text keeps each chip readable without colour.
#[test]
fn render_header_paints_label_and_state_chips() {
    let theme = Theme::groknight();
    // Wide rect so the location label never truncates regardless of how deep the test machine's checkout path is
    let area = Rect::new(0, 0, 400, 1);
    let mut buf = Buffer::empty(area);
    let mut state = DashboardState::new();
    let rows = vec![
        header_test_row(1, RowState::NeedsInput, "a"),
        header_test_row(2, RowState::NeedsInput, "b"),
        header_test_row(3, RowState::Working, "c"),
        header_test_row(4, RowState::Idle, "d"),
    ];
    render_header_only(&mut buf, area, &theme, &rows, &mut state);
    let content = buf_to_text(&buf);
    let basename = cwd_basename();
    assert!(
        content.contains(&basename),
        "header must show the current location (`{basename}`), got: {content:?}",
    );
    let row: String = (0..area.width)
        .map(|x| buf_cell(&buf, x, 0).symbol())
        .collect();
    let chips = format!(
        "{} 2 awaiting │ {} 1 working │ {} 1 idle",
        crate::glyphs::diamond_filled(),
        crate::glyphs::dot_spinner_frames()
            .first()
            .copied()
            .unwrap_or(""),
        crate::glyphs::diamond_hollow(),
    );
    assert!(
        row.trim_end().ends_with(&chips),
        "chips `{chips}` must be right-aligned in the header, got: {row:?}",
    );
    // The buttons live on the actions row now, not in the header
    for word in ["Agents", "New Agent"] {
        assert!(
            !content.contains(word),
            "header must not paint `{word}`, got: {content:?}",
        );
    }
    let chips_x = area.width - UnicodeWidthStr::width(chips.as_str()) as u16;
    assert_eq!(
        buf_cell(&buf, chips_x, 0).fg,
        theme.warning,
        "awaiting glyph"
    );
    assert_eq!(
        buf_cell(&buf, chips_x + 2, 0).fg,
        theme.gray,
        "awaiting count"
    );
}

/// Every state in the chip table renders when present, in priority order, with its own glyph colour and the shared gray count label.
#[test]
fn render_header_paints_every_state_chip_in_its_colour() {
    let theme = Theme::groknight();
    let area = Rect::new(0, 0, 400, 1);
    let mut buf = Buffer::empty(area);
    let mut state = DashboardState::new();
    let rows = vec![
        header_test_row(1, RowState::Failed, "e"),
        header_test_row(2, RowState::Completed, "d"),
        header_test_row(3, RowState::Idle, "c"),
        header_test_row(4, RowState::Working, "b"),
        header_test_row(5, RowState::NeedsInput, "a"),
    ];
    render_header_only(&mut buf, area, &theme, &rows, &mut state);
    let row: String = (0..area.width)
        .map(|x| buf_cell(&buf, x, 0).symbol())
        .collect();
    let filled = crate::glyphs::diamond_filled();
    let expected = [
        ("1 awaiting", filled, theme.warning),
        (
            "1 working",
            crate::glyphs::dot_spinner_frames()
                .first()
                .copied()
                .unwrap_or(""),
            theme.accent_running,
        ),
        ("1 idle", crate::glyphs::diamond_hollow(), theme.gray_dim),
        ("1 done", filled, theme.accent_success),
        ("1 failed", filled, theme.accent_error),
    ];
    let mut search_from = 0;
    for (label, glyph, color) in expected {
        let chip = format!("{glyph} {label}");
        let at = row
            .get(search_from..)
            .and_then(|rest| rest.find(&chip))
            .map(|i| i + search_from)
            .unwrap_or_else(|| panic!("`{chip}` must follow the previous chip, got: {row:?}"));
        let col = UnicodeWidthStr::width(row.get(..at).unwrap_or("")) as u16;
        assert_eq!(buf_cell(&buf, col, 0).fg, color, "`{label}` glyph colour");
        assert_eq!(
            buf_cell(&buf, col + 2, 0).fg,
            theme.gray,
            "`{label}` count colour"
        );
        search_from = at + chip.len();
    }
}

/// The header records a click target for the location label so the mouse handler can open the location picker.
#[test]
fn render_header_sets_location_click_target() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 120, 1);
    let mut buf = Buffer::empty(area);
    let mut state = DashboardState::new();
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    assert!(
        state.location_hit.rect.is_some(),
        "render_header must record a click target for the location label",
    );
}

/// On hover the location text (branch and path) is underlined; the `[Choose …]` hint that follows it is not.
#[test]
fn render_header_hover_underlines_only_location_text() {
    let theme = Theme::current();
    let area = Rect::new(0, 0, 400, 1);
    let mut buf = Buffer::empty(area);
    let mut state = DashboardState::new();
    state.cwd = std::path::PathBuf::from("/grok-hover-marker");
    state.location_hit.hovered = true;
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    let text = buf_to_text(&buf);
    let underlined = |x: usize| {
        buf.cell((x as u16, 0))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED)
    };

    let path_start = text.find("/grok-hover-marker").expect("path painted");
    let path_end = path_start + "/grok-hover-marker".len();
    assert!(
        underlined(path_start) && underlined(path_end - 1),
        "the path is underlined"
    );
    assert!(
        !underlined(path_end),
        "the space before the hint is not underlined"
    );
    let hint_start = text.find("[Choose").expect("hint painted");
    assert!(
        !underlined(hint_start) && !underlined(hint_start + 1),
        "the hint is not underlined"
    );
}

/// Hover underlines the branch and the path; the whitespace separator between them stays bare.
#[test]
fn underline_location_on_hover_skips_whitespace_spans() {
    let plain = Style::default();
    let spans = vec![
        Span::styled("main".to_string(), plain),
        Span::styled(" ".to_string(), plain), // branch↔path separator
        Span::styled("/home/me/repo".to_string(), plain),
    ];
    let out = underline_location_on_hover(spans);
    let underlined: Vec<(&str, bool)> = out
        .iter()
        .map(|s| {
            (
                s.content.as_ref(),
                s.style.add_modifier.contains(Modifier::UNDERLINED),
            )
        })
        .collect();
    assert_eq!(
        underlined,
        vec![("main", true), (" ", false), ("/home/me/repo", true)]
    );
}

/// Zero-count states are suppressed.
#[test]
fn render_header_suppresses_zero_count_chips() {
    let theme = Theme::current();
    let mut buf = Buffer::empty(Rect::new(0, 0, 120, 1));
    let mut state = DashboardState::new();
    // Only one Idle row: no awaiting/working/done/failed chips
    let rows = vec![header_test_row(1, RowState::Idle, "x")];
    render_header_only(&mut buf, Rect::new(0, 0, 120, 1), &theme, &rows, &mut state);
    let content = buf_to_text(&buf);
    assert!(
        content.contains("1 idle"),
        "expected `1 idle`, got: {content:?}"
    );
    for absent in ["0 awaiting", "0 working", "0 done", "0 failed", "0 blocked"] {
        assert!(
            !content.contains(absent),
            "zero-count chip `{absent}` must be suppressed, got: {content:?}",
        );
    }
}

/// Inactive (roster-only) rows get no header chip; only the section header carries their count.
#[test]
fn render_header_has_no_inactive_chip() {
    let theme = Theme::current();
    let mut buf = Buffer::empty(Rect::new(0, 0, 120, 1));
    let mut state = DashboardState::new();
    let rows = vec![
        header_test_row(1, RowState::Inactive, "a"),
        header_test_row(2, RowState::Idle, "b"),
    ];
    render_header_only(&mut buf, Rect::new(0, 0, 120, 1), &theme, &rows, &mut state);
    let content = buf_to_text(&buf);
    assert!(
        content.contains("1 idle"),
        "idle chip must still render, got: {content:?}"
    );
    assert!(
        !content.contains("inactive"),
        "no chip for Inactive rows, got: {content:?}"
    );
}

/// The left title is the current location (cwd display), shown with and without agent rows, mirroring the session views' top-bar location line.
#[test]
fn render_header_shows_location_label() {
    let theme = Theme::current();
    // Wide rect so the location label never truncates regardless of how deep the test machine's checkout path is
    let area = Rect::new(0, 0, 400, 1);
    let mut state = DashboardState::new();
    let basename = cwd_basename();

    // 0 agents: the location still shows
    let mut buf = Buffer::empty(area);
    render_header_only(&mut buf, area, &theme, &[], &mut state);
    let c = buf_to_text(&buf);
    assert!(
        c.contains(&basename),
        "0-agent header must show the location (`{basename}`), got: {c:?}"
    );

    // 1 agent.
    let mut buf = Buffer::empty(area);
    let rows = vec![header_test_row(1, RowState::Idle, "x")];
    render_header_only(&mut buf, area, &theme, &rows, &mut state);
    let c = buf_to_text(&buf);
    assert!(
        c.contains(&basename),
        "header must show the location (`{basename}`), got: {c:?}"
    );
}

/// On a narrow header the location label truncates against a 3-cell blank gutter before the leftmost chip and never paints over the chips.
#[test]
fn render_header_location_label_never_overlaps_chips() {
    let theme = Theme::current();
    // Narrow enough that a long path overflows the label budget once three chips are reserved.
    // Last two components stay full after always-on shortening, so they must be long enough
    // to still overflow — middle-component letters alone would fit the 60-col row.
    let area = Rect::new(0, 0, 60, 1);
    let mut buf = Buffer::empty(area);
    let mut state = DashboardState::new();
    state.cwd = std::path::PathBuf::from(
        "/grok-overlap/a/very/long/checkout/path/that/overflows-enough-after-shortening",
    );
    let rows = vec![
        header_test_row(1, RowState::NeedsInput, "a"),
        header_test_row(2, RowState::Working, "b"),
        header_test_row(3, RowState::Idle, "c"),
    ];
    render_header_only(&mut buf, area, &theme, &rows, &mut state);
    let content = buf_to_text(&buf);
    // Chips must survive the (long) location label, which is cut with an ellipsis.
    for chunk in ["1 awaiting", "1 working", "1 idle", "…"] {
        assert!(
            content.contains(chunk),
            "`{chunk}` must not be overpainted by the location label, got: {content:?}",
        );
    }
    assert!(
        !content.contains("Choose"),
        "the hint goes before the path is cut"
    );
    // The ellipsis ends the label; exactly three blank cells separate it from the first chip's glyph
    let row: String = (0..area.width)
        .map(|x| buf_cell(&buf, x, 0).symbol())
        .collect();
    let ellipsis_at = row.find('…').expect("truncated label ends in an ellipsis");
    let Some(after) = row.get(ellipsis_at + '…'.len_utf8()..) else {
        panic!("ellipsis not on a char boundary: {row:?}");
    };
    assert!(
        after.starts_with(&format!("   {}", crate::glyphs::diamond_filled())),
        "expected a 3-cell gutter then the awaiting glyph after the ellipsis, got: {after:?}",
    );
}

/// Subagents inherit their parent's state and must NOT inflate the header chip tallies.
/// The header counts top-level rows only.
#[test]
fn render_header_counts_top_level_rows_only() {
    let theme = Theme::current();
    let mut buf = Buffer::empty(Rect::new(0, 0, 160, 1));
    let mut state = DashboardState::new();
    let parent = DashboardRow {
        indent: 0,
        ..header_test_row(1, RowState::Working, "parent")
    };
    let sub_completed = DashboardRow {
        id: DashboardRowId::Subagent {
            parent: crate::app::agent::AgentId(1),
            child_session_id: "c1".to_string(),
        },
        indent: 1,
        ..header_test_row(11, RowState::Completed, "child")
    };
    let rows = vec![parent, sub_completed];
    render_header_only(&mut buf, Rect::new(0, 0, 160, 1), &theme, &rows, &mut state);
    let content = buf_to_text(&buf);
    // Only the top-level parent counts: its Working chip shows.
    assert!(
        content.contains("1 working"),
        "expected `1 working` chip for the top-level parent, got: {content:?}"
    );
    // Subagent's Completed must NOT show up as `1 done`.
    assert!(
        !content.contains("1 done"),
        "header must not count subagent state, got: {content:?}",
    );
}

/// Basename of the test process's cwd, the one deterministic fragment of the header's location label.
/// The full label depends on global git caches (`git_info::*`) that parallel tests may touch.
/// Every fallback path still renders a cwd display ending in the current directory's basename.
fn cwd_basename() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .expect("test process must have a cwd with a basename")
}
