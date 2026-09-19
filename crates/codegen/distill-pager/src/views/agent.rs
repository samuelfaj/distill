//! Agent view layout and rendering helpers.
//!
//! This module provides:
//! - [`AgentViewLayout`]: pure layout computation (screen area to pane rects)
//! - [`ActivePane`] / [`PaneAreas`]: pane identity and hit-testing
//! - Overlay helpers: small focused functions for selection/hover chrome
//! - [`build_hints`]: shortcuts bar hint generation
//!
//! The drawing itself happens in [`AgentView::draw()`](crate::app::agent_view::AgentView::draw).
//! It renders the shared widgets (StatusBar, ScrollbackPane, PromptWidget, ShortcutsBar) and uses these helpers for the agent-specific glue.
use crate::actions::{ActionId, ActionRegistry, When};
use crate::app::agent_view::ViewSurface;
use crate::appearance::{LayoutConfig, ScrollbarConfig};
use crate::render::SafeBuf;
use crate::render::scrollbar::render_scrollbar_styled;
use crate::scrollback::layout::HorizontalLayout;
use crate::scrollback::search::ScrollbackSearchState;
use crate::scrollback::selection::SelectionBox;
use crate::scrollback::state::ScrollbackState;
use crate::theme::Theme;
use crate::views::prompt_widget::PromptWidget;
use crate::views::queue_mutation::QueueMutation;
use crate::views::shortcuts_bar::HintItem;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Padding, Widget};
/// Which pane is currently active in the agent view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePane {
    #[default]
    Scrollback,
    Todo,
    Queue,
    Prompt,
    Tasks,
    Catalog,
    /// Consolidated panel dock above the prompt (remote `dock_enabled`).
    Dock,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InputMode {
    #[default]
    Vim,
    Simple,
}
/// Cached pane areas from the last render, used for mouse hit-testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneAreas {
    pub scrollback: Rect,
    pub todo: Rect,
    pub queue: Rect,
    pub prompt: Rect,
    pub tasks: Rect,
    pub catalog: Rect,
    /// Consolidated panel dock (remote `dock_enabled`); the embedded
    /// queue body inside it hit-tests as `Queue` (checked first).
    pub dock: Rect,
}
impl PaneAreas {
    /// Determine which pane a screen position falls in, if any.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<ActivePane> {
        let pos = (col, row).into();
        if self.tasks.area() > 0 && self.tasks.contains(pos) {
            return Some(ActivePane::Tasks);
        }
        if self.catalog.area() > 0 && self.catalog.contains(pos) {
            return Some(ActivePane::Catalog);
        }
        if self.todo.area() > 0 && self.todo.contains(pos) {
            return Some(ActivePane::Todo);
        }
        if self.queue.area() > 0 && self.queue.contains(pos) {
            return Some(ActivePane::Queue);
        }
        if self.dock.area() > 0 && self.dock.contains(pos) {
            return Some(ActivePane::Dock);
        }
        if self.scrollback.contains(pos) {
            return Some(ActivePane::Scrollback);
        }
        if self.prompt.contains(pos) {
            return Some(ActivePane::Prompt);
        }
        None
    }
}
/// Terminals at or below this height suppress the optional rows above the prompt (plugin CTA, follow-ups, banner/tip).
/// This keeps the prompt and the scrollback from being starved.
pub const SHORT_TERMINAL_ROWS: u16 = 16;
/// The scrollback's floor, pushed as the layout's only `Min`.
/// The solver ranks it above every `Length`, so an over-committed layout shrinks another row.
pub const SCROLLBACK_MIN_ROWS: u16 = 5;
/// Auto-compact threshold: at or below this height the compact flag handed to rendering is forced on.
/// Deliberately above [`SHORT_TERMINAL_ROWS`], which still gates the harder cuts (tip-row rendering, dropping the CTA and follow-up rows).
pub const AUTO_COMPACT_MAX_ROWS: u16 = 20;
const _: () = assert!(SHORT_TERMINAL_ROWS < AUTO_COMPACT_MAX_ROWS);
/// The result is never written back to `current_ui.compact_mode`, the render cache, or disk.
/// Growing the window therefore restores the user's choice. `terminal_rows == 0` means "not yet
/// measured" and never forces compact.
pub fn effective_compact(user_compact: bool, terminal_rows: u16) -> bool {
    user_compact || (terminal_rows > 0 && terminal_rows <= AUTO_COMPACT_MAX_ROWS)
}
/// Every input [`AgentViewLayout::compute`] reads: the screen area, the appearance config, and the
/// requested height of each row it stacks.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentViewLayoutParams {
    pub area: Rect,
    pub layout_cfg: LayoutConfig,
    pub scrollbar_cfg: ScrollbarConfig,
    /// Rail columns taken in place of the scrollbar (0 means hidden).
    /// The rail needs the scrollbar's gutter geometry, so a disabled scrollbar forces this to 0.
    pub timeline_width: u16,
    pub prompt_height: u16,
    pub tasks_height: u16,
    pub catalog_height: u16,
    pub todo_height: u16,
    pub queue_height: u16,
    pub btw_height: u16,
    pub turn_status_height: u16,
    pub banner_height: u16,
    /// Forced to 0 on short terminals (`area.height <= SHORT_TERMINAL_ROWS`) so the prompt and scrollback are never starved.
    pub cta_height: u16,
    /// Force-suppressed on short terminals on the same rule as `cta_height`.
    pub follow_ups_height: u16,
    /// Consolidated panel dock (Subagents/Tasks/Watchers/Queued) directly
    /// above the prompt. 0 = hidden (the default; remote `dock_enabled`).
    pub dock_height: u16,
    /// 0 or 1: the gap row between turn status (or scrollback) and the prompt.
    pub prompt_gap: u16,
    pub voice_recording_height: u16,
    pub shortcuts_height: u16,
    /// Clamped to the rows left over once every other row and the scrollback minimum are counted.
    /// A tall script loses its own rows rather than the prompt or the shortcuts bar losing theirs.
    pub status_line_height: u16,
    pub compact: bool,
}
/// Computed screen layout for the agent view. Pure data, no rendering. Computed from
/// [`AgentViewLayoutParams`]. Shared widgets use these rects to render into.
pub struct AgentViewLayout {
    pub status_bar: Rect,
    pub tasks: Rect,
    pub catalog: Rect,
    pub scrollback: Rect,
    pub todo: Rect,
    pub queue: Rect,
    /// Inline panel for /btw side questions (above the queue, turn status, and prompt rows).
    pub btw: Rect,
    pub turn_status: Rect,
    /// Banner rect above the prompt (mode-switch banner, ephemeral tips).
    pub banner: Rect,
    /// Inline plugin-CTA row (below banner, above the prompt gap).
    pub plugin_cta: Rect,
    /// Follow-up suggestion chips row (below the plugin CTA, above the prompt).
    pub follow_ups: Rect,
    /// Consolidated panel dock (Subagents/Tasks/Watchers/Queued) directly
    /// above the prompt; zero-area when hidden.
    pub dock: Rect,
    /// Single-row record indicator ("◉ Recording") directly above the prompt, shown only while voice capture is active.
    pub voice_recording: Rect,
    pub prompt: Rect,
    pub shortcuts: Rect,
    /// Bottom status_line row; zero-area when disabled.
    pub status_line: Rect,
    /// Scrollback area narrowed for scrollbar (content rendering uses this).
    pub scrollback_content: Rect,
    /// Scrollbar track position (x coordinate).
    pub scrollbar_x: u16,
    /// Timeline rail left edge, only meaningful when `timeline_width > 0`.
    /// The rail's right edge lands on the scrollbar column, which the rail replaces.
    pub timeline_x: u16,
    /// Columns reserved for the timeline rail (0 means the rail is hidden).
    /// Non-zero also means the scrollbar does not render this frame.
    pub timeline_width: u16,
}
impl AgentViewLayout {
    /// Stack the rows described by `params` into the screen area.
    ///
    /// Each row's rules are documented on [`AgentViewLayoutParams`].
    pub fn compute(params: AgentViewLayoutParams) -> Self {
        let AgentViewLayoutParams {
            area,
            layout_cfg,
            scrollbar_cfg,
            timeline_width,
            prompt_height,
            tasks_height,
            catalog_height,
            todo_height,
            queue_height,
            btw_height,
            turn_status_height,
            banner_height,
            cta_height,
            follow_ups_height,
            dock_height,
            prompt_gap,
            voice_recording_height,
            shortcuts_height,
            status_line_height,
            compact,
        } = params;
        let outer_vpad = layout_cfg.eff_outer_vpad(compact);
        let bottom_vpad = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            outer_vpad
        };
        let cta_height = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            cta_height
        };
        let follow_ups_height = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            follow_ups_height
        };
        let top_vpad = outer_vpad;
        let outer_block = Block::default().padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            top_vpad,
            bottom_vpad,
        ));
        let inner_area = outer_block.inner(area);
        let mut constraints = vec![
            Constraint::Length(1), // StatusBar
        ];
        let pane_gap = if top_vpad == 0 { 0u16 } else { 1 };
        if tasks_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(tasks_height));
        }
        if catalog_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(catalog_height));
        }
        if todo_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(todo_height));
        }
        let status_gap = if top_vpad == 0 { 0u16 } else { 1 };
        constraints.push(Constraint::Length(status_gap));
        constraints.push(Constraint::Min(SCROLLBACK_MIN_ROWS));
        if btw_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(btw_height));
        }
        if queue_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(queue_height));
        }
        if turn_status_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(turn_status_height));
        }
        if banner_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(banner_height));
        }
        if cta_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(cta_height));
        }
        if follow_ups_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(follow_ups_height));
        }
        if dock_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(dock_height));
        }
        if prompt_gap > 0 {
            constraints.push(Constraint::Length(prompt_gap));
        }
        if voice_recording_height > 0 {
            constraints.push(Constraint::Length(voice_recording_height));
        }
        constraints.push(Constraint::Length(prompt_height));
        let pushed = constraints
            .iter()
            .map(|c| match c {
                Constraint::Length(n) | Constraint::Min(n) | Constraint::Max(n) => *n,
                Constraint::Percentage(_) | Constraint::Ratio(_, _) | Constraint::Fill(_) => 0,
            })
            .fold(0u16, u16::saturating_add);
        let reserved = pushed.saturating_add(shortcuts_height);
        let status_line_height = status_line_height.min(inner_area.height.saturating_sub(reserved));
        let shortcuts_gap = u16::from(bottom_vpad > 0 && status_line_height == 0);
        if shortcuts_gap > 0 {
            constraints.push(Constraint::Length(shortcuts_gap));
        }
        if status_line_height > 0 {
            constraints.push(Constraint::Length(status_line_height));
        }
        constraints.push(Constraint::Length(shortcuts_height));
        let chunks = Layout::vertical(constraints).split(inner_area);
        let mut chunks = chunks.iter().copied();
        let status_bar = chunks.next().unwrap_or_default();
        let tasks = if tasks_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let catalog = if catalog_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let todo = if todo_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        chunks.next();
        let scrollback = chunks.next().unwrap_or_default();
        let btw = if btw_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let queue = if queue_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let turn_status = if turn_status_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let banner = if banner_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let plugin_cta = if cta_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let follow_ups = if follow_ups_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let dock = if dock_height > 0 {
            chunks.next();
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        if prompt_gap > 0 {
            chunks.next();
        }
        let voice_recording = if voice_recording_height > 0 {
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let prompt = chunks.next().unwrap_or_default();
        if shortcuts_gap > 0 {
            chunks.next();
        }
        let status_line = if status_line_height > 0 {
            chunks.next().unwrap_or_default()
        } else {
            Rect::default()
        };
        let shortcuts = chunks.next().unwrap_or_default();
        let scrollbar_x = area.right().saturating_sub(scrollbar_cfg.gap_right + 1);
        let timeline_width = if scrollbar_cfg.enabled {
            timeline_width
        } else {
            0
        };
        let timeline_x = (scrollbar_x + 1).saturating_sub(timeline_width);
        let content_end_x = if timeline_width > 0 {
            timeline_x.saturating_sub(scrollbar_cfg.gap_left)
        } else {
            scrollbar_x.saturating_sub(scrollbar_cfg.gap_left)
        };
        let scrollback_right = scrollback.x + scrollback.width;
        let scrollback_content = if !scrollbar_cfg.enabled || content_end_x >= scrollback_right {
            scrollback
        } else {
            Rect {
                width: content_end_x.saturating_sub(scrollback.x),
                ..scrollback
            }
        };
        Self {
            status_bar,
            tasks,
            catalog,
            scrollback,
            todo,
            queue,
            btw,
            turn_status,
            banner,
            plugin_cta,
            follow_ups,
            dock,
            voice_recording,
            prompt,
            shortcuts,
            status_line,
            scrollback_content,
            scrollbar_x,
            timeline_x,
            timeline_width,
        }
    }
    /// Measured through [`Self::compute`] rather than re-summed: a probe with a zero-row prompt hands
    /// the scrollback every row nothing else claimed.
    pub fn rows_available_for_prompt(params: AgentViewLayoutParams) -> u16 {
        let probe = Self::compute(AgentViewLayoutParams {
            prompt_height: 0,
            ..params
        });
        probe.scrollback.height.saturating_sub(SCROLLBACK_MIN_ROWS)
    }
    /// Inner area width (for prompt height computation before full layout).
    ///
    /// This computes just the inner width without the full layout split, since prompt height is needed as input to `compute()`.
    pub fn inner_width(area: Rect, layout_cfg: &LayoutConfig, compact: bool) -> u16 {
        let vpad = layout_cfg.eff_outer_vpad(compact);
        let outer_block = Block::default().padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            vpad,
            vpad,
        ));
        outer_block.inner(area).width
    }
    /// Convert to PaneAreas for mouse hit-testing.
    pub fn pane_areas(&self) -> PaneAreas {
        PaneAreas {
            scrollback: self.scrollback,
            todo: self.todo,
            queue: self.queue,
            prompt: self.prompt,
            tasks: self.tasks,
            catalog: self.catalog,
            dock: self.dock,
        }
    }
}
/// Fill the screen area with base background and outer padding.
pub fn fill_background(
    buf: &mut Buffer,
    area: Rect,
    layout_cfg: &LayoutConfig,
    compact: bool,
    theme: &Theme,
) {
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let vpad = layout_cfg.eff_outer_vpad(compact);
    let outer_styled = Block::default()
        .padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            vpad,
            vpad,
        ))
        .style(Style::default().bg(theme.bg_base));
    outer_styled.render(area, buf);
}
/// The result is therefore an index-aligned prefix of `suggestions`, not a filtered subset.
pub(crate) fn render_follow_ups(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    suggestions: &[String],
    hovered: Option<usize>,
) -> Vec<Rect> {
    use crate::render::line_utils::truncate_str;
    use unicode_width::UnicodeWidthStr;
    let mut rects = Vec::new();
    if area.height == 0 || area.width == 0 {
        return rects;
    }
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
            cell.set_char(' ');
            cell.fg = theme.text_secondary;
            cell.bg = theme.bg_base;
        }
    }
    const MAX_CHIP_LABEL: usize = 48;
    let chip_style = Style::default().fg(theme.link_fg);
    let hover_style = theme.hover_overlay().fg(theme.text_primary);
    let row_end = area.x + area.width;
    let mut x = area.x;
    for (i, label) in suggestions.iter().enumerate() {
        let label = truncate_str(label, MAX_CHIP_LABEL);
        let chip = format!("[ {label} ]");
        let chip_w = chip.width() as u16;
        if x >= row_end || chip_w > row_end - x {
            break;
        }
        let style = if hovered == Some(i) {
            hover_style
        } else {
            chip_style
        };
        buf.set_span_safe(x, area.y, &Span::styled(chip, style), chip_w);
        rects.push(Rect::new(x, area.y, chip_w, 1));
        x = x.saturating_add(chip_w).saturating_add(1);
    }
    rects
}
/// Render hover box around a scrollback entry (dimmed selection border).
///
/// Skipped if the hovered entry is also the selected entry (selection box wins).
pub fn render_entry_hover(
    buf: &mut Buffer,
    scrollback_area: Rect,
    scrollback: &ScrollbackState,
    hovered_entry: Option<usize>,
    theme: &Theme,
) {
    let Some(hover_idx) = hovered_entry else {
        return;
    };
    if scrollback.selected() == Some(hover_idx) {
        return;
    }
    let split_mode = scrollback
        .appearance()
        .scrollback
        .display
        .group_selection_split;
    let group = scrollback.group_range_of(hover_idx, split_mode);
    if group.len() <= 1 {
        if let Some((area, top_clipped, bottom_clipped)) =
            scrollback.entry_screen_area(hover_idx, scrollback_area)
        {
            SelectionBox::new(area, Style::default().fg(theme.hover_border))
                .with_top_clipped(top_clipped)
                .with_bottom_clipped(bottom_clipped)
                .render(buf);
        }
    } else {
        let first_area = scrollback.entry_screen_area(group.start, scrollback_area);
        let last_area = scrollback.entry_screen_area(group.end - 1, scrollback_area);
        if let (
            Some((first_rect, first_top_clipped, _)),
            Some((last_rect, _, last_bottom_clipped)),
        ) = (first_area, last_area)
        {
            let y_start = first_rect.y;
            let y_end = last_rect.y + last_rect.height;
            let combined = Rect::new(first_rect.x, y_start, first_rect.width, y_end - y_start);
            SelectionBox::new(combined, Style::default().fg(theme.hover_border))
                .with_top_clipped(first_top_clipped)
                .with_bottom_clipped(last_bottom_clipped)
                .render(buf);
        }
    }
}
/// Selection/hover chrome for a side pane (todo, queue, tasks). Focused panes get a dismiss control.
pub fn render_todo_chrome(
    buf: &mut Buffer,
    todo_area: Rect,
    layout_cfg: &LayoutConfig,
    focused: bool,
    hovered: bool,
    close_hovered: bool,
    theme: &Theme,
) -> Option<SelectionBox> {
    render_todo_chrome_with_close_label(
        buf,
        todo_area,
        layout_cfg,
        focused,
        hovered,
        close_hovered,
        theme,
        None,
    )
}
/// Like [`render_todo_chrome`], with optional close label (queue uses `[close]`).
#[allow(clippy::too_many_arguments)]
pub fn render_todo_chrome_with_close_label(
    buf: &mut Buffer,
    todo_area: Rect,
    layout_cfg: &LayoutConfig,
    focused: bool,
    hovered: bool,
    close_hovered: bool,
    theme: &Theme,
    close_label: Option<&'static str>,
) -> Option<SelectionBox> {
    if todo_area.area() == 0 {
        return None;
    }
    let color = if focused {
        theme.selection_border
    } else if hovered {
        theme.hover_border
    } else {
        return None;
    };
    let layout = HorizontalLayout::new(todo_area, layout_cfg);
    let mut sel = SelectionBox::new(layout.selection_area(), Style::default().fg(color))
        .with_closable(focused, close_hovered);
    if focused && let Some(label) = close_label {
        sel = sel.with_close_label(Some(label));
    }
    sel.render(buf);
    Some(sel)
}
/// Render the scrollbar track and thumb. When `is_following` is true, the scrollbar thumb is dimmed
/// to show the viewport is locked to the bottom. That makes follow mode (G) visible in the
/// scrollbar itself.
pub fn render_scrollbar(
    buf: &mut Buffer,
    scrollback_area: Rect,
    scrollbar_x: u16,
    scrollbar_cfg: &ScrollbarConfig,
    scroll_info: Option<crate::scrollback::selection::ScrollInfo>,
    is_following: bool,
    theme: &Theme,
) {
    if scrollbar_cfg.enabled
        && let Some(scroll_info) = scroll_info
    {
        let scrollbar_area = Rect {
            x: scrollbar_x,
            y: scrollback_area.y,
            width: 1,
            height: scrollback_area.height,
        };
        let scrollbar_bg = scrollbar_cfg.scrollbar_bg.unwrap_or(theme.scrollbar_bg);
        let scrollbar_fg = scrollbar_cfg.scrollbar_fg.unwrap_or(theme.scrollbar_fg);
        let thumb_fg = if is_following {
            crate::render::color::blend_color(scrollbar_bg, scrollbar_fg, 0.4)
                .unwrap_or(scrollbar_fg)
        } else {
            scrollbar_fg
        };
        let track_style = Style::default().bg(scrollbar_bg);
        let thumb_style = Style::default().fg(thumb_fg).bg(scrollbar_bg);
        let scale = if scroll_info.total_height > u16::MAX as usize {
            (scroll_info.total_height / u16::MAX as usize) + 1
        } else {
            1
        };
        let scaled_total = (scroll_info.total_height / scale) as u16;
        let scaled_offset = (scroll_info.scroll_offset / scale) as u16;
        render_scrollbar_styled(
            buf,
            Some(scrollbar_area),
            scaled_total,
            scroll_info.viewport_height,
            scaled_offset,
            track_style,
            thumb_style,
        );
    }
}
/// The scrollback's default focus hint: `Space` leaves for the prompt.
/// A parked blocking card replaces it with its own (pinned) route back.
pub fn prompt_focus_hint() -> HintItem {
    use crate::input::key::KeyShortcut;
    use crossterm::event::{KeyCode, KeyModifiers};
    HintItem {
        keys: vec![KeyShortcut::new(KeyCode::Char(' '), KeyModifiers::NONE)],
        label: "prompt".into(),
        custom_display: Some("Space"),
        description: None,
        pinned: false,
    }
}
/// A pinned one leads the bar and is offered once; an unpinned one is offered only in the selection
/// states where moving on is the useful next step.
#[expect(clippy::too_many_arguments)]
pub(crate) fn build_hints(
    active_pane: ActivePane,
    focus_hint: HintItem,
    prompt: &PromptWidget,
    registry: &ActionRegistry,
    is_editing_queued: bool,
    fold_label: Option<&'static str>,
    group_header_label: Option<&'static str>,
    tab_label: &'static str,
    thinking_label: &'static str,
    show_done: bool,
    selected_supports_copy: bool,
    selected_meta_label: Option<&'static str>,
    selected_supports_fullscreen: bool,
    can_demote: bool,
    selected_can_kill: bool,
    multiline_mode: bool,
    vim_mode: bool,
    surface: ViewSurface,
    is_turn_running: bool,
    has_queued_follow_up: bool,
    queue_mutation: QueueMutation,
    selected_is_user_prompt: bool,
    selected_is_agent_message: bool,
    prefer_alt_enter_newline: bool,
    scrollback_search: Option<&ScrollbackSearchState>,
) -> Vec<HintItem> {
    let mut hints = match active_pane {
        ActivePane::Todo => {
            let mut hints = Vec::new();
            hints.push(HintItem::new(
                crate::key!('h'),
                if show_done { "hide done" } else { "show done" },
            ));
            hints
        }
        ActivePane::Dock => {
            let mut hints = vec![HintItem::paired(
                crate::key!('j'),
                crate::key!('k'),
                "navigate",
            )];
            hints.push(HintItem::new(
                crate::key!('h'),
                if show_done { "hide done" } else { "show done" },
            ));
            if let Some(label) = group_header_label {
                hints.push(HintItem::new(crate::key!(Enter), label));
            }
            if selected_can_kill {
                hints.push(HintItem::new(crate::key!('x'), "kill"));
            }
            hints.push(HintItem::new(crate::key!(Tab), tab_label));
            hints
        }
        ActivePane::Queue => match queue_mutation {
            QueueMutation::ReadOnly => vec![HintItem::new(crate::key!('y'), "copy")],
            QueueMutation::PerRowKind => {
                let mut hints = vec![
                    HintItem::new(crate::key!('x'), "delete row"),
                    HintItem::new(crate::key!('e'), "edit"),
                    HintItem::paired(crate::key!('J'), crate::key!('K'), "reorder"),
                    HintItem::new(crate::key!('y'), "copy"),
                ];
                if is_turn_running && let Some(def) = registry.find(ActionId::InterjectPrompt) {
                    hints.push(def.hint());
                }
                hints
            }
        },
        ActivePane::Prompt if is_editing_queued => {
            let mut hints = Vec::new();
            if prompt.can_send() {
                hints.push(HintItem::new(crate::key!(Enter), "save"));
            }
            hints.push(HintItem::new(crate::key!(Esc), "cancel"));
            hints
        }
        ActivePane::Prompt if prompt.history_search.is_active() => {
            use crate::input::key::KeyShortcut;
            use crossterm::event::KeyCode;
            vec![
                HintItem::paired(
                    KeyShortcut::key(KeyCode::Up),
                    KeyShortcut::key(KeyCode::Down),
                    "nav",
                ),
                HintItem::paired(
                    KeyShortcut::key(KeyCode::PageUp),
                    KeyShortcut::key(KeyCode::PageDown),
                    "page",
                ),
                HintItem::new(KeyShortcut::key(KeyCode::Enter), "select"),
                HintItem::new(KeyShortcut::key(KeyCode::Esc), "cancel"),
            ]
        }
        ActivePane::Prompt => {
            let mut hints = Vec::new();
            let newline_key = if prefer_alt_enter_newline {
                crate::key!(Enter, ALT)
            } else {
                crate::key!(Enter, SHIFT)
            };
            let submit_label = if is_turn_running { "queue" } else { "send" };
            if let Some(key) = registry.key_for(ActionId::SendPrompt) {
                if prompt.paste_element_at_cursor().is_some() {
                    hints.push(HintItem::new(key, "expand"));
                } else if multiline_mode && prompt.can_send() {
                    hints.push(HintItem::new(newline_key, submit_label));
                } else if prompt.can_send() {
                    hints.push(HintItem::new(key, submit_label));
                } else if is_turn_running && has_queued_follow_up {
                    hints.push(HintItem::new(key, "send now"));
                }
            }
            if !multiline_mode && prompt.can_send() {
                hints.push(if prefer_alt_enter_newline {
                    HintItem::new(newline_key, "newline")
                } else {
                    HintItem::paired(newline_key, crate::key!(Enter, ALT), "newline")
                });
            }
            if prompt.file_ref_near_cursor() {
                hints.push(HintItem::new(crate::key!(':'), "lines"));
            }
            if prompt.prompt_suggestion_visible() {
                hints.push(
                    HintItem::paired(crate::key!(Tab), crate::key!(Right), "accept suggestion")
                        .pinned(),
                );
            }
            if surface == ViewSurface::Root {
                hints.push(HintItem::new(crate::key!(BackTab), "mode"));
            }
            for def in registry.hints(&[When::PromptFocused, When::AgentScreen, When::Always]) {
                if def.id == ActionId::SendPrompt
                    || def.id == ActionId::CommandPalette
                    || def.id == ActionId::Quit
                {
                    continue;
                }
                if def.id == ActionId::EnableVoiceMode || def.id == ActionId::VoiceToggle {
                    continue;
                }
                hints.push(def.hint());
            }
            hints
        }
        ActivePane::Tasks => {
            let mut hints = Vec::new();
            if selected_supports_fullscreen {
                hints.push(HintItem::new(crate::key!(Enter), "view"));
            }
            if selected_supports_copy {
                hints.push(HintItem::new(crate::key!('y'), "copy output"));
            }
            if selected_can_kill {
                hints.push(HintItem::new(crate::key!('x'), "kill"));
            }
            hints.push(HintItem::new(
                crate::key!('h'),
                if show_done { "hide done" } else { "show done" },
            ));
            hints
        }
        ActivePane::Catalog => vec![],
        ActivePane::Scrollback if scrollback_search.is_some() => {
            let mut hints = Vec::new();
            if vim_mode {
                if scrollback_search.is_some_and(|s| s.is_composing()) {
                    hints.push(HintItem::new(crate::key!(Enter), "go"));
                } else {
                    hints.push(HintItem::paired(
                        crate::key!('n'),
                        crate::key!('N'),
                        "next/prev",
                    ));
                }
            } else {
                use crate::input::key::KeyShortcut;
                use crossterm::event::KeyCode;
                hints.push(HintItem::paired(
                    KeyShortcut::key(KeyCode::Down),
                    KeyShortcut::key(KeyCode::Up),
                    "next/prev",
                ));
            }
            hints.push(HintItem::new(crate::key!(Esc), "cancel"));
            hints
        }
        ActivePane::Scrollback => {
            let mut hints = Vec::new();
            let focus_reachable = surface == ViewSurface::Root;
            if focus_reachable && focus_hint.pinned {
                hints.push(focus_hint.clone());
            }
            if surface == ViewSurface::ChildTakeover {
                hints.push(HintItem::paired(crate::key!('q'), crate::key!(Esc), "back").pinned());
            }
            let offer_focus_hint = |hints: &mut Vec<HintItem>| {
                if focus_reachable && !focus_hint.pinned {
                    hints.push(focus_hint.clone());
                }
            };
            let nothing_special = !selected_is_agent_message
                && !selected_is_user_prompt
                && fold_label.is_none()
                && group_header_label.is_none()
                && !selected_supports_fullscreen;
            if nothing_special {
                offer_focus_hint(&mut hints);
            }
            if selected_is_agent_message {
                if vim_mode
                    && selected_supports_copy
                    && let Some(key) = registry.key_for(ActionId::CopyBlockContent)
                {
                    hints.push(HintItem::new(key, "copy"));
                }
                offer_focus_hint(&mut hints);
            }
            if selected_is_user_prompt {
                let user_collapsed = fold_label == Some("expand");
                if user_collapsed {
                    let key = registry
                        .key_for_mode(ActionId::ToggleFold, vim_mode)
                        .or_else(|| registry.key_for_mode(ActionId::Expand, vim_mode));
                    if let Some(key) = key {
                        hints.push(HintItem::new(key, "expand"));
                    }
                }
                if let Some(key) = registry.key_for(ActionId::ExpandAllThinking) {
                    hints.push(HintItem::new(key, thinking_label));
                }
                if !user_collapsed {
                    offer_focus_hint(&mut hints);
                }
            }
            let user_collapsed_already_pushed =
                selected_is_user_prompt && fold_label == Some("expand");
            if let Some(label) = group_header_label {
                if let Some(key) = registry.key_for(ActionId::OpenBlockViewer) {
                    hints.push(HintItem::new(key, label));
                }
            } else if !user_collapsed_already_pushed && let Some(label) = fold_label {
                let directional = if label == "expand" {
                    ActionId::Expand
                } else {
                    ActionId::Collapse
                };
                let key = registry
                    .key_for_mode(ActionId::ToggleFold, vim_mode)
                    .or_else(|| registry.key_for_mode(directional, vim_mode));
                if let Some(key) = key {
                    hints.push(HintItem::new(key, label));
                }
            }
            if group_header_label.is_none()
                && selected_supports_fullscreen
                && let Some(key) = registry.key_for(ActionId::OpenBlockViewer)
            {
                hints.push(HintItem::new(key, "open"));
            }
            if vim_mode
                && let (Some(j), Some(k)) = (
                    registry.key_for(ActionId::SelectNext),
                    registry.key_for(ActionId::SelectPrev),
                )
            {
                hints.push(HintItem::paired(j, k, "nav").pinned());
            }
            if vim_mode
                && let (Some(h), Some(l)) = (
                    registry.key_for(ActionId::PrevTurn),
                    registry.key_for(ActionId::NextTurn),
                )
            {
                hints.push(HintItem::paired(l, h, "turn").pinned());
            }
            if !selected_is_user_prompt
                && let Some(key) = registry.key_for(ActionId::ExpandAllThinking)
            {
                hints.push(HintItem::new(key, thinking_label));
            }
            if vim_mode
                && let (Some(g), Some(bg)) = (
                    registry.key_for(ActionId::GotoTop),
                    registry.key_for(ActionId::GotoBottom),
                )
            {
                hints.push(HintItem::paired(g, bg, "top/btm"));
            }
            if vim_mode
                && !selected_is_agent_message
                && selected_supports_copy
                && let Some(key) = registry.key_for(ActionId::CopyBlockContent)
            {
                hints.push(HintItem::new(key, "copy"));
            }
            if vim_mode
                && let Some(label) = selected_meta_label
                && let Some(key) = registry.key_for(ActionId::CopyBlockMeta)
            {
                hints.push(HintItem::new(key, label));
            }
            if selected_can_kill {
                hints.push(HintItem::new(crate::key!('x'), "kill"));
            }
            hints
        }
    };
    if is_turn_running && let Some(def) = registry.find(ActionId::CancelTurn) {
        hints.push(def.hint());
    }
    let has_composer_payload = !prompt.text().trim().is_empty() || is_editing_queued;
    if matches!(active_pane, ActivePane::Prompt)
        && ActionRegistry::interjection_possible(is_turn_running, has_composer_payload)
        && let Some(def) = registry.find(ActionId::InterjectPrompt)
    {
        hints.push(def.hint());
    }
    if can_demote
        && surface == ViewSurface::Root
        && let Some(key) = registry.key_for(ActionId::SendToBackground)
    {
        hints.push(HintItem::new(key, "send to bg"));
    }
    hints
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionRegistry;
    /// Convenience: build hints for the Scrollback pane with sensible defaults.
    /// Override only what each test cares about.
    #[allow(clippy::too_many_arguments)]
    fn scrollback_hints(
        registry: &ActionRegistry,
        fold_label: Option<&'static str>,
        selected_supports_copy: bool,
        selected_supports_fullscreen: bool,
        selected_is_user_prompt: bool,
        selected_is_agent_message: bool,
    ) -> Vec<HintItem> {
        scrollback_hints_with_vim_mode(
            registry,
            fold_label,
            selected_supports_copy,
            selected_supports_fullscreen,
            selected_is_user_prompt,
            selected_is_agent_message,
            true,
        )
    }
    fn scrollback_hints_with_vim_mode(
        registry: &ActionRegistry,
        fold_label: Option<&'static str>,
        selected_supports_copy: bool,
        selected_supports_fullscreen: bool,
        selected_is_user_prompt: bool,
        selected_is_agent_message: bool,
        vim_mode: bool,
    ) -> Vec<HintItem> {
        build_hints(
            ActivePane::Scrollback,
            prompt_focus_hint(),
            &PromptWidget::default(),
            registry,
            false,
            fold_label,
            None,
            "prompt",
            "expand thinking",
            false,
            selected_supports_copy,
            None,
            selected_supports_fullscreen,
            false,
            false,
            false,
            vim_mode,
            ViewSurface::Root,
            false,
            false,
            QueueMutation::PerRowKind,
            selected_is_user_prompt,
            selected_is_agent_message,
            false,
            None,
        )
    }
    fn first_two_labels(hints: &[HintItem]) -> Vec<&str> {
        hints.iter().take(2).map(|h| h.label.as_ref()).collect()
    }
    #[test]
    fn demotion_hint_uses_registered_ctrl_b_binding() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Scrollback,
            prompt_focus_hint(),
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            true,
            ViewSurface::Root,
            false,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            None,
        );
        let hint = hints
            .iter()
            .find(|hint| hint.label == "send to bg")
            .expect("running Execute should advertise demotion");
        assert_eq!(hint.keys, vec![crate::key!('b', CONTROL)]);
    }
    fn queue_hint_labels(queue_mutation: QueueMutation, is_turn_running: bool) -> Vec<String> {
        let registry = ActionRegistry::defaults();
        build_hints(
            ActivePane::Queue,
            prompt_focus_hint(),
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            ViewSurface::Root,
            is_turn_running,
            true,
            queue_mutation,
            false,
            false,
            false,
            None,
        )
        .iter()
        .map(|h| h.label.to_string())
        .collect()
    }
    /// A read-only queue advertises nothing it would swallow: no mutation keys and no send-now chord, even mid-turn.
    #[test]
    fn queue_hints_read_only_show_copy_only() {
        assert_eq!(
            vec!["copy".to_owned()],
            queue_hint_labels(QueueMutation::ReadOnly, false)
        );
        assert_eq!(
            vec!["copy".to_owned(), "cancel".to_owned()],
            queue_hint_labels(QueueMutation::ReadOnly, true)
        );
        assert_eq!(
            vec![
                "delete row".to_owned(),
                "edit".to_owned(),
                "reorder".to_owned(),
                "copy".to_owned(),
                "send now".to_owned(),
                "cancel".to_owned(),
            ],
            queue_hint_labels(QueueMutation::PerRowKind, true)
        );
    }
    fn surface_hints(
        pane: ActivePane,
        surface: ViewSurface,
        prompt: &PromptWidget,
    ) -> Vec<HintItem> {
        build_hints_with_focus(pane, surface, prompt, prompt_focus_hint())
    }
    fn build_hints_with_focus(
        pane: ActivePane,
        surface: ViewSurface,
        prompt: &PromptWidget,
        focus_hint: HintItem,
    ) -> Vec<HintItem> {
        build_hints(
            pane,
            focus_hint,
            prompt,
            &ActionRegistry::defaults(),
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            false,
            surface,
            false,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            None,
        )
    }
    /// The child surface adds `q/Esc back` and drops the demote chip and the `BackTab mode` hint; the root keeps all three.
    #[test]
    fn build_hints_child_surface_adds_back_and_hides_demote_and_mode() {
        let has = |hints: &[HintItem], label: &str| hints.iter().any(|h| h.label == label);
        let mut prompt = PromptWidget::default();
        prompt.textarea.insert_str("draft");
        let root = surface_hints(ActivePane::Scrollback, ViewSurface::Root, &prompt);
        let child = surface_hints(ActivePane::Scrollback, ViewSurface::ChildTakeover, &prompt);
        assert!(!has(&root, "back") && has(&root, "send to bg"));
        assert!(has(&child, "back") && !has(&child, "send to bg"));
        assert!(has(&root, "prompt") && !has(&child, "prompt"));
        let pinned_focus = prompt_focus_hint().pinned();
        let child_pinned = build_hints_with_focus(
            ActivePane::Scrollback,
            ViewSurface::ChildTakeover,
            &prompt,
            pinned_focus.clone(),
        );
        let root_pinned = build_hints_with_focus(
            ActivePane::Scrollback,
            ViewSurface::Root,
            &prompt,
            pinned_focus,
        );
        assert!(has(&root_pinned, "prompt") && !has(&child_pinned, "prompt"));
        assert_eq!(Some("back"), child.first().map(|h| h.label.as_ref()));
        assert!(child.iter().any(|h| h.label == "back" && h.pinned));
        let root = surface_hints(ActivePane::Prompt, ViewSurface::Root, &prompt);
        let child = surface_hints(ActivePane::Prompt, ViewSurface::ChildTakeover, &prompt);
        assert!(has(&root, "mode") && !has(&child, "mode"));
        assert!(has(&child, "send"));
    }
    #[test]
    fn group_header_shows_enter_toggle_hint_instead_of_open_and_fold() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Scrollback,
            prompt_focus_hint(),
            &PromptWidget::default(),
            &registry,
            false,
            Some("expand"),
            Some("expand"),
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            true,
            false,
            false,
            false,
            true,
            ViewSurface::Root,
            false,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            None,
        );
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"open"),
            "no Enter:open hint on a group header, got {labels:?}"
        );
        let expand_hints: Vec<&HintItem> = hints.iter().filter(|h| h.label == "expand").collect();
        assert_eq!(
            expand_hints.len(),
            1,
            "exactly one expand hint (Enter toggle, no separate fold hint), got {labels:?}"
        );
        let enter_key = registry
            .key_for(crate::actions::ActionId::OpenBlockViewer)
            .expect("OpenBlockViewer has a default key");
        let [expand_hint] = expand_hints.as_slice() else {
            panic!("expected one expand hint: {expand_hints:?}");
        };
        assert_eq!(
            expand_hint.keys,
            vec![enter_key],
            "the header expand hint must be bound to Enter (OpenBlockViewer)"
        );
        assert_eq!(
            labels.first(),
            Some(&"expand"),
            "the Enter toggle takes the first compact slot, got {labels:?}"
        );
    }
    #[test]
    fn scrollback_user_prompt_collapsed_hoists_expand_then_thinking() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("expand"), false, false, true, false);
        assert_eq!(first_two_labels(&hints), vec!["expand", "expand thinking"]);
    }
    #[test]
    fn scrollback_user_prompt_expanded_hoists_thinking_then_space() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("collapse"), false, false, true, false);
        assert_eq!(first_two_labels(&hints), vec!["expand thinking", "prompt"]);
    }
    #[test]
    fn scrollback_agent_message_hoists_copy_then_space() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, true, false, false, true);
        assert_eq!(first_two_labels(&hints), vec!["copy", "prompt"]);
    }
    #[test]
    fn scrollback_agent_message_no_duplicate_y_copy_in_full_list() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, true, false, false, true);
        let copy_count = hints.iter().filter(|h| h.label == "copy").count();
        assert_eq!(copy_count, 1, "copy should appear exactly once");
    }
    #[test]
    fn scrollback_default_block_shows_prompt_first() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, false, false, false, false);
        assert_eq!(first_two_labels(&hints), vec!["prompt", "nav"]);
    }
    #[test]
    fn scrollback_foldable_non_user_block_hoists_fold_then_open() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("fold"), false, true, false, false);
        assert_eq!(first_two_labels(&hints), vec!["fold", "open"]);
    }
    #[test]
    fn scrollback_vim_mode_on_shows_nav_and_turn_hints() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, false, false, false, false, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"nav"),
            "vim mode should show j/k nav hint; got {labels:?}"
        );
        assert!(
            labels.contains(&"turn"),
            "vim mode should show Shift+l/h turn hint; got {labels:?}"
        );
        assert!(
            labels.contains(&"top/btm"),
            "vim mode should show g/G top/btm hint; got {labels:?}"
        );
    }
    #[test]
    fn scrollback_vim_mode_off_hides_nav_turn_and_topbtm() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, false, false, false, false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"nav"),
            "vim-off must hide j/k nav hint; got {labels:?}"
        );
        assert!(
            !labels.contains(&"turn"),
            "vim-off must hide Shift+l/h turn hint; got {labels:?}"
        );
        assert!(
            !labels.contains(&"top/btm"),
            "vim-off must hide g/G top/btm hint; got {labels:?}"
        );
    }
    #[test]
    fn scrollback_vim_mode_off_hides_y_copy_on_agent_message() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, true, false, false, true, false);
        assert!(
            !hints.iter().any(|h| h.label == "copy"),
            "vim-off must hide y:copy hint on agent message"
        );
    }
    #[test]
    fn scrollback_never_shows_rewind_hint_on_user_prompt() {
        let registry = ActionRegistry::defaults();
        for vim_mode in [true, false] {
            let hints = scrollback_hints_with_vim_mode(
                &registry,
                Some("expand"),
                false,
                false,
                true,
                false,
                vim_mode,
            );
            assert!(
                !hints.iter().any(|h| h.label == "rewind"),
                "rewind is slash-command only (vim_mode={vim_mode})"
            );
        }
    }
    /// Build scrollback hints with an open search session in the given phase.
    fn scrollback_search_hints(
        registry: &ActionRegistry,
        vim_mode: bool,
        composing: bool,
    ) -> Vec<HintItem> {
        let mut search = ScrollbackSearchState::open();
        if !composing {
            search.accept();
        }
        build_hints(
            ActivePane::Scrollback,
            prompt_focus_hint(),
            &PromptWidget::default(),
            registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            vim_mode,
            ViewSurface::Root,
            false,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            Some(&search),
        )
    }
    #[test]
    fn scrollback_search_hint_not_in_bottom_bar() {
        let registry = ActionRegistry::defaults();
        for vim_mode in [true, false] {
            let hints = scrollback_hints_with_vim_mode(
                &registry, None, false, false, false, false, vim_mode,
            );
            assert!(
                !hints.iter().any(|h| h.label == "search"),
                "bottom bar must not advertise / search (vim_mode={vim_mode})"
            );
        }
    }
    #[test]
    fn scrollback_search_composing_shows_go_and_cancel_only() {
        let registry = ActionRegistry::defaults();
        let labels: Vec<String> = scrollback_search_hints(&registry, true, true)
            .iter()
            .map(|h| h.label.to_string())
            .collect();
        assert!(labels.contains(&"go".to_string()), "got {labels:?}");
        assert!(labels.contains(&"cancel".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"next/prev".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"nav".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"search".to_string()), "got {labels:?}");
    }
    #[test]
    fn scrollback_search_browsing_shows_next_prev_and_cancel() {
        let registry = ActionRegistry::defaults();
        let labels: Vec<String> = scrollback_search_hints(&registry, true, false)
            .iter()
            .map(|h| h.label.to_string())
            .collect();
        assert!(labels.contains(&"next/prev".to_string()), "got {labels:?}");
        assert!(labels.contains(&"cancel".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"go".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"nav".to_string()), "got {labels:?}");
    }
    /// The keys behind the `next/prev` hint for a search in the given phase.
    fn next_prev_keys(
        registry: &ActionRegistry,
        vim_mode: bool,
        composing: bool,
    ) -> Vec<crate::input::key::KeyShortcut> {
        scrollback_search_hints(registry, vim_mode, composing)
            .into_iter()
            .find(|h| h.label == "next/prev")
            .map(|h| h.keys)
            .unwrap_or_default()
    }
    #[test]
    fn scrollback_search_vim_browsing_uses_n_keys() {
        use crossterm::event::KeyCode;
        let registry = ActionRegistry::defaults();
        let keys = next_prev_keys(&registry, true, false);
        let codes: Vec<KeyCode> = keys.iter().map(|k| k.code).collect();
        assert_eq!(codes, vec![KeyCode::Char('n'), KeyCode::Char('N')]);
    }
    #[test]
    fn scrollback_search_simple_mode_uses_arrow_keys_in_both_phases() {
        use crossterm::event::KeyCode;
        let registry = ActionRegistry::defaults();
        for composing in [true, false] {
            let codes: Vec<KeyCode> = next_prev_keys(&registry, false, composing)
                .iter()
                .map(|k| k.code)
                .collect();
            assert_eq!(
                codes,
                vec![KeyCode::Down, KeyCode::Up],
                "simple mode next/prev should be arrows (composing={composing})"
            );
        }
    }
    #[test]
    fn prompt_branch_excludes_exit_session_home_hint() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Prompt,
            prompt_focus_hint(),
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            true,
            ViewSurface::Root,
            false,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            None,
        );
        assert!(
            !hints.iter().any(|h| h.label == "home"),
            "ExitSession (home) must not appear in prompt-focused bar"
        );
    }
    fn prompt_hints_with_text(
        multiline_mode: bool,
        prefer_alt_enter_newline: bool,
    ) -> Vec<HintItem> {
        prompt_hints_with_text_and_turn("hello", multiline_mode, prefer_alt_enter_newline, false)
    }
    fn prompt_hints_with_text_and_turn(
        text: &str,
        multiline_mode: bool,
        prefer_alt_enter_newline: bool,
        is_turn_running: bool,
    ) -> Vec<HintItem> {
        let mut prompt = PromptWidget::default();
        prompt.textarea.insert_str(text);
        let registry = ActionRegistry::defaults();
        build_hints(
            ActivePane::Prompt,
            prompt_focus_hint(),
            &prompt,
            &registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            multiline_mode,
            true,
            ViewSurface::Root,
            is_turn_running,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            prefer_alt_enter_newline,
            None,
        )
    }
    #[test]
    fn prompt_idle_submit_hint_is_send() {
        let hints = prompt_hints_with_text_and_turn("hello", false, false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"send") && !labels.contains(&"queue"),
            "idle prompt must advertise Enter:send; got {labels:?}"
        );
    }
    #[test]
    fn prompt_running_submit_hint_is_queue_and_send_now() {
        let hints = prompt_hints_with_text_and_turn("hello", false, false, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"queue"),
            "mid-turn follow-up must advertise Enter:queue (not send); got {labels:?}"
        );
        assert!(
            !labels.contains(&"send"),
            "mid-turn must not mislabel Enter as send; got {labels:?}"
        );
        assert!(
            labels.contains(&"send now"),
            "mid-turn with composer text must advertise the send-now (interject) chord; got {labels:?}"
        );
    }
    /// Empty composer with a mid-turn queue: bare Enter is send-now in both normal and multiline modes.
    /// Multiline only inserts a newline when there is text.
    #[test]
    fn prompt_empty_mid_turn_queue_advertises_send_now_including_multiline() {
        for multiline in [false, true] {
            let prompt = PromptWidget::default();
            let registry = ActionRegistry::defaults();
            let hints = build_hints(
                ActivePane::Prompt,
                prompt_focus_hint(),
                &prompt,
                &registry,
                false,
                None,
                None,
                "prompt",
                "expand thinking",
                false,
                false,
                None,
                false,
                false,
                false,
                multiline,
                true,
                ViewSurface::Root,
                true,
                true,
                QueueMutation::PerRowKind,
                false,
                false,
                false,
                None,
            );
            let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
            assert!(
                labels.contains(&"send now"),
                "empty composer mid-turn with queue must advertise Enter:send now \
                 (multiline={multiline}); got {labels:?}"
            );
        }
    }
    /// The running-turn cancel hint always names the registry Ctrl+C binding: Esc never cancels a turn (it only hints at this key), in every mode and pane.
    #[test]
    fn running_turn_cancel_hint_is_always_ctrl_c() {
        let prompt = PromptWidget::default();
        let registry = ActionRegistry::defaults();
        for (vim_mode, pane) in [
            (false, ActivePane::Prompt),
            (true, ActivePane::Prompt),
            (false, ActivePane::Scrollback),
        ] {
            let hints = build_hints(
                pane,
                prompt_focus_hint(),
                &prompt,
                &registry,
                false,
                None,
                None,
                "prompt",
                "expand thinking",
                false,
                false,
                None,
                false,
                false,
                false,
                false,
                vim_mode,
                ViewSurface::Root,
                true,
                false,
                QueueMutation::PerRowKind,
                false,
                false,
                false,
                None,
            );
            let cancel = hints
                .iter()
                .find(|h| h.label == "cancel")
                .expect("running turn must surface the cancel hint");
            assert_eq!(
                vec![crate::key!('c', CONTROL)],
                cancel.keys,
                "cancel hint key for vim_mode={vim_mode} pane={pane:?}"
            );
            assert!(
                !hints
                    .iter()
                    .any(|h| h.label == "cancel" && h.keys == vec![crate::key!(Esc)]),
                "no hint may advertise Esc as the turn cancel (vim_mode={vim_mode} pane={pane:?})"
            );
        }
    }
    /// Running turn with an open scrollback search: the search's own `Esc cancel` hint stays the only Esc hint.
    /// The CancelTurn hint keeps Ctrl+C.
    /// The bar therefore never shows two different `Esc cancel` meanings at once.
    #[test]
    fn running_turn_with_scrollback_search_keeps_ctrl_c_cancel_hint() {
        let registry = ActionRegistry::defaults();
        let search = ScrollbackSearchState::open();
        let hints = build_hints(
            ActivePane::Scrollback,
            prompt_focus_hint(),
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            ViewSurface::Root,
            true,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            Some(&search),
        );
        let esc_cancels: Vec<&HintItem> = hints
            .iter()
            .filter(|h| h.label == "cancel" && h.keys == vec![crate::key!(Esc)])
            .collect();
        assert_eq!(
            esc_cancels.len(),
            1,
            "exactly one Esc:cancel hint (the search's own dismiss)"
        );
        assert!(
            hints
                .iter()
                .any(|h| h.label == "cancel" && h.keys == vec![crate::key!('c', CONTROL)]),
            "CancelTurn hint must stay on Ctrl+C while the search owns Esc"
        );
    }
    /// Running turn while editing a queued prompt: the edit's own `Esc cancel` (discard) hint is the only Esc-keyed row.
    /// The CancelTurn hint keeps Ctrl+C.
    /// The bar therefore never shows two contradictory `Esc cancel` rows.
    #[test]
    fn running_turn_editing_queued_keeps_ctrl_c_cancel_hint() {
        let registry = ActionRegistry::defaults();
        let mut prompt = PromptWidget::default();
        prompt.textarea.insert_str("edited row");
        let hints = build_hints(
            ActivePane::Prompt,
            prompt_focus_hint(),
            &prompt,
            &registry,
            true,
            None,
            None,
            "prompt",
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            ViewSurface::Root,
            true,
            false,
            QueueMutation::PerRowKind,
            false,
            false,
            false,
            None,
        );
        let esc_rows: Vec<&HintItem> = hints
            .iter()
            .filter(|h| h.keys.contains(&crate::key!(Esc)))
            .collect();
        assert_eq!(
            esc_rows.len(),
            1,
            "exactly one Esc-keyed hint (the edit's discard), got {:?}",
            hints.iter().map(|h| h.label.as_ref()).collect::<Vec<_>>()
        );
        let [esc_row] = esc_rows.as_slice() else {
            panic!("expected one Esc-keyed hint: {esc_rows:?}");
        };
        assert_eq!(esc_row.label, "cancel");
        assert!(
            hints
                .iter()
                .any(|h| h.label == "cancel" && h.keys == vec![crate::key!('c', CONTROL)]),
            "CancelTurn hint must stay on Ctrl+C while the edit owns Esc"
        );
    }
    #[test]
    fn prompt_legacy_vte_adds_alt_enter_newline_hint() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(false, true);
        let newline = hints
            .iter()
            .find(|h| h.label == "newline")
            .expect("legacy VTE must surface an explicit Alt+Enter newline hint");
        let [key] = newline.keys.as_slice() else {
            panic!(
                "prefer-Alt newline must be a single key, got {:?}",
                newline.keys
            );
        };
        assert!(
            key.modifiers.contains(KeyModifiers::ALT)
                && !key.modifiers.contains(KeyModifiers::SHIFT),
            "legacy VTE newline must be Alt+Enter, got {:?}",
            key.modifiers
        );
    }
    #[test]
    fn prompt_modern_terminal_pairs_shift_and_alt_newline() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(false, false);
        let newline = hints
            .iter()
            .find(|h| h.label == "newline")
            .expect("sendable draft must surface a newline hint");
        let [shift, alt] = newline.keys.as_slice() else {
            panic!(
                "local modern newline must pair Shift+Enter / Alt+Enter, got {:?}",
                newline.keys
            );
        };
        assert!(
            shift.modifiers.contains(KeyModifiers::SHIFT),
            "first chord must be Shift+Enter, got {:?}",
            shift.modifiers
        );
        assert!(
            alt.modifiers.contains(KeyModifiers::ALT),
            "second chord must be Alt+Enter, got {:?}",
            alt.modifiers
        );
    }
    #[test]
    fn prompt_empty_draft_no_newline_hint() {
        let hints = prompt_hints_with_text_and_turn("", false, false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"newline"),
            "empty composer must not show newline; got {labels:?}"
        );
    }
    #[test]
    fn prompt_btw_draft_shows_newline_hint() {
        let hints = prompt_hints_with_text_and_turn("/btw what about retries", false, true, false);
        assert!(
            hints.iter().any(|h| h.label == "newline"),
            "/btw draft must show newline; got {:?}",
            hints.iter().map(|h| h.label.as_ref()).collect::<Vec<_>>()
        );
    }
    #[test]
    fn prompt_multiline_mode_no_extra_newline_hint() {
        let hints = prompt_hints_with_text(true, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"newline"),
            "multiline mode must not show the extra newline hint \
             (Enter already inserts a newline); got {labels:?}"
        );
    }
    #[test]
    fn prompt_newline_footer_renders_alt_enter_when_prefer_alt() {
        use crate::views::shortcuts_bar::ShortcutsBar;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        let hints = prompt_hints_with_text(false, true);
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        ShortcutsBar::new(&hints)
            .compact(5, None)
            .render(area, &mut buf);
        let text: String = (0..80)
            .map(|x| buf.cell((x, 0)).expect("cell").symbol().to_owned())
            .collect::<String>()
            .trim_end()
            .to_owned();
        let alt = if cfg!(target_os = "macos") {
            "Opt+Enter"
        } else {
            "Alt+Enter"
        };
        assert!(
            text.contains("Enter:send") && text.contains(&format!("{alt}:newline")),
            "SSH/prefer-Alt footer must show Enter send and {alt} newline, got {text:?}"
        );
        assert!(
            !text.contains("Shift+Enter:newline"),
            "prefer-Alt footer must not advertise Shift+Enter as newline, got {text:?}"
        );
    }
    #[test]
    fn prompt_multiline_send_hint_uses_alt_on_legacy_vte() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(true, true);
        let send_hint = hints
            .iter()
            .find(|h| h.label == "send")
            .expect("multiline mode with text must show a send hint");
        let key = send_hint
            .keys
            .first()
            .expect("send hint must have at least one key");
        assert!(
            key.modifiers.contains(KeyModifiers::ALT),
            "legacy VTE multiline send hint must advertise Alt+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
        assert!(
            !key.modifiers.contains(KeyModifiers::SHIFT),
            "legacy VTE multiline send hint must NOT advertise Shift+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
    }
    #[test]
    fn prompt_multiline_send_hint_uses_shift_on_modern_terminal() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(true, false);
        let send_hint = hints
            .iter()
            .find(|h| h.label == "send")
            .expect("multiline mode with text must show a send hint");
        let key = send_hint
            .keys
            .first()
            .expect("send hint must have at least one key");
        assert!(
            key.modifiers.contains(KeyModifiers::SHIFT),
            "modern terminal multiline send hint must advertise Shift+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
    }
    fn base_params(area: Rect) -> AgentViewLayoutParams {
        AgentViewLayoutParams {
            area,
            prompt_height: 2,
            shortcuts_height: 1,
            ..Default::default()
        }
    }
    fn layout_with_rows(
        area: Rect,
        banner_height: u16,
        cta_height: u16,
        follow_ups_height: u16,
    ) -> AgentViewLayout {
        AgentViewLayout::compute(AgentViewLayoutParams {
            banner_height,
            cta_height,
            follow_ups_height,
            ..base_params(area)
        })
    }
    fn layout_with_cta(area: Rect, cta_height: u16) -> AgentViewLayout {
        layout_with_rows(area, 0, cta_height, 0)
    }
    fn layout_with_status_line(
        area: Rect,
        prompt_height: u16,
        status_line_height: u16,
    ) -> AgentViewLayout {
        AgentViewLayout::compute(AgentViewLayoutParams {
            prompt_height,
            status_line_height,
            ..base_params(area)
        })
    }
    #[test]
    fn status_line_row_sits_between_prompt_and_shortcuts_bar() {
        let layout = layout_with_status_line(Rect::new(0, 0, 80, 40), 2, 2);
        assert_eq!(
            layout.status_line.height, 2,
            "a 2-row status row has room at height 40"
        );
        assert_eq!(
            layout.status_line.y,
            layout.prompt.bottom(),
            "the row starts where the prompt ends, got {:?} under prompt {:?}",
            layout.status_line,
            layout.prompt,
        );
        assert_eq!(
            layout.status_line.bottom(),
            layout.shortcuts.y,
            "the shortcuts bar starts where the row ends, got {:?} under row {:?}",
            layout.shortcuts,
            layout.status_line,
        );
    }
    #[test]
    fn status_line_row_yields_to_a_prompt_at_its_cap() {
        let area = Rect::new(0, 0, 80, 20);
        let layout = layout_with_status_line(area, area.height / 2, 3);
        assert_eq!(
            layout.status_line.height, 0,
            "no rows are left over for the status row, got {:?}",
            layout.status_line,
        );
        assert!(
            layout.scrollback.height >= 5,
            "the scrollback keeps its Min(5), got {:?}",
            layout.scrollback,
        );
        assert!(
            layout.shortcuts.height == 1 && layout.shortcuts.bottom() <= area.bottom(),
            "the shortcuts bar keeps its row on screen, got {:?}",
            layout.shortcuts,
        );
    }
    #[test]
    fn the_status_row_takes_the_one_row_left_over() {
        let area = Rect::new(0, 0, 80, 21);
        let layout = layout_with_status_line(area, area.height / 2, 1);
        assert_eq!(
            layout.status_line.height, 1,
            "the one row left over goes to the status row, got {:?}",
            layout.status_line,
        );
        assert_eq!(
            layout.status_line.bottom(),
            layout.shortcuts.y,
            "the shortcuts bar starts where the row ends, got {:?} under row {:?}",
            layout.shortcuts,
            layout.status_line,
        );
        assert!(
            layout.shortcuts.height == 1 && layout.shortcuts.bottom() < area.bottom(),
            "the shortcuts bar keeps its row inside the padded inner area, got {:?}",
            layout.shortcuts,
        );
        assert!(
            layout.scrollback.height >= SCROLLBACK_MIN_ROWS,
            "the row does not come out of the scrollback minimum, got {:?}",
            layout.scrollback,
        );
    }
    #[test]
    fn prompt_budget_counts_the_rows_above_the_prompt() {
        let area = Rect::new(0, 0, 80, 25);
        let plain = base_params(area);
        assert_eq!(
            AgentViewLayout::rows_available_for_prompt(plain),
            25 - 11,
            "a frame with no optional row gives everything else to the prompt"
        );
        let with_rows = AgentViewLayoutParams {
            banner_height: 1,
            turn_status_height: 1,
            ..plain
        };
        assert_eq!(
            AgentViewLayout::rows_available_for_prompt(with_rows),
            25 - 11 - 4,
            "each row above the prompt takes its own height plus the gap above it"
        );
    }
    #[test]
    fn prompt_budget_excludes_the_prompts_own_requested_height() {
        let params = AgentViewLayoutParams {
            todo_height: 3,
            queue_height: 2,
            turn_status_height: 1,
            banner_height: 1,
            prompt_gap: 1,
            voice_recording_height: 1,
            status_line_height: 2,
            ..base_params(Rect::new(0, 0, 80, 40))
        };
        let budget = AgentViewLayout::rows_available_for_prompt(params);
        assert_eq!(
            AgentViewLayout::rows_available_for_prompt(AgentViewLayoutParams {
                prompt_height: 99,
                ..params
            }),
            budget,
            "the requested prompt height is not part of its own budget"
        );
        let at_budget = AgentViewLayout::compute(AgentViewLayoutParams {
            prompt_height: budget,
            ..params
        });
        assert_eq!(at_budget.prompt.height, budget);
        assert_eq!(at_budget.todo.height, 3);
        assert_eq!(at_budget.queue.height, 2);
        assert_eq!(at_budget.turn_status.height, 1);
        assert_eq!(at_budget.banner.height, 1);
        assert_eq!(at_budget.voice_recording.height, 1);
        assert_eq!(
            at_budget.status_line.height, 2,
            "a prompt at its budget leaves the status row whole, got {:?}",
            at_budget.status_line,
        );
        assert_eq!(at_budget.shortcuts.height, 1);
        assert_eq!(
            at_budget.scrollback.height, SCROLLBACK_MIN_ROWS,
            "the budget is the surplus over the scrollback minimum, got {:?}",
            at_budget.scrollback,
        );
        let over_budget = AgentViewLayout::compute(AgentViewLayoutParams {
            prompt_height: budget + 1,
            ..params
        });
        assert_eq!(
            over_budget.status_line.height, 1,
            "the row past the budget comes out of the status row, got {:?}",
            over_budget.status_line,
        );
        assert_eq!(over_budget.scrollback.height, SCROLLBACK_MIN_ROWS);
    }
    fn layout_with_rail(
        area: Rect,
        timeline_width: u16,
        scrollbar_cfg: &ScrollbarConfig,
    ) -> AgentViewLayout {
        AgentViewLayout::compute(AgentViewLayoutParams {
            timeline_width,
            scrollbar_cfg: *scrollbar_cfg,
            ..base_params(area)
        })
    }
    #[test]
    fn timeline_rail_replaces_the_scrollbar_column() {
        let area = Rect::new(0, 0, 80, 40);
        let scrollbar_cfg = ScrollbarConfig::default();
        let without = layout_with_rail(area, 0, &scrollbar_cfg);
        let with_rail = layout_with_rail(area, 3, &scrollbar_cfg);
        assert_eq!(with_rail.timeline_width, 3);
        assert_eq!(with_rail.scrollbar_x, without.scrollbar_x);
        assert_eq!(with_rail.timeline_x, with_rail.scrollbar_x + 1 - 3);
        assert!(
            with_rail.scrollback_content.x + with_rail.scrollback_content.width
                <= with_rail.timeline_x,
            "content (ends {}) must not overlap the rail (starts {})",
            with_rail.scrollback_content.x + with_rail.scrollback_content.width,
            with_rail.timeline_x,
        );
        assert!(
            with_rail.scrollback_content.width <= without.scrollback_content.width,
            "the rail never widens the content"
        );
        let no_scrollbar = ScrollbarConfig {
            enabled: false,
            ..ScrollbarConfig::default()
        };
        let forced_off = layout_with_rail(area, 3, &no_scrollbar);
        assert_eq!(forced_off.timeline_width, 0);
        assert_eq!(
            forced_off.scrollback_content.width, forced_off.scrollback.width,
            "no carve-out without the scrollbar gutter"
        );
    }
    #[test]
    fn plugin_cta_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_cta(area, 1);
        assert_eq!(layout.plugin_cta.height, 1);
        assert!(layout.plugin_cta.y < layout.prompt.y);
        assert!(layout.plugin_cta.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn plugin_cta_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_cta(area, 0);
        assert_eq!(layout.plugin_cta, Rect::default());
    }
    #[test]
    fn plugin_cta_row_suppressed_on_short_terminal() {
        let area = Rect::new(0, 0, 80, 16);
        let layout = layout_with_cta(area, 1);
        assert_eq!(layout.plugin_cta, Rect::default());
        assert!(layout.scrollback.height >= 5);
    }
    /// Banner row (mode banner / ephemeral tip slot): height 1 reserves a one-row rect directly above the prompt (gap row in between).
    #[test]
    fn banner_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 1, 0, 0);
        assert_eq!(layout.banner.height, 1);
        assert_eq!(layout.prompt.y, layout.banner.y + 1);
        assert!(layout.banner.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn banner_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 0);
        assert_eq!(layout.banner, Rect::default());
    }
    fn row_text(buf: &Buffer, area: Rect) -> String {
        (0..area.width)
            .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
            .collect()
    }
    #[test]
    fn render_follow_ups_emits_clickable_chips() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["Yes".to_string(), "No".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, None);
        assert_eq!(rects.len(), 2, "both chips fit on a wide row");
        let row = row_text(&buf, area);
        assert!(row.contains("Yes"), "row = {row:?}");
        assert!(row.contains("No"), "row = {row:?}");
        let [r0, r1] = rects.as_slice() else {
            panic!("expected two chip rects: {rects:?}");
        };
        assert!(r0.x + r0.width <= r1.x);
        assert!(r1.x + r1.width <= area.width);
    }
    #[test]
    fn render_follow_ups_drops_chips_that_do_not_fit() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["abc".to_string(), "way too long to fit here".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, None);
        assert_eq!(rects.len(), 1, "only the first chip fits");
    }
    #[test]
    fn render_follow_ups_zero_area_is_noop() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let theme = Theme::current();
        let rects = render_follow_ups(
            Rect::new(0, 0, 0, 0),
            &mut buf,
            &theme,
            &["x".to_string()],
            None,
        );
        assert!(rects.is_empty());
    }
    #[test]
    fn render_follow_ups_clamps_long_label() {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let long = "a".repeat(200);
        let rects = render_follow_ups(area, &mut buf, &theme, &[long], None);
        let [rect] = rects.as_slice() else {
            panic!("expected one chip rect: {rects:?}");
        };
        assert!(rect.width <= 52, "chip width {} not clamped", rect.width);
    }
    #[test]
    fn render_follow_ups_applies_hover_style() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["Yes".to_string(), "No".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, Some(0));
        let [r0, r1] = rects.as_slice() else {
            panic!("expected two chip rects: {rects:?}");
        };
        let cell = buf.cell((r0.x + 1, r0.y)).expect("hovered chip cell");
        assert_eq!(
            cell.fg, theme.text_primary,
            "hovered chip should use primary text"
        );
        assert_eq!(
            cell.bg, theme.bg_hover,
            "hovered chip should use theme.bg_hover"
        );
        let cell1 = buf.cell((r1.x + 1, r1.y)).expect("non-hovered chip cell");
        assert_eq!(cell1.fg, theme.link_fg, "non-hovered chip keeps link color");
    }
    #[test]
    fn follow_ups_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 1);
        assert_eq!(layout.follow_ups.height, 1);
        assert!(layout.follow_ups.y < layout.prompt.y);
        assert!(layout.follow_ups.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn follow_ups_row_below_plugin_cta() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 1, 1);
        assert!(layout.plugin_cta.height == 1 && layout.follow_ups.height == 1);
        assert!(layout.plugin_cta.y < layout.follow_ups.y);
        assert!(layout.follow_ups.y < layout.prompt.y);
    }
    #[test]
    fn follow_ups_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 0);
        assert_eq!(layout.follow_ups, Rect::default());
    }
    #[test]
    fn follow_ups_row_suppressed_on_short_terminal() {
        let area = Rect::new(0, 0, 80, 16);
        let layout = layout_with_rows(area, 0, 0, 1);
        assert_eq!(layout.follow_ups, Rect::default());
        assert!(layout.scrollback.height >= 5);
    }
}
