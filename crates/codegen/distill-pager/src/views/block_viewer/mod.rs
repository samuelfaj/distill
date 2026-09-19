// Modified for Distill by Samuel Fajreldines, 2026.
//! Fullscreen content viewer for scrollback blocks.
//!
//! Opens via Ctrl-F on a selected block, replaces the scrollback area.
//! Provides ListPane-based navigation, search, visual-select, and copy.
//!
//! Supports thinking/agent message blocks (markdown content).
//! Execute and edit viewers will be added in later phases.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use distill_workspace::permission::mcp_titleize_segment;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::StatefulWidget;

use crate::clipboard::SystemClipboard;
use crate::render::scrollbar::SCROLLBAR_TOTAL_COLS;
use crate::scrollback::block::{BlockContent, RenderBlock};
use crate::scrollback::blocks::ToolCallBlock;
use crate::scrollback::entry::{EntryId, ScrollbackEntry};
use crate::scrollback::types::{BlockContext, DisplayMode};
use crate::theme::{Theme, ThemeKind};
use crate::views::list_pane::{
    ListItem, ListPane, ListPaneConfig, ListPaneState, ListPaneStyle, WrapMode,
};
use crate::views::modal_window::ModalWindowState;
use crate::views::shortcuts_bar::HintItem;

mod selection;

pub(crate) use selection::format_blockquote;
pub use selection::{TextDrag, TextEndpoint};

// ---------------------------------------------------------------------------
// ContentLine: generic ListItem for the viewer
// ---------------------------------------------------------------------------

/// A single line of content displayed in the block viewer's ListPane.
#[derive(Clone)]
pub struct ContentLine {
    /// Styled content to display.
    content: Line<'static>,
    /// Plain text for search matching.
    plain_text: String,
    /// Stable identity (pre-wrap line index).
    id: u64,
    /// Optional full-width background color (e.g., code block bg).
    bg: Option<Color>,
}

impl ContentLine {
    /// Build content lines from pre-wrap rendered markdown lines.
    pub fn from_lines(lines: &[Line<'static>]) -> Vec<Self> {
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                Self {
                    content: line.clone(),
                    plain_text: plain,
                    id: i as u64,
                    bg: line.style.bg,
                }
            })
            .collect()
    }
}

impl ListItem for ContentLine {
    fn content(&self) -> &Line<'_> {
        &self.content
    }

    fn stable_id(&self) -> u64 {
        self.id
    }

    fn search_text(&self) -> &str {
        &self.plain_text
    }

    fn background(&self) -> Option<Color> {
        self.bg
    }
}

// ---------------------------------------------------------------------------
// DiffLineMeta: per-item diff metadata for edit viewer patch copy
// ---------------------------------------------------------------------------

/// Metadata for a single diff line, stored parallel to `items` in the edit viewer.
pub struct DiffLineMeta {
    pub tag: similar::ChangeTag,
    pub text: String,
    pub lo: usize,
    pub ln: usize,
}

// ---------------------------------------------------------------------------
// BlockViewerPane
// ---------------------------------------------------------------------------

/// What kind of block content the viewer is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerKind {
    /// Thinking or AgentMessage (markdown content).
    Markdown,
    /// Execute tool call (stdout).
    Execute,
    /// Edit tool call (diff).
    Edit,
    /// Background task (stdout from central store).
    BgTask,
    /// Web fetch tool call (fetched content).
    WebFetch,
    /// Web search tool call (search results and citations).
    WebSearch,
    /// Integration tool discovery (search_tool).
    IntegrationSearch,
    /// Integration tool dispatch (use_tool).
    UseTool,
    /// Read file tool call (file content).
    Read,
    Grep,
    /// Plain text content (e.g., catalog entry).
    PlainText,
}

impl ViewerKind {
    pub fn telemetry_kind(self) -> distill_telemetry::events::BlockViewerKind {
        use distill_telemetry::events::BlockViewerKind;
        match self {
            Self::Markdown => BlockViewerKind::Markdown,
            Self::Execute => BlockViewerKind::Execute,
            Self::Edit => BlockViewerKind::Edit,
            Self::BgTask => BlockViewerKind::BgTask,
            Self::WebFetch => BlockViewerKind::WebFetch,
            Self::WebSearch => BlockViewerKind::WebSearch,
            Self::IntegrationSearch => BlockViewerKind::IntegrationSearch,
            Self::UseTool => BlockViewerKind::UseTool,
            Self::Read => BlockViewerKind::Read,
            Self::Grep => BlockViewerKind::Grep,
            Self::PlainText => BlockViewerKind::PlainText,
        }
    }
}

/// Fullscreen block content viewer. Replaces the scrollback area when open. Owns a `ListPaneState`
/// for navigation, search, and visual-select. Reads block content from the scrollback via
/// `entry_id`.
pub struct BlockViewerPane {
    /// Which scrollback entry we are viewing.
    pub entry_id: EntryId,
    /// What kind of content.
    pub kind: ViewerKind,
    /// ListPane state (scroll, selection, search, follow).
    pub list_state: ListPaneState,
    /// Visual style for the ListPane.
    list_style: ListPaneStyle,
    /// Content items for the ListPane.
    items: Vec<ContentLine>,
    /// Cached content area from last render (for mouse hit-testing).
    last_content_area: Rect,

