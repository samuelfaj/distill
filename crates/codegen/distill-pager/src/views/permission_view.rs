// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use distill_workspace::permission::bash_command_splitting::{
    BashCommandHighlights, heredoc_payload_byte_ranges, range_fully_inside,
    soft_break_offsets_after_operators,
};
use distill_workspace::permission::{
    ALLOW_EDITS_SESSION_OPTION_ID, BashCommandPermission, McpToolPermission, mcp_titleize_segment,
    mcp_tool_action, mcp_tool_display_name,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionFocus {
    Options,
    FollowupInput,
    PatternEdit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternEditState {
    pub buffer: String,
    pub cursor: usize,
    dirty: bool,
}

impl PatternEditState {
    pub fn new(initial: impl Into<String>) -> Self {
        let buffer = initial.into();
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            dirty: false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn trimmed(&self) -> Option<&str> {
        let t = self.buffer.trim();
        (!t.is_empty()).then_some(t)
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(..self.cursor)
            .and_then(|s| s.chars().next_back())
        {
            self.cursor -= ch.len_utf8();
            self.buffer.remove(self.cursor);
            self.dirty = true;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
            self.dirty = true;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(..self.cursor)
            .and_then(|s| s.chars().next_back())
        {
            self.cursor -= ch.len_utf8();
        }
    }

    pub fn move_right(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(self.cursor..)
            .and_then(|s| s.chars().next())
        {
            self.cursor += ch.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn clear(&mut self) {
        if !self.buffer.is_empty() {
            self.dirty = true;
        }
        self.buffer.clear();
        self.cursor = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    Tool,
    Server,
}

#[derive(Debug, Clone)]
pub struct McpScopeState {
    pub tool_name: String,
    pub server_prefix: Option<String>,
    pub selected: McpScope,
}

impl McpScopeState {
    pub fn action(&self) -> &str {
        mcp_tool_action(&self.tool_name, self.server_prefix.as_deref())
    }

    pub fn display_name(&self) -> String {
        mcp_tool_display_name(&self.tool_name, self.server_prefix.as_deref())
    }
}

pub struct PermissionViewState {
    pub request: distill_acp_lib::AcpArgs<acp::RequestPermissionRequest>,

    pub id: usize,

    pub focus: PermissionFocus,

    pub options: Vec<acp::PermissionOption>,

    pub active_idx: usize,

    pub bash_highlights: Option<BashCommandHighlights>,

    pub bash_selection_count: usize,

    pub bash_deny_selection_count: usize,

    pub bash_command_raw: Option<String>,

    pub mcp_scope: Option<McpScopeState>,

    pub title: String,

    pub description: Vec<String>,

    pub args_expanded: bool,

    pub desc_scroll: u16,

    pub subagent_label: Option<String>,

    pub options_area_height: usize,

    pub options_scroll_offset: usize,
}

pub const ALLOW_ALWAYS_COMMAND_OPTION_ID: &str = "allow-always-command";
pub const REJECT_ALWAYS_COMMAND_OPTION_ID: &str = "reject-always-command";
pub const ALLOW_ALWAYS_MCP_OPTION_ID: &str = "allow-always-mcp";

impl PermissionViewState {
    pub fn has_adjustable_scope(&self) -> bool {
        let has_row = |id: &str| self.options.iter().any(|o| o.option_id.0.as_ref() == id);
        let bash_adjustable = self.bash_highlights.as_ref().is_some_and(|h| {
            let len = h.highlighted_words.len();
            let allow_adjustable = has_row(ALLOW_ALWAYS_COMMAND_OPTION_ID)
                && (1..=len)
                    .filter(|&n| distill_workspace::permission::always_allow_scope_persists(h, n))
                    .nth(1)
                    .is_some();
            let deny_adjustable = has_row(REJECT_ALWAYS_COMMAND_OPTION_ID) && len > 1;
            allow_adjustable || deny_adjustable
        });
        bash_adjustable
            || self
                .mcp_scope
                .as_ref()
                .is_some_and(|s| s.server_prefix.is_some())
    }

    pub fn has_editable_bash_pattern(&self) -> bool {
        self.bash_highlights.is_some() && self.allow_always_command_idx().is_some()
    }

    pub fn is_scoped_option(&self, option: &acp::PermissionOption) -> bool {
        let id = option.option_id.0.as_ref();
        if self.mcp_scope.is_some() {
            id == ALLOW_ALWAYS_MCP_OPTION_ID
        } else {
            matches!(
                id,
                ALLOW_ALWAYS_COMMAND_OPTION_ID | REJECT_ALWAYS_COMMAND_OPTION_ID
            )
        }
    }

    pub fn scoped_row_jump_idx(&self) -> Option<usize> {
        if let Some(idx) = self.scoped_allow_row_idx() {
            return Some(idx);
        }
        if self.mcp_scope.is_some() {
            return None;
        }
        self.options
            .iter()
            .position(|o| o.option_id.0.as_ref() == REJECT_ALWAYS_COMMAND_OPTION_ID)
    }

    pub fn scoped_allow_row_idx(&self) -> Option<usize> {
        let target = if self.mcp_scope.is_some() {
            ALLOW_ALWAYS_MCP_OPTION_ID
        } else {
            ALLOW_ALWAYS_COMMAND_OPTION_ID
        };
        self.options
            .iter()
            .position(|o| o.option_id.0.as_ref() == target)
    }

    pub fn allow_always_command_idx(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|o| o.option_id.0.as_ref() == ALLOW_ALWAYS_COMMAND_OPTION_ID)
    }

    pub fn step_persisting_allow_scope(&self, right: bool) -> usize {
        use distill_workspace::permission::always_allow_scope_persists;
        let current = self.bash_selection_count;
        let Some(h) = self.bash_highlights.as_ref() else {
            return current;
        };
        if right {
            (current + 1..=h.highlighted_words.len()).find(|&n| always_allow_scope_persists(h, n))
        } else {
            (1..current)
                .rev()
                .find(|&n| always_allow_scope_persists(h, n))
        }
        .unwrap_or(current)
    }

    pub fn has_collapsible_bash(&self, content_w: usize) -> bool {
        self.bash_command_raw.as_deref().is_some_and(|raw| {
            count_raw_bash_rows(raw, content_w, PERMISSION_COLLAPSED_ROWS + 1)
                > PERMISSION_COLLAPSED_ROWS
        })
    }

    pub fn has_collapsible_display(&self, content_w: usize) -> bool {
        let has_bash_body = self.bash_highlights.is_some() || self.bash_command_raw.is_some();
        let is_edit_prompt = self
            .options
            .iter()
            .any(|o| o.option_id.0.as_ref() == ALLOW_EDITS_SESSION_OPTION_ID);
        let mcp_args = !self.description.is_empty() && !has_bash_body && !is_edit_prompt;
        mcp_args || self.has_collapsible_bash(content_w)
    }
}

fn shortcut_char(index: usize) -> char {
    if index < 9 {
        char::from(b'1' + index as u8)
    } else {
        ' '
    }
}

const SHORTCUT_LABELS: [&str; 10] = ["  ", "1 ", "2 ", "3 ", "4 ", "5 ", "6 ", "7 ", "8 ", "9 "];

fn shortcut_label(index: usize) -> &'static str {
    SHORTCUT_LABELS
        .get(index + 1)
        .copied()
        .unwrap_or(SHORTCUT_LABELS[0])
}

pub use crate::app::subagent::SubagentInfo;

pub fn permission_chrome_height_pub(
    state: &PermissionViewState,
    content_w: usize,
    area_h: u16,
) -> u16 {
    let uncapped = permission_chrome_height(state, content_w);
    let options_and_pad = state.options.len() as u16 + 1;
    let max_chrome = area_h.saturating_sub(options_and_pad);
    uncapped.min(max_chrome)
}

fn permission_chrome_height(state: &PermissionViewState, content_w: usize) -> u16 {
    let (bash_rows, bash_indicator) = bash_visible_rows(state, content_w);
    let bash_line_count = bash_rows
        .saturating_add(bash_indicator as usize)
        .min(u16::MAX as usize) as u16;
    let mut h: u16 = 1;
    if state.subagent_label.is_some() {
        h += 1;
    }
    h += 1;
    h = h.saturating_add(bash_line_count);
    let (args_rows, indicator) = mcp_args_visible_rows(state, content_w);
    let args_rows = args_rows
        .saturating_add(indicator as usize)
        .min(u16::MAX as usize) as u16;
    h = h.saturating_add(args_rows);
    if state.focus == PermissionFocus::PatternEdit {
        h = h.saturating_add(2);
    } else if state.has_adjustable_scope() || state.has_editable_bash_pattern() {
        h = h.saturating_add(1);
    }
    h.saturating_add(1)
}

pub fn permission_view_height(state: &PermissionViewState, screen_h: u16, content_w: usize) -> u16 {
    let chrome_h = permission_chrome_height(state, content_w);
    let options_h = state.options.len() as u16;
    let vpad_bottom: u16 = 1;
    let total = chrome_h
        .saturating_add(options_h)
        .saturating_add(vpad_bottom);

    if state.args_expanded {
        return total.min(screen_h);
    }
    let cap = (screen_h as u32 / 2)
        .max(10)
        .min(screen_h as u32 * 80 / 100) as u16;
    total.min(cap)
}

pub const PERMISSION_COLLAPSED_ROWS: usize = 5;

fn mcp_args_visible_rows(state: &PermissionViewState, content_w: usize) -> (usize, bool) {
    let total: usize = state
        .description
        .iter()
        .map(|raw| char_wrap_row_count(raw, content_w))
        .sum();
    if !state.args_expanded && total > PERMISSION_COLLAPSED_ROWS {
        (PERMISSION_COLLAPSED_ROWS - 1, true)
    } else {
        (total, false)
    }
}

fn bash_visible_rows(state: &PermissionViewState, content_w: usize) -> (usize, bool) {
    if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
        let Some(raw) = state.bash_command_raw.as_deref() else {
            return (0, false);
        };
        if state.args_expanded {
            return (count_raw_bash_rows(raw, content_w, usize::MAX), false);
        }
        let capped = count_raw_bash_rows(raw, content_w, PERMISSION_COLLAPSED_ROWS + 1);
        if capped > PERMISSION_COLLAPSED_ROWS {
            (PERMISSION_COLLAPSED_ROWS - 1, true)
        } else {
            (capped, false)
        }
    } else if state.mcp_scope.is_some() {
        (1, false)
    } else {
        (0, false)
    }
}

fn char_wrap_row_count(s: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut cur_w = 0usize;
    let mut cur_empty = true;
    for ch in s.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + ch_w > width && !cur_empty {
            rows += 1;
            cur_w = 0;
        }
        cur_w += ch_w;
        cur_empty = false;
    }
    rows
}

fn hovered_bg(theme: &Theme) -> ratatui::style::Color {
    theme.bg_hover
}

pub struct PermissionRenderResult {
    pub inline_prompt: Option<InlinePromptArea>,
}

pub struct InlinePromptArea {
    pub text_x: u16,
    pub y: u16,
    pub text_w: u16,
    pub content_x: u16,
    pub content_w: u16,
}

pub fn inline_text_width(area_width: u16) -> u16 {
    const LEFT_PAD: u16 = 3;
    const PREFIX_W: u16 = 8;
    area_width.saturating_sub(LEFT_PAD + PREFIX_W)
}

pub fn render_permission_view(
    buf: &mut Buffer,
    area: Rect,
    state: &PermissionViewState,
    followup_text: &str,
    pattern_edit: Option<&PatternEditState>,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
) -> PermissionRenderResult {
    if area.height == 0 || area.width == 0 {
        return PermissionRenderResult {
            inline_prompt: None,
        };
    }

    let is_followup = state.focus == PermissionFocus::FollowupInput;
    let pattern_edit = pattern_edit.filter(|_| state.focus == PermissionFocus::PatternEdit);

    let bg = Style::default().bg(theme.bg_light);
    buf.set_style(area, bg);

    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar());
            cell.set_style(accent_style);
        }
    }

    let content_x = area.x + 3;
    let content_width = area.width.saturating_sub(5);
    let mut y = area.y;

    y += 1;

    let area_bottom = area.y + area.height;

    if let Some(ref label) = state.subagent_label {
        if y < area_bottom {
            let prov_style = Style::default().fg(theme.gray);
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(label.clone(), prov_style)),
                content_width,
            );
        }
        y += 1;
    }

    if y < area_bottom {
        let title_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);
        buf.set_line(
            content_x,
            y,
            &Line::from(Span::styled(state.title.clone(), title_style)),
            content_width,
        );
    }
    y += 1;

    let (bash_rows, bash_indicator) = bash_visible_rows(state, content_width as usize);
    let mut bash_lines: Vec<Line<'_>> =
        if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
            build_permission_bash_lines(
                state.bash_command_raw.as_deref(),
                content_width as usize,
                bash_rows,
            )
        } else if let Some(ref scope) = state.mcp_scope {
            build_mcp_scope_lines(scope, theme, content_width as usize)
        } else {
            Vec::new()
        };
    if bash_indicator {
        bash_lines.push(truncation_indicator_line(theme));
    }
    {
        let (args_rows, indicator) = mcp_args_visible_rows(state, content_width as usize);
        bash_lines.extend(build_mcp_args_lines(
            &state.description,
            theme,
            content_width as usize,
            args_rows,
        ));
        if indicator {
            bash_lines.push(truncation_indicator_line(theme));
        }
    }

    let show_scope_hint = state.has_adjustable_scope();
    let show_edit_hint = state.has_editable_bash_pattern();
    let header_extra_h: u16 = if pattern_edit.is_some() {
        2
    } else if show_scope_hint || show_edit_hint {
        1
    } else {
        0
    };
    let options_reserve = header_extra_h + 1 + state.options.len() as u16 + 1;
    let max_bash_y = (area.y + area.height).saturating_sub(options_reserve);

    let mut last_drawn_bash: Option<usize> = None;
    for (li, bash_line) in bash_lines.iter().enumerate() {
        if y >= max_bash_y {
            break;
        }
        buf.set_line(content_x, y, bash_line, content_width);
        last_drawn_bash = Some(li);
        y += 1;
    }
    if let Some(last_idx) = last_drawn_bash
        && last_idx + 1 < bash_lines.len()
    {
        let text_w = bash_lines
            .get(last_idx)
            .map(|l| l.width() as u16)
            .unwrap_or(0);
        let ellipsis_x = content_x + text_w.min(content_width.saturating_sub(2));
        let ellipsis_style = Style::default().fg(theme.gray);
        buf.set_span(
            ellipsis_x,
            y - 1,
            &Span::styled(" \u{2026}", ellipsis_style),
            2,
        );
    }
    if let Some(edit) = pattern_edit {
        if y < area.y + area.height {
            render_pattern_editor_line(buf, content_x, y, content_width, edit, theme);
            y += 1;
        }
        if y < area.y + area.height {
            let command = preview_command_text(state);
            render_pattern_preview_line(buf, content_x, y, content_width, edit, &command, theme);
            y += 1;
        }
    } else if (show_scope_hint || show_edit_hint) && y < area.y + area.height {
        let hint_style = Style::default()
            .fg(theme.text_secondary)
            .add_modifier(Modifier::DIM);
        let key_style = Style::default().fg(theme.accent_user);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if show_scope_hint {
            spans.push(Span::styled("\u{2190} \u{2192}", key_style));
            spans.push(Span::styled(" narrow scope", hint_style));
        }
        if show_edit_hint {
            if show_scope_hint {
                spans.push(Span::styled("  \u{00b7}  ", hint_style));
            }
            spans.push(Span::styled("e", key_style));
            spans.push(Span::styled(" edit pattern", hint_style));
        }
        buf.set_line(content_x, y, &Line::from(spans), content_width);
        y += 1;
    }

    y += 1;

    let visible_bottom = area.y + area.height;
    let hover_bg = hovered_bg(theme);

    let selected_words: Option<String> = state.bash_highlights.as_ref().map(|h| {
        allow_scope_label(
            h,
            state.bash_command_raw.as_deref(),
            state.bash_selection_count,
        )
    });
    let deny_selected_words: Option<String> = state.bash_highlights.as_ref().map(|h| {
        h.highlighted_words
            .get(..state.bash_deny_selection_count)
            .unwrap_or(&[])
            .join(" ")
    });

    let mut inline_prompt_result: Option<InlinePromptArea> = None;

    for (i, option) in state.options.iter().enumerate() {
        if y >= visible_bottom {
            break;
        }

        if is_followup && option.kind == acp::PermissionOptionKind::RejectOnce {
            // Always-focused input row. No reverse-video overlay here: it is
            // an editable line (hardware cursor + accent bar carry focus),
            // so the terminal theme simply renders it bandless.
            let row_bg = theme.bg_visual;

            let full_row = Rect {
                x: area.x + 1,
                y,
                width: area.width.saturating_sub(1),
                height: 1,
            };
            buf.set_style(full_row, Style::default().bg(row_bg));

            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_symbol(crate::glyphs::accent_bar());
                cell.set_style(Style::default().fg(theme.accent_user).bg(row_bg));
            }

            let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
            let marker_style = Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            let prompt_ind = Style::default().fg(theme.accent_user).bg(row_bg);
            buf.set_span(content_x, y, &Span::styled(shortcut_label(i), num_style), 2);
            buf.set_span(
                content_x + 2,
                y,
                &Span::styled(format!("({}) ", crate::glyphs::filled_dot()), marker_style),
                4,
            );
            buf.set_span(
                content_x + 6,
                y,
                &Span::styled(crate::glyphs::prompt_arrow(), prompt_ind),
                2,
            );

            let prefix_w: u16 = 8;
            let full_w = area.width.saturating_sub(3);
            inline_prompt_result = Some(InlinePromptArea {
                text_x: content_x + prefix_w,
                y,
                text_w: full_w.saturating_sub(prefix_w),
                content_x,
                content_w: full_w,
            });

            y += 1;
            continue;
        }

        let is_cursor = i == state.active_idx;
        let is_hovered = hovered_item == Some(i);
        // When the panel is unfocused, drop the cursor-row overlay so it
        // reads as "no active selection" - same rule as question_view.
        let row_bg = if is_hovered && !(is_cursor && focused) {
            hover_bg
        } else {
            theme.bg_light
        };

        let row_words = if option.option_id.0.as_ref() == REJECT_ALWAYS_COMMAND_OPTION_ID {
            deny_selected_words.as_deref()
        } else {
            selected_words.as_deref()
        };
        let line = build_permission_option_line(
            option,
            i,
            is_cursor,
            row_bg,
            row_words,
            state.mcp_scope.as_ref(),
            followup_text,
            content_width,
            theme,
        );

        let row_rect = Rect {
            x: content_x,
            y,
            width: content_width,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(row_bg));
        buf.set_line(content_x, y, &line, content_width);
        // Band on RGB themes; reverse video on the terminal theme.
        if is_cursor && focused {
            buf.set_style(row_rect, theme.selection_overlay());
        } else if is_hovered {
            buf.set_style(row_rect, theme.hover_overlay());
        }
        y += 1;
    }

    if !focused {
        crate::render::color::recede_area(buf, area, theme.bg_light, 0.66);
    }

    PermissionRenderResult {
        inline_prompt: inline_prompt_result,
    }
}

