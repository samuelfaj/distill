// Modified for Distill by Samuel Fajreldines, 2026.
//! Consolidated dock above the prompt: one header per non-empty section.
//! Experimental, gated by remote `dock_enabled`.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::render::line_utils::truncate_line;
use crate::theme::Theme;
use crate::views::turn_status::SPINNER_DIVISOR;

mod layout;

pub use layout::{DockLayout, MaxRows, SectionSlots, desired_height, is_show_all_needed};

/// Rows the dock takes at rest. A section opened with `show N more` lifts this
/// (see [`DockCounts::max_rows`]) so its rows are all reachable.
pub const MAX_DOCK_ROWS: u16 = 8;

const HEADER_INDENT: &str = " ";
/// Same gutter as the header chevron, so a one-item section is not nested.
const ROW_INDENT: &str = " ";
const MORE_INDENT: &str = "   ";
/// Lines the queue body's `#N` markers up with the column its header's title
/// starts in. The header spends three columns on its chevron and the queue pane
/// already insets its own content by two, so the dock adds the last one.
const QUEUE_BODY_INDENT: u16 = 1;
const STOP_LABEL: &str = "[stop]";

/// Seeded at startup and on `x.ai/settings/update` from Feature::Dock.
static ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// Per-thread override so a test can turn the dock on without racing
    /// parallel draw-based tests through the process-global flag.
    static ENABLED_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = ENABLED_OVERRIDE.with(std::cell::Cell::get) {
        return on;
    }
    ENABLED.load(Ordering::Acquire)
}

/// Test-only, thread-scoped [`enabled`] override; the thread ends with the test.
#[cfg(test)]
pub fn set_enabled_for_test(on: bool) {
    ENABLED_OVERRIDE.with(|cell| cell.set(Some(on)));
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
}

pub struct DockRow {
    pub kind: String,
    pub description: String,
    pub activity: Option<String>,
    /// Right-aligned meta column, e.g. `grok-4.5 2m14s` or `every 5m (next in 2m)`.
    pub meta: String,
    pub killable: bool,
    /// Loops only paint `[↗]` when a linked child still exists to open.
    pub openable: bool,
    /// Active work (subagents, background commands, monitors) animates the
    /// leading dot spinner; scheduled loops keep a static diamond.
    pub spinning: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Workflows,
    Subagents,
    Tasks,
    Watchers,
    Queued,
}

impl Section {
    /// Slot in the per-section values of [`SectionSlots`]; `Queued` has none.
    pub(crate) fn slot(self) -> Option<usize> {
        match self {
            Section::Workflows => Some(0),
            Section::Subagents => Some(1),
            Section::Tasks => Some(2),
            Section::Watchers => Some(3),
            Section::Queued => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Section::Workflows => "Workflows",
            Section::Subagents => "Subagents",
            Section::Tasks => "Tasks",
            Section::Watchers => "Watchers",
            Section::Queued => "Queued",
        }
    }

    /// Every killable dock row paints `[stop]`, including subagents.
    pub fn kill_label(self) -> &'static str {
        match self {
            Section::Workflows
            | Section::Subagents
            | Section::Tasks
            | Section::Watchers
            | Section::Queued => STOP_LABEL,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockItem {
    Header(Section),
    Row(Section, usize),
    RevealRemaining(Section),
}

/// Painted kill-control geometry for one frame. Click handling snapshots this
/// rect with the row's kill identity; a later click is ignored unless the
/// cell still resolves to that same identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockStopHit {
    pub rect: Rect,
    pub item: DockItem,
}

/// Paint, cursor, and hit-test all resolve rows from this through
/// [`DockLayout`], so they cannot drift apart.
#[derive(Default, Clone, Copy)]
pub struct DockCounts {
    pub workflows: usize,
    pub subagents: usize,
    pub tasks: usize,
    pub watchers: usize,
    pub queued: usize,
    pub workflows_expanded: bool,
    pub subagents_expanded: bool,
    pub tasks_expanded: bool,
    pub watchers_expanded: bool,
    pub workflows_show_all: bool,
    pub subagents_show_all: bool,
    pub tasks_show_all: bool,
    pub watchers_show_all: bool,
    pub queue_body_rows: u16,
    /// First row each section paints. Sections scroll inside their own band, so
    /// a scroll never moves a header.
    pub offsets: SectionSlots<usize>,
    /// Rows the dock may take. [`MAX_DOCK_ROWS`] at rest; the caller raises it
    /// for a section the user opened, bounded by the space around the dock.
    pub max_rows: MaxRows,
}

#[derive(Default)]
pub struct DockData {
    pub workflows: Vec<DockRow>,
    pub subagents: Vec<DockRow>,
    pub tasks: Vec<DockRow>,
    pub watchers: Vec<DockRow>,
    pub queued: usize,
    pub workflows_expanded: bool,
    pub subagents_expanded: bool,
    pub tasks_expanded: bool,
    pub watchers_expanded: bool,
    pub workflows_show_all: bool,
    pub subagents_show_all: bool,
    pub tasks_show_all: bool,
    pub watchers_show_all: bool,
    pub focused: bool,
    pub cursor: usize,
    /// Reserved for the caller's queue pane; the dock widget does not paint it.
    pub queue_body_rows: u16,
    /// See [`DockCounts::offsets`].
    pub offsets: SectionSlots<usize>,
    /// See [`DockCounts::max_rows`].
    pub max_rows: MaxRows,
    pub hovered: Option<DockItem>,
    /// True when the pointer sits on the action row's kill control. The
    /// `[stop]`/`[x]` label then paints red on direct hover only and stays gray
    /// at rest, matching the Tasks pane.
    pub stop_hovered: bool,
    /// Drives the leading dot-spinner frame on active rows. Sourced from the
    /// Tasks-pane animation tick so the dock animates in lockstep with it.
    pub spinner_tick: u64,
}

impl DockData {
    pub fn counts(&self) -> DockCounts {
        DockCounts {
            workflows: self.workflows.len(),
            subagents: self.subagents.len(),
            tasks: self.tasks.len(),
            watchers: self.watchers.len(),
            queued: self.queued,
            workflows_expanded: self.workflows_expanded,
            subagents_expanded: self.subagents_expanded,
            tasks_expanded: self.tasks_expanded,
            watchers_expanded: self.watchers_expanded,
            workflows_show_all: self.workflows_show_all,
            subagents_show_all: self.subagents_show_all,
            tasks_show_all: self.tasks_show_all,
            watchers_show_all: self.watchers_show_all,
            queue_body_rows: self.queue_body_rows,
            offsets: self.offsets,
            max_rows: self.max_rows,
        }
    }