    /// Set by handle_key when 'r' is pressed. Caller should toggle raw mode on the entry and call rebuild_items().
    pub raw_toggle_pending: bool,
    /// Set by handle_key when 'Y' is pressed. Caller should copy the command.
    pub copy_meta_pending: bool,
    /// Set by handle_key when 'y' is pressed on edit blocks. Caller should copy patch.
    pub copy_content_pending: bool,
    /// Per-item diff metadata for edit viewer (parallel to `items`).
    /// `None` entries are separator lines between hunks.
    diff_meta: Vec<Option<DiffLineMeta>>,
    /// Last observed content generation (for streaming change detection).
    last_generation: u64,
    /// Whether the block was running when the viewer last checked.
    /// Used to detect the transition from running to finished (disable follow once).
    was_running: bool,
    /// Task ID for BgTask viewers (for looking up stdout in central store).
    pub bg_task_id: Option<String>,
    /// Last theme kind seen; used to detect theme switches and restyle.
    last_theme: ThemeKind,
    /// Modal chrome state (close button hover, popup area, etc.).
    pub modal: ModalWindowState,
    /// Cached prepend (preamble) ContentLines from the last render, combined with `items` to produce the unified vec the ListPane sees.
    /// Needed so scroll / mouse / key handlers index into the same item space the layout was prepared against (`prepared_items_count`).
    prepend_items: Vec<ContentLine>,
    pub text_drag: Option<TextDrag>,
    /// Text copied on the last drag release.
    /// The `Up(Left)` handler extracts the text immediately (same as scrollback `finish_text_drag`).
    /// The caller drains this with `.take()` and copies it to the clipboard.
    pub drag_copy_text: Option<String>,
    /// Cached unified items vec (prepend then body).
    /// Rebuilt by `rebuild_unified_cache` to avoid re-cloning on every handler and render call within the same frame.
    /// Callers that hold `&mut self` call `rebuild_unified_cache()` first, then reference `self.cached_unified` via a disjoint field borrow.
    cached_unified: Vec<ContentLine>,
    click_count: u8,
    last_click_at: Option<Instant>,
    last_click_ep: Option<TextEndpoint>,
    reveal_selection_once: bool,
}