pub(crate) fn preview_command_text(state: &PermissionViewState) -> String {
    match state.bash_highlights.as_ref() {
        Some(h) => distill_workspace::permission::bash_command_splitting::unwrap_command_wrappers(
            &h.highlighted_words,
        )
        .join(" "),
        None => state.bash_command_raw.clone().unwrap_or_default(),
    }
}

fn render_pattern_editor_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    theme: &Theme,
) {
    let prompt_style = Style::default().fg(theme.accent_user);
    buf.set_span(content_x, y, &Span::styled("\u{276f} ", prompt_style), 2);

    let text_x = content_x + 2;
    let window = content_width.saturating_sub(2) as usize;
    if window == 0 {
        return;
    }

    let chars: Vec<char> = edit.buffer.chars().collect();
    let cursor_idx = edit
        .buffer
        .get(..edit.cursor)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let start = (cursor_idx + 1).saturating_sub(window);

    let text_style = Style::default().fg(theme.text_primary);
    // Accent-colored block on RGB themes; reverse video on the bandless
    // palette, whose Reset accent would paint no block at all.
    let caret_style = if theme.is_bandless() {
        theme.block_cursor_over(theme.bg_light)
    } else {
        Style::default().fg(theme.bg_light).bg(theme.accent_user)
    };

    let end = (start + window).min(chars.len());
    let mut col: u16 = 0;
    for (offset, ch) in chars.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let idx = start + offset;
        let style = if idx == cursor_idx {
            caret_style
        } else {
            text_style
        };
        buf.set_span(text_x + col, y, &Span::styled(ch.to_string(), style), 1);
        col += 1;
    }
    if cursor_idx >= chars.len() && (col as usize) < window {
        buf.set_span(text_x + col, y, &Span::styled(" ", caret_style), 1);
    }
}

