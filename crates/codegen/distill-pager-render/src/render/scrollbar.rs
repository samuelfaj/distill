//! This module provides scrollbar rendering using `tui-scrollbar` for smooth Unicode-based scrollbars with sub-character precision.
//!
//! # Visual Design
//!
//! The scrollbar visibility indicates follow mode state:
//! - **Following (at bottom):** Very dim scrollbar (subtle indicator of content above)
//! - **Not following:** Brighter scrollbar (draws attention to "scrolled up" state)
//!
//! # Layout
//!
//! Callers should reserve space for the scrollbar:
//! - 1 column gap (visual separation from content)
//! - 1 column track (the scrollbar itself)
//!
//! Use [`split_area_for_scrollbar`] to compute content and scrollbar areas.
//!
//! # TODO: Mouse Support
//!
//! `tui-scrollbar` already provides mouse interaction support via:
//! - [`tui_scrollbar::ScrollBarInteraction`] for drag state
//! - [`tui_scrollbar::ScrollEvent`] / [`tui_scrollbar::PointerEvent`] for input
//! - [`tui_scrollbar::ScrollBar::handle_event`] for hit testing and drag math
//!
//! To wire this up:
//! 1. Store `ScrollBarInteraction` in pane state
//! 2. Translate crossterm `MouseEvent` to `tui_scrollbar::PointerEvent`
//! 3. Call `scrollbar.handle_event()` to get `ScrollCommand::SetOffset`
//! 4. Update scroll position accordingly

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui_core::buffer::Buffer as CoreBuffer;
use ratatui_core::layout::Rect as CoreRect;
use ratatui_core::widgets::Widget as _;
use tui_scrollbar::ScrollBar;
use tui_scrollbar::ScrollLengths;
use tui_scrollbar::{SUBCELL, ScrollMetrics};

/// When set, every scrollbar renders as a no-op.
/// The pager toggles this on in minimal (scrollback-native) mode, where lists/dropdowns show no scrollbar at all.
/// They scroll internally and the footer carries the "↑/↓ navigate" hint.
static SCROLLBARS_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Globally hide or show all scrollbars. See [`SCROLLBARS_HIDDEN`].
pub fn set_scrollbars_hidden(hidden: bool) {
    SCROLLBARS_HIDDEN.store(hidden, Ordering::Relaxed);
}

/// Whether scrollbars are currently globally hidden.
pub fn scrollbars_hidden() -> bool {
    SCROLLBARS_HIDDEN.load(Ordering::Relaxed)
}

/// Number of columns reserved between content and the scrollbar track.
const SCROLLBAR_GAP_COLS: u16 = 1;

/// Width of the scrollbar track itself (in terminal cells).
const SCROLLBAR_TRACK_COLS: u16 = 1;

/// Total columns reserved for scrollbar UI (gap + track).
pub const SCROLLBAR_TOTAL_COLS: u16 = SCROLLBAR_GAP_COLS + SCROLLBAR_TRACK_COLS;

/// Always reserves the track plus a blank gap. `(content_area, None)` when too narrow.
/// Use [`maybe_split_for_scrollbar`] to reserve only when the bar will be shown.
pub fn split_area_for_scrollbar(area: Rect) -> (Rect, Option<Rect>) {
    if area.width <= SCROLLBAR_TOTAL_COLS {
        return (area, None);
    }

    let content_width = area.width.saturating_sub(SCROLLBAR_TOTAL_COLS);
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: content_width,
        height: area.height,
    };
    let scrollbar_area = Rect {
        x: area.right().saturating_sub(1),
        y: area.y,
        width: SCROLLBAR_TRACK_COLS,
        height: area.height,
    };

    (content_area, Some(scrollbar_area))
}

/// Unlike [`split_area_for_scrollbar`], this gives full width to content when scrollbar won't be shown (`total_lines <= viewport_lines`).
///
/// Use this when you know the content height before splitting.
pub fn maybe_split_for_scrollbar(area: Rect, total_lines: u16) -> (Rect, Option<Rect>) {
    if needs_scrollbar(total_lines, area.height) {
        split_area_for_scrollbar(area)
    } else {
        (area, None)
    }
}

/// Whether the scrollbar should be shown (content overflows viewport).
pub fn needs_scrollbar(total_lines: u16, viewport_lines: u16) -> bool {
    total_lines > viewport_lines
}

