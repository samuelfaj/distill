//! Responsive dashboard cards grouped by state, using the existing session rows and actions.

use indexmap::IndexMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::animation::{Animation, NEEDS_INPUT_BLINK_DIVISOR, SPINNER_DIVISOR};
use super::layout::MIN_DASHBOARD_WIDTH;
use super::row::DashboardRow;
use super::state::{
    DashboardRowId, DashboardState, Filter, Focusable, Grouping, RowState, SectionKey,
};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::render::line_utils::truncate_str;

const CARD_GAP: u16 = 1;
const COLLAPSED_H: u16 = 4;
const HEADER_H: u16 = 2;
const TWO_COL_MIN: u16 = 72;
const THREE_COL_MIN: u16 = 110;

pub(crate) fn board_columns(width: u16) -> u16 {
    if width < TWO_COL_MIN {
        1
    } else if width < THREE_COL_MIN {
        2
    } else {
        3
    }
}

fn card_height(
    row: &DashboardRow,
    state: &DashboardState,
    agents: &IndexMap<AgentId, AgentView>,
    width: usize,
) -> u16 {
    if state.board_card_expanded(&row.id) {
        u16::try_from(card_detail_lines(row, agents, width).len())
            .unwrap_or(u16::MAX)
            .saturating_add(3)
    } else {
        COLLAPSED_H
    }
}

fn state_icon(state: RowState, tick: u64) -> &'static str {
    match state {
        RowState::Working => {
            let frames = crate::glyphs::dot_spinner_frames();
            let i = (tick / SPINNER_DIVISOR) as usize % frames.len();
            frames.get(i).copied().unwrap_or("")
        }
        RowState::Idle | RowState::Inactive => crate::glyphs::diamond_hollow(),
        RowState::NeedsInput | RowState::Completed | RowState::Failed => {
            crate::glyphs::diamond_filled()
        }
    }
}

fn state_color(state: RowState, theme: &crate::theme::Theme) -> ratatui::style::Color {
    match state {
        RowState::Working => theme.accent_running,
        RowState::NeedsInput => theme.warning,
        RowState::Idle | RowState::Inactive => theme.gray_dim,
        RowState::Completed => theme.accent_success,
        RowState::Failed => theme.accent_error,
    }
}

fn needs_input_blink_visible(theme: &crate::theme::Theme) -> bool {
    theme.warning != theme.bg_base
}

fn needs_input_bullet_color(tick: u64, theme: &crate::theme::Theme) -> ratatui::style::Color {
    if (tick / NEEDS_INPUT_BLINK_DIVISOR) % 2 == 0 {
        theme.warning
    } else {
        theme.gray_dim
    }
}