impl BlockViewerPane {
    /// Create a viewer for a markdown block (thinking or agent message).
    pub fn for_markdown(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let lines = Self::extract_markdown_lines(&entry.block)?;
        let generation = Self::extract_generation(&entry.block).unwrap_or(0);

        let config = ListPaneConfig {
            follow_enabled: entry.is_running,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state =
            ListPaneState::new_with_config(WrapMode::Wrap, entry.is_running, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let items = ContentLine::from_lines(&lines);

        Some(Self {
            entry_id,
            kind: ViewerKind::Markdown,
            list_state,
            list_style: ListPaneStyle {
                uniform_visual_bg: true,
                ..ListPaneStyle::default()
            },
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta: Vec::new(),
            last_generation: generation,
            was_running: entry.is_running,
            bg_task_id: None,
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        })
    }

    /// Create a viewer for an execute block (stdout output).
    pub fn for_execute(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) = &entry.block else {
            return None;
        };

        let config = ListPaneConfig {
            follow_enabled: entry.is_running,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state =
            ListPaneState::new_with_config(WrapMode::Wrap, entry.is_running, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let theme = Theme::current();
        let items = Self::build_execute_items(exec.output.as_deref(), &theme);
        let last_output_len = exec.output.as_ref().map_or(0, |o| o.len());

        // Dark background style for terminal output
        let list_style = ListPaneStyle {
            uniform_visual_bg: true,
            ..ListPaneStyle::default()
        };

        Some(Self {
            entry_id,
            kind: ViewerKind::Execute,
            list_state,
            list_style,
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta: Vec::new(),
            last_generation: last_output_len as u64, // reuse generation field for output length
            was_running: entry.is_running,
            bg_task_id: None,
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        })
    }

    /// Create a viewer for static content lines (shared by web_fetch and web_search).
    fn for_static_content(entry_id: EntryId, kind: ViewerKind, lines: Vec<Line<'static>>) -> Self {
        let config = ListPaneConfig {
            follow_enabled: false,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state = ListPaneState::new_with_config(WrapMode::Wrap, false, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let items: Vec<ContentLine> = lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                ContentLine {
                    content: line,
                    plain_text: plain,
                    id: i as u64,
                    bg: None,
                }
            })
            .collect();

        Self {
            entry_id,
            kind,
            list_state,
            list_style: ListPaneStyle::default(),
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta: Vec::new(),
            last_generation: 0,
            was_running: false,
            bg_task_id: None,
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        }
    }

    /// Create a viewer for a web fetch block (fetched content).
    pub fn for_web_fetch(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::WebFetch(fetch)) = &entry.block else {
            return None;
        };

        let theme = Theme::current();
        let content = fetch.output.as_deref().unwrap_or("");
        let lines: Vec<Line<'static>> = content
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme.text_primary),
                ))
            })
            .collect();

        Some(Self::for_static_content(
            entry_id,
            ViewerKind::WebFetch,
            lines,
        ))
    }

    /// Create a viewer for a read file block (file content with syntax highlighting and absolute line numbers, or error text if content is absent).
    pub fn for_read(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::Read(read)) = &entry.block else {
            return None;
        };

        let theme = Theme::current();

        // Error-only blocks: show the error text
        if !read.has_content() {
            let error = read.error.as_deref()?;
            let error_style = Style::default().fg(theme.accent_error);
            let lines: Vec<Line<'static>> = error
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), error_style)))
                .collect();
            return Some(Self::for_static_content(entry_id, ViewerKind::Read, lines));
        }

        let content = read.content.as_deref().unwrap_or("");
        let raw_lines: Vec<&str> = content.lines().collect();
        let base_line = read.line_range.map_or(1, |r| r.start);
        let max_line = base_line + raw_lines.len().saturating_sub(1);
        let gutter_width = max_line.checked_ilog10().map_or(1, |d| d as usize + 1);
        let gutter_style = Style::default().fg(theme.gray_dim);
        let fallback = Style::default().fg(theme.text_primary);

        let syntect = crate::syntax::get_syntect();
        let mut highlighter =
            syntect.highlight_lines_by_file_path(std::path::Path::new(&read.path));

        let lines: Vec<Line<'static>> = raw_lines
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let gutter = format!("{:>w$}  ", base_line + i, w = gutter_width);
                let mut spans = vec![Span::styled(gutter, gutter_style)];
                spans.extend(crate::syntax::highlight_line(
                    text,
                    &mut highlighter,
                    syntect,
                    fallback,
                ));
                Line::from(spans)
            })
            .collect();

        Some(Self::for_static_content(entry_id, ViewerKind::Read, lines))
    }

    fn static_lines_from_block(
        entry: &ScrollbackEntry,
        block: &impl BlockContent,
    ) -> Vec<Line<'static>> {
        let ctx = BlockContext {
            mode: DisplayMode::Expanded,
            is_running: entry.is_running,
            width: 120,
            raw: false,
            max_lines: None,
            appearance: Default::default(),
            is_selected: false,
            cwd: None,
        };
        block
            .output(&ctx)
            .lines
            .into_iter()
            .map(|bl| bl.content)
            .collect()
    }

    pub fn for_grep(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::Search(search)) = &entry.block else {
            return None;
        };

        let lines = Self::static_lines_from_block(entry, search);
        Some(Self::for_static_content(entry_id, ViewerKind::Grep, lines))
    }

    pub fn for_list_dir(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::ListDir(list_dir)) = &entry.block else {
            return None;
        };

        let lines = Self::static_lines_from_block(entry, list_dir);
        Some(Self::for_static_content(
            entry_id,
            ViewerKind::PlainText,
            lines,
        ))
    }

    /// Create a viewer for a web search block (search results and citations).
    pub fn for_web_search(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::WebSearch(ws)) = &entry.block else {
            return None;
        };

        let theme = Theme::current();
        let content = ws.content.as_deref().unwrap_or("");

        let mut lines: Vec<Line<'static>> = content
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme.text_primary),
                ))
            })
            .collect();

        // Citations footer (separated by a horizontal rule).
        if !ws.citations.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "───────────────────────────────────",
                Style::default().fg(theme.gray_dim),
            )));
            lines.push(Line::from(Span::styled(
                format!("Sources ({})", ws.citations.len()),
                Style::default().fg(theme.text_secondary),
            )));
            let url_style = Style::default().fg(theme.gray);
            let prefix_style = Style::default().fg(theme.text_secondary);
            for (i, url) in ws.citations.iter().enumerate() {
                let prefix = format!("[{}] ", i + 1);
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(url.clone(), url_style),
                ]));
            }
        }

        Some(Self::for_static_content(
            entry_id,
            ViewerKind::WebSearch,
            lines,
        ))
    }

    /// Create a viewer for a search_tool block. Preamble renders the styled header ("Search Tools
    /// query"). Body shows limit, result count, then the structured tool list.
    pub fn for_integration_search(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::IntegrationSearch(st)) = &entry.block else {
            return None;
        };

        let theme = Theme::current();
        let label = Style::default().fg(theme.text_secondary);
        let value = Style::default().fg(theme.text_primary);
        let dim = Style::default().fg(theme.gray);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Metadata
        if let Some(limit) = st.limit {
            lines.push(Line::from(vec![
                Span::styled("limit: ", label),
                Span::styled(limit.to_string(), value),
            ]));
        }
        let s = if st.result_count == 1 { "" } else { "s" };
        lines.push(Line::from(Span::styled(
            format!("{} result{s}", st.result_count),
            label,
        )));

        // Tool list
        for (i, tool) in st.results.iter().enumerate() {
            lines.push(Line::from(""));
            let action =
                mcp_titleize_segment(crate::scrollback::blocks::discovered_tool_action(tool));
            let server = mcp_titleize_segment(&tool.server);
            lines.push(Line::from(vec![
                Span::styled(format!("{}. ", i + 1), dim),
                Span::styled(action, Style::default().fg(theme.text_primary)),
                Span::styled(format!("  {}", server), dim),
            ]));
            if !tool.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("   {}", tool.description),
                    dim,
                )));
            }
        }

        // The error and rowless payloads (e.g. a large-output note) have nowhere else to show
        let (text, style) = match (&st.error, &st.content) {
            (Some(error), _) => (Some(error), Style::default().fg(theme.accent_error)),
            (None, content) if st.results.is_empty() => (content.as_ref(), dim),
            _ => (None, dim),
        };
        if let Some(text) = text {
            lines.push(Line::from(""));
            lines.extend(
                text.lines()
                    .map(|line| Line::from(Span::styled(line.to_owned(), style))),
            );
        }

        Some(Self::for_static_content(
            entry_id,
            ViewerKind::IntegrationSearch,
            lines,
        ))
    }

    /// Create a viewer for a use_tool block. Preamble renders the styled header ("server action"). Body
    /// shows input parameters, then the full output.
    pub fn for_use_tool(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::UseTool(ut)) = &entry.block else {
            return None;
        };

        let theme = Theme::current();
        let label = Style::default().fg(theme.text_secondary);
        let value = Style::default().fg(theme.text_primary);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Input parameters
        for (k, v) in &ut.input_args {
            lines.push(Line::from(vec![
                Span::styled(format!("{k}: "), label),
                Span::styled(v.clone(), value),
            ]));
        }

        // Output or error
        let content = ut.output.as_deref().or(ut.error.as_deref());
        if let Some(text) = content {
            if !ut.input_args.is_empty() {
                lines.push(Line::from(""));
            }
            let style = if ut.error.is_some() && ut.output.is_none() {
                Style::default().fg(theme.accent_error)
            } else {
                value
            };
            for line in text.lines() {
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
        }

        Some(Self::for_static_content(
            entry_id,
            ViewerKind::UseTool,
            lines,
        ))
    }

    /// Create a viewer for a background task (stdout from central store).
    ///
    /// Unlike other viewers that read from the scrollback entry, this one takes stdout directly and stores the `task_id` for streaming updates.
    pub fn for_bg_task(entry_id: EntryId, task_id: &str, stdout: &str, is_running: bool) -> Self {
        let config = ListPaneConfig {
            follow_enabled: is_running,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state = ListPaneState::new_with_config(WrapMode::Wrap, is_running, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let theme = Theme::current();
        let items = Self::build_execute_items(Some(stdout).filter(|s| !s.is_empty()), &theme);

        let list_style = ListPaneStyle {
            selection_bg: theme.bg_highlight,
            visual_select_bg: theme.bg_visual,
            uniform_visual_bg: true,
            ..ListPaneStyle::default()
        };

        Self {
            entry_id,
            kind: ViewerKind::BgTask,
            list_state,
            list_style,
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta: Vec::new(),
            last_generation: stdout.len() as u64,
            was_running: is_running,
            bg_task_id: Some(task_id.to_string()),
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        }
    }

    /// Create a viewer for plain text content (e.g., catalog entry).
    pub fn for_plain_text(title: &str, text: &str) -> Self {
        let config = ListPaneConfig {
            follow_enabled: false,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state = ListPaneState::new_with_config(WrapMode::Wrap, false, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let theme = Theme::current();
        let title_style = Style::default()
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD);
        let style = Style::default().fg(theme.text_primary);

        let mut items = vec![
            ContentLine {
                content: Line::from(Span::styled(title.to_owned(), title_style)),
                plain_text: title.to_owned(),
                id: u64::MAX,
                bg: None,
            },
            ContentLine {
                content: Line::default(),
                plain_text: String::new(),
                id: u64::MAX - 1,
                bg: None,
            },
        ];
        items.extend(text.lines().enumerate().map(|(i, line)| ContentLine {
            content: Line::from(Span::styled(line.to_owned(), style)),
            plain_text: line.to_owned(),
            id: i as u64,
            bg: None,
        }));

        Self {
            entry_id: EntryId::new(0),
            kind: ViewerKind::PlainText,
            list_state,
            list_style: ListPaneStyle {
                uniform_visual_bg: true,
                ..ListPaneStyle::default()
            },
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta: Vec::new(),
            last_generation: 0,
            was_running: false,
            bg_task_id: None,
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        }
    }

    /// Update a BgTask viewer with new stdout content. Called from the tick path when stdout in the
    /// central store has changed. Returns `true` if a redraw is needed.
    pub fn tick_bg_task(&mut self, stdout: &str, is_running: bool) -> bool {
        let mut needs_redraw = false;
        let current_len = stdout.len() as u64;

        if current_len != self.last_generation {
            self.last_generation = current_len;
            let theme = Theme::current();
            self.items = Self::build_execute_items(Some(stdout).filter(|s| !s.is_empty()), &theme);
            self.list_state.invalidate_layout();
            self.clear_stale_selection();
            needs_redraw = true;
        }

        // Transition from running to finished: disable follow
        if self.was_running && !is_running {
            self.was_running = false;
            self.list_state.disable_follow_permanently();
            if let Some(id) = self.last_nonempty_body_id() {
                self.list_state.select_by_id(id);
            }
            self.request_reveal_selection();
            needs_redraw = true;
        }

        needs_redraw
    }

    /// Build content lines from execute stdout output.
    fn build_execute_items(output: Option<&str>, theme: &Theme) -> Vec<ContentLine> {
        let Some(output) = output else {
            return Vec::new();
        };
        let base = ratatui::style::Style::default().fg(theme.text_primary);
        crate::render::terminal_output::render_terminal_lines(output, base)
            .into_iter()
            .enumerate()
            .map(|(i, rl)| ContentLine {
                content: rl.line,
                plain_text: rl.plain,
                id: i as u64,
                bg: Some(theme.bg_dark),
            })
            .collect()
    }

    /// Create a viewer for an edit block (diff content).
    pub fn for_edit(entry_id: EntryId, entry: &ScrollbackEntry) -> Option<Self> {
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return None;
        };
        if edit.hunks.is_empty() {
            return None;
        }

        let config = ListPaneConfig {
            follow_enabled: false,
            wrap_toggle_enabled: true,
            search_enabled: true,
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: true,
            filter_enabled: true,
            goto_line_enabled: false,
        };
        let mut list_state = ListPaneState::new_with_config(WrapMode::Wrap, false, config);
        list_state.set_clipboard_provider(Box::new(SystemClipboard));

        let theme = Theme::current();
        let (items, diff_meta) = Self::build_edit_items(edit, &theme);

        Some(Self {
            entry_id,
            kind: ViewerKind::Edit,
            list_state,
            list_style: ListPaneStyle {
                uniform_visual_bg: true,
                ..ListPaneStyle::default()
            },
            items,
            last_content_area: Rect::default(),

            raw_toggle_pending: false,
            copy_meta_pending: false,
            copy_content_pending: false,
            diff_meta,
            last_generation: 0,
            was_running: false,
            bg_task_id: None,
            last_theme: Theme::current_kind(),
            modal: ModalWindowState::new(),
            prepend_items: Vec::new(),
            text_drag: None,
            drag_copy_text: None,
            cached_unified: Vec::new(),
            click_count: 0,
            last_click_at: None,
            last_click_ep: None,
            reveal_selection_once: false,
        })
    }

    /// Build content lines and diff metadata from edit block diff hunks.
    fn build_edit_items(
        edit: &crate::scrollback::blocks::EditToolCallBlock,
        theme: &Theme,
    ) -> (Vec<ContentLine>, Vec<Option<DiffLineMeta>>) {
        use crate::scrollback::blocks::DiffRenderConfig;

        let config = DiffRenderConfig::default();
        // Block-owned dispatch so the viewer paints the same highlight phase (including the file-scoped upgrade) as the scrollback output
        let rendered = edit.render_diff_lines(
            theme, 500, // wide width, NoWrap mode
            &config,
        );

        // Build a flat list of DiffLine references from all hunks, interleaving None for separator lines (which render_diff_lines inserts)
        // The rendered output has: [hunk0 lines...] [separator] [hunk1 lines...] ...
        let mut meta_source: Vec<Option<&distill_pager_diff::DiffLine>> = Vec::new();
        for (i, hunk) in edit.hunks.iter().enumerate() {
            if i > 0 && !config.hunk_separator.is_empty() {
                meta_source.push(None); // separator line
            }
            for diff_line in hunk {
                meta_source.push(Some(diff_line));
                // Wrapped lines: render_diff_hunk_highlighted may produce multiple DiffLineOutput per DiffLine
                // For now, assume 1:1 mapping since we use width=500 (very wide, unlikely to wrap)
            }
        }

        let mut items = Vec::with_capacity(rendered.len());
        let mut diff_meta = Vec::with_capacity(rendered.len());

        for (i, dl) in rendered.into_iter().enumerate() {
            let plain: String = dl.line.spans.iter().map(|s| s.content.as_ref()).collect();
            items.push(ContentLine {
                content: dl.line,
                plain_text: plain,
                id: i as u64,
                bg: dl.background,
            });
            diff_meta.push(meta_source.get(i).copied().flatten().map(|d| DiffLineMeta {
                tag: d.tag,
                text: d.text.clone(),
                lo: d.lo,
                ln: d.ln,
            }));
        }

        (items, diff_meta)
    }

    /// Extract pre-wrap lines from a markdown block (thinking or agent message).
    fn extract_markdown_lines(block: &RenderBlock) -> Option<Vec<Line<'static>>> {
        match block {
            RenderBlock::Thinking(b) => Some(b.content().pre_wrap_lines()),
            RenderBlock::AgentMessage(b) => Some(b.content().pre_wrap_lines()),
            _ => None,
        }
    }

    /// Extract a change-detection counter from the block. For markdown blocks: content generation
    /// counter. For execute blocks: output byte length (no generation counter available).
    fn extract_generation(block: &RenderBlock) -> Option<u64> {
        match block {
            RenderBlock::Thinking(b) => Some(b.content().generation()),
            RenderBlock::AgentMessage(b) => Some(b.content().generation()),
            RenderBlock::ToolCall(ToolCallBlock::Execute(b)) => {
                Some(b.output.as_ref().map_or(0, |o| o.len()) as u64)
            }
            _ => None,
        }
    }

    /// Extract the line source map from a markdown block.
    fn extract_line_source_map(block: &RenderBlock) -> Option<Vec<usize>> {
        match block {
            RenderBlock::Thinking(b) => Some(b.content().line_source_map()),
            RenderBlock::AgentMessage(b) => Some(b.content().line_source_map()),
            _ => None,
        }
    }

    /// Rebuild items from the block's current content. Called after raw mode toggle or when content
    /// changes during streaming. Invalidates the ListPane layout cache since item content/heights may
    /// differ.
    pub fn rebuild_items(&mut self, entry: &ScrollbackEntry) {
        match self.kind {
            ViewerKind::Markdown => {
                if let Some(lines) = Self::extract_markdown_lines(&entry.block) {
                    self.items = ContentLine::from_lines(&lines);
                    self.list_state.invalidate_layout();
                    self.clear_stale_selection();
                }
            }
            ViewerKind::Execute => {
                if let RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) = &entry.block {
                    let theme = Theme::current();
                    self.items = Self::build_execute_items(exec.output.as_deref(), &theme);
                    self.list_state.invalidate_layout();
                    self.clear_stale_selection();
                }
            }
            ViewerKind::Edit => {}
            ViewerKind::BgTask => {}
            ViewerKind::Read
            | ViewerKind::Grep
            | ViewerKind::WebFetch
            | ViewerKind::WebSearch
            | ViewerKind::IntegrationSearch
            | ViewerKind::UseTool
            | ViewerKind::PlainText => {}
        }
    }

    /// Look up the source line number for a given stable_id (pre-wrap line index).
    ///
    /// Used by the caller to capture cursor position before a raw toggle.
    pub fn source_line_for_id(block: &RenderBlock, id: u64) -> Option<usize> {
        Self::extract_line_source_map(block).and_then(|map| map.get(id as usize).copied())
    }

    /// Jump the cursor to the first rendered line matching the given source line.
    ///
    /// Called after raw toggle and rebuild to restore cursor position.
    pub fn jump_to_source_line(&mut self, entry: &ScrollbackEntry, target: Option<usize>) {
        let Some(target_source) = target else {
            return;
        };
        if let Some(new_map) = Self::extract_line_source_map(&entry.block) {
            let new_idx = new_map
                .iter()
                .position(|&sl| sl >= target_source)
                .unwrap_or(0);
            self.list_state.select_by_id(new_idx as u64);
        }
    }

    /// Update viewer state on tick (streaming content, follow mode).
    ///
    /// Returns `true` if a redraw is needed.
    pub fn tick(&mut self, entry: &ScrollbackEntry) -> bool {
        let mut needs_redraw = false;

        // Check if content changed via generation counter.
        // Generation is bumped on every push_chunk, finish, or set_raw_mode.
        if let Some(current_gen) = Self::extract_generation(&entry.block)
            && current_gen != self.last_generation
        {
            self.last_generation = current_gen;
            self.rebuild_items(entry);
            needs_redraw = true;
        }

        // On the transition from running to finished: disable follow permanently, and select the last item so the user has a cursor at the bottom
        if self.was_running && !entry.is_running {
            self.was_running = false;
            self.list_state.disable_follow_permanently();
            // Layout may be stale after invalidate_layout, so we can't use select_last, which reads layout.item_count
            // select_by_id is resolved on next prepare_layout
            if let Some(id) = self.last_nonempty_body_id() {
                self.list_state.select_by_id(id);
            }
            self.request_reveal_selection();
            needs_redraw = true;
        }

        needs_redraw
    }

    /// Build shortcuts bar hints for this viewer.
    pub fn shortcuts_hints(&self) -> Vec<HintItem> {
        let mut hints = vec![
            HintItem::new(crate::key!(Esc), "close"),
            HintItem::new(crate::key!(Enter), "quote"),
            HintItem::new(crate::key!('/'), "search"),
            HintItem::new(crate::key!('f'), "filter"),
            HintItem::new(crate::key!('v'), "select"),
            HintItem::new(crate::key!('w'), "wrap"),
        ];
        match self.kind {
            ViewerKind::Markdown => {
                hints.push(HintItem::new(crate::key!('r'), "raw"));
            }
            ViewerKind::Execute => {
                hints.push(HintItem::new(crate::key!('Y'), "copy cmd"));
            }
            ViewerKind::Edit => {
                hints.push(HintItem::new(crate::key!('Y'), "copy path"));
            }
            ViewerKind::WebFetch => {
                hints.push(HintItem::new(crate::key!('Y'), "copy url"));
            }
            ViewerKind::WebSearch => {
                hints.push(HintItem::new(crate::key!('Y'), "copy query"));
            }
            ViewerKind::Read => {
                hints.push(HintItem::new(crate::key!('Y'), "copy path"));
            }
            ViewerKind::Grep => {
                hints.push(HintItem::new(crate::key!('Y'), "copy pattern"));
            }
            ViewerKind::BgTask => {}
            ViewerKind::IntegrationSearch | ViewerKind::UseTool | ViewerKind::PlainText => {}
        }
        hints
    }

    // -- Input handling ------------------------------------------------------

    /// Check if a key is a close signal (Esc/q/Ctrl-F).
    ///
    /// Separated from `handle_key` so the caller can close the viewer before routing the key (avoids borrow conflicts).
    pub fn is_close_key(&self, key: &KeyEvent) -> bool {
        // Ctrl-F: close viewer (toggle off)
        if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        let idle = self.list_state.input_mode().is_none() && !self.list_state.visual_mode;
        if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::NONE {
            return idle;
        }
        idle && !self.has_sticky_selection() && matches!(key.code, KeyCode::Esc)
    }

    fn ensure_body_cursor(&mut self) {
        if self.list_state.follow_mode {
            return;
        }
        if let Some(id) = self.list_state.selected_id()
            && self.contains_item_id(id)
        {
            return;
        }
        if let Some(first) = self.items.first() {
            self.list_state.select_by_id(first.id);
        }
    }

    pub(crate) fn contains_item_id(&self, id: u64) -> bool {
        self.prepend_items.iter().any(|item| item.id == id)
            || self.items.iter().any(|item| item.id == id)
    }

    pub(crate) fn request_reveal_selection(&mut self) {
        self.reveal_selection_once = true;
    }

    fn maybe_reveal_selection(&mut self) {
        if !self.reveal_selection_once {
            return;
        }
        self.reveal_selection_once = false;
        self.list_state.reveal_selection();
    }

    fn last_nonempty_body_id(&self) -> Option<u64> {
        self.items
            .iter()
            .rev()
            .find(|item| !item.plain_text.is_empty())
            .or_else(|| self.items.last())
            .map(|item| item.id)
    }

    pub(crate) fn resume_selected_id(&self) -> Option<u64> {
        self.list_state
            .selected_id()
            .or_else(|| self.last_nonempty_body_id())
    }

    pub(crate) fn pin_to_tail(&mut self) {
        self.list_state.follow_mode = false;
        if let Some(id) = self.last_nonempty_body_id() {
            self.list_state.select_by_id(id);
        }
        self.request_reveal_selection();
    }

    /// Handle a key event while the viewer is focused. Returns `true` if the key was consumed, `false`
    /// if it should bubble up. Close keys should be checked via `is_close_key` before calling this.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Esc)
            && self.list_state.input_mode().is_none()
            && !self.list_state.visual_mode
            && self.has_sticky_selection()
        {
            self.clear_text_drag();
            return true;
        }

        if self.kind == ViewerKind::Markdown
            && key.code == KeyCode::Char('r')
            && key.modifiers == KeyModifiers::NONE
            && self.list_state.input_mode().is_none()
        {
            self.clear_text_drag();
            self.raw_toggle_pending = true;
            return true;
        }

        // Y: copy meta (execute: command, edit: path); handled by caller
        if matches!(
            self.kind,
            ViewerKind::Execute
                | ViewerKind::Edit
                | ViewerKind::Read
                | ViewerKind::Grep
                | ViewerKind::WebFetch
                | ViewerKind::WebSearch
        ) && key.code == KeyCode::Char('Y')
            && key.modifiers == KeyModifiers::SHIFT
            && self.list_state.input_mode().is_none()
        {
            self.clear_text_drag();
            self.copy_meta_pending = true;
            return true;
        }

        // y: copy patch format (edit blocks); intercept before ListPane
        // Non-visual: copies full patch (same as scrollback `y`).
        // Visual: copies patch for the selected range.
        if self.kind == ViewerKind::Edit
            && key.code == KeyCode::Char('y')
            && key.modifiers == KeyModifiers::NONE
            && self.list_state.input_mode().is_none()
        {
            self.clear_text_drag();
            self.copy_content_pending = true;
            return true;
        }

        self.rebuild_unified_cache();
        let consumed = self.list_state.handle_key_event(key, &self.cached_unified);
        if consumed {
            self.clear_text_drag();
        }
        consumed
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.rebuild_unified_cache();
        let consumed = self.list_state.handle_paste(text, &self.cached_unified);
        if consumed {
            self.clear_text_drag();
        }
        consumed
    }

    /// Generate a patch string from the diff metadata in the given item range.
    ///
    /// Returns `None` if this isn't an edit viewer or the range has no diff lines.
    pub fn patch_from_range(&self, path: &str, range: std::ops::Range<usize>) -> Option<String> {
        if self.diff_meta.is_empty() {
            return None;
        }

        let mut out = String::new();
        out.push_str(&format!("--- a/{path}\n"));
        out.push_str(&format!("+++ b/{path}\n"));

        // Collect non-None entries in the range
        let entries: Vec<&DiffLineMeta> = range
            .filter_map(|i| self.diff_meta.get(i).and_then(|m| m.as_ref()))
            .collect();
        if entries.is_empty() {
            return None;
        }

        // Compute hunk header
        let old_start = entries
            .iter()
            .filter(|e| e.tag != similar::ChangeTag::Insert)
            .map(|e| e.lo)
            .next()
            .unwrap_or(1);
        let new_start = entries
            .iter()
            .filter(|e| e.tag != similar::ChangeTag::Delete)
            .map(|e| e.ln)
            .next()
            .unwrap_or(1);
        let old_count = entries
            .iter()
            .filter(|e| e.tag != similar::ChangeTag::Insert)
            .count();
        let new_count = entries
            .iter()
            .filter(|e| e.tag != similar::ChangeTag::Delete)
            .count();

        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));

        for entry in &entries {
            let prefix = match entry.tag {
                similar::ChangeTag::Equal => ' ',
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Delete => '-',
            };
            let text = entry.text.trim_end_matches(['\r', '\n']);
            out.push(prefix);
            out.push_str(text);
            out.push('\n');
        }

        Some(out)
    }

    /// Process pending actions (copy meta, copy content) and return text to copy. Call after
    /// `handle_key` or `handle_mouse`. The caller is responsible for clipboard operations and toast
    /// display. Returns `None` if no copy pending.
    pub fn process_pending_copy(&mut self, entry: &ScrollbackEntry) -> Option<String> {
        // Y: copy command (execute blocks)
        if self.copy_meta_pending {
            self.copy_meta_pending = false;
            return entry.block.copy_meta().filter(|t| !t.is_empty());
        }

        // y: copy content (edit blocks: patch format; others: block content)
        if self.copy_content_pending {
            self.copy_content_pending = false;
            let result = if self.kind == ViewerKind::Edit {
                // Patch from copy range (visual selection or current line).
                // copy_range() returns indices into the unified vec (prepend then items), but diff_meta is parallel to items only
                // Adjust by subtracting the prepend offset so we index diff_meta correctly
                let copy_range = self.list_state.copy_range();
                let path = match &entry.block {
                    RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) => Some(&edit.path),
                    _ => None,
                };
                match (copy_range, path) {
                    (Some(range), Some(path)) => {
                        let offset = self.prepend_items.len();
                        let adj_start = range.start.saturating_sub(offset);
                        let adj_end = range.end.saturating_sub(offset);
                        if adj_start < adj_end {
                            self.patch_from_range(path, adj_start..adj_end)
                        } else {
                            None
                        }
                    }
                    _ => entry.block.copy_text(entry.raw),
                }
            } else {
                entry.block.copy_text(entry.raw)
            };
            self.list_state.exit_visual_mode();
            return result.filter(|t| !t.is_empty());
        }

        None
    }

    #[cfg(test)]
    pub(crate) fn prepare_for_test(&mut self, area: Rect) {
        self.last_content_area = area;
        self.rebuild_unified_cache();
        self.ensure_body_cursor();
        self.list_state
            .prepare_layout(&self.cached_unified, area.width, area.height);
        self.maybe_reveal_selection();
    }

    #[cfg(test)]
    pub(crate) fn select_body_line_for_test(&mut self, body_idx: usize) {
        let Some(id) = self.items.get(body_idx).map(|item| item.id) else {
            return;
        };
        self.list_state.select_by_id(id);
        self.rebuild_unified_cache();
        let area = self.last_content_area;
        if area.width > 0 {
            self.list_state
                .prepare_layout(&self.cached_unified, area.width, area.height);
        }
    }

    pub(crate) fn install_prepend_lines(&mut self, prepend_lines: &[Line<'static>]) {
        if !self.prepend_items.is_empty() && self.prepend_items.len() != prepend_lines.len() {
            self.clear_text_drag();
        }
        self.prepend_items = prepend_lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                ContentLine {
                    content: line.clone(),
                    plain_text: plain,
                    id: u64::MAX - i as u64,
                    bg: line.style.bg,
                }
            })
            .collect();
        self.rebuild_unified_cache();
    }

    fn rebuild_unified_cache(&mut self) {
        self.cached_unified.clear();
        self.cached_unified
            .extend(self.prepend_items.iter().cloned());
        self.cached_unified.extend(self.items.iter().cloned());
    }

    pub fn handle_scroll(&mut self, lines: i32) {
        self.rebuild_unified_cache();
        self.list_state.scroll_lines(lines, &self.cached_unified);
    }
}

