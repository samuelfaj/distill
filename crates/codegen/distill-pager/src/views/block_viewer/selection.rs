use std::time::Instant;

use crossterm::event::MouseEventKind;
use ratatui::text::Line;

use crate::scrollback::text_selection::{configured_word_separators, word_boundaries_at_col};
use crate::theme::Theme;
use crate::views::list_pane::ListItem;

use super::*;

/// Selection endpoint in the block viewer's unified item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextEndpoint {
    pub item_idx: usize,
    /// Zero is before the first character.
    pub col: u16,
}

/// Character-level selection in the block viewer modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextDrag {
    pub anchor: TextEndpoint,
    pub head: TextEndpoint,
    /// Sticky after release: still visible, not extended.
    pub active: bool,
}

impl TextDrag {
    pub fn ordered(&self) -> (TextEndpoint, TextEndpoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn is_non_empty(&self) -> bool {
        let (s, e) = self.ordered();
        s != e
    }

    /// Released one-cell word/paragraph ranges have `start == end`.
    fn covers_text(&self) -> bool {
        let (s, e) = self.ordered();
        s != e || !self.active
    }
}

const MULTI_CLICK_TIMEOUT_MS: u128 = 300;

pub(crate) fn format_blockquote(text: &str) -> String {
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return String::new();
    }
    text.lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl BlockViewerPane {
    pub(super) fn has_sticky_selection(&self) -> bool {
        self.text_drag.is_some_and(|d| !d.active && d.covers_text())
    }

    pub(super) fn clear_text_drag(&mut self) {
        self.text_drag = None;
        self.drag_copy_text = None;
        self.click_count = 0;
        self.last_click_at = None;
        self.last_click_ep = None;
    }

    pub(super) fn clear_stale_selection(&mut self) {
        self.clear_text_drag();
        self.list_state.exit_visual_mode();
    }

    pub fn selected_plain_text(&self) -> String {
        if let Some(drag) = self.text_drag
            && drag.covers_text()
        {
            let unified = self.unified_items_owned();
            return self.text_for_drag(drag, &unified).unwrap_or_default();
        }
        if self.list_state.visual_mode {
            return self.visual_plain_text().unwrap_or_default();
        }
        self.current_line_plain_text().unwrap_or_default()
    }

    fn unified_items_owned(&self) -> Vec<ContentLine> {
        if self.cached_unified.len() == self.prepend_items.len() + self.items.len()
            && !self.cached_unified.is_empty()
        {
            return self.cached_unified.clone();
        }
        let mut unified = self.prepend_items.clone();
        unified.extend(self.items.iter().cloned());
        unified
    }

    fn current_line_plain_text(&self) -> Option<String> {
        if let Some(vi) = self.list_state.selected_index() {
            return self.plain_text_at_physical(self.list_state.to_physical(vi));
        }
        if self.list_state.follow_mode {
            self.follow_mode_plain_text()
        } else {
            None
        }
    }

    fn follow_mode_plain_text(&self) -> Option<String> {
        self.list_state.visible_range().rev().find_map(|vi| {
            let text = self.body_plain_text_at_physical(self.list_state.to_physical(vi))?;
            (!text.is_empty()).then_some(text)
        })
    }

    fn plain_text_at_physical(&self, idx: usize) -> Option<String> {
        let pre = self.prepend_items.len();
        if idx < pre {
            return self.prepend_items.get(idx).map(|item| item.copy_text());
        }
        self.items.get(idx - pre).map(|item| item.copy_text())
    }

    fn body_plain_text_at_physical(&self, idx: usize) -> Option<String> {
        let pre = self.prepend_items.len();
        if idx < pre {
            return None;
        }
        self.items.get(idx - pre).map(|item| item.copy_text())
    }

    fn visual_plain_text(&self) -> Option<String> {
        let range = self.list_state.copy_range()?;
        let mut parts = Vec::new();
        for vi in range {
            let i = self.list_state.to_physical(vi);
            if let Some(text) = self.plain_text_at_physical(i) {
                parts.push(text);
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    fn word_range_at(&self, ep: TextEndpoint, items: &[ContentLine]) -> Option<TextDrag> {
        let item = items.get(ep.item_idx)?;
        let text = item.copy_text();
        let range = word_boundaries_at_col(&text, ep.col, configured_word_separators());
        if range.start >= range.end {
            return None;
        }
        Some(TextDrag {
            anchor: TextEndpoint {
                item_idx: ep.item_idx,
                col: range.start,
            },
            head: TextEndpoint {
                item_idx: ep.item_idx,
                col: range.end.saturating_sub(1),
            },
            active: false,
        })
    }

    fn paragraph_range_at(&self, ep: TextEndpoint) -> Option<TextDrag> {
        let pre = self.prepend_items.len();
        if ep.item_idx < pre {
            return None;
        }
        let body_idx = ep.item_idx - pre;
        if body_idx >= self.items.len() {
            return None;
        }
        let is_blank = |i: usize| {
            self.items
                .get(i)
                .is_none_or(|item| item.copy_text().is_empty())
        };
        if is_blank(body_idx) {
            return None;
        }
        let mut start = body_idx;
        while start > 0 && !is_blank(start - 1) {
            start -= 1;
        }
        let mut end = body_idx;
        while end + 1 < self.items.len() && !is_blank(end + 1) {
            end += 1;
        }
        let end_item = self.items.get(end)?;
        let end_width = crate::scrollback::types::str_display_cells(&end_item.copy_text())
            .min(u16::MAX as usize) as u16;
        Some(TextDrag {
            anchor: TextEndpoint {
                item_idx: pre + start,
                col: 0,
            },
            head: TextEndpoint {
                item_idx: pre + end,
                col: end_width.saturating_sub(1),
            },
            active: false,
        })
    }
    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind as MEK};
        self.rebuild_unified_cache();
        let pane_area = self.last_content_area;
        let scrollbar_x_range = self
            .list_state
            .scrollbar_area()
            .map(|sb| (sb.x, sb.x + sb.width));

        let in_content = |c: u16, r: u16| -> bool {
            r >= pane_area.y
                && r < pane_area.y + pane_area.height
                && c >= pane_area.x
                && c < pane_area.x + pane_area.width
                && match scrollbar_x_range {
                    Some((lo, hi)) => !(c >= lo && c < hi),
                    None => true,
                }
        };

        match kind {
            MEK::Down(MouseButton::Left) if in_content(col, row) => {
                self.list_state.exit_visual_mode();
                let virtual_y =
                    self.list_state.scroll_offset() + (row.saturating_sub(pane_area.y) as usize);
                self.list_state.select_at_y(virtual_y, &self.cached_unified);
                if let Some(ep) = self.screen_to_endpoint(col, row, &self.cached_unified) {
                    let now = Instant::now();
                    let same_spot = self.last_click_ep.is_some_and(|prev| {
                        prev.item_idx == ep.item_idx && prev.col.abs_diff(ep.col) <= 1
                    });
                    let within = self.last_click_at.is_some_and(|t| {
                        now.duration_since(t).as_millis() <= MULTI_CLICK_TIMEOUT_MS
                    });
                    if same_spot && within {
                        self.click_count = (self.click_count % 3) + 1;
                    } else {
                        self.click_count = 1;
                    }
                    self.last_click_at = Some(now);
                    self.last_click_ep = Some(ep);

                    let multi = match self.click_count {
                        2 => self.word_range_at(ep, &self.cached_unified),
                        3 => self.paragraph_range_at(ep),
                        _ => None,
                    };
                    if let Some(range) = multi {
                        self.drag_copy_text = self.text_for_drag(range, &self.cached_unified);
                        self.text_drag = Some(range);
                    } else {
                        self.text_drag = Some(TextDrag {
                            anchor: ep,
                            head: ep,
                            active: true,
                        });
                    }
                } else {
                    self.clear_text_drag();
                }
                true
            }
            MEK::Drag(MouseButton::Left) => {
                if let Some(drag) = self.text_drag
                    && drag.active
                {
                    if row < pane_area.y {
                        let distance = pane_area.y.saturating_sub(row) as i32;
                        self.list_state
                            .scroll_lines(-distance.clamp(1, 5), &self.cached_unified);
                    } else if row >= pane_area.y + pane_area.height {
                        let distance =
                            (row.saturating_sub(pane_area.y + pane_area.height) + 1) as i32;
                        self.list_state
                            .scroll_lines(distance.clamp(1, 5), &self.cached_unified);
                    }

                    let max_c = pane_area.x + pane_area.width.saturating_sub(1);
                    let max_r = pane_area.y + pane_area.height.saturating_sub(1);
                    let cc = col.clamp(pane_area.x, max_c);
                    let rr = row.clamp(pane_area.y, max_r);
                    if let Some(ep) = self.screen_to_endpoint(cc, rr, &self.cached_unified)
                        && let Some(d) = self.text_drag.as_mut()
                    {
                        d.head = ep;
                    }
                    return true;
                }
                false
            }
            MEK::Up(MouseButton::Left) => {
                if let Some(mut drag) = self.text_drag {
                    if drag.active {
                        let max_c = pane_area.x + pane_area.width.saturating_sub(1);
                        let max_r = pane_area.y + pane_area.height.saturating_sub(1);
                        let cc = col.clamp(pane_area.x, max_c);
                        let rr = row.clamp(pane_area.y, max_r);
                        if let Some(ep) = self.screen_to_endpoint(cc, rr, &self.cached_unified) {
                            drag.head = ep;
                        }

                        if drag.is_non_empty() {
                            self.drag_copy_text = self.text_for_drag(drag, &self.cached_unified);
                            drag.active = false;
                            self.text_drag = Some(drag);
                        } else {
                            self.text_drag = None;
                        }
                        return true;
                    }
                    return true;
                }
                true
            }
            MEK::Down(_) => {
                self.clear_text_drag();
                self.list_state
                    .handle_mouse_event(kind, col, row, pane_area, &self.cached_unified)
            }
            _ => {
                self.list_state
                    .handle_mouse_event(kind, col, row, pane_area, &self.cached_unified)
            }
        }
    }

    fn screen_to_endpoint(
        &self,
        col: u16,
        row: u16,
        items: &[ContentLine],
    ) -> Option<TextEndpoint> {
        let pane = self.last_content_area;
        if row < pane.y || col < pane.x {
            return None;
        }
        let virtual_y = self.list_state.scroll_offset() + (row - pane.y) as usize;
        let vis_idx = self.list_state.layout().item_at_y(virtual_y)?;
        let item_idx = self.list_state.to_physical(vis_idx);
        let item = items.get(item_idx)?;
        let item_top = self.list_state.layout().virtual_y(vis_idx);
        let sub_row = virtual_y.saturating_sub(item_top) as u16;
        let col_in_sub = col.saturating_sub(pane.x);

        // Without wrap joiners, absolute_col drifts at each break.
        let wrap_w = self.effective_wrap_width();
        let (wrapped, joiners) = self.wrap_item_with_joiners(item, wrap_w);
        let mut absolute_col: u16 = 0;
        for (i, line) in wrapped.iter().enumerate() {
            if i > 0
                && let Some(Some(j)) = joiners.get(i)
            {
                absolute_col = absolute_col
                    .saturating_add(crate::scrollback::types::str_display_cells(j.as_str()) as u16);
            }
            let line_w = line_display_width_u16(line);
            if i == sub_row as usize {
                if col_in_sub >= line_w && i + 1 < wrapped.len() {
                    let text = item.copy_text();
                    let total = crate::scrollback::types::str_display_cells(&text)
                        .min(u16::MAX as usize) as u16;
                    return Some(TextEndpoint {
                        item_idx,
                        col: total,
                    });
                }
                // Endpoints are logical (copy slices logical text), so map the clicked visual cell back to its logical column in this sub-row
                let sub_logical = crate::scrollback::types::line_plain_text(line);
                let logical_in_sub = crate::render::bidi::visual_col_to_logical_col(
                    &sub_logical,
                    col_in_sub.min(line_w) as usize,
                ) as u16;
                absolute_col = absolute_col.saturating_add(logical_in_sub);
                return Some(TextEndpoint {
                    item_idx,
                    col: absolute_col,
                });
            }
            absolute_col = absolute_col.saturating_add(line_w);
        }
        Some(TextEndpoint {
            item_idx,
            col: absolute_col,
        })
    }

    fn effective_wrap_width(&self) -> u16 {
        let pane = self.last_content_area;
        let pane_right = pane.x + pane.width;
        match self.list_state.scrollbar_area() {
            Some(sb) if sb.x >= pane.x && sb.x < pane_right => sb.x - pane.x,
            _ => pane.width,
        }
    }

    /// Joiner widths map screen columns back to the unwrapped line.
    fn wrap_item_with_joiners(
        &self,
        item: &ContentLine,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<Option<String>>) {
        if width == 0 {
            return (vec![item.content.clone()], vec![None]);
        }
        let (wrapped, joiners) =
            crate::render::wrapping::word_wrap_line_with_joiners(&item.content, width as usize);
        if wrapped.is_empty() {
            (vec![item.content.clone()], vec![None])
        } else {
            let lines = wrapped
                .iter()
                .map(crate::render::line_utils::line_to_static)
                .collect();
            (lines, joiners)
        }
    }

    fn col_at_char_start(text: &str, col: u16) -> u16 {
        crate::scrollback::types::grapheme_cells_at(text, col).map_or(col, |cells| cells.start)
    }

    fn text_for_drag(&self, drag: TextDrag, items: &[ContentLine]) -> Option<String> {
        let (start, end) = drag.ordered();
        let mut out = String::new();
        let mut wrote_any = false;
        for idx in start.item_idx..=end.item_idx {
            if self.list_state.to_visible(idx).is_none() {
                continue;
            }
            let Some(item) = items.get(idx) else {
                break;
            };
            let text = item.copy_text();
            let slice = if start.item_idx == end.item_idx {
                let lo = Self::col_at_char_start(&text, start.col);
                let hi = crate::scrollback::types::col_past_grapheme(&text, end.col);
                crate::scrollback::types::slice_display_cols(&text, lo, hi)
            } else if idx == start.item_idx {
                let lo = Self::col_at_char_start(&text, start.col);
                crate::scrollback::types::slice_display_cols(&text, lo, u16::MAX)
            } else if idx == end.item_idx {
                let hi = crate::scrollback::types::col_past_grapheme(&text, end.col);
                crate::scrollback::types::slice_display_cols(&text, 0, hi)
            } else {
                text
            };
            if wrote_any {
                out.push('\n');
            }
            out.push_str(&slice);
            wrote_any = true;
        }
        if out.is_empty() { None } else { Some(out) }
    }

    /// Paint the text selection after render_content.
    pub fn render_text_drag_overlay(&self, buf: &mut ratatui::buffer::Buffer) {
        let Some(drag) = self.text_drag else {
            return;
        };
        if !drag.covers_text() {
            return;
        }
        let theme = Theme::current();
        let pane = self.last_content_area;
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        let (start, end) = drag.ordered();

        // Advance end past the character under the cursor so the highlight includes the pointed-at character (matches copy)
        let end_col_hi = self
            .cached_unified
            .get(end.item_idx)
            .map(|item| crate::scrollback::types::col_past_grapheme(&item.copy_text(), end.col))
            .unwrap_or(end.col);
        let end = TextEndpoint {
            item_idx: end.item_idx,
            col: end_col_hi,
        };

        // Floor the start onto its grapheme so the overlay matches the copy.
        let start_col_lo = self
            .cached_unified
            .get(start.item_idx)
            .map(|item| Self::col_at_char_start(&item.copy_text(), start.col))
            .unwrap_or(start.col);
        let start = TextEndpoint {
            item_idx: start.item_idx,
            col: start_col_lo,
        };

        let scroll = self.list_state.scroll_offset();
        let pane_top = pane.y;
        let pane_bottom = pane.y + pane.height;
        let wrap_w = self.effective_wrap_width();

        for idx in start.item_idx..=end.item_idx {
            let Some(item) = self.cached_unified.get(idx) else {
                break;
            };
            let Some(vi) = self.list_state.to_visible(idx) else {
                continue;
            };
            let item_top = self.list_state.layout().virtual_y(vi);
            let item_h = self.list_state.layout().item_height(vi) as usize;
            if item_top + item_h <= scroll {
                continue;
            }
            if item_top >= scroll + pane.height as usize {
                break;
            }
            let (wrapped, joiners) = self.wrap_item_with_joiners(item, wrap_w);
            let (item_lo, item_hi) = if start.item_idx == end.item_idx {
                (start.col, end.col)
            } else if idx == start.item_idx {
                (start.col, u16::MAX)
            } else if idx == end.item_idx {
                (0, end.col)
            } else {
                (0, u16::MAX)
            };
            let mut acc_col: u16 = 0;
            for (sub_i, line) in wrapped.iter().enumerate() {
                if sub_i > 0
                    && let Some(Some(j)) = joiners.get(sub_i)
                {
                    acc_col = acc_col.saturating_add(crate::scrollback::types::str_display_cells(
                        j.as_str(),
                    ) as u16);
                }
                let line_w = line_display_width_u16(line);
                let sub_start = acc_col;
                let sub_end = acc_col.saturating_add(line_w);
                acc_col = sub_end;
                let sel_start = item_lo.max(sub_start);
                let sel_end = item_hi.min(sub_end);
                if sel_end <= sel_start {
                    continue;
                }
                let virtual_y = item_top + sub_i;
                if virtual_y < scroll {
                    continue;
                }
                let screen_y = pane_top + (virtual_y - scroll) as u16;
                if screen_y >= pane_bottom {
                    break;
                }
                let local_lo = sel_start - sub_start;
                let local_hi = sel_end - sub_start;
                let sub_logical = crate::scrollback::types::line_plain_text(line);
                let visual_ranges = if crate::render::bidi::is_enabled()
                    && crate::render::bidi::needs_bidi(&sub_logical)
                {
                    crate::render::bidi::logical_cols_to_visual(
                        &sub_logical,
                        local_lo as usize,
                        local_hi as usize,
                    )
                } else {
                    vec![(local_lo as usize, local_hi as usize)]
                };
                let pane_right = pane.x + pane.width;
                for (vlo, vhi) in visual_ranges {
                    let x_lo = pane.x.saturating_add(vlo as u16).min(pane_right);
                    let x_hi = pane.x.saturating_add(vhi as u16).min(pane_right);
                    for x in x_lo..x_hi {
                        if let Some(cell) = buf.cell_mut((x, screen_y)) {
                            crate::scrollback::text_selection::apply_selection_highlight(
                                &theme, cell,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Grapheme width so ligature rows agree with visual_col_to_logical_col.
fn line_display_width_u16(line: &Line<'_>) -> u16 {
    let w: usize = line
        .spans
        .iter()
        .map(|s| crate::scrollback::types::str_display_cells(s.content.as_ref()))
        .sum();
    w.min(u16::MAX as usize) as u16
}
