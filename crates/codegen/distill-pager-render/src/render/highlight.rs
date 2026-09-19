//! Match-highlight overlay shared by the list pane and other views that show search matches.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::render::bidi::{is_enabled, logical_cols_to_visual, needs_bidi};
use crate::render::wrapping::{
    byte_offset_to_display_col, byte_range_to_row_cols, wrap_byte_ranges_matching,
};

/// Post-pass invert so matches show regardless of underlying colors. Stops at `viewport_bottom`.
/// Remap match columns to visual cells only when the caller painted bidi-reordered; logical painters must pass `map_visual = false`.
#[allow(clippy::too_many_arguments)]
pub fn paint_match_highlights(
    buf: &mut Buffer,
    area: Rect,
    row_y: u16,
    viewport_bottom: u16,
    skip: u16,
    prefix_w: u16,
    text: &str,
    re: &regex::Regex,
    single_row: bool,
    map_visual: bool,
) {
    if text.is_empty() {
        return;
    }

    if single_row {
        let map_bidi = map_visual && is_enabled() && needs_bidi(text);
        for m in re.find_iter(text) {
            let log_start = byte_offset_to_display_col(text, m.start());
            let log_end = byte_offset_to_display_col(text, m.end());
            let ranges = if map_bidi {
                logical_cols_to_visual(text, log_start, log_end)
            } else {
                vec![(log_start, log_end)]
            };
            for (col_start, col_end) in ranges {
                for col in col_start..col_end {
                    let x = area.x + prefix_w + col as u16;
                    if x < area.x + area.width
                        && let Some(cell) = buf.cell_mut((x, row_y))
                    {
                        invert_cell(cell);
                    }
                }
            }
        }
        return;
    }

    let text_w = area.width.saturating_sub(prefix_w) as usize;
    let ranges = wrap_byte_ranges_matching(text, text_w);
    for m in re.find_iter(text) {
        for seg in byte_range_to_row_cols(text, &ranges, m.start()..m.end()) {
            if seg.row < skip as usize {
                continue;
            }
            let y = row_y + (seg.row - skip as usize) as u16;
            if y >= viewport_bottom {
                break;
            }
            let Some(row_range) = ranges.get(seg.row) else {
                continue;
            };
            let Some(row_text) = text.get(row_range.start..row_range.end) else {
                continue;
            };
            let visual_ranges = if map_visual && is_enabled() && needs_bidi(row_text) {
                logical_cols_to_visual(row_text, seg.col_start, seg.col_end)
            } else {
                vec![(seg.col_start, seg.col_end)]
            };
            for (col_start, col_end) in visual_ranges {
                for col in col_start..col_end {
                    let x = area.x + prefix_w + col as u16;
                    if x < area.x + area.width
                        && let Some(cell) = buf.cell_mut((x, y))
                    {
                        invert_cell(cell);
                    }
                }
            }
        }
    }
}

/// Apply the terminal's REVERSED attribute so the fg/bg swap is native and respects the user's theme.
fn invert_cell(cell: &mut ratatui::buffer::Cell) {
    cell.modifier.insert(ratatui::style::Modifier::REVERSED);
}
