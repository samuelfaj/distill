//! Dashboard chrome above the row list: the header row (location label, `[Choose …]` hint, state chips, promo CTA)
//! and the primary actions row (`+ New Agent`, `Open Previous /resume`, `Worktree Ctrl+w`).
//! Both paint into rects from [`crate::views::dashboard::layout::DashboardLayout`] and register their click targets on [`DashboardState`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::render::line_utils::truncate_line;
use crate::theme::Theme;
use crate::views::agent_status::AgentStatusBar;
use crate::views::dashboard::animation::{Animation, SPINNER_DIVISOR};
use crate::views::dashboard::row::DashboardRow;
use crate::views::dashboard::state::{ActionsFocus, DashboardState, RowState};
use crate::views::location::{LocationParts, location_parts, worktree_badge};

/// Promo upgrade CTA for the dashboard header, resolved through the shared slot gate by the producer (`app_view`).
#[derive(Clone, Copy)]
pub struct HeaderUpgradeCta<'a> {
    /// The `[label]` button text.
    pub label: &'a str,
    /// True when the promo is non-dismissible, so the `Ctrl+O` override applies.
    pub pinned: bool,
    /// The promo's trimmed `cta.caption` value; painted only when `pinned` is set.
    pub caption: Option<&'a str>,
}