/// State-bucket order for Board: NeedsInput first, pins ignored. Does not mutate saved grouping.
fn board_state_rows(rows: &[DashboardRow]) -> Vec<DashboardRow> {
    let mut clusters: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = 0usize;
    while i < rows.len() {
        let start = i;
        i += 1;
        while i < rows.len() && rows[i].indent != 0 {
            i += 1;
        }
        clusters.push(start..i);
    }
    clusters.sort_by(|left, right| {
        let a = &rows[left.start];
        let b = &rows[right.start];
        b.state
            .group_priority()
            .cmp(&a.state.group_priority())
            .then_with(|| b.last_change_at.cmp(&a.last_change_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = Vec::with_capacity(rows.len());
    for range in clusters {
        for row in &rows[range] {
            let mut cloned = row.clone();
            cloned.pinned = false;
            out.push(cloned);
        }
    }
    out
}

/// Keyboard order matching the board's state headers, cards, and overflow controls.
pub(crate) fn board_focusables(
    rows: &[DashboardRow],
    filter: &Filter,
    collapsed: &std::collections::HashSet<SectionKey>,
    idle_show_all: bool,
    search_active: bool,
) -> Vec<Focusable> {
    let board_rows = board_state_rows(rows);
    super::render::focusables(
        &board_rows,
        Grouping::State,
        filter,
        collapsed,
        idle_show_all,
        search_active,
    )
}

/// Paint the board body and register row / section / delete hits.
pub(crate) fn render_board(
    buf: &mut Buffer,
    area: Rect,
    theme: &crate::theme::Theme,
    rows: &[DashboardRow],
    _grouping: Grouping,
    state: &mut DashboardState,
    agents: &IndexMap<AgentId, AgentView>,
) {
    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    state.idle_overflow_rect = None;
    state.board_slots.clear();
    if area.area() == 0 {
        return;
    }

    let board_rows = board_state_rows(rows);
    let lines = super::render::build_dashboard_lines(
        &board_rows,
        Grouping::State,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    let cols = board_columns(area.width.max(MIN_DASHBOARD_WIDTH));
    let col_w = if cols == 0 {
        area.width
    } else {
        let gaps = CARD_GAP.saturating_mul(cols.saturating_sub(1));
        area.width.saturating_sub(gaps) / cols.max(1)
    };

    let mut blocks: Vec<BoardBlock<'_>> = Vec::new();
    for line in &lines {
        match line {
            super::render::DashboardLine::Header { state: rs, count } => {
                blocks.push(BoardBlock::Header {
                    key: SectionKey::State(*rs),
                    label: rs.group_label(),
                    count: *count,
                });
            }
            super::render::DashboardLine::PinnedHeader { count } => {
                blocks.push(BoardBlock::Header {
                    key: SectionKey::Pinned,
                    label: "Pinned",
                    count: *count,
                });
            }
            super::render::DashboardLine::IdleOverflow { hidden, expanded } => {
                blocks.push(BoardBlock::Overflow {
                    hidden: *hidden,
                    expanded: *expanded,
                });
            }
            super::render::DashboardLine::Row(row) if !row.is_more_placeholder => {
                blocks.push(BoardBlock::Card(row));
            }
            _ => {}
        }
    }

    let mut heights: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        match blocks[i] {
            BoardBlock::Header { .. } | BoardBlock::Overflow { .. } => {
                heights.push(HEADER_H);
                i += 1;
            }
            BoardBlock::Card(_) => {
                let mut row_h = 0u16;
                let mut taken = 0u16;
                while i < blocks.len() && matches!(blocks[i], BoardBlock::Card(_)) && taken < cols {
                    if let BoardBlock::Card(row) = blocks[i] {
                        row_h = row_h.max(card_height(
                            row,
                            state,
                            agents,
                            col_w.saturating_sub(1) as usize,
                        ));
                    }
                    i += 1;
                    taken += 1;
                }
                heights.push(row_h);
            }
        }
    }

    let total_cells: usize = heights.iter().map(|h| *h as usize).sum();
    let mut selected_cell: Option<(usize, u16)> = None;
    let mut cum = 0usize;
    let mut hi = 0usize;
    let mut bi = 0usize;
    while bi < blocks.len() && hi < heights.len() {
        let h = heights[hi];
        let is_cursor = match blocks[bi] {
            BoardBlock::Header { key, .. } => state.selected_section == Some(key),
            BoardBlock::Overflow { .. } => state.selected_idle_overflow,
            BoardBlock::Card(row) => state.selected.as_ref().is_some_and(|s| *s == row.id),
        };
        if is_cursor {
            selected_cell = Some((cum, h));
        }
        match blocks[bi] {
            BoardBlock::Card(_) => {
                let mut taken = 0u16;
                while bi < blocks.len() && matches!(blocks[bi], BoardBlock::Card(_)) && taken < cols
                {
                    if let BoardBlock::Card(row) = blocks[bi]
                        && state.selected.as_ref().is_some_and(|s| *s == row.id)
                    {
                        selected_cell = Some((cum, h));
                    }
                    bi += 1;
                    taken += 1;
                }
            }
            _ => {
                bi += 1;
            }
        }
        cum += h as usize;
        hi += 1;
    }
    let viewport_h = area.height as usize;
    let snap_target = selected_cell.map(|(top, h)| top + (h as usize).saturating_sub(1));
    let mut offset = state.clamp_viewport(snap_target, viewport_h, total_cells);
    if !state.manual_scroll_active
        && let Some((sel_top, _)) = selected_cell
        && sel_top < offset
    {
        state.viewport_offset = sel_top;
        offset = sel_top;
    }
    state.viewport_offset = offset;

    let max_y = area.y + area.height;
    let mut cell_y = 0usize;
    let mut hi = 0usize;
    let mut bi = 0usize;
    let mut grid_row = 0u16;
    while bi < blocks.len() && hi < heights.len() {
        let h = heights[hi];
        let next_cell_y = cell_y + h as usize;
        let fully_above = next_cell_y <= offset;
        let fully_below = cell_y >= offset + viewport_h;
        let paint = !fully_above && !fully_below;
        match blocks[bi] {
            BoardBlock::Header { key, label, count } => {
                if paint {
                    let visible_top = cell_y.max(offset);
                    let y = area.y + (visible_top - offset) as u16;
                    if y < max_y {
                        let render_h = ((next_cell_y - visible_top) as u16).min(max_y - y);
                        let rect = Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: render_h,
                        };
                        super::render::render_group_header(
                            buf,
                            rect,
                            theme,
                            label,
                            count,
                            state.is_section_collapsed(key),
                            state.selected_section == Some(key),
                            state.hovered_section == Some(key),
                        );
                        state.section_rects.push((key, rect));
                    }
                }
                bi += 1;
                grid_row = grid_row.saturating_add(1);
            }
            BoardBlock::Overflow { hidden, expanded } => {
                if paint {
                    let visible_top = cell_y.max(offset);
                    let y = area.y + (visible_top - offset) as u16;
                    if y < max_y {
                        let render_h = ((next_cell_y - visible_top) as u16).min(max_y - y);
                        let rect = Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: render_h,
                        };
                        super::render::render_idle_overflow(
                            buf,
                            rect,
                            theme,
                            hidden,
                            expanded,
                            state.selected_idle_overflow,
                            state.hovered_idle_overflow,
                        );
                        state.idle_overflow_rect = Some(rect);
                    }
                }
                bi += 1;
                grid_row = grid_row.saturating_add(1);
            }
            BoardBlock::Card(_) => {
                let clip_rows = offset.saturating_sub(cell_y).min(h as usize) as u16;
                let mut taken = 0u16;
                while bi < blocks.len() && matches!(blocks[bi], BoardBlock::Card(_)) && taken < cols
                {
                    if let BoardBlock::Card(row) = blocks[bi] {
                        if paint {
                            let visible_top = cell_y.max(offset);
                            let y = area.y + (visible_top - offset) as u16;
                            if y < max_y {
                                let render_h = ((next_cell_y - visible_top) as u16).min(max_y - y);
                                let x = area.x + taken * (col_w + CARD_GAP);
                                let rect = Rect {
                                    x,
                                    y,
                                    width: col_w,
                                    height: render_h,
                                };
                                paint_card(buf, rect, theme, row, state, agents, clip_rows);
                                if !row.is_more_placeholder {
                                    state.row_rects.push((row.id.clone(), rect));
                                }
                            }
                        }
                        if !row.is_more_placeholder {
                            state.board_slots.push((row.id.clone(), (taken, grid_row)));
                        }
                    }
                    bi += 1;
                    taken += 1;
                }
                grid_row = grid_row.saturating_add(1);
            }
        }
        cell_y = next_cell_y;
        hi += 1;
    }
}

#[derive(Clone, Copy)]
enum BoardBlock<'a> {
    Header {
        key: SectionKey,
        label: &'static str,
        count: usize,
    },
    Overflow {
        hidden: usize,
        expanded: bool,
    },
    Card(&'a DashboardRow),
}

fn wrap_detail_lines(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() || width == 0 {
        return Vec::new();
    }
    crate::render::wrapping::wrap_byte_ranges_matching(text, width)
        .into_iter()
        .filter_map(|range| text.get(range))
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn pending_wraps_question(secondary: &str, question: Option<&str>) -> bool {
    if secondary == "Pending: question" {
        return true;
    }
    question.is_some_and(|q| {
        secondary
            .strip_prefix("Pending: ")
            .is_some_and(|rest| rest == q)
    })
}

fn expanded_fields(row: &DashboardRow, question: Option<&str>) -> Vec<String> {
    let mut fields = Vec::new();
    let mut push_unique = |value: &str| {
        if value.is_empty() {
            return;
        }
        if fields.iter().any(|have| have == value) {
            return;
        }
        fields.push(value.to_string());
    };
    if let Some(q) = question {
        push_unique(q);
    }
    if let Some(activity) = row.activity.as_deref() {
        push_unique(activity);
    }
    if let Some(secondary) = row.secondary_line.as_deref()
        && !pending_wraps_question(secondary, question)
    {
        push_unique(secondary);
    }
    if !row.cwd_display.is_empty() {
        push_unique(&row.cwd_display);
    }
    if let Some(subtitle) = row.subtitle.as_deref() {
        push_unique(subtitle);
    }
    fields
}

fn card_detail_lines(
    row: &DashboardRow,
    agents: &IndexMap<AgentId, AgentView>,
    width: usize,
) -> Vec<String> {
    let question = super::peek::compute_peek_fields(&row.id, agents)
        .and_then(|fields| fields.question)
        .or_else(|| {
            let DashboardRowId::TopLevel(id) = &row.id else {
                return None;
            };
            let view = agents.get(id)?.question_view.as_ref()?;
            let question = view.questions.get(view.active_tab)?;
            Some(
                crate::views::session_title::sanitize_display_text(&question.question).into_owned(),
            )
        });
    expanded_fields(row, question.as_deref())
        .iter()
        .flat_map(|field| wrap_detail_lines(field, width))
        .collect()
}

fn card_line_y(logical: u16, clip_rows: u16, rect: Rect) -> Option<u16> {
    if logical < clip_rows {
        return None;
    }
    let y = rect.y.saturating_add(logical - clip_rows);
    if y >= rect.y.saturating_add(rect.height) {
        return None;
    }
    Some(y)
}

fn paint_card(
    buf: &mut Buffer,
    rect: Rect,
    theme: &crate::theme::Theme,
    row: &DashboardRow,
    state: &mut DashboardState,
    agents: &IndexMap<AgentId, AgentView>,
    clip_rows: u16,
) {
    if rect.area() == 0 {
        return;
    }
    let selected = state.selected.as_ref().is_some_and(|s| *s == row.id);
    let hovered = state.hovered_row.as_ref().is_some_and(|h| *h == row.id);
    let bg = if selected {
        theme.bg_highlight
    } else if hovered {
        theme.bg_hover
    } else {
        theme.bg_base
    };
    buf.set_style(rect, Style::default().bg(bg));
    let expanded = state.board_card_expanded(&row.id);
    let logical_h = card_height(row, state, agents, rect.width.saturating_sub(1) as usize);
    let icon = state_icon(row.state, state.spinner_tick);
    let mut color = state_color(row.state, theme);
    if row.state == RowState::NeedsInput {
        color = needs_input_bullet_color(state.spinner_tick, theme);
        if needs_input_blink_visible(theme) {
            state.painted_animations.mark(Animation::Blink);
        }
    }
    if row.state == RowState::Working {
        state.painted_animations.mark(Animation::Spinner);
    }
    let marker = if selected {
        crate::glyphs::selection_bar()
    } else {
        " "
    };
    if let Some(y0) = card_line_y(0, clip_rows, rect) {
        buf.set_span(
            rect.x,
            y0,
            &Span::styled(
                marker,
                Style::default()
                    .bg(bg)
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            ),
            1,
        );
        let icon_w = UnicodeWidthStr::width(icon) as u16;
        buf.set_span(
            rect.x + 1,
            y0,
            &Span::styled(icon, Style::default().bg(bg).fg(color)),
            icon_w,
        );
        let label_x = rect.x + 1 + icon_w + 1;
        let delete_label = "[✗]";
        let delete_w = UnicodeWidthStr::width(delete_label) as u16;
        let show_delete = row.state.allows_delete()
            && !matches!(row.id, DashboardRowId::Roster { .. })
            && !row
                .session_id
                .as_ref()
                .is_some_and(|sid| state.conversation_row_ids.contains(sid));
        let text_w = rect
            .width
            .saturating_sub(1 + icon_w + 1)
            .saturating_sub(if show_delete { delete_w + 1 } else { 0 });
        let label = truncate_str(&row.label, text_w as usize);
        buf.set_span(
            label_x,
            y0,
            &Span::styled(
                label,
                Style::default()
                    .bg(bg)
                    .fg(theme.text_primary)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            text_w,
        );
        if show_delete && rect.width > delete_w {
            let dx = rect.x + rect.width.saturating_sub(delete_w);
            let fg = if state.hovered_delete.as_ref() == Some(&row.id)
                || state.armed_delete_row_ref() == Some(&row.id)
            {
                theme.accent_error
            } else {
                theme.text_secondary
            };
            buf.set_string(dx, y0, delete_label, Style::default().bg(bg).fg(fg));
            state.row_delete_rects.push((
                row.id.clone(),
                Rect {
                    x: dx,
                    y: y0,
                    width: delete_w,
                    height: 1,
                },
            ));
        }
    }

    if let Some(y1) = card_line_y(1, clip_rows, rect) {
        let status = row.state.group_label();
        buf.set_span(
            rect.x + 1,
            y1,
            &Span::styled(status, Style::default().bg(bg).fg(color)),
            rect.width.saturating_sub(1),
        );
    }

    let text_w = rect.width.saturating_sub(1) as usize;
    let preview = row
        .activity
        .as_deref()
        .or(row.secondary_line.as_deref())
        .unwrap_or("");
    let detail_lines: Vec<String> = if expanded {
        card_detail_lines(row, agents, text_w)
    } else if !preview.is_empty() {
        vec![truncate_str(preview, text_w)]
    } else {
        Vec::new()
    };
    for (i, line) in detail_lines.iter().enumerate() {
        let logical = 2 + i as u16;
        if logical >= logical_h {
            break;
        }
        let Some(y) = card_line_y(logical, clip_rows, rect) else {
            continue;
        };
        buf.set_span(
            rect.x + 1,
            y,
            &Span::styled(line.as_str(), theme.dim().bg(bg)),
            rect.width.saturating_sub(1),
        );
    }
}

/// Neighbour card in `dir` columns (negative = left).
pub(crate) fn board_neighbour_horizontal(
    state: &DashboardState,
    dir: i16,
) -> Option<DashboardRowId> {
    let sel = state.selected.as_ref()?;
    let &(col, row) = state
        .board_slots
        .iter()
        .find(|(id, _)| id == sel)
        .map(|(_, pos)| pos)?;
    let target = i32::from(col) + i32::from(dir);
    if target < 0 {
        return None;
    }
    let target = target as u16;
    state
        .board_slots
        .iter()
        .find(|(_, (c, r))| *c == target && *r == row)
        .map(|(id, _)| id.clone())
}

/// Neighbour card in `dir` grid rows (negative = up). Header/overflow rows occupy a grid row with
/// no card slot, so this is `None` and state section nav should take over.
pub(crate) fn board_neighbour_vertical(state: &DashboardState, dir: i16) -> Option<DashboardRowId> {
    let sel = state.selected.as_ref()?;
    let &(col, row) = state
        .board_slots
        .iter()
        .find(|(id, _)| id == sel)
        .map(|(_, pos)| pos)?;
    let target = i32::from(row) + i32::from(dir);
    if target < 0 {
        return None;
    }
    let target = target as u16;
    state
        .board_slots
        .iter()
        .filter(|(_, (_, r))| *r == target)
        .min_by_key(|(_, (c, _))| c.abs_diff(col))
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
