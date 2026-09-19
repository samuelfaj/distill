//! Shared prompt-area list overlay: accent bar, bold title, and a scrollable single-line row list with a cursor.
//!
//! `/rewind`'s picker phase and `/jump` each used to keep this row geometry in sync by hand across their render, hit-test, and height functions.
//! Row content stays with the caller (a closure); this module owns the accent bar, title, cursor styling, and the scroll window.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Rows shown before the list scrolls (matches the historical picker cap).
const MAX_ROWS: usize = 15;

/// List geometry: row count and cursor position.
/// Construct per call; every method derives the same scroll window from these two fields, so the render, hit-test, and height paths cannot drift.
pub struct ListOverlay {
    pub len: usize,
    pub selected: usize,
}

/// Per-row style context handed to the row-content closure.
pub struct RowCtx {
    pub is_cursor: bool,
    /// Resolved row background (cursor rows get the visual-selection bg).
    pub row_bg: Color,
    /// Width available for the row's content.
    pub content_width: u16,
}

impl ListOverlay {
    /// Overlay height: title plus rows (at most [`MAX_ROWS`]), capped at 60% of the screen, plus one padding row.
    pub fn height(&self, screen_h: u16) -> u16 {
        let rows = self.len.min(MAX_ROWS) as u16;
        let h = 2 + rows;
        let cap = (screen_h as u32 * 60 / 100).max(6) as u16;
        h.min(cap) + 1
    }

    /// Rows that fit in `area` (title and padding excluded).
    fn visible_rows(area: Rect) -> usize {
        area.height.saturating_sub(3) as usize
    }

    /// First visible row index (keeps the cursor inside the window).
    fn scroll_offset(&self, visible_rows: usize) -> usize {
        if visible_rows > 0 && self.selected >= visible_rows {
            self.selected - visible_rows + 1
        } else {
            0
        }
    }

    /// Row index under a screen position, or `None` when the position misses the rows.
    pub fn row_at(&self, area: Rect, col: u16, row: u16) -> Option<usize> {
        if area.height == 0 || area.width < 10 {
            return None;
        }
        if col < area.x || col >= area.x + area.width {
            return None;
        }
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let first = area.y + 2;
        if row < first {
            return None;
        }
        let visible_rows = Self::visible_rows(area);
        let rel = (row - first) as usize;
        if rel >= visible_rows {
            return None;
        }
        let idx = self.scroll_offset(visible_rows) + rel;
        (idx < self.len).then_some(idx)
    }

    /// Render the overlay: bg fill, accent bar, title, then the visible window of rows.
    /// `row_line(idx, ctx)` produces each row's content; cursor and row backgrounds are painted here.
    /// Applies the standard unfocus dim, so callers must not blend again.
    pub fn render(
        &self,
        buf: &mut Buffer,
        area: Rect,
        title: &str,
        focused: bool,
        mut row_line: impl FnMut(usize, &RowCtx) -> Line<'static>,
    ) {
        if area.height == 0 || area.width < 10 {
            return;
        }

        let theme = Theme::current();
        let bg = theme.bg_light;
        buf.set_style(area, Style::default().bg(bg));

        let accent_style = Style::default().fg(theme.accent_user);
        for row in area.y..area.y + area.height {
            if let Some(cell) = buf.cell_mut((area.x, row)) {
                cell.set_symbol(crate::glyphs::accent_bar());
                cell.set_style(accent_style);
            }
        }

        let content_x = area.x + 3;
        let content_w = area.width.saturating_sub(5);

        let title_style = Style::default()
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD);
        let mut y = area.y + 1;
        buf.set_line(
            content_x,
            y,
            &Line::from(Span::styled(title.to_string(), title_style)),
            content_w,
        );
        y += 1;

        let visible_rows = Self::visible_rows(area);
        let scroll_offset = self.scroll_offset(visible_rows);