fn render_pattern_preview_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    command: &str,
    theme: &Theme,
) {
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    let sep = Span::styled("  \u{00b7}  ", dim);

    let mut spans: Vec<Span<'static>> = Vec::new();
    match edit.trimmed() {
        None => {
            spans.push(Span::styled(
                "type a command pattern to allow (e.g. gh api repos/*)",
                dim,
            ));
        }
        Some(pattern) if distill_workspace::permission::bash_glob_is_catchall(pattern) => {
            spans.push(Span::styled(
                "\u{2717} matches everything, won't be saved",
                Style::default().fg(theme.accent_error),
            ));
            spans.push(sep);
            spans.push(Span::styled("Esc", Style::default().fg(theme.accent_user)));
            spans.push(Span::styled(" cancel", dim));
        }
        Some(pattern) => {
            if distill_workspace::permission::bash_pattern_matches_command(pattern, command) {
                spans.push(Span::styled(
                    "\u{2713} matches this command",
                    Style::default().fg(theme.accent_success),
                ));
            } else {
                spans.push(Span::styled(
                    "\u{2717} won't match this command",
                    Style::default().fg(theme.accent_error),
                ));
            }
            if distill_workspace::permission::bash_pattern_is_broad(pattern) {
                spans.push(sep.clone());
                spans.push(Span::styled(
                    "\u{26a0} very broad",
                    Style::default().fg(theme.warning),
                ));
            }
            spans.push(sep);
            spans.push(Span::styled(
                "Enter",
                Style::default().fg(theme.accent_user),
            ));
            spans.push(Span::styled(" save  ", dim));
            spans.push(Span::styled("Esc", Style::default().fg(theme.accent_user)));
            spans.push(Span::styled(" cancel", dim));
        }
    }
    buf.set_line(content_x, y, &Line::from(spans), content_width);
}

pub(crate) fn render_bash_command_display_lines(
    command: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    build_raw_bash_lines(command, content_width, usize::MAX)
}

fn build_permission_bash_lines(
    raw: Option<&str>,
    content_width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    match raw {
        Some(command) => build_raw_bash_lines(command, content_width, max_rows),
        None => Vec::new(),
    }
}

fn prepare_bash_display_text(command: &str) -> String {
    let normalized = command.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    while out.ends_with('\n') {
        let Some(without) = out.len().checked_sub(1).and_then(|n| out.get(..n)) else {
            break;
        };
        if without.ends_with('\\') {
            out.pop();
            break;
        }
        if without.is_empty() || without.ends_with('\n') {
            out.pop();
            continue;
        }
        out.pop();
        break;
    }
    out
}

fn soft_wrap_row_texts<'a>(
    line: &'a str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
    max_rows: usize,
) -> Vec<&'a str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if content_width == 0 {
        return vec![line];
    }

    if line.len() <= content_width {
        return vec![line];
    }

    let line_end = line_start + line.len();
    if range_fully_inside(line_start, line_end, heredoc_payload) {
        return vec![line];
    }

    let first_inside = full_breaks.partition_point(|&b| b <= line_start);
    let mut bounds = full_breaks
        .get(first_inside..)
        .unwrap_or(&[])
        .iter()
        .copied()
        .take_while(|&b| b < line_end)
        .map(|b| b - line_start)
        .filter(|&b| line.is_char_boundary(b))
        .chain(std::iter::once(line.len()))
        .peekable();

    if bounds.peek().copied() == Some(line.len()) {
        return bash_quote_aware_wrap(line, content_width, max_rows);
    }

    let mut out: Vec<&'a str> = Vec::new();
    let mut pos = 0usize;
    let mut first_row = true;
    while pos < line.len() && out.len() < max_rows {
        let mut start = pos;
        if !first_row {
            while start < line.len()
                && line
                    .as_bytes()
                    .get(start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                start += 1;
            }
            while bounds.peek().is_some_and(|&b| b <= start) {
                bounds.next();
            }
            if start >= line.len() {
                break;
            }
        }
        first_row = false;

        let Some(mut end) = bounds.next() else {
            break;
        };
        if UnicodeWidthStr::width(line.get(start..end).unwrap_or("")) <= content_width {
            while let Some(&next_end) = bounds.peek() {
                if UnicodeWidthStr::width(line.get(start..next_end).unwrap_or("")) <= content_width
                {
                    end = next_end;
                    bounds.next();
                } else {
                    break;
                }
            }
            out.push(line.get(start..end).unwrap_or("").trim_end());
        } else {
            let row = line.get(start..end).unwrap_or("").trim_end();
            if UnicodeWidthStr::width(row) <= content_width {
                out.push(row);
            } else {
                out.extend(bash_quote_aware_wrap(
                    row,
                    content_width,
                    max_rows - out.len(),
                ));
            }
        }
        pos = end;
    }
    out
}

fn bash_quote_aware_wrap(line: &str, width: usize, max_rows: usize) -> Vec<&str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if width == 0 || line.len() <= width {
        return vec![line];
    }

    let mut break_points = QuoteAwareBreakPoints::new(line).peekable();
    if break_points.peek().is_none() {
        return vec![line];
    }

    let mut rows: Vec<&str> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break = 0usize;

    let candidates = break_points.chain(std::iter::once(line.len()));

    for b in candidates {
        if b <= row_start {
            continue;
        }
        let candidate = line.get(row_start..b).unwrap_or("").trim_end();
        if UnicodeWidthStr::width(candidate) <= width {
            last_break = b;
            continue;
        }
        if last_break > row_start {
            let row = line.get(row_start..last_break).unwrap_or("").trim_end();
            if !row.is_empty() {
                rows.push(row);
                if rows.len() >= max_rows {
                    return rows;
                }
            }
            row_start = last_break;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
            if b > row_start {
                let candidate = line.get(row_start..b).unwrap_or("").trim_end();
                if UnicodeWidthStr::width(candidate) <= width {
                    last_break = b;
                } else {
                    let force_end = b;
                    let row = line.get(row_start..force_end).unwrap_or("").trim_end();
                    if !row.is_empty() {
                        rows.push(row);
                        if rows.len() >= max_rows {
                            return rows;
                        }
                    }
                    row_start = force_end;
                    while row_start < line.len()
                        && line
                            .as_bytes()
                            .get(row_start)
                            .is_some_and(|b| b.is_ascii_whitespace())
                    {
                        row_start += 1;
                    }
                    last_break = row_start;
                }
            }
        } else {
            let row = line.get(row_start..b).unwrap_or("").trim_end();
            if !row.is_empty() {
                rows.push(row);
                if rows.len() >= max_rows {
                    return rows;
                }
            }
            row_start = b;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
        }
    }
    if row_start < line.len() && rows.len() < max_rows {
        let row = line.get(row_start..).unwrap_or("").trim_end();
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() { vec![line] } else { rows }
}

struct QuoteAwareBreakPoints<'a> {
    bytes: &'a [u8],
    i: usize,
    in_single: bool,
    in_double: bool,
}

impl<'a> QuoteAwareBreakPoints<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            bytes: line.as_bytes(),
            i: 0,
            in_single: false,
            in_double: false,
        }
    }
}

impl Iterator for QuoteAwareBreakPoints<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.i < self.bytes.len() {
            let c = *self.bytes.get(self.i)?;
            if self.in_single {
                if c == b'\'' {
                    self.in_single = false;
                }
                self.i += 1;
                continue;
            }
            if self.in_double {
                if c == b'\\' && self.i + 1 < self.bytes.len() {
                    self.i += 2;
                    continue;
                }
                if c == b'"' {
                    self.in_double = false;
                }
                self.i += 1;
                continue;
            }
            match c {
                b'\'' => {
                    self.in_single = true;
                    self.i += 1;
                }
                b'"' => {
                    self.in_double = true;
                    self.i += 1;
                }
                b if b.is_ascii_whitespace() => {
                    let start = self.i;
                    while self.i < self.bytes.len()
                        && self
                            .bytes
                            .get(self.i)
                            .is_some_and(|b| b.is_ascii_whitespace())
                    {
                        self.i += 1;
                    }
                    if start > 0 {
                        return Some(start);
                    }
                }
                _ => self.i += 1,
            }
        }
        None
    }
}

fn build_raw_bash_lines(
    command: &str,
    content_width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let text = prepare_bash_display_text(command);
    if text.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let full_breaks = soft_break_offsets_after_operators(&text);
    let heredoc_payload = heredoc_payload_byte_ranges(&text);

    let syntect = crate::syntax::get_syntect();
    let fallback = Style::default().fg(Theme::current().command);
    let grammar = if cfg!(windows) { "powershell" } else { "bash" };
    let mut hl = syntect
        .highlight_lines_for_token(grammar)
        .or_else(|| syntect.highlight_lines_for_token("bash"));

    let mut out = Vec::new();
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if out.len() >= max_rows {
            break;
        }
        if idx > 0 {
            offset += 1;
        }
        let spans = crate::syntax::highlight_line(physical, &mut hl, syntect, fallback);
        if physical.is_empty() {
            out.push(Line::default());
            continue;
        }
        debug_assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            physical,
            "highlight spans must flatten back to the physical line"
        );
        for row in soft_wrap_row_texts(
            physical,
            offset,
            &full_breaks,
            &heredoc_payload,
            content_width,
            max_rows - out.len(),
        ) {
            let start = (row.as_ptr() as usize) - (physical.as_ptr() as usize);
            out.push(Line::from(slice_highlighted_spans(
                &spans,
                start,
                start + row.len(),
            )));
        }
        offset += physical.len();
    }
    out
}

fn count_raw_bash_rows(command: &str, content_width: usize, max_rows: usize) -> usize {
    let text = prepare_bash_display_text(command);
    if text.is_empty() {
        return 0;
    }
    let full_breaks = soft_break_offsets_after_operators(&text);
    let heredoc_payload = heredoc_payload_byte_ranges(&text);
    let mut rows = 0usize;
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if rows >= max_rows {
            return rows;
        }
        if idx > 0 {
            offset += 1;
        }
        if physical.is_empty() {
            rows += 1;
        } else {
            rows += soft_wrap_row_texts(
                physical,
                offset,
                &full_breaks,
                &heredoc_payload,
                content_width,
                max_rows - rows,
            )
            .len();
        }
        offset += physical.len();
    }
    rows
}