    fn rows(&self, section: Section) -> &[DockRow] {
        match section {
            Section::Workflows => &self.workflows,
            Section::Subagents => &self.subagents,
            Section::Tasks => &self.tasks,
            Section::Watchers => &self.watchers,
            Section::Queued => &[],
        }
    }
}

pub fn items(counts: &DockCounts) -> Vec<DockItem> {
    DockLayout::new(counts).rows().to_vec()
}

pub fn visible_items(data: &DockData) -> Vec<DockItem> {
    items(&data.counts())
}

/// Clips with the dock so headers never hit-test as `Queue`.
pub fn queue_body_rect(area: Rect, data: &DockData) -> Rect {
    let layout = DockLayout::with_cap(&data.counts(), area.height as usize);
    let rows = layout.rows().len() as u16;
    let height = layout
        .queue_body_rows()
        .min(area.height.saturating_sub(rows));
    if data.queued == 0 || height == 0 {
        return Rect::default();
    }
    // The queue pane paints its own `#N` markers; line them up with the header
    // title above them so the dock reads as one list.
    let indent = QUEUE_BODY_INDENT.min(area.width);
    Rect {
        x: area.x + indent,
        y: area.y + rows,
        width: area.width - indent,
        height,
    }
}

pub fn render(buf: &mut Buffer, area: Rect, theme: &Theme, data: &DockData) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let counts = data.counts();
    let layout = DockLayout::with_cap(&counts, area.height as usize);
    let bottom = area.bottom();
    let mut y = area.y;
    let action_item = action_item(data, layout.rows());
    let selected_at = |idx: usize| data.focused && idx == data.cursor;
    let highlight = |buf: &mut Buffer, y: u16, selected: bool, hovered: bool| {
        if selected {
            highlight_row(buf, area, y, theme.bg_highlight);
        } else if hovered {
            highlight_row(buf, area, y, theme.row_hover_bg());
        }
    };

    for (item_index, item) in layout.rows().iter().copied().enumerate() {
        if y >= bottom {
            return;
        }
        match item {
            DockItem::Header(section) => {
                let (count, expanded) = match section {
                    Section::Workflows => (counts.workflows, counts.workflows_expanded),
                    Section::Subagents => (counts.subagents, counts.subagents_expanded),
                    Section::Tasks => (counts.tasks, counts.tasks_expanded),
                    Section::Watchers => (counts.watchers, counts.watchers_expanded),
                    Section::Queued => (counts.queued, data.queue_body_rows > 0),
                };
                let line = section_header(theme, area.width, expanded, section.label(), count);
                buf.set_line(area.x, y, &line, area.width);
                highlight(
                    buf,
                    y,
                    selected_at(item_index),
                    data.hovered == Some(DockItem::Header(section)),
                );
            }
            DockItem::Row(section, i) => {
                let selected = selected_at(item_index);
                let hovered = data.hovered == Some(DockItem::Row(section, i));
                let show_actions = action_item == Some(DockItem::Row(section, i));
                let kill_hovered = show_actions && data.stop_hovered;
                let Some(row) = data.rows(section).get(i) else {
                    continue;
                };
                paint_row(
                    buf,
                    area,
                    y,
                    theme,
                    row,
                    show_actions,
                    kill_hovered,
                    data.spinner_tick,
                    section,
                );
                highlight(buf, y, selected, hovered);
            }
            DockItem::RevealRemaining(section) => {
                let selected = selected_at(item_index);
                // Rows the section holds but is not showing. Scrolling the band
                // changes which rows those are, never how many, so the count
                // stays put while the user moves through the section.
                let hidden = layout.hidden_rows(section);
                let arrow = crate::glyphs::disclosure_open();
                let indent_len = MORE_INDENT.len().min(area.width.saturating_sub(1) as usize);
                let line = Line::from(Span::styled(
                    format!(
                        "{}{arrow} show {hidden} more",
                        MORE_INDENT.get(..indent_len).unwrap_or(MORE_INDENT)
                    ),
                    Style::default().fg(theme.gray),
                ));
                buf.set_line(area.x, y, &line, area.width);
                highlight(
                    buf,
                    y,
                    selected,
                    data.hovered == Some(DockItem::RevealRemaining(section)),
                );
            }
        }
        y += 1;
    }
}

fn highlight_row(buf: &mut Buffer, area: Rect, y: u16, bg: ratatui::style::Color) {
    for x in area.x..area.x + area.width {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_bg(bg);
        }
    }
}

fn header_chevron(expanded: bool) -> String {
    let ch = if expanded {
        crate::glyphs::disclosure_open()
    } else {
        crate::glyphs::disclosure_closed()
    };
    format!("{ch} ")
}

fn section_header(
    theme: &Theme,
    width: u16,
    expanded: bool,
    label: &str,
    count: usize,
) -> Line<'static> {
    let indent = if width > 1 { HEADER_INDENT } else { "" };
    let chevron = header_chevron(expanded);
    // Title and count only. A trailing rule made one-item sections look like a panel.
    Line::from(vec![
        Span::raw(indent),
        Span::styled(chevron, Style::default().fg(theme.gray)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {count}"), Style::default().fg(theme.gray)),
    ])
}

fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    theme: &Theme,
    row: &DockRow,
    show_actions: bool,
    kill_hovered: bool,
    spinner_tick: u64,
    section: Section,
) {
    let accent = Style::default().fg(theme.accent_running);
    let kill_color = if kill_hovered {
        theme.accent_error
    } else {
        theme.gray
    };
    let icon = if row.spinning {
        let frames = crate::glyphs::dot_spinner_frames();
        frames
            .get((spinner_tick / SPINNER_DIVISOR) as usize % frames.len())
            .copied()
            .unwrap_or("")
    } else {
        crate::glyphs::diamond_filled()
    };
    let mut spans = vec![
        Span::raw(ROW_INDENT),
        Span::styled(format!("{icon} "), accent),
        Span::styled(row.kind.clone(), accent),
        Span::raw(" "),
        Span::styled(
            row.description.clone(),
            Style::default().fg(theme.text_primary),
        ),
    ];
    if let Some(activity) = row.activity.as_deref().filter(|s| !s.is_empty()) {
        spans.push(Span::styled(
            format!(" — {activity}"),
            Style::default().fg(theme.gray),
        ));
    }
    let left = Line::from(spans);

    let mut meta_spans = vec![Span::styled(
        row.meta.clone(),
        Style::default().fg(theme.gray),
    )];
    if show_actions {
        if row.openable || row.killable {
            meta_spans.push(Span::raw(" "));
        }
        if row.openable {
            meta_spans.push(Span::styled(
                crate::glyphs::enlarge_button(),
                Style::default().fg(theme.gray_bright),
            ));
        }
        if row.killable {
            meta_spans.push(Span::styled(
                section.kill_label(),
                Style::default().fg(kill_color),
            ));
        }
    }
    let mut meta_line = Line::from(meta_spans);
    if meta_line.width() > area.width as usize {
        let kill = section.kill_label();
        meta_line = if show_actions && row.killable && area.width as usize >= kill.width() {
            Line::from(Span::styled(kill, Style::default().fg(kill_color)))
        } else {
            truncate_line(meta_line, area.width as usize)
        };
    }
    let meta_width = meta_line.width() as u16;
    let reserved = u16::from(meta_width > 0).saturating_add(meta_width);
    let left_budget = area.width.saturating_sub(reserved.min(area.width));
    if left_budget > 0 {
        let left = truncate_line(left, left_budget as usize);
        buf.set_line(area.x, y, &left, left_budget);
    }

    if meta_width > 0 && meta_width <= area.width {
        let x = area.x + area.width - meta_width;
        buf.set_line(x, y, &meta_line, meta_width);
    }
}