        for i in (scroll_offset..self.len).take(visible_rows) {
            if y >= area.y + area.height {
                break;
            }
            let is_cursor = i == self.selected;
            let row_rect = Rect {
                x: content_x.saturating_sub(1),
                y,
                width: content_w + 2,
                height: 1,
            };
            buf.set_style(row_rect, Style::default().bg(bg));

            let ctx = RowCtx {
                is_cursor,
                row_bg: bg,
                content_width: content_w,
            };
            let line = row_line(i, &ctx);
            buf.set_line(content_x, y, &line, content_w);
            // Selection band on RGB themes; reverse video on the terminal
            // theme (patched over the rendered row).
            if is_cursor && focused {
                buf.set_style(row_rect, theme.selection_overlay());
            }
            y += 1;
        }

        // Unfocus dim: blend foregrounds toward the panel bg so the overlay recedes when the prompt area is unfocused (prompt_widget pattern)
        if !focused {
            crate::render::color::recede_area(buf, area, bg, 0.66);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        }
    }

    #[test]
    fn row_at_maps_rows_and_rejects_chrome() {
        let list = ListOverlay {
            len: 3,
            selected: 0,
        };
        // Title at y+1; rows start at y+2.
        assert_eq!(list.row_at(area(), 5, 1), None);
        assert_eq!(list.row_at(area(), 5, 2), Some(0));
        assert_eq!(list.row_at(area(), 5, 4), Some(2));
        // Past the last row.
        assert_eq!(list.row_at(area(), 5, 5), None);
        // Outside horizontally.
        assert_eq!(list.row_at(area(), 99, 2), None);
    }

    #[test]
    fn row_at_respects_scroll_window() {
        // 20 rows, 7 visible (height 10 - 3), cursor at the end: the window starts at 13 so the cursor stays visible
        let list = ListOverlay {
            len: 20,
            selected: 19,
        };
        assert_eq!(list.row_at(area(), 5, 2), Some(13));
        assert_eq!(list.row_at(area(), 5, 8), Some(19));
    }

    /// Terminal theme (zero opaque cells): the cursor row carries reverse
    /// video instead of a painted band. RGB themes keep the `bg_visual`
    /// band and the row's own fgs.
    #[test]
    fn terminal_theme_cursor_row_uses_reverse_video() {
        use ratatui::style::Modifier;

        let _guard = crate::theme::cache::pin_theme();
        let list = ListOverlay {
            len: 3,
            selected: 1,
        };
        let render = || {
            let theme = Theme::current();
            let mut buf = Buffer::empty(area());
            list.render(&mut buf, area(), "Pick", true, |i, ctx| {
                Line::from(Span::styled(
                    format!("row {i}"),
                    Style::default().fg(theme.text_primary).bg(ctx.row_bg),
                ))
            });
            // Rows start at y+2; content at x+3.
            let Some(cursor) = buf.cell((3, 3)) else {
                panic!("cursor cell");
            };
            let Some(normal) = buf.cell((3, 2)) else {
                panic!("normal cell");
            };
            (cursor.style(), normal.style())
        };

        crate::theme::cache::set(crate::theme::ThemeKind::Terminal);
        let (cursor, normal) = render();
        assert!(
            cursor.add_modifier.contains(Modifier::REVERSED),
            "cursor row uses reverse video, got {cursor:?}"
        );
        assert_eq!(cursor.bg, Some(Color::Reset), "no painted band");
        assert!(!normal.add_modifier.contains(Modifier::REVERSED));

        crate::theme::cache::set(crate::theme::ThemeKind::GrokNight);
        let (cursor, normal) = render();
        let theme = Theme::current();
        assert_eq!(cursor.bg, Some(theme.bg_visual), "RGB keeps the band");
        assert_eq!(cursor.fg, Some(theme.text_primary), "RGB keeps row fgs");
        assert!(!cursor.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(normal.bg, Some(theme.bg_light));
    }

    #[test]
    fn height_caps_at_max_rows_and_screen_fraction() {
        let two = ListOverlay {
            len: 2,
            selected: 0,
        };
        assert_eq!(two.height(40), 5); // title + 2 rows + padding
        let many = ListOverlay {
            len: 30,
            selected: 0,
        };
        assert_eq!(many.height(40), 18); // 15-row cap
        assert_eq!(many.height(12), 8); // 60% screen cap
    }
}