fn slice_highlighted_spans(
    spans: &[Span<'static>],
    start: usize,
    end: usize,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let span_start = pos;
        let span_end = pos + span.content.len();
        pos = span_end;
        if span_end <= start {
            continue;
        }
        if span_start >= end {
            break;
        }
        let lo = start.max(span_start) - span_start;
        let hi = end.min(span_end) - span_start;
        if lo >= hi {
            continue;
        }
        let Some(slice) = span.content.get(lo..hi) else {
            debug_assert!(false, "row boundary off a char boundary");
            continue;
        };
        out.push(Span::styled(slice.to_owned(), span.style));
    }
    out
}

fn build_mcp_scope_lines(
    scope: &McpScopeState,
    theme: &Theme,
    _content_w: usize,
) -> Vec<Line<'static>> {
    let active_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(theme.gray).add_modifier(Modifier::DIM);

    let spans: Vec<Span<'static>> = match (scope.selected, scope.server_prefix.as_deref()) {
        (_, None) => vec![Span::styled(scope.display_name(), active_style)],
        (McpScope::Tool, Some(_)) => vec![Span::styled(scope.display_name(), active_style)],
        (McpScope::Server, Some(prefix)) => vec![
            Span::styled(format!("({}) ", mcp_titleize_segment(prefix)), active_style),
            Span::styled(mcp_titleize_segment(scope.action()), inactive_style),
        ],
    };
    vec![Line::from(spans)]
}

fn build_mcp_args_lines(
    description: &[String],
    theme: &Theme,
    content_w: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if description.is_empty() || max_rows == 0 {
        return Vec::new();
    }
    let fallback = Style::default().fg(theme.text_secondary);
    let syntect = crate::syntax::get_syntect();
    let mut hl = syntect.highlight_lines_for_token("json");
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in description {
        if out.len() >= max_rows {
            break;
        }
        let spans = crate::syntax::highlight_line(raw, &mut hl, syntect, fallback);
        out.extend(char_wrap_spans(spans, content_w));
    }
    out.truncate(max_rows);
    out
}

fn truncation_indicator_line(theme: &Theme) -> Line<'static> {
    let style = Style::default().fg(theme.gray).bg(theme.bg_light);
    Line::from(vec![
        Span::styled("... ", style),
        Span::styled(
            "Ctrl-F",
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ),
        Span::styled(" to expand", style),
    ])
}

fn char_wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let line_has_content = !run.is_empty() || !line_spans.is_empty();
            if col + ch_w > width && line_has_content {
                if !run.is_empty() {
                    line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                lines.push(Line::from(std::mem::take(&mut line_spans)));
                col = 0;
            }
            if style != run_style && !run.is_empty() {
                line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push(ch);
            col += ch_w;
        }
    }
    if !run.is_empty() {
        line_spans.push(Span::styled(run, run_style));
    }
    if !line_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(line_spans));
    }
    lines
}