/// Track plus one column of slop: a thumb flush against a border is read as a two-column widget, so near-miss presses must still grab.
pub fn scrollbar_grab_zone(track: Rect) -> Rect {
    let x = track.x.saturating_sub(SCROLLBAR_GAP_COLS);
    Rect {
        x,
        y: track.y,
        width: (track.x - x).saturating_add(track.width).saturating_add(1),
        height: track.height,
    }
}

/// Result of mapping a scrollbar click/drag position to a scroll offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarClickResult {
    /// Jump to the very top (click on first row of track).
    Top,
    /// Jump to the very bottom (click on last row of track).
    Bottom,
    /// Set scroll offset to this value (proportional position).
    Offset(usize),
}

/// Inverse of the renderer's `ScrollMetrics`, so a click lands where the thumb is drawn.
/// First/last row is Top/Bottom; otherwise the thumb is centered on the click.
pub fn scrollbar_click_to_offset(
    cell_index: u16,
    track_cells: u16,
    total_lines: u16,
    viewport_lines: u16,
) -> ScrollbarClickResult {
    if track_cells == 0 {
        return ScrollbarClickResult::Top;
    }

    if cell_index == 0 {
        return ScrollbarClickResult::Top;
    }
    if cell_index >= track_cells.saturating_sub(1) {
        return ScrollbarClickResult::Bottom;
    }

    let lengths = ScrollLengths {
        content_len: total_lines as usize,
        viewport_len: viewport_lines as usize,
    };
    let metrics = ScrollMetrics::new(lengths, 0, track_cells);

    // Center the thumb on the clicked cell (same as tui_scrollbar JumpToClick).
    let position = (cell_index as usize)
        .saturating_mul(SUBCELL)
        .saturating_add(SUBCELL / 2);
    let half_thumb = metrics.thumb_len() / 2;
    let thumb_start = position.saturating_sub(half_thumb);
    let offset = metrics.offset_for_thumb_start(thumb_start);

    ScrollbarClickResult::Offset(offset)
}

/// Always drawn on overflow. Dim while following; brighter when detached so the user notices they left the tail.
pub fn render_scrollbar(
    buf: &mut Buffer,
    scrollbar_area: Option<Rect>,
    total_lines: u16,
    viewport_lines: u16,
    offset: u16,
    is_following: bool,
) {
    let (track_style, thumb_style) = scrollbar_styles(is_following);
    render_scrollbar_styled(
        buf,
        scrollbar_area,
        total_lines,
        viewport_lines,
        offset,
        track_style,
        thumb_style,
    );
}

/// Some emulators (notably macOS Terminal.app) do not stretch the `█` glyph over the cell's line-gap pixels.
/// A foreground-only thumb therefore renders striped with dark bars; the background fill covers the whole cell box.
fn thumb_fill_style(thumb_style: Style) -> Style {
    match thumb_style.fg {
        Some(fg) => thumb_style.bg(fg),
        None => thumb_style,
    }
}

/// Get track and thumb styles based on follow mode.
fn scrollbar_styles(is_following: bool) -> (Style, Style) {
    let theme = crate::theme::Theme::current();
    if is_following {
        // Very dim: scrollbar is subtle when following
        let track_style = Style::new().bg(theme.scrollbar_bg);
        let thumb_style = Style::new().fg(theme.scrollbar_fg).bg(theme.scrollbar_bg);
        (track_style, thumb_style)
    } else {
        // Brighter: scrollbar stands out when scrolled up
        let track_style = Style::new().bg(theme.bg_highlight);
        let thumb_style = Style::new().fg(theme.gray).bg(theme.bg_highlight);
        (track_style, thumb_style)
    }
}