/// Paint the dashboard header into `area` and write `location_hit` / `upgrade_cta_hit`.
pub(super) fn render_header(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
    registry: &crate::actions::ActionRegistry,
    upgrade_cta: Option<HeaderUpgradeCta<'_>>,
) {
    if area.area() == 0 {
        return;
    }
    let bg = Style::default().bg(theme.bg_base);
    let dim = theme.dim().bg(theme.bg_base);
    buf.set_style(area, bg);

    // Subagents inherit the parent's group; skip `indent > 0` so they are not counted twice.
    let mut awaiting = 0usize;
    let mut working = 0usize;
    let mut idle = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;
    for r in rows.iter().filter(|r| r.indent == 0) {
        match r.state {
            RowState::NeedsInput => awaiting += 1,
            RowState::Working => working += 1,
            RowState::Idle => idle += 1,
            // Inactive count lives on the section header, not a chip.
            RowState::Inactive => {}
            RowState::Completed => done += 1,
            RowState::Failed => failed += 1,
        }
    }

    // Chip order matches `RowState::group_priority` (awaiting leftmost).
    let frames = crate::glyphs::dot_spinner_frames();
    let spinner = frames
        .get((state.spinner_tick / SPINNER_DIVISOR) as usize % frames.len())
        .copied()
        .unwrap_or("");
    let chip_specs = [
        (
            "awaiting",
            crate::glyphs::diamond_filled(),
            theme.warning,
            awaiting,
        ),
        ("working", spinner, theme.accent_running, working),
        (
            "idle",
            crate::glyphs::diamond_hollow(),
            theme.gray_dim,
            idle,
        ),
        (
            "done",
            crate::glyphs::diamond_filled(),
            theme.accent_success,
            done,
        ),
        (
            "failed",
            crate::glyphs::diamond_filled(),
            theme.accent_error,
            failed,
        ),
    ];
    let mut status = AgentStatusBar::new(theme);
    for (label, glyph, color, count) in chip_specs.into_iter().filter(|(_, _, _, count)| *count > 0)
    {
        status.push(
            label,
            Line::from(vec![
                Span::styled(glyph, bg.fg(color)),
                Span::styled(format!(" {count} {label}"), bg.fg(theme.gray)),
            ]),
        );
    }
    let chip_rects = status.render(buf, area);
    // Mark the spinner from the painted chip, not the working count (a collapsed section can have workers and no chip).
    if chip_rects.contains_key("working") {
        state.painted_animations.mark(Animation::Spinner);
    }

    // 3-cell gutter so the location label never paints under the leftmost chip.
    let full_label_budget = chip_rects
        .values()
        .map(|r| r.x)
        .min()
        .map(|min_x| min_x.saturating_sub(3).saturating_sub(area.x))
        .unwrap_or(area.width) as usize;
    // Reserve the CTA first so the path truncates instead of the button.
    let upgrade_caption = upgrade_cta.and_then(|cta| cta.pinned.then_some(cta.caption).flatten());
    let upgrade_reserve = upgrade_cta.map_or(0usize, |cta| {
        1 + crate::views::announcements::upgrade_cta_reserve(cta.label, upgrade_caption) as usize
    });
    let label_budget = full_label_budget.saturating_sub(upgrade_reserve);

    let LocationParts {
        branch,
        is_worktree,
        cwd_display,
    } = location_parts(&state.cwd);
    let mut location_spans: Vec<Span<'static>> = Vec::new();
    if let Some(branch) = branch {
        location_spans.push(Span::styled(branch, dim));
        location_spans.push(Span::styled(" ", bg));
    }
    if is_worktree {
        location_spans.push(worktree_badge(theme).patch_style(bg));
    }
    location_spans.push(Span::styled(cwd_display, bg.fg(theme.text_secondary)));
    let mut location = truncate_line(Line::from(location_spans), label_budget);
    // `HitArea::set` keeps last-frame `hovered`; underline text only, not the branch/path space.
    if state.location_hit.hovered {
        location.spans = underline_location_on_hover(std::mem::take(&mut location.spans));
    }
    let location_w = location.width() as u16;
    buf.set_line(area.x, area.y, &location, location_w);

    let mut choose_hint = hint_line(
        Span::styled("Choose", dim),
        chord_hint(
            theme,
            registry,
            crate::actions::ActionId::DashboardOpenLocationPicker,
        ),
    );
    choose_hint.spans.insert(0, Span::styled(" [", dim));
    choose_hint.spans.push(Span::styled("]", dim));
    let choose_hint_w = choose_hint.width() as u16;
    let hint_w = if (location_w + choose_hint_w) as usize <= label_budget {
        buf.set_line(area.x + location_w, area.y, &choose_hint, choose_hint_w);
        choose_hint_w
    } else {
        0
    };

    // Clamp the hit to the label budget so it never extends under the chips.
    let label_w = location_w + hint_w;
    let hit_w = label_w.min(label_budget as u16);
    if hit_w > 0 {
        state.location_hit.set(Some(Rect {
            x: area.x,
            y: area.y,
            width: hit_w,
            height: 1,
        }));
    }

    if let Some(HeaderUpgradeCta { label, .. }) = upgrade_cta {
        let avail = full_label_budget.saturating_sub(label_w as usize);
        if avail > 1 {
            let cta_x = area.x + label_w;
            buf.set_span(cta_x, area.y, &Span::styled(" ", bg), 1);
            let painted = crate::views::announcements::render_cta_button(
                buf,
                theme,
                cta_x + 1,
                area.y,
                (avail - 1) as u16,
                label,
                upgrade_caption,
                state.upgrade_cta_hit.hovered,
            );
            state.upgrade_cta_hit.set(painted);
        }
    }
}

fn key_hint_style(theme: &Theme) -> Style {
    theme.faint().bg(theme.bg_base)
}

fn hint_line(label: Span<'static>, hint: Option<Span<'static>>) -> Line<'static> {
    let mut spans = vec![label];
    if let Some(hint) = hint {
        spans.push(Span::styled(" ", hint.style));
        spans.push(hint);
    }
    Line::from(spans)
}

fn chord_hint(
    theme: &Theme,
    registry: &crate::actions::ActionRegistry,
    id: crate::actions::ActionId,
) -> Option<Span<'static>> {
    registry
        .key_for(id)
        .map(|key| Span::styled(key.display(), key_hint_style(theme)))
}

fn underline_location_on_hover(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| {
            if span.content.chars().any(|c| !c.is_whitespace()) {
                span.patch_style(Style::default().add_modifier(Modifier::UNDERLINED))
            } else {
                span
            }
        })
        .collect()
}