#[cfg(test)]
fn char_wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + ch_w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += ch_w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn build_permission_option_line<'a>(
    option: &acp::PermissionOption,
    index: usize,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
    followup_text: &str,
    row_width: u16,
    theme: &Theme,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);

    let sc = shortcut_char(index);

    if option.kind == acp::PermissionOptionKind::RejectOnce {
        return build_reject_once_line(sc, is_cursor, row_bg, followup_text, theme);
    }

    let (label_prefix, scope_words) = dynamic_option_label(option, selected_words, mcp_scope);
    let scope_is_mcp = mcp_scope.is_some();

    let marker = if is_cursor {
        format!("({})", crate::glyphs::filled_dot())
    } else {
        "(\u{25cb})".to_string()
    };
    let marker_style = if is_cursor {
        Style::default()
            .fg(theme.text_primary)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.gray).bg(row_bg)
    };
    let label_style = Style::default()
        .fg(theme.text_primary)
        .bg(row_bg)
        .add_modifier(if is_cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let mut spans = vec![
        Span::styled(format!("{sc} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(label_prefix, label_style),
    ];

    if let Some(scope) = scope_words {
        let prefix_w: usize = spans.iter().map(|s| s.width()).sum();
        let max_scope = (row_width as usize).saturating_sub(prefix_w + 1);
        let truncated = if scope.width() > max_scope {
            crate::render::line_utils::truncate_str(&scope, max_scope)
        } else {
            scope
        };
        if scope_is_mcp {
            spans.push(Span::styled(truncated, label_style));
        } else {
            for s in crate::views::tasks_pane::highlight_bash_command(&truncated) {
                spans.push(Span::styled(s.content.into_owned(), s.style.bg(row_bg)));
            }
        }
    }

    Line::from(spans).style(Style::default().bg(row_bg))
}

fn build_reject_once_line<'a>(
    shortcut_ch: char,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    followup_text: &str,
    theme: &Theme,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
    let has_text = !followup_text.trim().is_empty();

    let (marker, marker_style) = if is_cursor {
        (
            format!("({})", crate::glyphs::filled_dot()),
            Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "(\u{25cb})".to_string(),
            Style::default().fg(theme.gray).bg(row_bg),
        )
    };

    let prompt_indicator = Style::default().fg(theme.accent_user).bg(row_bg);

    let (label, label_style) = if has_text {
        let first_line = followup_text.lines().next().unwrap_or("");
        let preview = crate::render::line_utils::truncate_str(first_line, 50);
        (preview, Style::default().fg(theme.text_primary).bg(row_bg))
    } else {
        (
            "No, reject (type to add feedback)".to_string(),
            Style::default().fg(theme.gray).bg(row_bg),
        )
    };

    let mut spans = vec![
        Span::styled(format!("{shortcut_ch} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
    ];
    if has_text {
        spans.push(Span::styled(
            crate::glyphs::prompt_arrow(),
            prompt_indicator,
        ));
    }
    spans.push(Span::styled(label, label_style));

    Line::from(spans).style(Style::default().bg(row_bg))
}

fn dynamic_option_label(
    option: &acp::PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> (String, Option<String>) {
    if matches!(
        option.kind,
        acp::PermissionOptionKind::AllowAlways | acp::PermissionOptionKind::RejectAlways
    ) && let Some(ref meta) = option.meta
    {
        if let Some(scope) = mcp_scope
            && let Ok(perm) =
                serde_json::from_value::<McpToolPermission>(serde_json::Value::Object(meta.clone()))
        {
            let scope_text = match scope.selected {
                McpScope::Tool => perm.display_name(),
                McpScope::Server => match scope.server_prefix.as_deref() {
                    Some(s) => format!("all tools from {}", mcp_titleize_segment(s)),
                    None => perm.display_name(),
                },
            };
            return (format!("{} ", perm.prompt_prefix), Some(scope_text));
        }

        if let Some(words) = selected_words
            && let Ok(bash_perm) = serde_json::from_value::<BashCommandPermission>(
                serde_json::Value::Object(meta.clone()),
            )
        {
            return (
                format!("{} ", bash_perm.prompt_prefix),
                Some(words.to_owned()),
            );
        }
    }
    (option.name.clone(), None)
}

pub(crate) fn allow_scope_label(
    h: &BashCommandHighlights,
    raw_command: Option<&str>,
    count: usize,
) -> String {
    let words = &h.highlighted_words;
    let n = count.min(words.len());
    let uses_raw_key = n == words.len()
        && n > 0
        && h.prefix.is_empty()
        && h.suffix.is_empty()
        && words
            .get(..n)
            .is_some_and(|w| w.iter().any(|w| w.chars().any(char::is_whitespace)));
    match raw_command.filter(|_| uses_raw_key) {
        Some(raw) => raw.to_owned(),
        None => words.get(..n).unwrap_or(&[]).join(" "),
    }
}

pub(crate) fn option_label_for_selection(
    option: &acp::PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> String {
    let (prefix, scope_text) = dynamic_option_label(option, selected_words, mcp_scope);
    match scope_text {
        Some(scope) => format!("{prefix}{scope}"),
        None => prefix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn at<'a, T>(xs: &'a [T], i: usize) -> &'a T {
        match xs.get(i) {
            Some(v) => v,
            None => panic!("index {i} out of {}", xs.len()),
        }
    }

    #[test]
    fn pattern_edit_edits_at_the_cursor() {
        let mut e = PatternEditState::new("ghapi");
        assert!(!e.is_dirty());
        assert_eq!(e.cursor, "ghapi".len());
        e.move_home();
        e.move_right();
        e.move_right();
        assert!(!e.is_dirty(), "cursor moves are not content mutations");
        e.insert_char(' ');
        assert!(e.is_dirty());
        assert_eq!(e.buffer, "gh api");
        e.delete();
        assert_eq!(e.buffer, "gh pi");
        e.move_home();
        e.backspace();
        assert_eq!((e.buffer.as_str(), e.cursor), ("gh pi", 0));
        e.clear();
        assert_eq!(e.trimmed(), None);
        assert!(e.is_dirty());
    }

    #[test]
    fn pattern_edit_respects_char_boundaries() {
        let mut e = PatternEditState::new("café");
        e.backspace();
        assert_eq!(e.buffer, "caf");
        e.insert_char('é');
        assert_eq!(e.buffer, "café");
        assert!(e.is_dirty());
    }

    fn mcp_state(tool: &str, server: Option<&str>, selected: McpScope) -> McpScopeState {
        McpScopeState {
            tool_name: tool.to_owned(),
            server_prefix: server.map(|s| s.to_owned()),
            selected,
        }
    }

    fn allow_always_mcp_option(tool: &str, server: Option<&str>) -> acp::PermissionOption {
        let perm = McpToolPermission {
            prompt_prefix: "Always allow:".to_owned(),
            tool_name: tool.to_owned(),
            server_prefix: server.map(|s| s.to_owned()),
        };
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-always-mcp")),
            format!("Always allow: {}", tool),
            acp::PermissionOptionKind::AllowAlways,
        )
        .meta(
            serde_json::to_value(perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        )
    }

    fn permission_state_with_title(title: &str, n_options: usize) -> PermissionViewState {
        let (response_tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::RequestPermissionRequest::new(
            acp::SessionId::new(Arc::from("test")),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("call-1")),
                acp::ToolCallUpdateFields::default(),
            ),
            vec![],
        );
        let options: Vec<acp::PermissionOption> = (0..n_options)
            .map(|i| {
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(Arc::from(format!("opt-{i}"))),
                    format!("Option {i}"),
                    acp::PermissionOptionKind::AllowOnce,
                )
            })
            .collect();
        PermissionViewState {
            request: distill_acp_lib::AcpArgs {
                request,
                response_tx,
            },
            id: 0,
            focus: PermissionFocus::Options,
            options,
            active_idx: 0,
            bash_highlights: None,
            bash_selection_count: 0,
            bash_deny_selection_count: 0,
            bash_command_raw: Some("cargo test --all".to_string()),
            mcp_scope: None,
            title: title.to_string(),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: Some("subagent: worker".to_string()),
            options_area_height: 0,
            options_scroll_offset: 0,
        }
    }

    #[test]
    fn render_short_area_at_buffer_bottom_does_not_panic() {
        let theme = Theme::current();
        for buf_h in [10u16, 12, 24] {
            for area_h in 0u16..=5 {
                for area_y in 0..buf_h {
                    if area_y + area_h > buf_h {
                        continue;
                    }
                    let state = permission_state_with_title("Allow command?", 3);
                    let area = Rect::new(2, area_y, 145, area_h);
                    let mut buf = Buffer::empty(Rect::new(0, 0, 147, buf_h));
                    let _ = render_permission_view(
                        &mut buf, area, &state, "", None, None, &theme, true,
                    );
                }
            }
        }
    }

    #[test]
    fn render_tiny_areas_with_args_do_not_panic() {
        let theme = Theme::current();
        for expanded in [false, true] {
            for buf_w in 0u16..=10 {
                for area_h in 0u16..=6 {
                    for area_y in [0u16, 4, 8] {
                        if area_y + area_h > 10 {
                            continue;
                        }
                        let mut state = long_args_state();
                        state.args_expanded = expanded;
                        state.subagent_label = Some("subagent: worker".into());
                        let area = Rect::new(0, area_y, buf_w, area_h);
                        let mut buf = Buffer::empty(Rect::new(0, 0, buf_w.max(1), 10));
                        let _ = render_permission_view(
                            &mut buf, area, &state, "follow", None, None, &theme, true,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn view_height_stays_sane_on_tiny_screens() {
        let mut state = long_args_state();
        for screen_h in 0u16..=12 {
            let collapsed = permission_view_height(&state, screen_h, 20);
            assert!(
                collapsed <= screen_h.max(10),
                "collapsed {collapsed} exceeds screen {screen_h} (min-floor 10)"
            );
            state.args_expanded = true;
            let expanded = permission_view_height(&state, screen_h, 20);
            assert!(
                expanded <= screen_h,
                "expanded {expanded} > screen {screen_h}"
            );
            state.args_expanded = false;
        }
    }

    #[test]
    fn dynamic_option_label_server_scope_renders_all_tools_from_wording() {
        let opt = allow_always_mcp_option("linear__list", Some("linear"));
        let scope = mcp_state("linear__list", Some("linear"), McpScope::Server);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("all tools from Linear"));
    }

    #[test]
    fn dynamic_option_label_tool_scope_renders_pretty_name() {
        let opt = allow_always_mcp_option("linear__list_issues", Some("linear"));
        let scope = mcp_state("linear__list_issues", Some("linear"), McpScope::Tool);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("(Linear) List Issues"));
    }

    fn empty_view_state(mcp_scope: Option<McpScopeState>) -> PermissionViewState {
        let (response_tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::RequestPermissionRequest::new(
            acp::SessionId::new(Arc::from("test")),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("call-1")),
                acp::ToolCallUpdateFields::default(),
            ),
            vec![],
        );
        let perm = distill_acp_lib::AcpArgs {
            request,
            response_tx,
        };
        PermissionViewState {
            request: perm,
            id: 0,
            focus: PermissionFocus::Options,
            options: vec![],
            active_idx: 0,
            bash_highlights: None,
            bash_selection_count: 0,
            bash_deny_selection_count: 0,
            bash_command_raw: None,
            mcp_scope,
            title: String::new(),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: None,
            options_area_height: 0,
            options_scroll_offset: 0,
        }
    }

    #[test]
    fn mcp_scope_no_server_prefix_disables_toggle() {
        let state = empty_view_state(Some(mcp_state("standalone", None, McpScope::Tool)));
        assert!(!state.has_adjustable_scope());
    }

    #[test]
    fn has_adjustable_scope_true_when_mcp_has_server() {
        let state = empty_view_state(Some(mcp_state(
            "linear__list",
            Some("linear"),
            McpScope::Tool,
        )));
        assert!(state.has_adjustable_scope());
    }

    #[test]
    fn has_adjustable_scope_false_for_plain_prompt() {
        let state = empty_view_state(None);
        assert!(!state.has_adjustable_scope());
    }

    #[test]
    fn char_wrap_row_count_matches_char_wrap() {
        let cases = [
            "",
            "a",
            "abcdef",
            "  \"key\": \"value with spaces\",",
            "你好世界你好世界",
            "mixed 你 width 好 text",
            &"x".repeat(500),
        ];
        for s in cases {
            for width in [1usize, 2, 3, 7, 10, 80, 500] {
                assert_eq!(
                    char_wrap_row_count(s, width),
                    char_wrap(s, width).len(),
                    "{s:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn char_wrap_respects_width_and_yields_blank_row_for_empty() {
        assert_eq!(char_wrap("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(char_wrap("abcd", 3), vec!["abc", "d"]);
        assert_eq!(char_wrap("", 10), vec![""]);
        assert_eq!(char_wrap("ab", 0), vec!["a", "b"]);
        assert_eq!(char_wrap("你好", 2), vec!["你", "好"]);
    }

    #[test]
    fn char_wrap_spans_mirrors_char_wrap_boundaries() {
        let text = "  \"key\": \"a long value with spaces and 你好 wide chars\",";
        for width in [1usize, 2, 7, 10, 80] {
            let chars: Vec<char> = text.chars().collect();
            let spans: Vec<Span<'static>> = chars
                .chunks(5)
                .enumerate()
                .map(|(i, chunk)| {
                    let style = if i % 2 == 0 {
                        Style::default().fg(ratatui::style::Color::Red)
                    } else {
                        Style::default().fg(ratatui::style::Color::Blue)
                    };
                    Span::styled(chunk.iter().collect::<String>(), style)
                })
                .collect();
            let lines = char_wrap_spans(spans, width);
            let plain = char_wrap(text, width);
            assert_eq!(lines.len(), plain.len(), "width {width}");
            for (line, expect) in lines.iter().zip(&plain) {
                let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(&flat, expect, "width {width}");
            }
        }
    }

    #[test]
    fn char_wrap_spans_preserves_styles_across_wrap() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let blue = Style::default().fg(ratatui::style::Color::Blue);
        let spans = vec![Span::styled("aaaa", red), Span::styled("bbbb", blue)];
        let lines = char_wrap_spans(spans, 6);
        assert_eq!(lines.len(), 2);
        assert_eq!(at(&lines, 0).spans.len(), 2);
        assert_eq!(at(&at(&lines, 0).spans, 0).content.as_ref(), "aaaa");
        assert_eq!(at(&at(&lines, 0).spans, 0).style, red);
        assert_eq!(at(&at(&lines, 0).spans, 1).content.as_ref(), "bb");
        assert_eq!(at(&at(&lines, 0).spans, 1).style, blue);
        assert_eq!(at(&at(&lines, 1).spans, 0).content.as_ref(), "bb");
        assert_eq!(at(&at(&lines, 1).spans, 0).style, blue);
    }

    #[test]
    fn build_mcp_args_lines_highlights_without_altering_text_or_count() {
        let description: Vec<String> = vec![
            "{".into(),
            format!("  \"body\": \"{}\",", "x".repeat(120)),
            "  \"n\": 42".into(),
            "}".into(),
        ];
        let theme = Theme::current();
        for width in [10usize, 40, 80] {
            let lines = build_mcp_args_lines(&description, &theme, width, usize::MAX);
            let plain: Vec<String> = description
                .iter()
                .flat_map(|raw| char_wrap(raw, width))
                .collect();
            assert_eq!(lines.len(), plain.len(), "width {width}");
            for (line, expect) in lines.iter().zip(&plain) {
                let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(&flat, expect, "width {width}");
            }
        }
        assert!(
            crate::syntax::get_syntect()
                .highlight_lines_for_token("json")
                .is_some(),
            "JSON syntax missing from the two-face syntax set"
        );
    }

    #[test]
    fn chrome_height_counts_mcp_args_lines() {
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        let base = permission_chrome_height(&state, 80);
        state.description = vec!["{".into(), "  \"body\": \"hi\"".into(), "}".into()];
        assert_eq!(permission_chrome_height(&state, 80), base + 3);
        state.description = vec!["x".repeat(100)];
        assert_eq!(permission_chrome_height(&state, 80), base + 2);
    }

    #[test]
    fn render_shows_planned_mcp_args() {
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        state.title = "Allow Jira: Addjira Comment?".to_string();
        state.description = vec![
            "{".to_string(),
            "  \"issue\": \"ABC-123\",".to_string(),
            "  \"body\": \"hello from grok\"".to_string(),
            "}".to_string(),
        ];
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        )];
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        let _ = render_permission_view(&mut buf, area, &state, "", None, None, &theme, true);

        let text: String = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| {
                        buf.cell((col, row))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(
            text.contains("\"issue\": \"ABC-123\","),
            "args JSON not rendered:\n{text}"
        );
        assert!(
            text.contains("\"body\": \"hello from grok\""),
            "args JSON not rendered:\n{text}"
        );
        assert!(text.contains("Yes"), "options row missing:\n{text}");
    }

    fn long_args_state() -> PermissionViewState {
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        state.title = "Allow Jira: Addjira Comment?".to_string();
        state.description = (0..50).map(|i| format!("\"line{i}\": {i},")).collect();
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        )];
        state
    }

    fn render_to_text(state: &PermissionViewState, area: Rect) -> String {
        let theme = Theme::current();
        let mut buf = Buffer::empty(area);
        let _ = render_permission_view(&mut buf, area, state, "", None, None, &theme, true);
        (0..area.height)
            .map(|row| {
                (area.x..area.x + area.width)
                    .map(|col| {
                        buf.cell((col, row))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn render_collapses_long_mcp_args_with_ctrl_f_indicator() {
        let state = long_args_state();
        let text = render_to_text(&state, Rect::new(0, 0, 80, 30));
        assert!(text.contains("\"line3\": 3,"), "4th content row:\n{text}");
        assert!(
            !text.contains("\"line4\": 4,"),
            "5th row must be the indicator:\n{text}"
        );
        assert!(
            text.contains("... Ctrl-F to expand"),
            "indicator missing:\n{text}"
        );
        assert!(text.contains("Yes"), "options row missing:\n{text}");
    }

    #[test]
    fn render_expanded_mcp_args_clips_at_area_keeping_options_visible() {
        let mut state = long_args_state();
        state.args_expanded = true;
        let text = render_to_text(&state, Rect::new(0, 0, 80, 12));
        assert!(
            !text.contains("Ctrl-F to expand"),
            "no indicator when expanded:\n{text}"
        );
        assert!(text.contains("Yes"), "options row missing:\n{text}");
        assert!(
            text.contains('\u{2026}'),
            "area-clipped args missing ellipsis:\n{text}"
        );
        assert!(
            !text.contains("\"line49\": 49,"),
            "args should have been clipped:\n{text}"
        );
        let text_tall = render_to_text(&state, Rect::new(0, 0, 80, 40));
        assert!(
            text_tall.contains("\"line20\": 20,"),
            "expanded view must show deep rows:\n{text_tall}"
        );
    }

    #[test]
    fn mcp_args_visible_rows_budget_and_boundary() {
        let mut state = long_args_state();
        assert_eq!(mcp_args_visible_rows(&state, 80), (4, true));
        state.args_expanded = true;
        assert_eq!(mcp_args_visible_rows(&state, 80), (50, false));
        state.args_expanded = false;
        state.description = (0..PERMISSION_COLLAPSED_ROWS)
            .map(|i| format!("l{i}"))
            .collect();
        assert_eq!(
            mcp_args_visible_rows(&state, 80),
            (PERMISSION_COLLAPSED_ROWS, false)
        );
    }

    #[test]
    fn expanded_args_lift_the_view_height_cap() {
        let mut state = long_args_state();
        let screen_h = 40;
        let collapsed = permission_view_height(&state, screen_h, 80);
        assert!(
            collapsed <= screen_h / 2,
            "collapsed view respects the 50% cap: {collapsed}"
        );
        state.args_expanded = true;
        let expanded = permission_view_height(&state, screen_h, 80);
        assert!(
            expanded > screen_h / 2 && expanded <= screen_h,
            "expanded view may grow past 50% up to the screen: {expanded}"
        );
    }

    fn long_bash_state() -> PermissionViewState {
        let mut state = empty_view_state(None);
        state.title = "Allow command?".to_string();
        state.bash_command_raw = Some(
            (0..25)
                .map(|i| format!("echo line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes, proceed".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        )];
        state
    }

    #[test]
    fn bash_visible_rows_budget_and_boundary() {
        let mut state = long_bash_state();
        assert_eq!(bash_visible_rows(&state, 80), (4, true));
        state.args_expanded = true;
        assert_eq!(bash_visible_rows(&state, 80), (25, false));
        state.args_expanded = false;
        state.bash_command_raw = Some(
            (0..PERMISSION_COLLAPSED_ROWS)
                .map(|i| format!("echo l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(
            bash_visible_rows(&state, 80),
            (PERMISSION_COLLAPSED_ROWS, false)
        );
    }

    #[test]
    fn has_collapsible_bash_thresholds() {
        let mut state = long_bash_state();
        assert!(state.has_collapsible_bash(80));
        state.args_expanded = true;
        assert!(state.has_collapsible_bash(80));
        state.args_expanded = false;
        state.bash_command_raw = Some("echo ".repeat(40));
        assert!(state.has_collapsible_bash(10));
        assert!(!state.has_collapsible_bash(400));
        state.bash_command_raw = Some("echo short".into());
        assert!(!state.has_collapsible_bash(80));
        state.bash_command_raw = None;
        assert!(!state.has_collapsible_bash(80));
    }

    #[test]
    fn render_collapses_long_bash_with_ctrl_f_indicator() {
        let state = long_bash_state();
        let text = render_to_text(&state, Rect::new(0, 0, 80, 30));
        assert!(text.contains("echo line3"), "4th content row:\n{text}");
        assert!(
            !text.contains("echo line4"),
            "5th row must be the indicator:\n{text}"
        );
        assert!(
            text.contains("... Ctrl-F to expand"),
            "indicator missing:\n{text}"
        );
        assert!(
            text.contains("Yes, proceed"),
            "options row missing:\n{text}"
        );
    }

    #[test]
    fn render_short_bash_has_no_ctrl_f_indicator() {
        let mut state = long_bash_state();
        state.bash_command_raw = Some("echo a\necho b".into());
        let text = render_to_text(&state, Rect::new(0, 0, 80, 30));
        assert!(
            text.contains("echo a") && text.contains("echo b"),
            "full short script must render:\n{text}"
        );
        assert!(
            !text.contains("Ctrl-F to expand"),
            "no indicator within the budget:\n{text}"
        );
    }

    #[test]
    fn render_expanded_bash_shows_deep_rows_without_indicator() {
        let mut state = long_bash_state();
        state.args_expanded = true;
        let text = render_to_text(&state, Rect::new(0, 0, 80, 30));
        assert!(
            !text.contains("Ctrl-F to expand"),
            "no indicator when expanded:\n{text}"
        );
        assert!(
            text.contains("echo line20"),
            "expanded view must show deep rows:\n{text}"
        );
        assert!(
            text.contains("Yes, proceed"),
            "options row missing:\n{text}"
        );
    }

    #[test]
    fn collapsed_long_bash_chrome_uses_the_budget_not_the_full_wrap() {
        let mut state = long_bash_state();
        assert_eq!(permission_chrome_height(&state, 80), 8);
        state.args_expanded = true;
        assert_eq!(permission_chrome_height(&state, 80), 3 + 25);
    }

    #[test]
    fn expanded_bash_lifts_the_view_height_cap() {
        let mut state = long_bash_state();
        let screen_h = 40;
        let collapsed = permission_view_height(&state, screen_h, 80);
        assert!(
            collapsed <= screen_h / 2,
            "collapsed view respects the 50% cap: {collapsed}"
        );
        state.args_expanded = true;
        let expanded = permission_view_height(&state, screen_h, 80);
        assert!(
            expanded > screen_h / 2 && expanded <= screen_h,
            "expanded view may grow past 50% up to the screen: {expanded}"
        );
    }

    #[test]
    fn build_raw_bash_lines_stops_at_max_rows() {
        let script = (0..100)
            .map(|i| format!("echo line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = build_raw_bash_lines(&script, 80, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(row_text(at(&rows, 3)), "echo line3");
        assert!(build_raw_bash_lines(&script, 80, 0).is_empty());
    }

    #[test]
    fn count_raw_bash_rows_matches_build_and_stops_early() {
        let script = "echo one && echo two\n\ncat <<EOF\nbody line stays intact here\nEOF";
        for w in [10usize, 20, 80] {
            assert_eq!(
                count_raw_bash_rows(script, w, usize::MAX),
                build_raw_bash_lines(script, w, usize::MAX).len(),
                "width {w}"
            );
        }
        let long: String = (0..50)
            .map(|i| format!("echo line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(count_raw_bash_rows(&long, 80, 6), 6);
    }

    #[test]
    fn soft_wrap_row_texts_respects_max_rows() {
        let line = "aa bb cc dd ee ff gg hh";
        let breaks = soft_break_offsets_after_operators(line);
        let all = soft_wrap_row_texts(line, 0, &breaks, &[], 5, usize::MAX);
        assert!(all.len() > 3, "expected several rows, got {all:?}");
        let capped = soft_wrap_row_texts(line, 0, &breaks, &[], 5, 3);
        assert_eq!(capped.len(), 3);
        assert_eq!(all.get(..3).unwrap_or(&[]), capped.as_slice());
        assert!(soft_wrap_row_texts(line, 0, &breaks, &[], 5, 0).is_empty());

        let op_line = "echo a && echo b && echo c && echo d && echo e";
        let op_breaks = soft_break_offsets_after_operators(op_line);
        let op_all = soft_wrap_row_texts(op_line, 0, &op_breaks, &[], 10, usize::MAX);
        assert!(op_all.len() > 2, "expected several rows, got {op_all:?}");
        let op_capped = soft_wrap_row_texts(op_line, 0, &op_breaks, &[], 10, 2);
        assert_eq!(op_capped.len(), 2);
        assert_eq!(op_all.get(..2).unwrap_or(&[]), op_capped.as_slice());
    }

    #[test]
    fn collapsed_budget_caps_wrap_rows_inside_one_physical_line() {
        let script = "echo ".repeat(10_000);
        let rows = build_raw_bash_lines(&script, 10, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            count_raw_bash_rows(&script, 10, PERMISSION_COLLAPSED_ROWS + 1),
            PERMISSION_COLLAPSED_ROWS + 1
        );

        let line = script.trim_end();
        let capped = soft_wrap_row_texts(line, 0, &[], &[], 10, 4);
        assert_eq!(capped.len(), 4);
        let last = capped.last().unwrap();
        let consumed = (last.as_ptr() as usize - line.as_ptr() as usize) + last.len();
        assert!(
            consumed * 100 < line.len(),
            "capped wrap consumed {consumed} of {} bytes",
            line.len()
        );

        let mut state = empty_view_state(None);
        state.bash_command_raw = Some(script);
        assert_eq!(bash_visible_rows(&state, 10), (4, true));
        assert!(state.has_collapsible_bash(10));
    }

    #[test]
    fn quote_aware_break_points_are_discovered_lazily() {
        let line = "echo ".repeat(10_000);
        let mut it = QuoteAwareBreakPoints::new(&line);
        for _ in 0..8 {
            it.next().expect("break point");
        }
        assert!(it.i < 64, "scanned {} bytes for 8 break points", it.i);
        let quoted = "aa 'no break inside' bb cc";
        let breaks: Vec<usize> = QuoteAwareBreakPoints::new(quoted).collect();
        assert_eq!(breaks, vec![2, 20, 23]);
    }

    #[test]
    fn collapsed_budget_caps_chunk_packing_on_a_huge_operator_line() {
        let script = "echo a && ".repeat(5_000) + "echo a";
        let breaks = soft_break_offsets_after_operators(&script);
        assert!(
            breaks.len() > 1_000,
            "expected many operator breaks, got {}",
            breaks.len()
        );
        let capped = soft_wrap_row_texts(&script, 0, &breaks, &[], 12, 4);
        assert_eq!(capped.len(), 4);
        let wider = soft_wrap_row_texts(&script, 0, &breaks, &[], 12, 8);
        assert_eq!(wider.get(..4).unwrap_or(&[]), capped.as_slice());
        let last = capped.last().unwrap();
        let consumed = (last.as_ptr() as usize - script.as_ptr() as usize) + last.len();
        assert!(
            consumed * 100 < script.len(),
            "capped operator wrap consumed {consumed} of {} bytes",
            script.len()
        );
    }

    #[test]
    fn has_collapsible_display_discriminates_mcp_args_edit_and_bash() {
        let mut mcp = empty_view_state(None);
        mcp.description = vec!["{".into(), "  \"k\": 1".into(), "}".into()];
        assert!(mcp.has_collapsible_display(80));
        mcp.mcp_scope = Some(mcp_state("linear__list", Some("linear"), McpScope::Tool));
        assert!(mcp.has_collapsible_display(80));
        mcp.description.clear();
        assert!(!mcp.has_collapsible_display(80));

        let mut edit = empty_view_state(None);
        edit.description = vec!["Warning: this file is protected".into()];
        edit.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from(ALLOW_EDITS_SESSION_OPTION_ID)),
            "Allow all edits this session".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        )];
        assert!(!edit.has_collapsible_display(80));

        let mut bash = empty_view_state(None);
        bash.bash_command_raw = Some("echo short".into());
        bash.description = vec!["stray".into()];
        assert!(!bash.has_collapsible_display(80));
        assert!(long_bash_state().has_collapsible_display(80));
    }

    #[test]
    fn dynamic_option_label_server_scope_without_prefix_falls_back_to_tool() {
        let opt = allow_always_mcp_option("standalone", None);
        let scope = mcp_state("standalone", None, McpScope::Server);
        let (_prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(scope_text.as_deref(), Some("Standalone"));
    }

    #[test]
    fn dynamic_option_label_falls_back_to_bash_when_no_mcp() {
        let bash_perm = BashCommandPermission {
            prompt_prefix: "Always allow:".to_owned(),
        };
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-always-command")),
            "Always allow: cargo test".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        )
        .meta(
            serde_json::to_value(bash_perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        );
        let (prefix, scope_text) = dynamic_option_label(&opt, Some("cargo test"), None);
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("cargo test"));
    }

    #[test]
    fn dynamic_option_label_rebuilds_reject_always_bash_row() {
        let bash_perm = BashCommandPermission {
            prompt_prefix: "Never allow:".to_owned(),
        };
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("reject-always-command")),
            "Never allow: cargo test --workspace".to_owned(),
            acp::PermissionOptionKind::RejectAlways,
        )
        .meta(
            serde_json::to_value(bash_perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        );
        let (prefix, scope_text) = dynamic_option_label(&opt, Some("cargo test"), None);
        assert_eq!(prefix, "Never allow: ");
        assert_eq!(scope_text.as_deref(), Some("cargo test"));
    }

    #[test]
    fn option_label_for_selection_matches_persisted_scope() {
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("reject-always-command")),
            "Never allow: cargo test --workspace".to_owned(),
            acp::PermissionOptionKind::RejectAlways,
        )
        .meta(
            serde_json::to_value(BashCommandPermission {
                prompt_prefix: "Never allow:".to_owned(),
            })
            .ok()
            .and_then(|v| v.as_object().cloned()),
        );
        assert_eq!(
            option_label_for_selection(&opt, Some("cargo"), None),
            "Never allow: cargo"
        );
        let plain = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes, proceed".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        );
        assert_eq!(
            option_label_for_selection(&plain, Some("cargo"), None),
            "Yes, proceed"
        );
    }

    #[test]
    fn allow_scope_label_shows_raw_for_full_ambiguous_scope() {
        let h = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec![
                "git".into(),
                "commit".into(),
                "-m".into(),
                "fix stuff".into(),
            ],
            suffix: vec![],
        };
        let raw = r#"git commit -m "fix stuff""#;
        assert_eq!(allow_scope_label(&h, Some(raw), 4), raw);
        assert_eq!(allow_scope_label(&h, Some(raw), 3), "git commit -m");
        let plain = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["cargo".into(), "test".into()],
            suffix: vec![],
        };
        assert_eq!(
            allow_scope_label(&plain, Some("cargo test"), 2),
            "cargo test"
        );
    }

    #[test]
    fn prepare_bash_display_preserves_backslash_continuations() {
        let raw = "docker run \\\n  -v /tmp:/tmp \\\n  -e FOO=bar \\\n  alpine:latest\n";
        let prepared = prepare_bash_display_text(raw);
        assert!(
            prepared.contains("docker run \\\n  -v /tmp:/tmp \\\n  -e FOO=bar \\\n  alpine:latest"),
            "expected multi-line continuations, got: {prepared:?}"
        );
        assert!(!prepared.contains("docker run \\  -v"));
        assert_eq!(prepared.lines().count(), 4);
    }

    #[test]
    fn prepare_bash_display_drops_dangling_trailing_continuation_newline() {
        let prepared = prepare_bash_display_text("echo a \\\n");
        assert_eq!(prepared, "echo a \\");
        let rows = build_raw_bash_lines("echo a \\\n", 80, usize::MAX);
        assert_eq!(rows.len(), 1, "no trailing blank row");
        assert_eq!(prepare_bash_display_text("echo a \\\n\n"), "echo a \\");
        assert_eq!(prepare_bash_display_text("a \\\nb\n"), "a \\\nb");
    }

    #[test]
    fn build_raw_bash_lines_keeps_continuation_rows() {
        let raw = "cargo test \\\n  --all \\\n  -- --nocapture";
        let lines = build_raw_bash_lines(raw, 80, usize::MAX);
        assert!(
            lines.len() >= 3,
            "expected one row per physical line, got {}",
            lines.len()
        );
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("cargo test \\"));
        assert!(joined.contains("--all \\"));
        assert!(joined.contains("-- --nocapture"));
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn soft_wrap_prefers_shell_operators_over_every_space() {
        let line = "git status --short --branch && cargo test --workspace --all-features";
        let width = 40;
        assert!(UnicodeWidthStr::width(line) > width);
        let breaks = soft_break_offsets_after_operators(line);
        assert!(
            !breaks.is_empty(),
            "tree-sitter should find the real && operator"
        );
        let rows = soft_wrap_row_texts(line, 0, &breaks, &[], width, usize::MAX);
        assert!(
            rows.len() >= 2,
            "expected operator split, got {} rows",
            rows.len()
        );
        let first = at(&rows, 0);
        assert!(
            first.contains("&&"),
            "first row should keep the operator: {first:?}"
        );
        assert!(
            !first.contains("cargo"),
            "cargo should be on a later row, not packed with git: {first:?}"
        );
        let second = at(&rows, 1);
        assert!(
            !second.starts_with(' '),
            "no leading space on continuation row: {second:?}"
        );
        assert!(second.starts_with("cargo"), "second={second:?}");
    }

    #[test]
    fn soft_wrap_does_not_break_inside_jq_single_quoted_filter() {
        let line = r#"gh search prs --author=@me --sort=updated --limit=15 --json number,title,url,state,updatedAt,repository,isDraft --jq '.[] | "\(.state)\t#\(.number)\t\(.updatedAt)\t\(.repository.nameWithOwner)\t\(.title)\t\(.url)"'"#;
        let width = 60;
        assert!(UnicodeWidthStr::width(line) > width);
        let breaks = soft_break_offsets_after_operators(line);
        assert!(
            breaks.is_empty(),
            "no shell list ops on this fragment: {breaks:?}"
        );
        let rows = soft_wrap_row_texts(line, 0, &breaks, &[], width, usize::MAX);
        let rendered: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        for r in &rendered {
            assert!(
                !(r.ends_with(".[]") || r.ends_with(".[] |") || r.trim_end() == "'.[] |"),
                "must not break after .[] |; rows={rendered:?}"
            );
        }
        let joined = rendered.join("\n");
        assert!(
            !joined.contains(".[]\n") && !joined.contains(".[] |\n"),
            "jq filter split across rows: {rendered:?}"
        );
    }

    #[test]
    fn bash_quote_aware_wrap_keeps_single_quoted_span_together() {
        let line = "prefix_ok_here '.[] | not a pipe' trailing_words_here_too";
        let width = 20;
        let rows = bash_quote_aware_wrap(line, width, usize::MAX);
        let has_split_inside_quotes = rows
            .iter()
            .any(|r| r.contains(".[]") && !r.contains("not a pipe"));
        assert!(!has_split_inside_quotes, "split inside quotes: {rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("'.[] | not a pipe'")),
            "quoted span must be intact in some row: {rows:?}"
        );
    }

    #[test]
    fn soft_wrap_does_not_break_on_heredoc_body_and() {
        let script = "cat <<EOF && echo after\nfoo && bar inside body\nEOF";
        let lines = build_raw_bash_lines(script, 80, usize::MAX);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let body_rows: Vec<&String> = rendered
            .iter()
            .filter(|r| r.contains("foo && bar"))
            .collect();
        assert_eq!(
            body_rows.len(),
            1,
            "heredoc body must stay one row, got {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("cat <<EOF") && r.contains("&&")),
            "opener with real && should render: {rendered:?}"
        );
    }

    #[test]
    fn soft_wrap_does_not_break_on_quoted_and() {
        let line = r#"echo "keep && together" && echo next"#;
        let breaks = soft_break_offsets_after_operators(line);
        assert_eq!(breaks.len(), 1, "breaks={breaks:?}");
        let width = 28;
        assert!(UnicodeWidthStr::width(line) > width);
        let rows = soft_wrap_row_texts(line, 0, &breaks, &[], width, usize::MAX);
        let first = at(&rows, 0);
        assert!(
            first.contains(r#""keep && together""#),
            "quoted && must stay on the first row: {first:?}"
        );
        assert!(first.contains("&&"), "real operator stays with first row");
    }

    #[test]
    fn body_renders_raw_layout_only_and_missing_raw_renders_no_body() {
        let raw = "cd /tmp && \\\n  git status";
        let lines = build_permission_bash_lines(Some(raw), 200, usize::MAX);
        let flat: Vec<String> = lines.iter().map(row_text).collect();
        assert_eq!(flat, vec!["cd /tmp && \\", "  git status"]);
        assert!(
            build_permission_bash_lines(None, 200, usize::MAX).is_empty(),
            "missing raw must render an empty body"
        );
    }

    #[test]
    fn short_single_line_stays_one_row() {
        let lines = build_raw_bash_lines("echo hello", 80, usize::MAX);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn heredoc_body_line_does_not_wrap_at_spaces() {
        let script = "cat <<EOF && echo after\nthis is a very long heredoc body line with many spaces that would otherwise wrap\nEOF";
        let prepared = prepare_bash_display_text(script);
        let body_line = prepared
            .lines()
            .find(|l| l.contains("very long heredoc"))
            .expect("body line");
        let width = 20;
        assert!(UnicodeWidthStr::width(body_line) > width);
        let body_start = prepared.find(body_line).unwrap();
        let breaks = soft_break_offsets_after_operators(&prepared);
        let heredoc = heredoc_payload_byte_ranges(&prepared);
        assert!(
            range_fully_inside(body_start, body_start + body_line.len(), &heredoc),
            "body must be classified as heredoc payload"
        );
        let rows = soft_wrap_row_texts(body_line, body_start, &breaks, &heredoc, width, usize::MAX);
        assert_eq!(
            rows.len(),
            1,
            "heredoc body must stay one row even when narrow: {rows:?}"
        );
        assert_eq!(*at(&rows, 0), body_line);
    }

    #[test]
    fn incomplete_quote_and_heredoc_never_reconstruct_tokens() {
        for raw in [
            "echo \"unterminated\nstill inside the string",
            "cat <<EOF\nheredoc body with no terminator",
        ] {
            let lines = build_permission_bash_lines(Some(raw), 200, usize::MAX);
            let flat: Vec<String> = lines.iter().map(row_text).collect();
            let expected: Vec<&str> = raw.split('\n').collect();
            assert_eq!(flat, expected, "raw text must render verbatim: {raw:?}");
        }
    }

    #[test]
    fn body_wraps_identically_regardless_of_scope_state() {
        let raw = r#"gh search prs --author=@me --json number,title,url --jq '.[] | "\(.state)\t#\(.number)\t\(.url)"'"#;
        let width = 60;
        let body_rows: Vec<String> = build_permission_bash_lines(Some(raw), width, usize::MAX)
            .iter()
            .map(row_text)
            .collect();
        for r in &body_rows {
            assert!(
                !(r.trim_end().ends_with(".[]") || r.trim_end().ends_with(".[] |")),
                "jq filter split inside quotes; rows={body_rows:?}"
            );
        }
        let raw_rows: Vec<String> = build_raw_bash_lines(raw, width, usize::MAX)
            .iter()
            .map(row_text)
            .collect();
        assert_eq!(
            body_rows, raw_rows,
            "overlay body must be exactly the raw render"
        );
    }

    fn dump_script_twin() -> &'static str {
        "# Probe the outputs dir\n\
         ls /tmp/hw-test-outputs 2>/dev/null\n\
         \n\
         # Reset scratch dir and run the suite\n\
         rm -rf /tmp/hw-test-outputs && mkdir -p /tmp/hw-test-outputs\n\
         ./bazelw test //hw-tests/integration/... --test_output=errors 2>&1 | tee /tmp/hw-test-outputs/run.log | tail -n 40"
    }

    fn fg_at(line: &Line<'_>, byte_idx: usize) -> Option<ratatui::style::Color> {
        let mut pos = 0usize;
        for span in &line.spans {
            let end = pos + span.content.len();
            if byte_idx < end {
                return span.style.fg;
            }
            pos = end;
        }
        None
    }

    #[test]
    fn full_script_body_preserves_structure_without_dim() {
        let script = dump_script_twin();
        let lines = build_permission_bash_lines(Some(script), 400, usize::MAX);
        let flat: Vec<String> = lines.iter().map(row_text).collect();
        let expected: Vec<&str> = script.split('\n').collect();
        assert_eq!(flat, expected, "body must be the raw script, line for line");
        assert_eq!(at(&flat, 2), "");
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.style.add_modifier.contains(Modifier::DIM),
                    "body span {:?} must not be DIM",
                    span.content
                );
            }
        }
    }

    #[test]
    fn wrap_rows_keep_the_unwrapped_line_styles() {
        let _theme = crate::theme::cache::pin_theme();
        let script = dump_script_twin();
        let wide = build_permission_bash_lines(Some(script), 400, usize::MAX);
        let bazel_row = wide
            .iter()
            .find(|l| row_text(l).starts_with("./bazelw"))
            .expect("bazelw line");
        let bazel_text = row_text(bazel_row);
        let test_fg = fg_at(bazel_row, bazel_text.find(" test ").unwrap() + 1);
        let target_fg = fg_at(bazel_row, bazel_text.find("//hw-tests").unwrap());
        let comment_row = wide
            .iter()
            .find(|l| row_text(l).starts_with('#'))
            .expect("comment line");
        let comment_fg = fg_at(comment_row, 0);

        let narrow = build_permission_bash_lines(Some(script), 12, usize::MAX);
        let wrapped_test = narrow
            .iter()
            .find(|l| row_text(l) == "test")
            .expect("wrapped `test` row");
        assert_eq!(
            fg_at(wrapped_test, 0),
            test_fg,
            "wrapped `test` must keep its unwrapped fg"
        );
        let wrapped_target = narrow
            .iter()
            .find(|l| row_text(l).starts_with("//hw-tests"))
            .expect("wrapped //hw-tests row");
        assert_eq!(
            fg_at(wrapped_target, 0),
            target_fg,
            "wrapped `//hw-tests` must keep its unwrapped fg"
        );
        if target_fg != comment_fg {
            assert_ne!(
                fg_at(wrapped_target, 0),
                comment_fg,
                "wrapped `//hw-tests` must not use the comment fg"
            );
        }
    }

    #[test]
    fn execute_header_display_matches_overlay_body() {
        // Both render paths read the process-global theme; hold the theme
        // test lock so a concurrent theme test can't flip the palette (and
        // the markdown polarity mode) between the two renders.
        let _theme = crate::theme::cache::pin_theme();
        let script = dump_script_twin();
        for width in [12usize, 40, 400] {
            assert_eq!(
                render_bash_command_display_lines(script, width),
                build_permission_bash_lines(Some(script), width, usize::MAX),
                "width {width}"
            );
        }
    }

    #[test]
    fn interior_blank_line_renders_empty_row() {
        let lines = build_raw_bash_lines("echo a\n\necho b", 80, usize::MAX);
        assert_eq!(lines.len(), 3, "blank separator must keep its row");
        assert_eq!(at(&lines, 1).width(), 0, "separator row must be empty");
        assert_eq!(row_text(at(&lines, 0)), "echo a");
        assert_eq!(row_text(at(&lines, 2)), "echo b");
    }

    #[test]
    fn heredoc_payload_stays_one_row_at_narrow_width() {
        let script = "cat <<EOF\nthis heredoc body line is much wider than the panel\nEOF";
        let lines = build_raw_bash_lines(script, 20, usize::MAX);
        let flat: Vec<String> = lines.iter().map(row_text).collect();
        assert_eq!(
            flat,
            vec![
                "cat <<EOF",
                "this heredoc body line is much wider than the panel",
                "EOF"
            ]
        );
    }

    #[test]
    fn stale_highlights_without_scoped_rows_disable_scope_ui() {
        let mut state = empty_view_state(None);
        state.bash_command_raw = Some("git status && cargo test".to_owned());
        state.bash_highlights = Some(BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["git".into(), "status".into()],
            suffix: vec![],
        });
        state.bash_selection_count = 2;
        state.options = vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("allow-once")),
                "Yes, proceed".to_owned(),
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("reject-once")),
                "No".to_owned(),
                acp::PermissionOptionKind::RejectOnce,
            ),
        ];
        assert!(!state.has_adjustable_scope(), "no scoped rows -> no arrows");
        assert!(
            !state.has_editable_bash_pattern(),
            "no allow-always-command -> no `e` editor"
        );

        state.options.push(acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("always-allow")),
            "always allow".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        ));
        assert!(
            !state.has_adjustable_scope(),
            "generic always-allow id must not enable arrows"
        );
        assert!(
            !state.has_editable_bash_pattern(),
            "generic always-allow id must not enable the editor"
        );

        state.options.push(acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-always-command")),
            "Always allow: git status".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        ));
        assert!(state.has_adjustable_scope());
        assert!(state.has_editable_bash_pattern());
    }

    #[test]
    fn reject_always_command_alone_enables_arrows_but_not_editor() {
        let mut state = empty_view_state(None);
        state.bash_command_raw = Some("cargo test --workspace".to_owned());
        state.bash_highlights = Some(BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["cargo".into(), "test".into()],
            suffix: vec![],
        });
        state.bash_selection_count = 2;
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("reject-always-command")),
            "Never allow: cargo test".to_owned(),
            acp::PermissionOptionKind::RejectAlways,
        )];
        assert!(state.has_adjustable_scope());
        assert!(
            !state.has_editable_bash_pattern(),
            "editor requires the exact allow-always-command row"
        );
    }

    #[test]
    fn body_never_dims_any_span() {
        for raw in [
            "git status --short && cargo test --workspace",
            "# comment first\ncargo test",
            "ps aux | grep pattern",
        ] {
            for width in [20usize, 200] {
                for line in build_permission_bash_lines(Some(raw), width, usize::MAX) {
                    for span in &line.spans {
                        assert!(
                            !span.style.add_modifier.contains(Modifier::DIM),
                            "body span {:?} must not be DIM ({raw:?} @ {width})",
                            span.content
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn prepare_bash_display_normalizes_crlf() {
        let raw = "echo a\r\necho b\r\n";
        let prepared = prepare_bash_display_text(raw);
        assert!(
            !prepared.contains('\r'),
            "CRLF not normalized: {prepared:?}"
        );
        assert_eq!(prepared, "echo a\necho b");
    }

    #[test]
    fn tiny_widths_do_not_panic() {
        let raw = "git status --short && cargo test --workspace | grep ok";
        for w in [0usize, 1, 2, 3] {
            let _ = build_raw_bash_lines(raw, w, usize::MAX);
            let _ = build_permission_bash_lines(Some(raw), w, usize::MAX);
            let _ = build_raw_bash_lines("échø 'ünîcødé && stüff' && lß", w, usize::MAX);
        }
    }

    #[test]
    fn multiline_continuation_wraps_without_delimiter_soft_breaks() {
        let raw = "cd /tmp && \\\n  git status --short --branch --verbose --long";
        let lines = build_permission_bash_lines(Some(raw), 20, usize::MAX);
        assert!(!lines.is_empty());
        let rows: Vec<String> = lines.iter().map(row_text).collect();
        for r in &rows {
            let t = r.trim_end();
            assert!(
                !(t.ends_with("&&") && rows.len() > 1),
                "must not soft-break at && for display: {rows:?}"
            );
        }
    }
}