pub fn stop_button_rect(area: Rect, y: u16, section: Section) -> Option<Rect> {
    let width = section.kill_label().width() as u16;
    (area.width >= width).then(|| Rect::new(area.right() - width, y, width, 1))
}

fn action_item(data: &DockData, items: &[DockItem]) -> Option<DockItem> {
    match data.hovered.filter(|hovered| items.contains(hovered)) {
        Some(hovered) => Some(hovered),
        None if data.focused => items.get(data.cursor).copied(),
        None => None,
    }
}

pub fn hovered_stop_button_rect(area: Rect, data: &DockData) -> Option<DockStopHit> {
    let layout = DockLayout::with_cap(&data.counts(), area.height as usize);
    let item = action_item(data, layout.rows())?;
    let visible_row = layout.rows().iter().position(|it| *it == item)?;
    if visible_row >= area.height as usize {
        return None;
    }
    let DockItem::Row(section, index) = item else {
        return None;
    };
    let dock_row = data.rows(section).get(index)?;
    dock_row
        .killable
        .then(|| stop_button_rect(area, area.y + visible_row as u16, section))
        .flatten()
        .map(|rect| DockStopHit { rect, item })
}

pub fn fmt_elapsed(secs: u64) -> String {
    crate::views::goal_detail::format_elapsed(secs.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol()))
            .collect()
    }

    fn header_prefix(expanded: bool, label: &str, count: usize) -> String {
        format!("{HEADER_INDENT}{}{label} {count}", header_chevron(expanded))
    }

    fn subagent_hover_actions() -> String {
        format!("{}{STOP_LABEL}", crate::glyphs::enlarge_button())
    }

    fn row(kind: &str, description: &str, meta: &str, killable: bool) -> DockRow {
        DockRow {
            kind: kind.into(),
            description: description.into(),
            activity: None,
            meta: meta.into(),
            killable,
            openable: true,
            spinning: false,
        }
    }

    fn sample() -> DockData {
        DockData {
            workflows: Vec::new(),
            subagents: vec![
                DockRow {
                    kind: "Explore".into(),
                    description: "find dashboard render path".into(),
                    activity: Some("reading render.rs".into()),
                    meta: "grok-4.5 2m14s".into(),
                    killable: true,
                    openable: true,
                    spinning: false,
                },
                row("General", "fix flaky pty scroll test", "12s", true),
                row("General", "third", "1s", true),
            ],
            tasks: vec![row("Run", "cargo test -p theme (bg)", "12s", true)],
            watchers: vec![row("Monitor", "watch build log", "3m01s", true), {
                let mut loop_row = row("Loop", "check CI status", "every 5m", false);
                loop_row.openable = false;
                loop_row
            }],
            queued: 2,
            workflows_expanded: true,
            subagents_expanded: true,
            tasks_expanded: false,
            watchers_expanded: false,
            workflows_show_all: false,
            subagents_show_all: false,
            tasks_show_all: false,
            watchers_show_all: false,
            focused: false,
            cursor: 0,
            queue_body_rows: 0,
            offsets: SectionSlots::default(),
            max_rows: MaxRows::default(),
            hovered: None,
            stop_hovered: false,
            spinner_tick: 0,
        }
    }

    #[test]
    fn all_zero_dock_renders_nothing() {
        let data = DockData::default();
        assert!(visible_items(&data).is_empty());
        assert_eq!(desired_height(&data), 0);
    }

    #[test]
    fn zero_count_sections_are_hidden() {
        let data = DockData {
            queued: 2,
            queue_body_rows: 3,
            ..DockData::default()
        };
        assert_eq!(
            visible_items(&data),
            vec![DockItem::Header(Section::Queued)]
        );
        assert_eq!(desired_height(&data), 4);

        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(
            row_text(&buf, 0).starts_with(&header_prefix(true, "Queued", 2)),
            "expanded"
        );
        assert_eq!(
            queue_body_rect(area, &data),
            Rect::new(QUEUE_BODY_INDENT, 1, 40 - QUEUE_BODY_INDENT, 3),
            "the queue body sits under the header at the row indent"
        );
    }

    #[test]
    fn all_sections_render_with_counts_and_collapse_state() {
        let theme = Theme::tokyonight();
        let data = sample();
        assert_eq!(desired_height(&data), 7);

        let area = Rect::new(0, 0, 100, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);

        assert!(row_text(&buf, 0).starts_with(&header_prefix(true, "Subagents", 3)));
        let first = row_text(&buf, 1);
        let diamond = crate::glyphs::diamond_filled();
        assert!(
            first.starts_with(&format!("{ROW_INDENT}{diamond} Explore")),
            "row must start with indent + diamond, got {first:?}"
        );
        assert!(
            !first.trim_start().starts_with('.'),
            "row must not start with '.', got {first:?}"
        );
        assert!(
            first.contains("Explore find dashboard render path — reading render.rs"),
            "{first}"
        );
        assert!(first.trim_end().ends_with("grok-4.5 2m14s"), "{first}");
        assert!(
            row_text(&buf, 3).contains("General third"),
            "third row is in the preview, not folded: {}",
            row_text(&buf, 3)
        );
        assert!(
            !row_text(&buf, 3).contains("more"),
            "no N-more line: {}",
            row_text(&buf, 3)
        );
        assert!(row_text(&buf, 4).starts_with(&header_prefix(false, "Tasks", 1)));
        assert!(row_text(&buf, 5).starts_with(&header_prefix(false, "Watchers", 2)));
        assert!(row_text(&buf, 6).starts_with(&header_prefix(false, "Queued", 2)));
    }

    #[test]
    fn expanded_tasks_and_watchers_show_rows() {
        let theme = Theme::tokyonight();
        let mut data = sample();
        data.subagents_expanded = false;
        data.tasks_expanded = true;
        data.watchers_expanded = true;
        assert_eq!(desired_height(&data), 7);

        let area = Rect::new(0, 0, 80, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(row_text(&buf, 0).starts_with(&header_prefix(false, "Subagents", 3)));
        assert!(row_text(&buf, 1).starts_with(&header_prefix(true, "Tasks", 1)));
        assert!(row_text(&buf, 2).contains("Run cargo test -p theme (bg)"));
        assert!(row_text(&buf, 3).starts_with(&header_prefix(true, "Watchers", 2)));
        assert!(row_text(&buf, 4).contains("Monitor watch build log"));
        let loop_row = row_text(&buf, 5);
        assert!(loop_row.contains("Loop check CI status"), "{loop_row}");
        assert!(loop_row.trim_end().ends_with("every 5m"), "{loop_row}");
    }

    #[test]
    fn focused_cursor_highlights_and_shows_row_actions() {
        let theme = Theme::tokyonight();
        let mut data = sample();
        data.focused = true;
        data.cursor = 1;
        let area = Rect::new(0, 0, 100, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);

        let first = row_text(&buf, 1);
        assert!(
            first.trim_end().ends_with(&subagent_hover_actions()),
            "{first}"
        );
        assert_eq!(buf.cell((0, 1)).map(|c| c.bg), Some(theme.bg_highlight));

        let mut data = sample();
        data.hovered = Some(DockItem::Row(Section::Subagents, 0));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let hovered = row_text(&buf, 1);
        assert!(
            hovered.trim_end().ends_with(&subagent_hover_actions()),
            "{hovered}"
        );
        assert_eq!(buf.cell((0, 1)).map(|c| c.bg), Some(theme.row_hover_bg()));
        let kill = Section::Subagents.kill_label();
        assert_eq!(
            hovered_stop_button_rect(area, &data).map(|hit| hit.rect),
            Some(Rect::new(
                area.right() - kill.width() as u16,
                1,
                kill.width() as u16,
                1
            ))
        );

        let mut data = sample();
        data.subagents_expanded = false;
        data.tasks_expanded = true;
        data.hovered = Some(DockItem::Row(Section::Tasks, 0));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let hovered = row_text(&buf, 2);
        let task_actions = format!("{}{STOP_LABEL}", crate::glyphs::enlarge_button());
        assert!(hovered.trim_end().ends_with(&task_actions), "{hovered}");

        let mut data = sample();
        if let Some(row) = data.subagents.get_mut(0) {
            row.killable = false;
        }
        data.hovered = Some(DockItem::Row(Section::Subagents, 0));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let pending = row_text(&buf, 1);
        assert!(
            pending
                .trim_end()
                .ends_with(crate::glyphs::enlarge_button()),
            "{pending}"
        );
        assert!(!pending.contains("[stop]"), "{pending}");
        assert!(
            !pending.contains(crate::glyphs::ballot_x_button()),
            "{pending}"
        );

        let mut data = sample();
        data.subagents_expanded = false;
        data.tasks_expanded = false;
        data.watchers_expanded = true;
        data.focused = true;
        data.cursor = visible_items(&data)
            .iter()
            .position(|item| *item == DockItem::Row(Section::Watchers, 1))
            .expect("loop row");
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let loop_row = row_text(&buf, data.cursor as u16);
        assert!(!loop_row.contains("[stop]"), "{loop_row}");
        assert!(
            !loop_row.contains(crate::glyphs::enlarge_button()),
            "{loop_row}"
        );

        if let Some(row) = data.watchers.get_mut(1) {
            row.killable = true;
            row.openable = true;
        }
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let linked = row_text(&buf, data.cursor as u16);
        let loop_actions = format!("{}{STOP_LABEL}", crate::glyphs::enlarge_button());
        assert!(linked.trim_end().ends_with(&loop_actions), "{linked}");
    }

    #[test]
    fn painted_kill_matches_hovered_stop_button_rect() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 100, 7);
        let actions = subagent_hover_actions();
        let kill = Section::Subagents.kill_label();
        let kill_rect = |y| {
            Some(Rect::new(
                area.right() - kill.width() as u16,
                y,
                kill.width() as u16,
                1,
            ))
        };

        let mut data = sample();
        data.focused = true;
        data.cursor = 1;
        data.hovered = Some(DockItem::Header(Section::Subagents));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let selected = row_text(&buf, 1);
        assert!(
            !selected.contains(&actions),
            "selected kill must not paint while a header is hovered: {selected}"
        );
        assert!(hovered_stop_button_rect(area, &data).is_none());

        data.hovered = Some(DockItem::Row(Section::Subagents, 1));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let first = row_text(&buf, 1);
        let second = row_text(&buf, 2);
        assert!(
            !first.contains(&actions),
            "unhovered selected row must not paint kill: {first}"
        );
        assert!(
            second.trim_end().ends_with(&actions),
            "hovered row must paint kill: {second}"
        );
        assert_eq!(
            hovered_stop_button_rect(area, &data).map(|hit| hit.rect),
            kill_rect(2)
        );
    }

    #[test]
    fn kill_label_stays_gray_until_the_button_is_hovered() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 100, 7);
        let kill = Section::Subagents.kill_label();
        let kill_x = area.right() - kill.width() as u16;

        // Row hovered but pointer off the kill control: gray, never destructive red.
        let mut data = sample();
        data.hovered = Some(DockItem::Row(Section::Subagents, 0));
        let hit = hovered_stop_button_rect(area, &data).expect("hovered row has a kill rect");
        data.stop_hovered = hit.rect.contains((0, 1).into());
        assert!(!data.stop_hovered);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(buf.cell((kill_x, 1)).map(|c| c.fg), Some(theme.gray));
        assert_ne!(
            buf.cell((kill_x, 1)).map(|c| c.fg),
            Some(theme.accent_error)
        );

        // Keyboard-focused row, pointer still off the kill control: gray.
        data.hovered = None;
        data.focused = true;
        data.cursor = 1;
        let hit = hovered_stop_button_rect(area, &data).expect("focused row has a kill rect");
        data.stop_hovered = hit.rect.contains((0, 1).into());
        assert!(!data.stop_hovered);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(buf.cell((kill_x, 1)).map(|c| c.fg), Some(theme.gray));
        assert_ne!(
            buf.cell((kill_x, 1)).map(|c| c.fg),
            Some(theme.accent_error)
        );

        // Pointer on the kill control: red.
        data.stop_hovered = hit.rect.contains((kill_x, 1).into());
        assert!(data.stop_hovered);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(
            buf.cell((kill_x, 1)).map(|c| c.fg),
            Some(theme.accent_error)
        );
    }

    #[test]
    fn active_rows_animate_the_dot_spinner() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 60, 2);
        // Leading marker sits just past the row indent.
        let icon_x = ROW_INDENT.width() as u16;
        let frames = crate::glyphs::dot_spinner_frames();

        let spinning = |tick: u64| {
            let data = DockData {
                subagents: vec![{
                    let mut r = row("Explore", "reading", "1s", true);
                    r.spinning = true;
                    r
                }],
                subagents_expanded: true,
                spinner_tick: tick,
                ..DockData::default()
            };
            let mut buf = Buffer::empty(area);
            render(&mut buf, area, &theme, &data);
            buf.cell((icon_x, 1))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default()
        };

        // The marker advances through the spinner frames as the tick climbs.
        assert_eq!(spinning(0), frames.first().copied().unwrap_or(""));
        assert_eq!(
            spinning(SPINNER_DIVISOR),
            frames.get(1).copied().unwrap_or("")
        );
        assert_ne!(spinning(0), spinning(SPINNER_DIVISOR));

        // A non-spinning row (scheduled loop) keeps the static diamond.
        let data = DockData {
            watchers: vec![row("Loop", "check CI", "every 5m", true)],
            watchers_expanded: true,
            ..DockData::default()
        };
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(
            buf.cell((icon_x, 1)).map(|c| c.symbol()),
            Some(crate::glyphs::diamond_filled())
        );
    }

    #[test]
    fn stale_hover_outside_items_falls_back_to_focused_cursor() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 100, 8);
        let mut data = DockData {
            tasks: (0..6)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            tasks_show_all: true,
            focused: true,
            cursor: 1,
            // A row index the layout no longer paints: the stale hover must fall
            // back to the cursor rather than swallowing its kill control.
            hovered: Some(DockItem::Row(Section::Tasks, 99)),
            ..DockData::default()
        };
        let y = data.cursor as u16;
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted = row_text(&buf, y);
        assert!(
            painted.contains("[stop]"),
            "stale more-row hover must not hide the cursor row kill: {painted}"
        );
        assert_eq!(
            hovered_stop_button_rect(area, &data).map(|hit| hit.rect),
            stop_button_rect(area, y, Section::Tasks),
            "the kill control sits at the row's right edge"
        );

        data.focused = false;
        assert!(
            hovered_stop_button_rect(area, &data).is_none(),
            "stale hover with no focus is not a kill target"
        );
    }

    #[test]
    fn item_at_maps_rows() {
        let data = sample();
        let c = data.counts();
        let layout = DockLayout::new(&c);
        let at = |row| layout.item_at(row);
        assert_eq!(at(0), Some(DockItem::Header(Section::Subagents)));
        assert_eq!(at(1), Some(DockItem::Row(Section::Subagents, 0)));
        assert_eq!(at(2), Some(DockItem::Row(Section::Subagents, 1)));
        assert_eq!(at(3), Some(DockItem::Row(Section::Subagents, 2)));
        assert_eq!(at(4), Some(DockItem::Header(Section::Tasks)));
        assert_eq!(at(5), Some(DockItem::Header(Section::Watchers)));
        assert_eq!(at(6), Some(DockItem::Header(Section::Queued)));
        assert_eq!(at(7), None, "past the end / queue body");
    }

    /// Ten tasks and two watchers: more rows than the dock can paint.
    fn crowded() -> DockData {
        DockData {
            tasks: (0..10)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..2)
                .map(|i| row("Monitor", &format!("watch {i}"), "3m01s", true))
                .collect(),
            tasks_expanded: true,
            watchers_expanded: true,
            ..DockData::default()
        }
    }

    #[test]
    fn crowded_dock_keeps_every_header_and_never_grows_past_its_cap() {
        let theme = Theme::tokyonight();
        let data = crowded();
        assert_eq!(desired_height(&data), MAX_DOCK_ROWS);

        let area = Rect::new(0, 0, 60, MAX_DOCK_ROWS);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert_eq!(
            DockLayout::new(&data.counts()).rows().len(),
            MAX_DOCK_ROWS as usize
        );
        assert!(
            painted
                .first()
                .is_some_and(|row| row.starts_with(&header_prefix(true, "Tasks", 10))),
            "{:?}",
            painted.first()
        );
        assert!(
            painted
                .iter()
                .any(|line| line.starts_with(&header_prefix(true, "Watchers", 2))),
            "a crowded section must not push another section's header off: {painted:#?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("▾ show 7 more")),
            "the rows that do not fit are summarized: {painted:#?}"
        );
        for watcher in ["watch 0", "watch 1"] {
            assert!(
                painted.iter().any(|line| line.contains(watcher)),
                "every section keeps a share of the rows: {painted:#?}"
            );
        }
    }

    #[test]
    fn a_short_frame_still_paints_every_header() {
        let theme = Theme::tokyonight();
        let data = DockData {
            subagents: (0..4)
                .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
                .collect(),
            tasks: (0..10)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..2)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            queued: 1,
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            queue_body_rows: 2,
            ..DockData::default()
        };
        assert_eq!(
            desired_height(&data),
            11,
            "three 2-row floors plus a queue-body row outrank the resting cap"
        );

        let assigned = 4;
        let area = Rect::new(0, 0, 60, assigned);
        let layout = DockLayout::with_cap(&data.counts(), assigned as usize);
        assert_eq!(
            layout.rows(),
            &[
                DockItem::Header(Section::Subagents),
                DockItem::Header(Section::Tasks),
                DockItem::Header(Section::Watchers),
                DockItem::Header(Section::Queued),
            ],
            "height == header count leaves every section a 0-row grant, so reveal must not steal a later header: {:?}",
            layout.rows()
        );

        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        for label in ["Subagents", "Tasks", "Watchers", "Queued"] {
            assert!(
                painted.iter().any(|line| line.contains(label)),
                "{label} must stay reachable when the frame assigns fewer rows than max_rows: {painted:#?}"
            );
        }
    }

    #[test]
    fn crowded_and_short_caps_keep_every_header() {
        let data = DockData {
            subagents: (0..4)
                .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
                .collect(),
            tasks: (0..10)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..2)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            queued: 1,
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            queue_body_rows: 2,
            ..DockData::default()
        };
        let headers = [
            DockItem::Header(Section::Subagents),
            DockItem::Header(Section::Tasks),
            DockItem::Header(Section::Watchers),
            DockItem::Header(Section::Queued),
        ];
        for cap in [4usize, MAX_DOCK_ROWS as usize] {
            let layout = DockLayout::with_cap(&data.counts(), cap);
            assert!(
                layout.rows().len() <= cap,
                "cap {cap} must not emit more rows than it was given: {:?}",
                layout.rows()
            );
            for header in headers {
                assert!(
                    layout.rows().contains(&header),
                    "cap {cap} dropped {header:?}: {:?}",
                    layout.rows()
                );
            }
        }
    }

    #[test]
    fn show_all_drops_once_the_section_fits_again() {
        let mut data = crowded();
        data.tasks_show_all = true;
        assert!(is_show_all_needed(&data.counts(), Section::Tasks));
        data.tasks.truncate(2);
        assert!(
            !is_show_all_needed(&data.counts(), Section::Tasks),
            "a section that shrank back inside the resting height drops its reveal"
        );
    }

    #[test]
    fn a_capped_reveal_keeps_the_rest_behind_its_summary() {
        let theme = Theme::tokyonight();
        let mut data = crowded();
        data.tasks_show_all = true;
        // Room for four more rows than the resting height: the section opens as
        // far as it can and keeps the rest behind a scroll.
        data.max_rows = MaxRows::new(MAX_DOCK_ROWS + 4);
        let layout = DockLayout::new(&data.counts());
        assert!(layout.rows_below(Section::Tasks) > 0);

        let area = Rect::new(0, 0, 60, desired_height(&data));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();

        let hidden = layout.hidden_rows(Section::Tasks);
        assert!(
            painted
                .iter()
                .any(|line| line.contains(&format!("show {hidden} more"))),
            "what the reveal could not fit stays behind the summary: {painted:#?}"
        );
        assert!(
            painted.iter().all(|line| !line.contains('\u{2588}')),
            "the dock paints no scrollbar: {painted:#?}"
        );
    }

    #[test]
    fn revealing_one_section_leaves_the_others_at_their_resting_share() {
        let mut data = crowded();
        data.subagents = (0..6)
            .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
            .collect();
        data.subagents_expanded = true;
        // The fixture grew after `crowded()` set its ceiling; re-derive it.
        let resting = DockLayout::new(&data.counts());
        let subagents_at_rest = resting.visible_rows(Section::Subagents);
        let watchers_at_rest = resting.visible_rows(Section::Watchers);

        data.tasks_show_all = true;
        data.max_rows = MaxRows::new(20);
        let opened = DockLayout::new(&data.counts());
        assert!(
            opened.visible_rows(Section::Tasks) > resting.visible_rows(Section::Tasks),
            "the opened section takes the extra rows"
        );
        assert_eq!(
            opened.visible_rows(Section::Subagents),
            subagents_at_rest,
            "an untouched section keeps its resting share"
        );
        assert_eq!(
            opened.visible_rows(Section::Watchers),
            watchers_at_rest,
            "an untouched section keeps its resting share"
        );
    }

    #[test]
    fn reveal_does_not_shrink_below_the_resting_cap() {
        let mut data = DockData {
            tasks: (0..12)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            ..DockData::default()
        };
        let resting = desired_height(&data);
        assert_eq!(resting, MAX_DOCK_ROWS);
        let tasks_at_rest = DockLayout::new(&data.counts()).visible_rows(Section::Tasks);

        // Typical terminal: `dock_max_rows` stays at 8 (`above_prompt / 2 <= 8`).
        // The 2-row floor is 3, so stacking the raise on the floor alone
        // would drop desired_height from 8 to 3.
        data.tasks_show_all = true;
        assert!(
            desired_height(&data) >= resting,
            "reveal must not drop desired_height below the resting band: {} -> {}",
            resting,
            desired_height(&data)
        );
        assert!(
            DockLayout::new(&data.counts()).visible_rows(Section::Tasks) >= tasks_at_rest,
            "reveal must not hide task rows that were already on screen"
        );
    }

    #[test]
    fn reveal_does_not_exceed_the_half_prompt_ceiling() {
        let mut data = DockData {
            subagents: (0..4)
                .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
                .collect(),
            tasks: (0..10)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..2)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            queued: 1,
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            queue_body_rows: 2,
            ..DockData::default()
        };
        assert_eq!(
            desired_height(&data),
            11,
            "three 2-row floors plus a queue-body row outrank the resting cap"
        );
        data.tasks_show_all = true;
        data.max_rows = MaxRows::new(16);
        assert!(
            desired_height(&data) <= 16,
            "reveal must not stack the raise on a floor-taller dock past the half-prompt ceiling: {}",
            desired_height(&data)
        );
    }

    #[test]
    fn revealing_a_section_opens_it_past_the_resting_cap() {
        let theme = Theme::tokyonight();
        let mut data = crowded();
        assert_eq!(desired_height(&data), MAX_DOCK_ROWS);

        // The caller raises the cap for a revealed section.
        data.tasks_show_all = true;
        data.max_rows = MaxRows::new(20);
        assert_eq!(
            desired_height(&data),
            14,
            "two headers, ten tasks, and two watchers"
        );
        assert_eq!(
            DockLayout::new(&data.counts()).visible_rows(Section::Tasks),
            10,
            "every row of the revealed section is on screen"
        );

        let area = Rect::new(0, 0, 60, desired_height(&data));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            painted.iter().any(|line| line.contains("task 9")),
            "the last row is painted: {painted:#?}"
        );
        assert!(
            !painted.iter().any(|line| line.contains("show ")),
            "nothing is left to summarize: {painted:#?}"
        );
        for watcher in ["watch 0", "watch 1"] {
            assert!(
                painted.iter().any(|line| line.contains(watcher)),
                "the other sections keep their rows: {painted:#?}"
            );
        }
    }

    #[test]
    fn a_section_scrolls_inside_its_own_band() {
        let theme = Theme::tokyonight();
        let mut data = crowded();
        let area = Rect::new(0, 0, 60, MAX_DOCK_ROWS);
        let counts = data.counts();
        let visible = DockLayout::new(&counts).visible_rows(Section::Tasks);
        assert_eq!(
            DockLayout::new(&counts).rows_below(Section::Tasks),
            10 - visible
        );

        data.offsets.set(Section::Tasks, 4);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            painted
                .first()
                .is_some_and(|row| row.starts_with(&header_prefix(true, "Tasks", 10))),
            "{:?}",
            painted.first()
        );
        assert!(
            painted.get(1).is_some_and(|line| line.contains("task 4")),
            "the band starts at the scroll offset: {:?}",
            painted.get(1)
        );
        assert!(
            painted
                .iter()
                .any(|line| line.starts_with(&header_prefix(true, "Watchers", 2))),
            "scrolling one section cannot move another's header: {painted:#?}"
        );
        for watcher in ["watch 0", "watch 1"] {
            assert!(painted.iter().any(|line| line.contains(watcher)));
        }
        assert!(
            painted.iter().any(|line| line.contains("▾ show 7 more")),
            "the summary counts the rows the section is not showing, which the \
             scroll does not change: {painted:#?}"
        );

        // 99 clamps to the last band; the summary then points back up.
        data.offsets.set(Section::Tasks, 99);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            painted.get(1).is_some_and(|line| line.contains("task 7")),
            "the last rows are reachable: {painted:#?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("▾ show 7 more")),
            "and still counts the same rows at the end of the band: {painted:#?}"
        );
        assert_eq!(
            DockLayout::new(&data.counts()).rows_below(Section::Tasks),
            0
        );
    }

    #[test]
    fn a_two_row_section_shows_one_row_and_says_the_other_is_hidden() {
        let theme = Theme::tokyonight();
        // The shape from the bug report: a busy Subagents section beside a small
        // Watchers section that still has one row it cannot show.
        let data = DockData {
            subagents: (0..10)
                .map(|i| row("General", &format!("agent {i}"), "1s", true))
                .collect(),
            tasks: (0..3)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..2)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 60, desired_height(&data));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();

        let layout = DockLayout::new(&data.counts());
        for section in [Section::Subagents, Section::Tasks, Section::Watchers] {
            assert_ne!(
                layout.visible_rows(section),
                0,
                "{section:?} paints a header with nothing under it: {painted:#?}"
            );
            // Whatever a section cannot show, it says so: the reported bug was a
            // `Watchers 2` header over one row with no sign of the second.
            let hidden = layout.rows_below(section);
            assert!(
                hidden == 0
                    || painted
                        .iter()
                        .any(|line| line.contains(&format!("show {hidden} more"))),
                "{section:?} hides {hidden} rows without saying so: {painted:#?}"
            );
        }
        assert!(
            painted.iter().any(|line| line.contains("watch 1")),
            "a section that fits shows every row: {painted:#?}"
        );
    }

    #[test]
    fn every_expanded_section_shows_a_row_under_its_header() {
        let theme = Theme::tokyonight();
        let data = DockData {
            subagents: (0..4)
                .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
                .collect(),
            tasks: (0..4)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..4)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            ..DockData::default()
        };
        let layout = DockLayout::new(&data.counts());
        let area = Rect::new(0, 0, 60, desired_height(&data));
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();

        for (section, first) in [
            (Section::Subagents, "subagent 0"),
            (Section::Tasks, "task 0"),
            (Section::Watchers, "watch 0"),
        ] {
            assert!(
                layout.visible_rows(section) >= 1,
                "{section:?} shows no row of its own: {painted:#?}"
            );
            assert!(
                painted.iter().any(|line| line.contains(first)),
                "{section:?} paints a header with nothing under it: {painted:#?}"
            );
        }
        assert_eq!(
            painted
                .iter()
                .filter(|line| line.contains("▾ show 3 more"))
                .count(),
            3,
            "each section keeps a summary for the rows it cannot show: {painted:#?}"
        );

        let layout = DockLayout::new(&data.counts());
        for section in [Section::Subagents, Section::Tasks, Section::Watchers] {
            assert_eq!(
                layout.visible_rows(section),
                1,
                "{section:?} spends its two-row grant on one row plus show-more"
            );
            assert_eq!(
                layout.total_rows(section),
                4,
                "{section:?} reveal label needs the hidden count: {}",
                layout.total_rows(section)
            );
            assert_eq!(
                layout.rows_below(section),
                3,
                "{section:?} must report the rows still below the painted one"
            );
        }
    }

    #[test]
    fn sections_fill_the_dock_before_hiding_anything() {
        let data = DockData {
            tasks: (0..6)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            ..DockData::default()
        };
        assert_eq!(desired_height(&data), 7, "header + all six rows");
        assert_eq!(
            DockLayout::new(&data.counts()).visible_rows(Section::Tasks),
            6
        );
        assert!(
            !visible_items(&data).contains(&DockItem::RevealRemaining(Section::Tasks)),
            "nothing is hidden while the dock has room"
        );
    }

    #[test]
    fn a_crowded_eight_row_dock_keeps_tasks_body_or_show_more() {
        // Headers + Tasks + Watchers + Queued inside the resting 8-row band. One Subagents row keeps that
        // header from taking a 2-row floor, so the leftover body rows are the ones the giveback pass used
        // to hand to. Watchers after collapsing Tasks.
        let data = DockData {
            subagents: vec![row("Explore", "subagent 0", "1s", true)],
            tasks: (0..12)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..4)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            queued: 1,
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            queue_body_rows: 2,
            ..DockData::default()
        };
        let layout = DockLayout::with_cap(&data.counts(), MAX_DOCK_ROWS as usize);
        assert!(
            layout.rows().contains(&DockItem::Header(Section::Tasks)),
            "Tasks header stays: {:?}",
            layout.rows()
        );
        assert!(
            layout.visible_rows(Section::Tasks) >= 1
                || layout
                    .rows()
                    .contains(&DockItem::RevealRemaining(Section::Tasks)),
            "giveback must not leave Tasks with a bare header and no show-more: {:?}",
            layout.rows()
        );
        assert!(
            layout.queue_body_rows() >= 1,
            "Queued keeps its body floor: {}",
            layout.queue_body_rows()
        );
        for section in [Section::Subagents, Section::Tasks, Section::Watchers] {
            let reveal = layout.rows().contains(&DockItem::RevealRemaining(section));
            assert!(
                layout.visible_rows(section) >= 1 || !reveal,
                "{section:?} must not paint show-more with no row of its own: {:?}",
                layout.rows()
            );
        }
    }

    #[test]
    fn a_crowded_dock_reserves_one_queue_body_row() {
        let data = DockData {
            subagents: (0..4)
                .map(|i| row("Explore", &format!("subagent {i}"), "1s", true))
                .collect(),
            tasks: (0..4)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            watchers: (0..4)
                .map(|i| row("Monitor", &format!("watch {i}"), "1s", true))
                .collect(),
            queued: 2,
            subagents_expanded: true,
            tasks_expanded: true,
            watchers_expanded: true,
            queue_body_rows: 2,
            ..DockData::default()
        };
        let layout = DockLayout::new(&data.counts());
        assert!(
            layout.queue_body_rows() >= 1,
            "three section minima must not spend the last spare and leave Queued with no body: {}",
            layout.queue_body_rows()
        );
        assert!(
            layout.rows().contains(&DockItem::Header(Section::Queued)),
            "the Queued header stays: {:?}",
            layout.rows()
        );
    }

    #[test]
    fn queue_body_takes_the_rows_left_below_the_last_header() {
        let data = DockData {
            tasks: (0..6)
                .map(|i| row("Run", &format!("task {i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            queued: 2,
            queue_body_rows: 2,
            ..DockData::default()
        };
        let counts = data.counts();
        let area = Rect::new(0, 0, 60, MAX_DOCK_ROWS);
        let rows = DockLayout::new(&counts).rows().to_vec();
        assert_eq!(
            rows.last(),
            Some(&DockItem::Header(Section::Queued)),
            "the Queued header anchors its body: {rows:?}"
        );
        let body = queue_body_rect(area, &data);
        assert_eq!(body.y, area.y + rows.len() as u16);
        assert_eq!(
            body.bottom(),
            area.bottom(),
            "the queue body fills the dock's bottom edge"
        );
        assert_eq!(
            body.width,
            area.width - QUEUE_BODY_INDENT,
            "the queue body clears the row indent"
        );
    }

    #[test]
    fn row_marker_shares_the_header_gutter() {
        let theme = Theme::tokyonight();
        let data = DockData {
            watchers: vec![row("Loop", "check CI status", "every 5m", true)],
            watchers_expanded: true,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 80, 2);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(
            HEADER_INDENT.width(),
            ROW_INDENT.width(),
            "row indent must place the marker under the header chevron"
        );
        let header = row_text(&buf, 0);
        let item = row_text(&buf, 1);
        let chevron = crate::glyphs::disclosure_open();
        let diamond = crate::glyphs::diamond_filled();
        let chevron_at = header.find(chevron).expect("header chevron");
        let diamond_at = item.find(diamond).expect("row diamond");
        let chevron_col = header[..chevron_at].width();
        let diamond_col = item[..diamond_at].width();
        assert_eq!(
            diamond_col, chevron_col,
            "marker col {diamond_col} vs chevron col {chevron_col}\nheader={header:?}\nitem={item:?}"
        );
        let label_at = header.find("Watchers").expect("header label");
        let row_label_at = item.find("Loop").expect("row label");
        assert_eq!(
            header[..label_at].width(),
            item[..row_label_at].width(),
            "row title should sit under the section title\nheader={header:?}\nitem={item:?}"
        );
        assert!(
            !header.contains('─'),
            "header must not paint a rule fill: {header:?}"
        );
    }

    #[test]
    fn one_item_section_has_no_header_rule() {
        let theme = Theme::tokyonight();
        let data = DockData {
            tasks: vec![{
                let mut task = row("Task", "Pull the new h", "1s", true);
                task.spinning = true;
                task
            }],
            tasks_expanded: true,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let header = row_text(&buf, 0);
        assert_eq!(
            header.trim_end(),
            header_prefix(true, "Tasks", 1),
            "header must be title and count only: {header:?}"
        );
        assert!(!header.contains('─'), "no rule fill: {header:?}");
        let item = row_text(&buf, 1);
        let spinner = crate::glyphs::dot_spinner_frames()
            .first()
            .copied()
            .expect("spinner frames");
        let spinner_at = item.find(spinner).expect("row spinner");
        let chevron_at = header
            .find(crate::glyphs::disclosure_open())
            .expect("header chevron");
        assert_eq!(
            header[..chevron_at].width(),
            item[..spinner_at].width(),
            "spinner must share the header gutter\nheader={header:?}\nitem={item:?}"
        );
    }

    #[test]
    fn long_row_truncates_to_keep_meta() {
        let theme = Theme::tokyonight();
        let data = sample();
        let area = Rect::new(0, 0, 30, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let first = row_text(&buf, 1);
        assert!(
            first.contains('\u{2026}'),
            "description must truncate: {first:?}"
        );
        assert!(
            first.contains("grok-4.5"),
            "meta must stay visible: {first:?}"
        );
    }

    #[test]
    fn narrow_rows_never_render_blank() {
        let theme = Theme::tokyonight();
        let mut data = DockData {
            tasks: vec![row("Run", "cargo test", "very-long-metadata", true)],
            tasks_expanded: true,
            focused: false,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(!row_text(&buf, 1).trim().is_empty());

        data.focused = true;
        data.cursor = 1;
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let line = row_text(&buf, 1);
        assert!(line.trim_end().ends_with("[stop]"), "{line:?}");

        data.focused = false;
        data.hovered = Some(DockItem::Row(Section::Tasks, 0));
        let narrow = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(narrow);
        render(&mut buf, narrow, &theme, &data);
        assert!(!row_text(&buf, 1).trim().is_empty());
        assert!(!row_text(&buf, 1).contains("[stop]"));
        assert!(hovered_stop_button_rect(narrow, &data).is_none());
    }

    #[test]
    fn hovered_stop_rect_tracks_the_painted_row_and_killability() {
        let mut data = sample();
        data.hovered = Some(DockItem::Row(Section::Subagents, 1));
        let area = Rect::new(2, 4, 80, MAX_DOCK_ROWS);
        assert_eq!(
            hovered_stop_button_rect(area, &data).map(|hit| hit.rect),
            stop_button_rect(area, area.y + 2, Section::Subagents)
        );
        assert!(
            hovered_stop_button_rect(Rect { height: 2, ..area }, &data).is_none(),
            "a row below the dock's last painted line has no kill control"
        );

        if let Some(row) = data.subagents.get_mut(1) {
            row.killable = false;
        }
        assert!(hovered_stop_button_rect(area, &data).is_none());
        data.hovered = None;
        assert!(hovered_stop_button_rect(area, &data).is_none());

        if let Some(row) = data.subagents.get_mut(1) {
            row.killable = true;
        }
        data.focused = true;
        data.cursor = items(&data.counts())
            .iter()
            .position(|item| *item == DockItem::Row(Section::Subagents, 1))
            .expect("row 1");
        assert_eq!(
            hovered_stop_button_rect(area, &data).map(|hit| hit.rect),
            stop_button_rect(area, area.y + 2, Section::Subagents)
        );
        data.cursor = 0;
        assert!(hovered_stop_button_rect(area, &data).is_none());
    }

    #[test]
    fn long_loop_prompt_truncates_so_actions_stay_visible() {
        let theme = Theme::tokyonight();
        let data = DockData {
            watchers: vec![row(
                "Loop",
                "Run one pass of the Distill feedback-ingest pipeline. STEP 1 — stale-process guard. Run: ps -eo pid,etime,command",
                "every 5m",
                true,
            )],
            watchers_expanded: true,
            focused: true,
            cursor: 1,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 80, 2);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let line = row_text(&buf, 1);
        assert!(line.contains('\u{2026}'), "prompt must truncate: {line:?}");
        assert!(
            !line.contains("stale-process"),
            "prompt tail must be cut: {line:?}"
        );
        assert!(line.contains("every 5m"), "schedule must stay: {line:?}");
        assert!(
            line.trim_end().ends_with("[stop]"),
            "kill action must stay: {line:?}"
        );
    }

    #[test]
    fn header_has_a_small_inset_from_the_dock_left_edge() {
        let theme = Theme::tokyonight();
        let data = DockData {
            tasks: vec![row("Run", "cargo test", "1s", true)],
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let header = row_text(&buf, 0);
        assert!(
            header.starts_with(&format!(
                "{HEADER_INDENT}{}",
                crate::glyphs::disclosure_closed()
            )),
            "header must be inset one column, got {header:?}"
        );
    }

    #[test]
    fn narrow_headers_and_overflow_rows_keep_their_chevrons() {
        let theme = Theme::tokyonight();
        let data = DockData {
            tasks: (0..4)
                .map(|i| row("Run", &format!("row-{i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            ..DockData::default()
        };

        let mut header = Buffer::empty(Rect::new(0, 0, 1, 1));
        render(&mut header, Rect::new(0, 0, 1, 1), &theme, &data);
        assert_eq!(row_text(&header, 0), crate::glyphs::disclosure_open());

        // A section longer than the dock keeps its show-more chevron.
        let crowded = DockData {
            tasks: (0..12)
                .map(|i| row("Run", &format!("row-{i}"), "1s", true))
                .collect(),
            tasks_expanded: true,
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 5, MAX_DOCK_ROWS);
        let mut more = Buffer::empty(area);
        render(&mut more, area, &theme, &crowded);
        let last = row_text(&more, MAX_DOCK_ROWS - 1);
        assert!(last.contains('▾'), "{last}");
    }

    #[test]
    fn a_long_section_shows_what_fits_then_summarizes_the_rest() {
        let theme = Theme::tokyonight();
        let mut data = DockData {
            subagents: (0..20)
                .map(|i| row("Explore", &format!("row-{i}"), "1s", true))
                .collect(),
            subagents_expanded: true,
            ..DockData::default()
        };
        assert_eq!(desired_height(&data), MAX_DOCK_ROWS);
        let c = data.counts();
        assert_eq!(
            DockLayout::new(&c).item_at(0),
            Some(DockItem::Header(Section::Subagents))
        );
        assert_eq!(
            DockLayout::new(&c).item_at(1),
            Some(DockItem::Row(Section::Subagents, 0))
        );
        assert_eq!(
            DockLayout::new(&c).item_at(6),
            Some(DockItem::Row(Section::Subagents, 5))
        );
        assert_eq!(
            DockLayout::new(&c).item_at(7),
            Some(DockItem::RevealRemaining(Section::Subagents))
        );
        assert_eq!(
            DockLayout::new(&c).item_at(8),
            None,
            "the dock never paints past its cap"
        );

        let area = Rect::new(0, 0, 80, MAX_DOCK_ROWS);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(row_text(&buf, 1).contains("row-0"), "{}", row_text(&buf, 1));
        assert!(row_text(&buf, 6).contains("row-5"), "{}", row_text(&buf, 6));
        assert!(
            row_text(&buf, 7).contains("▾ show 14 more"),
            "{}",
            row_text(&buf, 7)
        );

        data.subagents_expanded = false;
        assert_eq!(desired_height(&data), 1);
        assert_eq!(
            visible_items(&data),
            vec![DockItem::Header(Section::Subagents)]
        );
    }

    #[test]
    fn hover_paints_terminal_row_hover_bg_when_unfocused() {
        let theme = Theme::tokyonight();
        let mut data = DockData {
            tasks: vec![row("Run", "cargo test", "1s", true)],
            tasks_expanded: true,
            hovered: Some(DockItem::Row(Section::Tasks, 0)),
            ..DockData::default()
        };
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let hover_bg = theme.row_hover_bg();
        assert_ne!(
            hover_bg, theme.bg_hover,
            "dock hover must match the terminal row blend, not the dropdown band"
        );
        assert_eq!(buf.cell((0, 1)).map(|c| c.bg), Some(hover_bg));
        assert_ne!(buf.cell((0, 0)).map(|c| c.bg), Some(hover_bg));

        data.focused = true;
        data.cursor = 1;
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert_eq!(
            buf.cell((0, 1)).map(|c| c.bg),
            Some(theme.bg_highlight),
            "selection wins over hover"
        );
    }
}