/// Paint the primary actions row into `area` and write the three action hit rects.
pub(super) fn render_actions_row(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &mut DashboardState,
    registry: &crate::actions::ActionRegistry,
    workspace_dashboard_enabled: bool,
) {
    if area.area() == 0 {
        fall_back_from_unpainted_item(state);
        return;
    }
    let bg = Style::default().bg(theme.bg_base);
    let key_style = key_hint_style(theme);
    buf.set_style(area, bg);

    let button_fg = |focused: bool, hovered: bool, resting: Color| {
        if focused {
            theme.accent_success
        } else if hovered {
            theme.text_primary
        } else {
            resting
        }
    };

    let worktree_armed = state.worktree_armed();
    let new_agent_label = if worktree_armed {
        "+ New Agent in Worktree"
    } else {
        "+ New Agent"
    };
    let new_agent_w = (UnicodeWidthStr::width(new_agent_label) as u16).min(area.width);

    // Right-hand items are laid out before `+ New Agent` so a focus fallback (below) can still colour that button in the same frame
    // Each right-hand item keeps a 2-cell gap from the `+ New Agent` button so the two sides never touch
    let mut right_edge = area.x + area.width;
    let left_limit = area.x + new_agent_w + 2;
    let fits = |right_edge: u16, w: u16| right_edge.checked_sub(w).is_some_and(|x| x >= left_limit);
    let place_right =
        |right_edge: &mut u16, line: &Line<'static>, buf: &mut Buffer| -> Option<Rect> {
            let w = line.width() as u16;
            if !fits(*right_edge, w) {
                return None;
            }
            let x = *right_edge - w;
            buf.set_line(x, area.y, line, w);
            *right_edge = x;
            Some(Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            })
        };

    let worktree_label = if worktree_armed {
        "Disable Worktree"
    } else {
        "Worktree"
    };
    let worktree_hint = hint_line(
        Span::styled(
            worktree_label,
            bg.fg(button_fg(
                state.worktree_toggle_focused(),
                state.worktree_toggle_hit.hovered,
                theme.gray,
            )),
        ),
        chord_hint(
            theme,
            registry,
            crate::actions::ActionId::DashboardToggleWorktree,
        ),
    );
    let worktree_rect = place_right(&mut right_edge, &worktree_hint, buf);
    state.worktree_toggle_hit.set(worktree_rect);

    // Strict right-to-left priority: once the worktree toggle is out, nothing to its left is tried either, so a narrower
    // `Open Previous` can never take the place of the (wider, armed) toggle
    if workspace_dashboard_enabled && worktree_rect.is_some() {
        // The session picker has no dashboard chord (`Ctrl+R` is rename here), so the hint names the slash command that opens it
        let open_previous = hint_line(
            Span::styled(
                "Open Previous",
                bg.fg(button_fg(
                    state.open_session_button_focused(),
                    state.open_session_button_hit.hovered,
                    theme.gray,
                )),
            ),
            Some(Span::styled("/resume", key_style)),
        );
        let divider = Line::from(Span::styled(" │ ", key_style));
        let needed = (open_previous.width() + divider.width()) as u16;
        let open_rect = fits(right_edge, needed).then(|| {
            place_right(&mut right_edge, &divider, buf);
            place_right(&mut right_edge, &open_previous, buf)
        });
        state.open_session_button_hit.set(open_rect.flatten());
    }
    fall_back_from_unpainted_item(state);

    let new_agent_fg = button_fg(
        state.new_agent_button_focused(),
        state.new_agent_button_hit.hovered,
        theme.text_secondary,
    );
    buf.set_string(
        area.x,
        area.y,
        crate::util::truncate_to_width(new_agent_label, new_agent_w as usize),
        bg.fg(new_agent_fg),
    );
    state.new_agent_button_hit.set(Some(Rect {
        x: area.x,
        y: area.y,
        width: new_agent_w,
        height: 1,
    }));
}

/// A cursor parked on a right-hand item needs a painted item under it; when this frame dropped the item (row too narrow, or no
/// actions row at all), the cursor falls back to `+ New Agent`, which is painted last and so shows the focus colour this frame.
fn fall_back_from_unpainted_item(state: &mut DashboardState) {
    if state
        .actions_focus
        .is_some_and(|item| item != ActionsFocus::NewAgent && !item.is_painted(state))
    {
        state.focus_new_agent_button();
    }
}

#[cfg(test)]
#[path = "chrome_tests.rs"]
mod tests;