/// Like [`render_scrollbar`] but allows custom styling for theme integration.
pub fn render_scrollbar_styled(
    buf: &mut Buffer,
    scrollbar_area: Option<Rect>,
    total_lines: u16,
    viewport_lines: u16,
    offset: u16,
    track_style: Style,
    thumb_style: Style,
) {
    if SCROLLBARS_HIDDEN.load(Ordering::Relaxed) {
        return;
    }

    let Some(scrollbar_area) = scrollbar_area else {
        return;
    };

    if scrollbar_area.width == 0 || scrollbar_area.height == 0 {
        return;
    }

    if !needs_scrollbar(total_lines, viewport_lines) {
        return;
    }

    let lengths = ScrollLengths {
        content_len: total_lines as usize,
        viewport_len: viewport_lines as usize,
    };

    let scrollbar = ScrollBar::vertical(lengths).offset(offset as usize);

    // Render into ratatui-core scratch buffer
    let core_area = CoreRect {
        x: scrollbar_area.x,
        y: scrollbar_area.y,
        width: scrollbar_area.width,
        height: scrollbar_area.height,
    };
    let mut scratch = CoreBuffer::empty(core_area);
    (&scrollbar).render(core_area, &mut scratch);

    // Copy to ratatui buffer with custom styling
    let thumb_fill = thumb_fill_style(thumb_style);
    for row in 0..scrollbar_area.height {
        let x = scrollbar_area.x;
        let y = scrollbar_area.y + row;
        let Some(src) = scratch.cell((x, y)) else {
            continue;
        };
        let Some(dst) = buf.cell_mut((x, y)) else {
            continue;
        };
        if src.symbol() == " " {
            dst.set_symbol(" ");
            dst.set_style(track_style);
        } else {
            dst.set_symbol("\u{2588}");
            dst.set_style(thumb_fill);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn test_split_area_normal() {
        let area = Rect::new(0, 0, 40, 10);
        let (content, scrollbar) = split_area_for_scrollbar(area);

        // Content width is 40 minus the 2 columns for gap + track
        assert_eq!(content.width, 38);
        assert_eq!(content.height, 10);

        let sb = scrollbar.expect("scrollbar area");
        assert_eq!(sb.x, 39);
        assert_eq!(sb.width, 1);
        assert_eq!(sb.height, 10);
    }

    #[test]
    fn test_split_area_too_narrow() {
        let area = Rect::new(0, 0, 2, 10);
        let (content, scrollbar) = split_area_for_scrollbar(area);

        assert_eq!(content, area);
        assert!(scrollbar.is_none());
    }

    #[test]
    fn test_maybe_split_reserves_when_needed() {
        let area = Rect::new(0, 0, 40, 10);

        // Content overflows (20 > 10), so scrollbar space is reserved
        let (content, scrollbar) = maybe_split_for_scrollbar(area, 20);
        assert_eq!(content.width, 38); // Reduced by 2 for gap + scrollbar track
        assert!(scrollbar.is_some());
    }

    #[test]
    fn test_maybe_split_full_width_when_not_needed() {
        let area = Rect::new(0, 0, 40, 10);

        // Content fits (5 <= 10), so content gets full width
        let (content, scrollbar) = maybe_split_for_scrollbar(area, 5);
        assert_eq!(content.width, 40);
        assert!(scrollbar.is_none());
    }

    #[test]
    fn test_scrollbar_grab_zone() {
        let zone = scrollbar_grab_zone(Rect::new(39, 2, 1, 10));
        assert_eq!(zone, Rect::new(38, 2, 3, 10));
        assert!(zone.contains((38, 2).into()), "gap column grabs");
        assert!(zone.contains((39, 11).into()), "track grabs");
        assert!(zone.contains((40, 5).into()), "border column grabs");
        assert!(!zone.contains((37, 5).into()), "two columns left is out");
        assert!(!zone.contains((41, 5).into()), "two columns right is out");
        assert!(!zone.contains((39, 12).into()), "rows are bounded");

        let zone = scrollbar_grab_zone(Rect::new(0, 0, 1, 4));
        assert_eq!(zone, Rect::new(0, 0, 2, 4));
    }

    #[test]
    fn test_needs_scrollbar() {
        assert!(needs_scrollbar(100, 10)); // Content > viewport
        assert!(!needs_scrollbar(10, 10)); // Content == viewport
        assert!(!needs_scrollbar(5, 10)); // Content < viewport
    }

    #[test]
    fn test_render_scrollbar_no_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 10));
        // Should not panic with None area
        render_scrollbar(&mut buf, None, 100, 10, 0, false);
    }

    #[test]
    fn test_render_scrollbar_no_overflow() {
        let area = Rect::new(0, 0, 10, 10);
        let (_, scrollbar_area) = split_area_for_scrollbar(area);
        let mut buf = Buffer::empty(area);

        // Content fits, so nothing renders
        render_scrollbar(&mut buf, scrollbar_area, 5, 10, 0, false);

        // Check scrollbar column is empty (spaces with no custom background)
        let sb = scrollbar_area.unwrap();
        for y in 0..sb.height {
            let Some(cell) = buf.cell((sb.x, sb.y + y)) else {
                panic!("missing scrollbar cell at y={y}");
            };
            assert_eq!(cell.symbol(), " ");
            if let Some(Color::Rgb(_, _, _)) = cell.style().bg {
                panic!("Should not have RGB background when no scrollbar rendered");
            }
        }
    }

    #[test]
    fn test_render_scrollbar_following_vs_not() {
        // Pinned: asserts distinct RGB thumb bgs, which the ambient terminal
        // theme (all-Reset bgs) legitimately doesn't produce.
        let _guard = crate::theme::cache::pin_theme();
        let area = Rect::new(0, 0, 10, 10);
        let (_, scrollbar_area) = split_area_for_scrollbar(area);

        // Render following
        let mut buf_following = Buffer::empty(area);
        render_scrollbar(&mut buf_following, scrollbar_area, 100, 10, 90, true);

        // Render not following
        let mut buf_not_following = Buffer::empty(area);
        render_scrollbar(&mut buf_not_following, scrollbar_area, 100, 10, 50, false);

        let sb = scrollbar_area.unwrap();
        let Some(following_style) = buf_following.cell((sb.x, sb.y)).map(|c| c.style()) else {
            panic!("missing following scrollbar cell");
        };
        let Some(not_following_style) = buf_not_following.cell((sb.x, sb.y)).map(|c| c.style())
        else {
            panic!("missing not-following scrollbar cell");
        };

        assert!(following_style.bg.is_some());
        assert!(not_following_style.bg.is_some());

        // At 256-color or truecolor the backgrounds differ; at Basic (16-color) both dark grays map to Black
        if crate::theme::color_support::get().has_256() {
            assert_ne!(following_style.bg, not_following_style.bg);
        }
    }

    /// macOS Terminal.app leaves line-gap pixels unpainted under a foreground-only `█`, striping the thumb with dark bars.
    #[test]
    fn test_thumb_cells_fill_background() {
        let area = Rect::new(0, 0, 10, 10);
        let (_, scrollbar_area) = split_area_for_scrollbar(area);
        let sb = scrollbar_area.unwrap();

        let track = Style::new().bg(Color::Black);
        let thumb = Style::new().fg(Color::White).bg(Color::Black);
        let mut buf = Buffer::empty(area);
        render_scrollbar_styled(&mut buf, scrollbar_area, 100, 10, 50, track, thumb);

        let mut thumb_cells = 0;
        for y in 0..sb.height {
            let Some(cell) = buf.cell((sb.x, sb.y + y)) else {
                panic!("missing scrollbar cell at y={y}");
            };
            if cell.symbol() == "\u{2588}" {
                thumb_cells += 1;
                assert_eq!(
                    cell.style().bg,
                    cell.style().fg,
                    "thumb cell background must match the glyph color"
                );
            } else {
                assert_eq!(cell.style().bg, Some(Color::Black), "track keeps its bg");
            }
        }
        assert!(thumb_cells > 0, "a thumb must be rendered");
    }

    #[test]
    fn test_scrollbar_thumb_position() {
        let area = Rect::new(0, 0, 10, 10);
        let (_, scrollbar_area) = split_area_for_scrollbar(area);
        let sb = scrollbar_area.unwrap();

        // At top
        let mut buf_top = Buffer::empty(area);
        render_scrollbar(&mut buf_top, scrollbar_area, 100, 10, 0, false);

        // At bottom
        let mut buf_bottom = Buffer::empty(area);
        render_scrollbar(&mut buf_bottom, scrollbar_area, 100, 10, 90, false);

        // Count thumb cells (non-space)
        let count_thumb = |buf: &Buffer| -> usize {
            (0..sb.height)
                .filter(|&y| {
                    buf.cell((sb.x, sb.y + y))
                        .is_some_and(|c| c.symbol() != " ")
                })
                .count()
        };

        let top_thumb = count_thumb(&buf_top);
        let bottom_thumb = count_thumb(&buf_bottom);
        assert!(top_thumb > 0, "Should have thumb at top");
        assert!(bottom_thumb > 0, "Should have thumb at bottom");

        assert_eq!(top_thumb, bottom_thumb, "Thumb size should be consistent");

        let thumb_positions = |buf: &Buffer| -> Vec<u16> {
            (0..sb.height)
                .filter(|&y| {
                    buf.cell((sb.x, sb.y + y))
                        .is_some_and(|c| c.symbol() != " ")
                })
                .collect()
        };

        let top_pos = thumb_positions(&buf_top);
        let bottom_pos = thumb_positions(&buf_bottom);
        assert_ne!(
            top_pos, bottom_pos,
            "Thumb should be at different positions"
        );
    }
}
