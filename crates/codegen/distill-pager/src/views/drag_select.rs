//! In-app drag-select over a scrolled column of plain text lines, shared by modals that copy the
//! selection on mouse-up (usage modal Session-info tab, memory modal preview).
//!
//! Endpoints are display columns into `lines`; callers own the press/drag state machine and the
//! copy, this module only maps mouse cells to text and paints the band.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

use crate::scrollback::text_selection::apply_selection_highlight;
use crate::scrollback::types::{col_past_grapheme, grapheme_cells_at, slice_display_cols};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextEndpoint {
    pub line_idx: usize,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextDrag {
    pub anchor: TextEndpoint,
    pub head: TextEndpoint,
}

impl TextDrag {
    pub(crate) fn ordered(self) -> (TextEndpoint, TextEndpoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub(crate) fn is_non_empty(self) -> bool {
        let (s, e) = self.ordered();
        s != e
    }
}

/// Text endpoint under a screen cell, given the visible `rect` and the first visible line index.
pub(crate) fn endpoint_at(
    lines: &[String],
    rect: Rect,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TextEndpoint> {
    if rect.width == 0 || rect.height == 0 || lines.is_empty() {
        return None;
    }
    let visible_row = row.saturating_sub(rect.y) as usize;
    let line_idx = scroll.saturating_add(visible_row);
    let text = lines.get(line_idx)?;
    let line_w = text.width().min(u16::MAX as usize) as u16;
    let col = column.saturating_sub(rect.x).min(line_w);
    Some(TextEndpoint { line_idx, col })
}

/// Like [`endpoint_at`], but a pointer outside `rect` (a drag past the edge) clamps to the nearest cell.
pub(crate) fn endpoint_at_clamped(
    lines: &[String],
    rect: Rect,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TextEndpoint> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let max_c = rect.x.saturating_add(rect.width.saturating_sub(1));
    let max_r = rect.y.saturating_add(rect.height.saturating_sub(1));
    endpoint_at(
        lines,
        rect,
        scroll,
        column.clamp(rect.x, max_c),
        row.clamp(rect.y, max_r),
    )
}

fn col_at_char_start(text: &str, col: u16) -> u16 {
    grapheme_cells_at(text, col).map_or(col, |cells| cells.start)
}

/// Display-column range `[lo, hi)` for one line of a drag, clamped to the panel width so copy and highlight cover the same characters.
pub(crate) fn selection_cols(
    drag: TextDrag,
    line_idx: usize,
    text: &str,
    panel_width: u16,
) -> Option<(u16, u16)> {
    let (start, end) = drag.ordered();
    if line_idx < start.line_idx || line_idx > end.line_idx {
        return None;
    }
    let line_w = (text.width().min(u16::MAX as usize) as u16).min(panel_width);
    let (raw_lo, raw_hi) = if start.line_idx == end.line_idx {
        (
            col_at_char_start(text, start.col),
            col_past_grapheme(text, end.col),
        )
    } else if line_idx == start.line_idx {
        (col_at_char_start(text, start.col), line_w)
    } else if line_idx == end.line_idx {
        (0, col_past_grapheme(text, end.col))
    } else {
        (0, line_w)
    };
    let lo = raw_lo.min(line_w);
    let hi = raw_hi.min(line_w);
    if hi < lo {
        return None;
    }
    // Blank lines yield hi == lo == 0; keep them so multi-line copy preserves newlines.
    if hi == lo && line_w > 0 {
        return None;
    }
    Some((lo, hi))
}

pub(crate) fn text_for_drag(drag: TextDrag, lines: &[String], panel_width: u16) -> Option<String> {
    let (start, end) = drag.ordered();
    if start.line_idx >= lines.len() {
        return None;
    }
    let mut out = String::new();
    let mut wrote_any = false;
    let last = end.line_idx.min(lines.len().saturating_sub(1));
    for (idx, text) in lines.iter().enumerate().take(last + 1).skip(start.line_idx) {
        let Some((lo, hi)) = selection_cols(drag, idx, text, panel_width) else {
            continue;
        };
        let slice = slice_display_cols(text, lo, hi);
        if wrote_any {
            out.push('\n');
        }
        out.push_str(&slice);
        wrote_any = true;
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Paint the selection band for the visible part of `drag`.
pub(crate) fn paint_text_drag(
    drag: TextDrag,
    lines: &[String],
    rect: Rect,
    scroll: usize,
    buf: &mut Buffer,
    theme: &Theme,
) {
    if !drag.is_non_empty() || rect.width == 0 || rect.height == 0 {
        return;
    }
    let (start, end) = drag.ordered();
    for idx in start.line_idx..=end.line_idx {
        let Some(text) = lines.get(idx) else {
            break;
        };
        let Some(visible_row) = idx.checked_sub(scroll) else {
            continue;
        };
        if visible_row >= rect.height as usize {
            break;
        }
        let Some((lo, hi)) = selection_cols(drag, idx, text, rect.width) else {
            continue;
        };
        let screen_y = rect.y + visible_row as u16;
        let x_lo = (rect.x + lo).min(rect.x + rect.width);
        let x_hi = (rect.x + hi).min(rect.x + rect.width);
        for x in x_lo..x_hi {
            if let Some(cell) = buf.cell_mut((x, screen_y)) {
                apply_selection_highlight(theme, cell);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_cols_clamps_to_panel_width() {
        let drag = TextDrag {
            anchor: TextEndpoint {
                line_idx: 0,
                col: 0,
            },
            head: TextEndpoint {
                line_idx: 1,
                col: 3,
            },
        };
        let long = "0123456789abcdef";
        assert_eq!(selection_cols(drag, 0, long, 10), Some((0, 10)));
        assert_eq!(selection_cols(drag, 1, "abcde", 10), Some((0, 4)));
    }
}