impl BlockViewerPane {
    // -- Rendering -----------------------------------------------------------

    /// They get their own stable IDs in the high half of the u64 space so they don't conflict with
    /// content item IDs.
    pub fn render_content(
        &mut self,
        content_area: Rect,
        buf: &mut Buffer,
        entry: &ScrollbackEntry,
        focused: bool,
        prepend_lines: &[Line<'static>],
    ) {
        let theme = Theme::current();

        // Detect theme switch and rebuild items and list style
        let current_theme = Theme::current_kind();
        if current_theme != self.last_theme {
            self.last_theme = current_theme;
            self.list_style = match self.kind {
                ViewerKind::BgTask | ViewerKind::Execute => ListPaneStyle {
                    selection_bg: theme.bg_highlight,
                    visual_select_bg: theme.bg_visual,
                    uniform_visual_bg: true,
                    ..ListPaneStyle::default()
                },
                _ => ListPaneStyle {
                    uniform_visual_bg: true,
                    ..ListPaneStyle::default()
                },
            };
            match self.kind {
                ViewerKind::Execute => {
                    let output = entry.block.copy_text(entry.raw);
                    self.items = Self::build_execute_items(
                        output.as_deref().filter(|s| !s.is_empty()),
                        &theme,
                    );
                    self.list_state.invalidate_layout();
                }
                ViewerKind::BgTask => {
                    self.last_generation = u64::MAX;
                }
                _ => {}
            }
        }

        self.last_content_area = content_area;

        // Cache prepend lines as ContentLines so the input handlers (scroll / mouse / key) can rebuild the same unified vec the ListPane saw
        // Otherwise they index a smaller `items` vec and panic when the cursor / scroll math points past its end
        // Header IDs are placed in the high u64 range so they never collide with content IDs (which start at 0 and grow upward)
        self.install_prepend_lines(prepend_lines);
        self.ensure_body_cursor();

        if content_area.height > 0 && content_area.width > 0 && !self.cached_unified.is_empty() {
            let likely_scrollbar = self.cached_unified.len() > content_area.height as usize;
            let render_area = if likely_scrollbar {
                Rect {
                    width: content_area.width + SCROLLBAR_TOTAL_COLS.min(2),
                    ..content_area
                }
            } else {
                content_area
            };

            self.list_state.prepare_layout(
                &self.cached_unified,
                render_area.width,
                render_area.height,
            );

            let render_area = if !likely_scrollbar
                && self.list_state.total_height() > render_area.height as usize
            {
                let wider = Rect {
                    width: content_area.width + SCROLLBAR_TOTAL_COLS.min(2),
                    ..content_area
                };
                self.list_state
                    .prepare_layout(&self.cached_unified, wider.width, wider.height);
                wider
            } else {
                render_area
            };

            self.maybe_reveal_selection();

            ListPane::new(&self.cached_unified)
                .focused(focused)
                .style(self.list_style)
                .render(render_area, buf, &mut self.list_state);
        }
    }
}

#[cfg(test)]
#[path = "block_viewer_tests.rs"]
mod tests;
