//! Layout cache and lazy viewport measurement for [`ScrollbackState`].

use super::verb_group::{RunStep, run_step, scan_run_forward};
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SettlementFollowPolicy {
    Evaluate,
    Defer,
}

/// A width-stable anchor for the content at the viewport top, captured before a width rebuild so the same content
/// can be re-pinned afterward. The position is stored as `(entry, logical_line, sub_rows)` rather than an absolute
/// wrapped-row count. It covers vpad and mid-paragraph anchors, but is exact only for a non-wrapping anchor line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollAnchor {
    entry_idx: usize,
    logical_line: usize,
    sub_rows: i64,
}

/// Unlike [`ScrollAnchor`] it is keyed by stable [`EntryId`], because the arming mutation is exactly what shifts
/// indices. Its raw row offset is only meaningful at an unchanged width, so width changes re-anchor via
/// [`ScrollAnchor`]'s logical-line mapping instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StructuralScrollAnchor {
    /// Entry at the viewport top when the arming mutation happened.
    id: EntryId,
    /// Wrapped rows from that entry's top down to the viewport top, measured over the entry's full layout span (content rows plus trailing gap).
    /// This matches `entry_at_virtual_row`'s attribution of gap rows to the entry above, so a top parked on a gap row stays a gap row.
    rows_into_span: usize,
    /// `scroll_offset` at arm time.
    /// A mismatch at consume time means explicit navigation moved the viewport between the mutation and its frame.
    /// The anchor is then stale and must not override that.
    armed_scroll_offset: usize,
}

/// Cached layout data for efficient navigation and rendering.
/// This is rebuilt when entries change or viewport width changes.
/// It provides O(1) lookup for sticky header heights needed by navigation.
#[derive(Debug, Clone, Default)]
pub(super) struct LayoutCache {
    /// Per-entry layout info (height and gap_after).
    pub(super) entries: Vec<EntryLayoutInfo>,
    /// Truncated height of each entry (for sticky header min_height).
    /// Separate from EntryLayoutInfo because it's only needed during PromptDescriptor building, not during rendering.
    pub(super) entry_truncated_heights: Vec<u16>,
    /// Whether each entry's cached `height`/`truncated_height` is an exact measurement (`true`) or a cheap estimate
    /// (`false`). Entries are measured exactly only when they enter (or are near) the viewport.
    pub(super) measured: Vec<bool>,
    /// Virtual Y position of each entry (cumulative heights and gaps).
    pub(super) virtual_y: Vec<usize>,
    /// Prompt descriptors for sticky layout computation.
    pub(super) prompt_descriptors: Vec<PromptDescriptor>,
    /// Group spans computed by the last fold pass: the authoritative model the per-entry flags in `entries` are projected from (see `state::groups`).
    /// Stale between an incremental append and the next structural rebuild, exactly like those flags.
    pub(super) groups: Vec<groups::GroupSpan>,
    /// Width used to compute this cache.
    pub(super) width: u16,
}

impl LayoutCache {
    pub fn take(mut self) -> Self {
        // Used primarily to avoid reallocs when updating
        self.entries.clear();
        self.entry_truncated_heights.clear();
        self.measured.clear();
        self.virtual_y.clear();
        self.prompt_descriptors.clear();
        self.groups.clear();
        self.width = 0;
        self
    }

    /// Binary-search `virtual_y` to find the entry that contains `content_y`. `content_y` is an absolute position in
    /// the virtual content space. `valid_range` restricts the search to a subset of entries.
    fn entry_at_content_y(&self, content_y: usize, valid_range: Range<usize>) -> Option<usize> {
        if valid_range.is_empty() || self.virtual_y.is_empty() {
            return None;
        }

        let slice = self.virtual_y.get(valid_range.clone())?;

        // partition_point returns the first index where virtual_y > content_y, so the entry we want is the one before that
        let pos = slice.partition_point(|&y| y <= content_y);
        if pos == 0 {
            return None;
        }

        let idx = valid_range.start + pos - 1;
        let entry_start = *self.virtual_y.get(idx)?;
        let entry_end = entry_start + self.entries.get(idx)?.height as usize;

        if content_y < entry_end {
            Some(idx)
        } else {
            // In the gap after this entry
            None
        }
    }
}

impl ScrollbackState {
    /// Invalidate and rebuild the layout cache from scratch.
    /// `ensure_layout_cache` skips work when the width and entry count are unchanged.
    /// An in-place display-mode or group change must therefore null the cache first for the next read to see fresh heights, gaps, and totals.
    pub(super) fn rebuild_layout(&mut self) {
        #[cfg(test)]
        {
            self.layout_rebuilds += 1;
        }
        self.gaps_may_be_dirty = true;
        self.layout_cache = None;
        if self.last_width > 0 {
            self.ensure_layout_cache(self.last_width);
            self.compute_total_height_from_cache();
        }
    }

    // These methods provide read-only access to cached layout data.

    /// Get cached height for a single entry.
    /// Returns None if cache is invalid or index out of bounds.
    /// Call prepare_layout() before render to ensure cache is valid.
    pub fn get_cached_entry_height(&self, idx: usize) -> Option<u16> {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx).map(|e| e.height))
    }

    /// Group spans computed by the last fold pass: the model behind every group header and hidden row (see [`groups::GroupSpan`]).
    /// Empty when the layout cache is invalid; call `prepare_layout()` first.
    pub fn group_spans(&self) -> &[groups::GroupSpan] {
        self.layout_cache
            .as_ref()
            .map_or(&[], |c| c.groups.as_slice())
    }

    /// The group span containing the entry at `idx`, if the last fold pass folded it (header, hidden member, or visible tail of a truncation run).
    /// Same freshness contract as [`Self::group_spans`].
    pub fn span_at(&self, idx: usize) -> Option<&groups::GroupSpan> {
        groups::span_containing(self.group_spans(), idx)
    }

    /// Check if an entry is hidden by group truncation (height=0 in cache).
    /// Returns false if the cache is missing or the index is out of bounds
    /// (conservative: treat uncached entries as visible).
    pub(super) fn is_entry_hidden(&self, idx: usize) -> bool {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx))
            .is_some_and(|e| e.height == 0)
    }

    /// If the current selection is on a hidden entry (height=0), move it to the nearest visible entry.
    /// Prefers the group header (the first entry of the truncated run, which has height=1) since it is the row that expands the group.
    pub(super) fn fixup_hidden_selection(&mut self) {
        let Some(sel) = self.selected else { return };
        if !self.is_entry_hidden(sel) {
            return;
        }
        // Walk backward to find the group header (group_header_count > 0)
        for idx in (0..sel).rev() {
            if let Some(ref cache) = self.layout_cache
                && let Some(info) = cache.entries.get(idx)
                && info.height > 0
            {
                self.selected = Some(idx);
                return;
            }
        }
        // Fallback: walk forward
        let n = self.entries.len();
        for idx in (sel + 1)..n {
            if let Some(ref cache) = self.layout_cache
                && let Some(info) = cache.entries.get(idx)
                && info.height > 0
            {
                self.selected = Some(idx);
                return;
            }
        }
    }

    /// Get all cached entry layout info (height and gap_after per entry).
    ///
    /// Returns None if cache is invalid.
    pub fn get_cached_entry_layouts(&self) -> Option<&[EntryLayoutInfo]> {
        self.layout_cache.as_ref().map(|c| c.entries.as_slice())
    }

    /// Get cached virtual Y positions for all entries.
    /// Each entry's virtual Y is its cumulative position in the scrollable content.
    /// Returns None if cache is invalid.
    pub fn get_cached_virtual_y(&self) -> Option<&[usize]> {
        self.layout_cache.as_ref().map(|c| c.virtual_y.as_slice())
    }

    /// Get cached prompt descriptors for sticky header layout.
    ///
    /// Returns None if cache is invalid.
    pub fn get_cached_prompt_descriptors(&self) -> Option<&[PromptDescriptor]> {
        self.layout_cache
            .as_ref()
            .map(|c| c.prompt_descriptors.as_slice())
    }

    /// Get cached truncated height for a single entry.
    /// Truncated height is the height when displayed in Truncated mode,
    /// used for sticky header min_height calculations.
    pub fn get_cached_truncated_height(&self, idx: usize) -> Option<u16> {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entry_truncated_heights.get(idx).copied())
    }

    /// Get entries in range as a Vec.
    /// With IndexMap, we can't return a slice directly, so we collect references.
    pub fn entries_in_range(&self, range: Range<usize>) -> Vec<&ScrollbackEntry> {
        range
            .filter_map(|i| self.entries.get_index(i).map(|(_, v)| v))
            .collect()
    }

    /// Get entries in range (deprecated, use entries_in_range instead).
    /// This allocates a Vec to maintain compatibility.
    #[deprecated(note = "Use entries_in_range() instead")]
    pub fn entries_slice(&self, range: Range<usize>) -> Vec<&ScrollbackEntry> {
        self.entries_in_range(range)
    }

    /// Map a screen row to an entry index. Requires `prepare_layout()` to have been called (layout cache must be
    /// valid).
    pub fn entry_index_at_screen_row(
        &self,
        screen_row: u16,
        scrollback_area: Rect,
    ) -> Option<usize> {
        if screen_row < scrollback_area.y
            || screen_row >= scrollback_area.y + scrollback_area.height
        {
            return None;
        }

        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        if visible_range.is_empty() {
            return None;
        }

        let sticky = self.current_sticky_layout(cache, &visible_range);
        let row_in_area = screen_row - scrollback_area.y;
        let header_rows = sticky.header_screen_rows();

        if row_in_area < header_rows {
            // In the header zone: check if we hit a pushed or pinned prompt
            return sticky.entry_at_header_row(row_in_area);
        }

        // Convert screen row to absolute content-space Y
        let base_y = *cache.virtual_y.get(visible_range.start)?;
        let content_y = base_y + (screen_row - scrollback_area.y) as usize + self.scroll_offset;

        cache.entry_at_content_y(content_y, visible_range)
    }

    /// Compute the screen area for an entry at the given index. Handles both content entries (below the header) and
    /// prompt entries rendered as sticky headers (pushed/pinned). For header prompts, returns the header area. for
    /// content, clips to exclude the header zone. Requires `prepare_layout()` to have been called.
    pub fn entry_screen_area(
        &self,
        entry_idx: usize,
        scrollback_area: Rect,
    ) -> Option<(Rect, bool, bool)> {
        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&entry_idx) {
            return None;
        }

        let layout = HorizontalLayout::new(scrollback_area, &self.appearance.scrollback.layout);
        let sel = layout.selection_area();

        // Check if entry is rendered as a sticky header prompt
        let sticky = self.current_sticky_layout(cache, &visible_range);
        if let Some((header_y, header_h, is_pushed)) = sticky.header_entry_area(entry_idx) {
            // Pushed prompts have their top clipped (disappearing upward)
            let top_clipped = is_pushed && sticky.pushed.is_some_and(|p| p.clip_top > 0);
            return Some((
                Rect {
                    x: sel.x,
                    y: scrollback_area.y + header_y,
                    width: sel.width,
                    height: header_h,
                },
                top_clipped,
                false, // header prompts are never bottom-clipped
            ));
        }

        // Content entry: compute from virtual_y coordinates
        // Keep the cumulative positions in usize (tall sessions exceed u16::MAX)
        // The final screen y / height are viewport-relative and provably fit in u16
        let base_y = *cache.virtual_y.get(visible_range.start)?;
        let entry_start = *cache.virtual_y.get(entry_idx)? - base_y;
        let entry_height = cache.entries.get(entry_idx)?.height;
        let entry_end = entry_start + entry_height as usize;

        // Check if entry is within viewport
        let vp_start = self.scroll_offset;
        let vp_end = self.scroll_offset + self.viewport_height as usize;
        if entry_end <= vp_start || entry_start >= vp_end {
            return None;
        }

        let top_clipped = entry_start < vp_start;
        let bottom_clipped = entry_end > vp_end;

        // Screen coordinates: viewport-relative deltas always fit in u16.
        let mut screen_y = if top_clipped {
            scrollback_area.y
        } else {
            scrollback_area.y + (entry_start - vp_start) as u16
        };

        let mut visible_height = if top_clipped && bottom_clipped {
            self.viewport_height
        } else if top_clipped {
            (entry_end - vp_start) as u16
        } else if bottom_clipped {
            (vp_end - entry_start) as u16
        } else {
            entry_height
        };

        // Clip to below the sticky header
        let header_rows = sticky.header_screen_rows();
        let content_top = scrollback_area.y + header_rows;
        if screen_y + visible_height <= content_top {
            return None; // Entirely behind header
        }
        let mut top_clipped = top_clipped;
        if screen_y < content_top {
            let clip = content_top - screen_y;
            visible_height = visible_height.saturating_sub(clip);
            screen_y = content_top;
            top_clipped = true;
        }
        if visible_height == 0 {
            return None;
        }

        Some((
            Rect {
                x: sel.x,
                y: screen_y,
                width: sel.width,
                height: visible_height,
            },
            top_clipped,
            bottom_clipped,
        ))
    }

    // Lazy viewport height measurement (see `rebuild_layout_cache` for the estimate side; these upgrade on/near-screen entries to exact heights)

    /// Virtual-space `(top, bottom)` of the viewport (relative to entry 0).
    /// `top` is the current scroll position, `bottom` is one past the last visible row.
    /// `None` when the cache is absent, the visible range is empty, or the cache is stale (range start past `virtual_y`).
    pub(super) fn viewport_virtual_bounds(&self) -> Option<(usize, usize)> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if range.is_empty() {
            return None;
        }
        let base_y = cache.virtual_y.get(range.start).copied()?;
        let top = base_y + self.scroll_offset;
        let bottom = top + self.viewport_height as usize;
        Some((top, bottom))
    }

    /// Maximum valid `scroll_offset`: content height that doesn't fit the viewport.
    /// `scroll_offset` is always clamped to `[0, max_scroll_offset()]`.
    pub(super) fn max_scroll_offset(&self) -> usize {
        self.total_height
            .saturating_sub(self.viewport_height as usize)
    }

    /// Capture a width-stable [`ScrollAnchor`] for the content at the viewport top, from the current layout cache. The
    /// same content can then be re-pinned to the viewport top via [`restore_scroll_anchor`]. That conversion survives
    /// the entry's own re-wrapping (the whole transcript can be one giant entry).
    pub(super) fn capture_scroll_anchor(&self) -> Option<ScrollAnchor> {
        // `entry_at_virtual_row` resolves the viewport-top entry deterministically, including a gap row, which it attributes to the entry above
        // (`entry_at_content_y` would return None in a gap; this path has no such special case.)
        let (top_content_y, _) = self.viewport_virtual_bounds()?;
        let entry_idx = self.entry_at_virtual_row(top_content_y)?;
        let cache = self.layout_cache.as_ref()?;
        let entry_y = *cache.virtual_y.get(entry_idx)?;
        let rows_into_entry = top_content_y.saturating_sub(entry_y);

        // Convert the display-row offset within the entry to a logical line and signed sub-row offset, both resolved at the cache's (old) width
        let area_width = self.entry_area_width(cache.width);
        let (_, entry) = self.entries.get_index(entry_idx)?;
        let theme = Theme::current();
        let renderer = EntryRenderer::new(entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(self.cwd());
        let rows = u16::try_from(rows_into_entry).unwrap_or(u16::MAX);
        let logical_line = renderer.logical_line_of_rendered_row(area_width, rows);
        let line_start = renderer.rendered_row_of_logical_line(area_width, logical_line);
        let sub_rows = rows as i64 - line_start as i64;
        Some(ScrollAnchor {
            entry_idx,
            logical_line,
            sub_rows,
        })
    }

    /// Re-derive `scroll_offset` so the content captured by [`capture_scroll_anchor`] sits at the viewport top again.
    /// Call after the layout cache and `total_height` are rebuilt at the new width, before `settle` re-pins.
    pub(super) fn restore_scroll_anchor(&mut self, anchor: ScrollAnchor) {
        let ScrollAnchor {
            entry_idx,
            logical_line,
            sub_rows,
        } = anchor;
        // Re-resolve the logical line's start row at the new width; this is what makes a re-wrapping top entry re-anchor correctly
        // The wrapped rows above the anchor line grow/shrink, and the rebuilt start rows account for that
        // `sub_rows` is width-stable for a non-wrapping line
        let Some(width) = self.layout_cache.as_ref().map(|c| c.width) else {
            return;
        };
        let area_width = self.entry_area_width(width);
        let (new_line_start, line_last_row) = {
            let Some((_, entry)) = self.entries.get_index(entry_idx) else {
                return;
            };
            let theme = Theme::current();
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(self.cwd());
            let (starts, last_content_row) = renderer.logical_line_start_rows(area_width);
            let new_line_start = starts
                .get(logical_line)
                .copied()
                .unwrap_or(last_content_row);
            // Last rendered row of the anchor line: one before the next line's start, or the entry's last row when the anchor is the final line
            let line_last_row = starts
                .get(logical_line + 1)
                .map(|&next| next.saturating_sub(1))
                .unwrap_or(last_content_row);
            (new_line_start, line_last_row.max(new_line_start))
        };
        // Clamp the intra-line offset to the anchor line's wrapped extent at the new width
        // A re-wrapped (now shorter) anchor line then can't push the viewport top past itself into the next logical line
        let new_rows_into_entry =
            (new_line_start as i64 + sub_rows).clamp(0, line_last_row as i64) as usize;

        let Some(cache) = self.layout_cache.as_ref() else {
            return;
        };
        let range = self.visible_entry_range();
        let (Some(&base_y), Some(&entry_y)) = (
            cache.virtual_y.get(range.start),
            cache.virtual_y.get(entry_idx),
        ) else {
            return;
        };
        let new_top_content_y = entry_y + new_rows_into_entry;
        self.scroll_offset = new_top_content_y
            .saturating_sub(base_y)
            .min(self.max_scroll_offset());
    }

    /// Arm a one-shot [`StructuralScrollAnchor`] for the current viewport top. Call before mutating the entries map.
    /// The first mutation before a frame wins: it saw the on-screen geometry. later mutations have no cache to anchor
    /// against. Removals keep the armed anchor honest via [`Self::migrate_structural_anchor_past_removal`].
    pub(super) fn arm_structural_scroll_anchor(&mut self) {
        if self.structural_scroll_anchor.is_some() {
            return;
        }
        let Some((entry_idx, rows_into_span)) = self.viewport_top_anchor_point() else {
            return;
        };
        let Some((id, _)) = self.entries.get_index(entry_idx) else {
            return;
        };
        self.structural_scroll_anchor = Some(StructuralScrollAnchor {
            id: *id,
            rows_into_span,
            armed_scroll_offset: self.scroll_offset,
        });
    }

    /// Keep an armed anchor meaningful across the removal that follows it (and any further removals before the next
    /// frame). The survivor is pinned to the vacated viewport-top row. With nothing surviving below the removal, the
    /// anchor is dropped and the plain max-offset clamp takes over.
    pub(super) fn migrate_structural_anchor_past_removal(
        &mut self,
        removed_id: EntryId,
        removed_index: usize,
    ) {
        let Some(anchor) = self.structural_scroll_anchor else {
            return;
        };
        if anchor.id != removed_id {
            return;
        }
        let survivor = self.entries.get_index(removed_index).map(|(id, _)| *id);
        self.structural_scroll_anchor = survivor.map(|id| StructuralScrollAnchor {
            id,
            rows_into_span: 0,
            armed_scroll_offset: anchor.armed_scroll_offset,
        });
    }

    /// Drop an armed anchor whose entry no longer exists.
    /// For bulk tail removal (`remove_from`), where everything at and after the anchor can vanish at once with no later survivor to migrate to.
    pub(super) fn prune_dead_structural_anchor(&mut self) {
        if let Some(anchor) = self.structural_scroll_anchor
            && self.index_of_id(anchor.id).is_none()
        {
            self.structural_scroll_anchor = None;
        }
    }

    /// Apply a taken [`StructuralScrollAnchor`] after the same-width full rebuild that the arming mutation forced.
    /// This re-pins the pre-mutation viewport-top content.
    /// Skipped when follow took over or when explicit navigation moved the viewport since arming (`armed_scroll_offset` mismatch; user intent wins).
    pub(super) fn apply_structural_scroll_anchor(
        &mut self,
        anchor: Option<StructuralScrollAnchor>,
        width: u16,
    ) {
        let Some(anchor) = anchor else {
            return;
        };
        if self.follow_mode || self.scroll_offset != anchor.armed_scroll_offset {
            return;
        }
        let Some(entry_idx) = self.index_of_id(anchor.id) else {
            return;
        };
        // The rebuild reset every height to a cheap estimate, while `rows_into_span` was measured against the entry's
        // exact pre-mutation layout. Otherwise the span clamp in the re-pin would squeeze an exact row against a transient
        // under-estimate. The viewport would jump within an entry whose content never changed.
        self.measure_span_and_rebuild(entry_idx, entry_idx, width);
        self.repin_viewport_top_to_entry(entry_idx, anchor.rows_into_span);
    }

    /// Identity of the viewport-top row as `(entry_idx, rows_into_span)`. The entry owns the top row (gap rows
    /// attribute to the entry above, per `entry_at_virtual_row`). A top parked on a gap row thus round-trips as that
    /// same gap row.
    pub(super) fn viewport_top_anchor_point(&self) -> Option<(usize, usize)> {
        if self.follow_mode || self.scroll_offset == 0 {
            return None;
        }
        let (top, _) = self.viewport_virtual_bounds()?;
        let entry_idx = self.entry_at_virtual_row(top)?;
        let entry_y = *self.layout_cache.as_ref()?.virtual_y.get(entry_idx)?;
        Some((entry_idx, top.saturating_sub(entry_y)))
    }

    /// Re-derive `scroll_offset` so the row `rows_into_span` below entry `entry_idx`'s top sits at the viewport top
    /// again. Below-viewport mutations therefore never move the viewport. A gap-row park is never squeezed onto a
    /// content row, and a genuinely shrunken anchor entry cannot spill the top into unrelated content.
    pub(super) fn repin_viewport_top_to_entry(&mut self, entry_idx: usize, rows_into_span: usize) {
        let Some(cache) = self.layout_cache.as_ref() else {
            return;
        };
        let range = self.visible_entry_range();
        if !range.contains(&entry_idx) {
            return;
        }
        let (Some(&base_y), Some(&entry_y)) = (
            cache.virtual_y.get(range.start),
            cache.virtual_y.get(entry_idx),
        ) else {
            return;
        };
        let span_rows = cache
            .entries
            .get(entry_idx)
            .map_or(0, |e| e.height as usize + e.gap_after as usize);
        let rows_into_span = rows_into_span.min(span_rows.saturating_sub(1));
        self.scroll_offset = (entry_y + rows_into_span)
            .saturating_sub(base_y)
            .min(self.max_scroll_offset());
    }

    /// Index range `[start, end]` of entries to measure exactly for the current viewport: every on-screen entry plus a
    /// small below-margin. There is deliberately no above-margin: entries above the first visible one stay estimated.
    fn measurement_window(&self) -> Option<(usize, usize)> {
        // `start` is the first visible entry (shared, canonical predicate).
        let start = self.first_visible_entry()?;
        let (_, bottom) = self.viewport_virtual_bounds()?;
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        let vy = cache.virtual_y.get(range.clone())?;

        // Last entry whose start is before the viewport bottom, plus a small below-margin (no above-margin; see the doc comment above)
        let last_rel = vy.partition_point(|&y| y < bottom).saturating_sub(1);
        let last_visible = (range.start + last_rel).max(start);
        let end = (last_visible + MEASURE_MARGIN_ENTRIES).min(range.end - 1);
        Some((start, end))
    }

    /// Whether the entry at `idx` falls inside the current viewport window (visible rows plus the small below-margin from `measurement_window`).
    /// Conservative: with no layout yet (before the first draw / after an invalidation), every entry counts as visible.
    /// Animation gating then never starves a redraw it can't reason about.
    pub(super) fn entry_index_in_viewport(&self, idx: usize) -> bool {
        // A wedged offset (viewport top at/past the content end, e.g. after a shrink under a follow pin) yields a degenerate window with no entry.
        // Treat it like an absent layout so animation gating can't mute the redraws that heal the state
        // (A legit page-flip pin keeps `scroll_offset < total_height` via the re-clamp in `follow_scroll_to_bottom`, so it never hits this arm.)
        if self.scroll_offset >= self.total_height {
            return true;
        }
        match self.measurement_window() {
            Some((start, end)) => idx >= start && idx <= end,
            None => true,
        }
    }

    /// Evict heavyweight render caches from entries far outside the viewport. For a multi-MB transcript those copies
    /// are easily hundreds of MB that can never be seen without scrolling. Heights are cached separately
    /// (`cached_truncated_height` / `cached_estimate_lines` / the layout cache) and are deliberately kept.
    pub(crate) fn evict_offscreen_render_caches(&self) -> usize {
        let Some((win_start, win_end)) = self.measurement_window() else {
            // No layout (nothing rendered yet): nothing worth sweeping
            return 0;
        };
        let keep_start = win_start.saturating_sub(EVICT_KEEP_MARGIN_ENTRIES);
        let keep_end = win_end.saturating_add(EVICT_KEEP_MARGIN_ENTRIES);
        let mut evicted = 0usize;
        for (idx, (_, entry)) in self.entries.iter().enumerate() {
            if idx >= keep_start && idx <= keep_end {
                continue;
            }
            if self.selected == Some(idx) {
                continue;
            }
            if entry.evict_render_cache() {
                evicted += 1;
            }
        }
        evicted
    }

    /// Full entry render-area width (accent, padding, content) for a viewport of `width`.
    /// This is the width handed to `EntryRenderer`, which subtracts chrome itself to reach the content width.
    /// Centralizes the layout round-trip so the reveal row mapping, exact height measurement, and prompt-descriptor layout can't drift apart.
    pub(super) fn entry_area_width(&self, width: u16) -> u16 {
        let simulated_area = Rect::new(0, 0, width, 1);
        HorizontalLayout::new(simulated_area, &self.appearance.scrollback.layout)
            .entry_content_area()
            .width
    }

    /// Content-column width (excluding accent bar and block padding) for a full scrollback width.
    /// Used to size the inline edit textarea.
    pub fn entry_text_column_width(&self, width: u16) -> u16 {
        let simulated_area = Rect::new(0, 0, width, 1);
        HorizontalLayout::new(simulated_area, &self.appearance.scrollback.layout).content_width()
    }

    /// Measure exact heights for not-yet-measured entries in `[start, end]`. Returns `(entry_index, height_delta)` for
    /// each estimate replaced by an exact height. Hidden (group-truncated, height 0) and synthetic group-header rows
    /// render no markdown, so they are skipped. Their height is owned by group truncation, not measurement.
    fn measure_window_exact(&mut self, width: u16, start: usize, end: usize) -> Vec<(usize, i32)> {
        // Pre-scan: bail before building a Theme and layout when every in-window entry is already measured or non-rendered (hidden / group-header)
        {
            let Some(cache) = self.layout_cache.as_ref() else {
                return Vec::new();
            };
            let needs_measure = (start..=end).any(|idx| {
                cache.entries.get(idx).is_some_and(|info| {
                    cache.measured.get(idx).is_some_and(|m| !m)
                        && info.height != 0
                        && (!info.is_group_header() || info.is_expanded_verb_header())
                })
            });
            if !needs_measure {
                return Vec::new();
            }
        }

        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let inline_edit_height = self.inline_edit_height;

        let Some(cache) = self.layout_cache.as_mut() else {
            return Vec::new();
        };

        let mut changes = Vec::new();
        for idx in start..=end {
            if idx >= cache.entries.len() {
                break;
            }
            if cache.measured.get(idx) != Some(&false) {
                continue;
            }
            let Some(info) = cache.entries.get(idx).copied() else {
                continue;
            };
            // Estimated entries always have height >= 1, so a height of 0 here means group truncation hid this entry
            // Synthetic-only headers need no block render; an expanded verb header also owns member 0 rows
            if info.height == 0 || (info.is_group_header() && !info.is_expanded_verb_header()) {
                continue;
            }
            let Some((entry_id, entry)) = self.entries.get_index(idx) else {
                continue;
            };
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(cwd);
            let member_height = match inline_edit_height {
                Some((edit_id, h)) if edit_id == *entry_id => h,
                _ => renderer.desired_height(entry_area_width),
            };
            let exact_height = info.with_verb_header_row(member_height);
            let delta = exact_height as i32 - info.height as i32;
            if let Some(slot) = cache.entries.get_mut(idx) {
                slot.height = exact_height;
            }
            changes.push((idx, delta));
            // Truncated height only feeds prompt sticky-header min_height, so only prompts pay for the extra Truncated-mode render
            // Others keep their seeded value (unused for non-prompts)
            if entry.block.is_user_prompt()
                && let Some(slot) = cache.entry_truncated_heights.get_mut(idx)
            {
                *slot = renderer.compute_truncated_height(entry_area_width);
            }
            if let Some(slot) = cache.measured.get_mut(idx) {
                *slot = true;
            }
        }
        changes
    }

    /// Upgrade the on-screen entries from estimated to exact heights and re-anchor the viewport so what the user is looking at stays put.
    /// Iterates because an exact height shifts later entries, which can reveal a new entry at the bottom edge.
    /// `measured` grows monotonically so it terminates; the loop bound is a defensive cap.
    pub(super) fn settle_visible_measurements(
        &mut self,
        width: u16,
        follow_policy: SettlementFollowPolicy,
    ) {
        if self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        let max_iters = self.entries.len().saturating_add(2);
        for _ in 0..max_iters {
            let Some((start, end)) = self.measurement_window() else {
                return;
            };
            let changes = self.measure_window_exact(width, start, end);
            if changes.is_empty() {
                // Everything visible is exact: render will match the layout.
                return;
            }
            // Estimates became exact: rebuild offsets (cheap arithmetic, no markdown) and re-pin the viewport
            self.shift_pin_reserve_target_for_changes(&changes);
            self.rebuild_virtual_y_from_heights();
            self.compute_total_height_from_cache();
            if self.follow_mode && follow_policy == SettlementFollowPolicy::Evaluate {
                // Bottom-anchored: re-pin to the (now exact) bottom.
                self.follow_scroll_to_bottom();
            } else if self.follow_mode {
                // Full rebuilds defer release decisions until their authoritative target is restored/reset,
                // but settlement still needs re-anchoring so its next measurement window converges.
                if self.follow_preserve_scroll && self.pin_reserve_active {
                    if let Some(target) = self.pin_reserve_prompt_scroll_target() {
                        self.scroll_offset = target;
                    }
                } else if !self.follow_preserve_scroll {
                    self.scroll_offset = self.max_scroll_offset();
                }
            } else {
                // Top-anchored: the first visible entry's offset is unchanged (nothing above it was measured), so scroll stays put
                // Only clamp if the content shrank past the end
                let max_offset = self
                    .total_height
                    .saturating_sub(self.viewport_height as usize);
                if self.scroll_offset > max_offset {
                    self.scroll_offset = max_offset;
                }
            }
        }
    }

    /// An immediate scroll-up then reveals already-exact heights instead of triggering an estimate-to-exact rebuild
    /// (which could jump). There `follow_scroll_to_bottom` keeps the prompt put, so the shift would move it down: a
    /// jump.
    pub(super) fn warm_measure_pages_above(&mut self, width: u16) {
        if !self.follow_mode
            || self.follow_preserve_scroll
            || self.viewport_height == 0
            || width == 0
        {
            return;
        }
        let Some((top, _)) = self.viewport_virtual_bounds() else {
            return;
        };
        let Some(first_visible) = self.entry_at_virtual_row(top) else {
            return;
        };
        let warm_top =
            top.saturating_sub(RESUME_WARM_PAGES as usize * self.viewport_height as usize);
        let Some(start) = self.entry_at_virtual_row(warm_top) else {
            return;
        };
        self.measure_span_and_rebuild(start, first_visible, width);
        self.follow_scroll_to_bottom();
    }

    /// Index of the entry whose span contains virtual row `row` (the last entry starting at or before it).
    /// `None` if the cache/viewport is empty.
    pub(super) fn entry_at_virtual_row(&self, row: usize) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if range.is_empty() {
            return None;
        }
        let vy = cache.virtual_y.get(range.clone())?;
        if vy.is_empty() {
            return None;
        }
        let rel = vy.partition_point(|&y| y <= row).saturating_sub(1);
        let idx = range.start.saturating_add(rel);
        // Guard stale cache vs range drift; never return an OOB index
        if idx >= range.end || idx >= cache.virtual_y.len() || idx >= cache.entries.len() {
            return None;
        }
        Some(idx)
    }

    /// Index of the entry at the top of the current viewport (the one whose span contains the scroll top).
    /// `None` if the cache/viewport is empty.
    fn first_visible_entry(&self) -> Option<usize> {
        if self.viewport_height == 0 {
            return None;
        }
        let (top, _) = self.viewport_virtual_bounds()?;
        self.entry_at_virtual_row(top)
    }

    /// Measure exact heights for entries in `[start, end]` (clamped to the visible range) and rebuild cached offsets if anything was newly measured.
    pub(super) fn measure_span_and_rebuild(&mut self, start: usize, end: usize, width: u16) {
        if self.viewport_height == 0 || self.layout_cache.is_none() {
            return;
        }
        let range = self.visible_entry_range();
        if range.is_empty() {
            return;
        }
        let start = start.max(range.start);
        let end = end.min(range.end - 1);
        if start > end {
            return;
        }
        let changes = self.measure_window_exact(width, start, end);
        if !changes.is_empty() {
            self.shift_pin_reserve_target_for_changes(&changes);
            self.rebuild_virtual_y_from_heights();
            self.compute_total_height_from_cache();
        }
    }

    /// Measuring above the viewport therefore doesn't desync them. `ensure_selected_visible` calls this only for an
    /// off-viewport selection (an on-viewport selection measures nothing). The viewport would jump.
    pub(super) fn measure_around_entry(&mut self, entry_idx: usize, width: u16) {
        if !self.visible_entry_range().contains(&entry_idx) {
            return;
        }
        let span = self.viewport_height as usize;
        self.measure_span_and_rebuild(
            entry_idx.saturating_sub(span),
            entry_idx.saturating_add(span),
            width,
        );
    }

    /// Measure everything a scroll-to-target computation reads.
    /// That is the window around the target, plus (in SingleTurn mode) the turn's sticky prompt at the visible range start.
    /// The sticky prompt drives the sticky-header height in the scroll math but can sit far above the target window.
    pub(super) fn measure_scroll_target(&mut self, target: usize, width: u16) {
        self.measure_around_entry(target, width);
        if self.view_mode == ViewMode::SingleTurn {
            let start = self.visible_entry_range().start;
            self.measure_span_and_rebuild(start, start, width);
        }
    }

    // Turn Pinning

    /// Check if the current turn's prompt should be pinned as a sticky header.
    pub fn should_pin_prompt(&self) -> bool {
        self.pinned_prompt_index().is_some()
    }

    /// Get the index of the prompt that should be pinned (if any).
    /// Returns the prompt entry index if it should be pinned.
    /// Pins when scroll_offset > 0 to keep the turn's prompt visible.
    pub fn pinned_prompt_index(&self) -> Option<usize> {
        // Only pin in SingleTurn mode or when we have a current turn
        let turn_idx = self.current_turn?;
        let turn = self.turns.get(turn_idx)?;
        let prompt_idx = turn.prompt_index;

        // Check if prompt is in the visible range
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&prompt_idx) {
            return None;
        }

        // Pin the prompt when we've scrolled at all
        // This keeps the turn's prompt visible while scrolling through content
        if prompt_idx == visible_range.start && self.scroll_offset > 0 {
            Some(prompt_idx)
        } else {
            None
        }
    }

    /// Get scroll info for scrollbar rendering. Returns `(scroll_offset, viewport_height, total_height)`. The two
    /// cumulative quantities are `usize` (tall sessions exceed `u16::MAX`);. `viewport_height` stays `u16`.
    pub fn scroll_info(&self) -> (usize, u16, usize) {
        (self.scroll_offset, self.viewport_height, self.total_height)
    }

    // Layout Cache (for navigation with sticky headers)

    /// Invalidate the layout cache (call when entries change).
    pub(super) fn invalidate_layout_cache(&mut self) {
        self.layout_cache = None;
        // Mark all heights as dirty
        self.dirty_heights = self.entries.keys().copied().collect();
        self.gaps_may_be_dirty = true;
    }

    /// Ensure layout cache is valid for the given width.
    /// Rebuilds the cache if needed.
    pub(super) fn ensure_layout_cache(&mut self, width: u16) {
        // Check if cache is valid
        if let Some(ref cache) = self.layout_cache
            && cache.width == width
            && cache.entries.len() == self.entries.len()
        {
            return; // Cache is valid
        }

        self.rebuild_layout_cache(width);
    }

    /// Only sums heights for entries in `visible_entry_range()`. In SingleTurn mode only the current turn's entries are
    /// counted, so scroll_down cannot scroll past the end of the visible content. `total_height`/`scroll_offset` are
    /// `usize`, matching `virtual_y` (`Vec<usize>`), so the summed rows are never truncated.
    pub(super) fn compute_total_height_from_cache(&mut self) {
        let Some(cache) = self.layout_cache.as_ref() else {
            return;
        };
        let range = self.visible_entry_range();
        // Sum entry heights and gap_after in the visible range
        // Per-entry heights are u16; accumulate into usize so a long session (many entries / tall content) is not truncated
        // The last entry's gap_after (always 1) is the trailing gap for the selection box, so the sum is correct as-is
        let Some(entries) = cache.entries.get(range) else {
            return;
        };
        let total: usize = entries
            .iter()
            .map(|e| e.height as usize + e.gap_after as usize)
            .sum();
        self.pin_reserve_pad = self.pin_reserve_pad_rows(total);
        self.total_height = total.saturating_add(self.pin_reserve_pad);
    }

    /// Update heights for dirty entries only.
    pub(super) fn update_dirty_entry_heights(&mut self, width: u16) -> Vec<(usize, i32)> {
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let inline_edit_height = self.inline_edit_height;
        let Some(cache) = self.layout_cache.as_mut() else {
            return Vec::new();
        };

        let theme = Theme::current();

        let mut changes = Vec::new();

        // Collect (id, idx) pairs first to avoid borrow issues
        let dirty_entries: Vec<(EntryId, usize)> = self
            .dirty_heights
            .iter()
            .filter_map(|&id| self.entries.get_index_of(&id).map(|idx| (id, idx)))
            .collect();

        for (id, idx) in dirty_entries {
            if idx >= cache.entries.len() {
                continue; // Entry added after cache was built
            }

            let Some((_, entry)) = self.entries.get_index(idx) else {
                continue;
            };
            let Some(info) = cache.entries.get(idx).copied() else {
                continue;
            };
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(cwd);
            let member_height = match inline_edit_height {
                Some((edit_id, h)) if edit_id == id => h,
                _ => renderer.desired_height(entry_area_width),
            };
            let new_height = info.with_verb_header_row(member_height);
            let old_height = info.height;
            // This entry now has an exact (re)measured height, so it no longer needs the lazy viewport measurement pass
            if let Some(slot) = cache.measured.get_mut(idx) {
                *slot = true;
            }

            // A measured prompt's exact truncated height feeds sticky min_height. Refresh it unconditionally, matching the
            // sibling measure paths. The height can be unchanged while the seed is still the conservative MAX. Cheap: prompts
            // are rarely re-dirtied.
            if entry.block.is_user_prompt()
                && let Some(slot) = cache.entry_truncated_heights.get_mut(idx)
            {
                *slot = renderer.compute_truncated_height(entry_area_width);
            }

            if new_height != old_height {
                if let Some(slot) = cache.entries.get_mut(idx) {
                    slot.height = new_height;
                }
                changes.push((idx, new_height as i32 - old_height as i32));
            }
        }

        changes
    }

    /// Rebuild virtual_y positions and gap_after values from cached entry layout info.
    /// Called after dirty height updates or lazy viewport measurement.
    /// Recomputes gap_after (because display_mode changes affect the pairwise gap rule) and then rebuilds virtual_y.
    pub(super) fn rebuild_virtual_y_from_heights(&mut self) {
        let Some(cache) = self.layout_cache.as_mut() else {
            return;
        };

        // Recompute gap_after; display_mode may have changed
        Self::recompute_gap_after(&self.entries, &mut cache.entries);

        // Re-apply verb-group folding and group truncation after gap recomputation
        let max_visible = self.appearance.scrollback.display.group_max_visible as usize;
        cache.groups = groups::apply(
            &self.entries,
            &mut cache.entries,
            max_visible,
            &self.expanded_groups,
        );

        cache.virtual_y.clear();
        cache.prompt_descriptors.clear();

        let mut y = 0usize;

        for (idx, layout) in cache.entries.iter().enumerate() {
            cache.virtual_y.push(y);

            if let Some((_, entry)) = self.entries.get_index(idx)
                && entry.block.is_user_prompt()
            {
                let truncated_height = cache
                    .entry_truncated_heights
                    .get(idx)
                    .copied()
                    .unwrap_or(MAX_TRUNCATED_HEADER_HEIGHT);
                let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
                // Expanded foldable prompts participate in push calculations but don't stick themselves; they scroll away normally
                let sticky =
                    !(entry.block.is_foldable() && entry.display_mode == DisplayMode::Expanded);
                cache.prompt_descriptors.push(PromptDescriptor {
                    entry_idx: idx,
                    y_virtual: y,
                    full_height: layout.height,
                    min_height,
                    sticky,
                });
            }

            y += layout.height as usize + layout.gap_after as usize;
        }
    }

    /// Incrementally patch virtual_y positions after height-only changes. This is the fast path for streaming, for when
    /// only entry heights changed (no display_mode or structural changes).
    pub(super) fn patch_virtual_y_for_dirty(&mut self, changes: &[(usize, i32)]) -> i32 {
        if changes.is_empty() {
            return 0;
        }

        let Some(cache) = self.layout_cache.as_mut() else {
            return 0;
        };

        // Find the earliest changed index
        // All virtual_y entries after it need shifting by the cumulative delta up to that point
        // For the common streaming case (one entry at the end), this loop touches zero virtual_y entries
        let earliest_idx = changes.iter().map(|&(idx, _)| idx).min().unwrap_or(0);

        // Build a cumulative delta: for each position from earliest_idx onward, the delta is the sum of all changes at or before that position
        // Sort changes by index to apply them in order.
        let mut sorted_changes = changes.to_vec();
        sorted_changes.sort_unstable_by_key(|&(idx, _)| idx);

        let total_delta: i32 = sorted_changes.iter().map(|&(_, d)| d).sum();

        // Apply deltas to virtual_y
        // Walk from earliest_idx+1 to the end, accumulating the delta as we pass each change point
        let mut change_iter = sorted_changes.iter().peekable();
        let mut cumulative_delta: i64 = 0;

        // Skip changes before earliest_idx (shouldn't happen, but defensive)
        while change_iter
            .peek()
            .is_some_and(|&&(idx, _)| idx < earliest_idx)
        {
            let &(_, d) = change_iter.next().unwrap();
            cumulative_delta += d as i64;
        }

        // Apply the delta at earliest_idx itself (affects entries after it)
        if change_iter
            .peek()
            .is_some_and(|&&(idx, _)| idx == earliest_idx)
        {
            let &(_, d) = change_iter.next().unwrap();
            cumulative_delta += d as i64;
        }

        // Now shift virtual_y[earliest_idx+1..] and update prompt_descriptors
        for idx in (earliest_idx + 1)..cache.virtual_y.len() {
            // Check if this index has its own height change
            if change_iter.peek().is_some_and(|&&(cidx, _)| cidx == idx) {
                let &(_, d) = change_iter.next().unwrap();
                // Apply delta from earlier changes first, then add this one
                if let Some(slot) = cache.virtual_y.get_mut(idx) {
                    *slot = (*slot as i64 + cumulative_delta) as usize;
                }
                cumulative_delta += d as i64;
            } else if let Some(slot) = cache.virtual_y.get_mut(idx) {
                *slot = (*slot as i64 + cumulative_delta) as usize;
            }
        }

        // Update prompt_descriptors y_virtual values for affected prompts
        for pd in cache.prompt_descriptors.iter_mut() {
            if pd.entry_idx > earliest_idx {
                pd.y_virtual = (pd.y_virtual as i64 + total_delta as i64) as usize;
            } else if pd.entry_idx == earliest_idx {
                // The prompt itself didn't move, but its full_height may have changed
                // Update from the cache, which update_dirty_entry_heights already patched
                if let Some(info) = cache.entries.get(pd.entry_idx) {
                    pd.full_height = info.height;
                }
            }
        }

        // Also update full_height for any prompts at dirty indices
        for &(idx, _) in changes {
            for pd in cache.prompt_descriptors.iter_mut() {
                if pd.entry_idx == idx
                    && let Some(info) = cache.entries.get(idx)
                {
                    pd.full_height = info.height;
                }
            }
        }

        total_delta
    }

    /// Returns false if the cache is missing or out of sync so the caller falls back to a full rebuild.
    /// Avoids the O(N) rebuild that `invalidate_layout_cache` would force on the next `prepare_layout` — the dominant cost during subagent streaming.
    /// `total_height` is left stale on purpose; the next prepare recomputes it. Only the new entry's `virtual_y` depends on the previous `gap_after`.
    pub(super) fn extend_layout_cache_with_new_entry(&mut self, new_idx: usize) -> bool {
        // Read the cache's own width before the mutable borrow below, so the shared entry_area_width helper (which borrows &self) doesn't clash
        let Some(width) = self.layout_cache.as_ref().map(|c| c.width) else {
            return false;
        };
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let Some(cache) = self.layout_cache.as_mut() else {
            return false;
        };

        // Defensive: cache should be in sync with entries up to but not including new_idx.
        if cache.entries.len() != new_idx
            || cache.virtual_y.len() != new_idx
            || cache.entry_truncated_heights.len() != new_idx
            || cache.measured.len() != new_idx
            || new_idx >= self.entries.len()
        {
            // Cache is out of sync (concurrent state mutation, bug, or batch).
            // Bail out and let the caller invalidate.
            return false;
        }

        let theme = Theme::current();

        // Borrow the new entry to compute its layout info.
        let Some((_, new_entry)) = self.entries.get_index(new_idx) else {
            return false;
        };

        let renderer = EntryRenderer::new(new_entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(cwd);
        let height = renderer.desired_height(entry_area_width);
        let is_prompt = new_entry.block.is_user_prompt();
        // Truncated height only feeds prompt sticky-header min_height; only prompts pay for the extra Truncated-mode render (others seed the MAX)
        let truncated_height = if is_prompt {
            renderer.compute_truncated_height(entry_area_width)
        } else {
            MAX_TRUNCATED_HEADER_HEIGHT
        };
        let is_foldable = new_entry.block.is_foldable();
        let new_display_mode = new_entry.display_mode;

        // Recompute the previous entry's gap_after now that it's no longer the trailing entry
        // The defensive check above guarantees `new_idx < self.entries.len()`, so when `new_idx > 0` the previous entry is in range
        // We still use `if let Some(...)` to keep the access panic-free
        if new_idx > 0
            && let Some((_, prev_entry)) = self.entries.get_index(new_idx - 1)
            && let Some(prev) = new_idx.checked_sub(1)
            && let Some(slot) = cache.entries.get_mut(prev)
        {
            slot.gap_after = gap_after_between(prev_entry, new_entry);
        }

        // Compute the new entry's virtual_y (start position) using the previous entry's (now-correct) gap_after
        let new_y = if let Some(prev) = new_idx.checked_sub(1) {
            let Some(&vy) = cache.virtual_y.get(prev) else {
                return false;
            };
            let Some(info) = cache.entries.get(prev) else {
                return false;
            };
            vy + info.height as usize + info.gap_after as usize
        } else {
            0
        };

        // Append the new entry. It's now the trailing entry, so gap_after = 1.
        cache.entries.push(EntryLayoutInfo {
            height,
            gap_after: 1,
            group_header_count: 0,
            group_collapse_header: false,
            verb_group_header: false,
        });
        cache.entry_truncated_heights.push(truncated_height);
        // New entries append at the bottom (visible/streaming) and are measured exactly above via `desired_height`, so mark them measured
        cache.measured.push(true);
        cache.virtual_y.push(new_y);

        if is_prompt {
            let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
            // Expanded foldable prompts participate in push calculations but don't stick themselves; they scroll away normally
            let sticky = !(is_foldable && new_display_mode == DisplayMode::Expanded);
            cache.prompt_descriptors.push(PromptDescriptor {
                entry_idx: new_idx,
                y_virtual: new_y,
                full_height: height,
                min_height,
                sticky,
            });
        }

        true
    }

    /// Rebuild the layout cache for the given width. Reuses existing Vec allocations when possible to avoid repeated
    /// allocations.
    fn rebuild_layout_cache(&mut self, width: u16) {
        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(width);

        // Reuse existing cache's Vecs to avoid allocations
        let mut cache = self.layout_cache.take().unwrap_or_default().take();
        cache.width = width;

        // Pass 1: Compute a cheap height estimate for every entry (no markdown render / word-wrap). This keeps the
        // bulk-load rebuild O(history) in cheap arithmetic instead of O(history) markdown renders.
        for entry in self.entries.values() {
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(self.cwd());
            let height = renderer.estimate_height(entry_area_width);
            cache.entries.push(EntryLayoutInfo {
                height,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            });
            // Truncated height only feeds prompt sticky-header min_height, and is an estimate until the entry is measured
            // Seed it with the MAX so an as-yet-unmeasured pinned prompt never under-reserves and overlaps its content
            // The exact value is filled in on measurement
            cache
                .entry_truncated_heights
                .push(MAX_TRUNCATED_HEADER_HEIGHT);
            cache.measured.push(false);
        }

        // Pass 2: Compute gap_after using the pairwise grouping rule.
        Self::recompute_gap_after(&self.entries, &mut cache.entries);

        // Pass 2b: Apply verb-group folding and group truncation
        let max_visible = self.appearance.scrollback.display.group_max_visible as usize;
        cache.groups = groups::apply(
            &self.entries,
            &mut cache.entries,
            max_visible,
            &self.expanded_groups,
        );

        // Pass 3: Build virtual_y and prompt descriptors from heights and gaps
        let mut y: usize = 0;
        for (idx, entry_layout) in cache.entries.iter().enumerate() {
            cache.virtual_y.push(y);

            if let Some((_, entry)) = self.entries.get_index(idx)
                && entry.block.is_user_prompt()
            {
                let truncated_height = cache
                    .entry_truncated_heights
                    .get(idx)
                    .copied()
                    .unwrap_or(MAX_TRUNCATED_HEADER_HEIGHT);
                let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
                let sticky =
                    !(entry.block.is_foldable() && entry.display_mode == DisplayMode::Expanded);
                cache.prompt_descriptors.push(PromptDescriptor {
                    entry_idx: idx,
                    y_virtual: y,
                    full_height: entry_layout.height,
                    min_height,
                    sticky,
                });
            }

            y += entry_layout.height as usize + entry_layout.gap_after as usize;
        }

        self.layout_cache = Some(cache);
    }

    /// Compute gap_after for all entries using the pairwise grouping rule ([`gap_after_between`]). The last entry
    /// always gets gap_after=1 (trailing gap for selection box bottom corner). We thus do not leave a double spacer (a
    /// gap into thinking and a gap out).
    fn recompute_gap_after(
        entries: &IndexMap<EntryId, ScrollbackEntry>,
        cached_entries: &mut [EntryLayoutInfo],
    ) {
        let n = cached_entries.len();
        if n == 0 {
            return;
        }

        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();

        for (i, cached) in cached_entries.iter_mut().enumerate() {
            let (_, a) = entries.get_index(i).unwrap();
            if a.is_hidden_thinking(show_thinking) {
                cached.gap_after = 0;
                continue;
            }

            // Skip over a run of hidden thinking to the next visible neighbor.
            let mut j = i + 1;
            while j < n {
                let (_, mid) = entries.get_index(j).unwrap();
                if !mid.is_hidden_thinking(show_thinking) {
                    break;
                }
                j += 1;
            }

            if j >= n {
                // Only trailing hidden thinking after `a` (or `a` is last).
                cached.gap_after = 1;
                continue;
            }

            let (_, b) = entries.get_index(j).unwrap();
            cached.gap_after = gap_after_between(a, b);
        }
    }

    /// A turn-terminal marker is always a singleton so the walk cannot absorb neighbouring tool rows. Otherwise,
    /// returns the range of the contiguous group. The walk is bounded by [`Self::joins_dense_run`], so the dense range
    /// agrees with the truncation pass's claimed-entry and turn-marker breaks.
    pub fn group_range_of(&self, idx: usize, collapsed_only: bool) -> Range<usize> {
        // A verb-group run is its own group regardless of `collapsed_only`
        // Members are collapsed by construction; the run stays the toggle / collapse / selection unit while expanded
        if let Some(range) = self.verb_group_span_range(idx) {
            return range;
        }

        let Some((_, entry)) = self.entries.get_index(idx) else {
            return idx..idx + 1;
        };

        if !groups::can_join_dense_run(entry, collapsed_only) {
            return idx..idx + 1;
        }

        let matches = |i: usize| self.joins_dense_run(i, collapsed_only);

        let mut start = idx;
        while start > 0 && matches(start - 1) {
            start -= 1;
        }
        let mut end = idx + 1;
        while end < self.entries.len() && matches(end) {
            end += 1;
        }
        start..end
    }

    /// Whether the entry at `i` joins a dense (non-verb) run walk. Sharing it means their run shapes can't drift apart.
    /// Verb-claimed entries never join: truncation breaks its runs at claimed entries. A turn-terminal marker never
    /// joins ([`groups::can_join_dense_run`]), matching the truncation pass.
    pub(super) fn joins_dense_run(&self, i: usize, collapsed_only: bool) -> bool {
        if let Some((_, e)) = self.entries.get_index(i) {
            groups::can_join_dense_run(e, collapsed_only) && self.verb_group_range_of(i).is_none()
        } else {
            false
        }
    }

    /// The folded verb run containing the claimed entry at `idx`, read from the last fold pass's spans
    /// ([`Self::span_at`]). This mirrors the walk's anchor check in [`Self::verb_group_range_of`]. The walk predicts
    /// the next fold from current entry state.
    fn verb_group_span_range(&self, idx: usize) -> Option<Range<usize>> {
        let span = self.span_at(idx)?;
        let groups::GroupKind::VerbRun { .. } = span.kind else {
            return None;
        };
        let (_, entry) = self.entries.get_index(idx)?;
        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
        match run_step(entry, show_thinking) {
            RunStep::Member(_) | RunStep::ThoughtMember => Some(span.range.clone()),
            RunStep::Transparent | RunStep::Break => None,
        }
    }

    /// The folding verb-group run (per `RunScan::folds`, `group_tool_verbs` on) containing the claimed entry at `idx`.
    /// The broader dense-group run would leak across separators like Edit. Post-layout queries go through
    /// [`Self::verb_group_span_range`] instead.
    pub(super) fn verb_group_range_of(&self, idx: usize) -> Option<Range<usize>> {
        if !crate::appearance::cache::load_group_tool_verbs() {
            return None;
        }
        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
        // An unclaimable `idx` has no range, even when its neighbors form a run
        let (_, entry) = self.entries.get_index(idx)?;
        match run_step(entry, show_thinking) {
            RunStep::Member(_) | RunStep::ThoughtMember => {}
            RunStep::Transparent | RunStep::Break => return None,
        }

        // The backward half only finds the run's start; the shared forward scan from `start` then measures the whole run in one pass
        let mut start = idx;
        let mut scan = idx;
        while scan > 0 {
            let (_, e) = self.entries.get_index(scan - 1)?;
            match run_step(e, show_thinking) {
                RunStep::Member(_) | RunStep::ThoughtMember => start = scan - 1,
                RunStep::Transparent => {}
                RunStep::Break => break,
            }
            scan -= 1;
        }

        let entry_at = |i: usize| self.entries.get_index(i).map(|(_, e)| e);
        let run = scan_run_forward(entry_at, start, show_thinking)?;
        run.folds().then_some(start..run.end)
    }

    /// Paint window for one scroll frame: the sub-range of `visible_range` whose entries can intersect the content
    /// viewport. Thin wrapper over [`compute_paint_window`] fed from the layout cache. The aggregated header labels
    /// thus still see off-screen members.
    pub fn paint_window(
        &self,
        visible_range: Range<usize>,
        scroll: usize,
        viewport_h: usize,
    ) -> (Range<usize>, usize) {
        let virtual_y = self
            .get_cached_virtual_y()
            .expect("layout cache must be valid - was prepare_layout() called?");
        let layouts = self
            .get_cached_entry_layouts()
            .expect("layout cache must be valid - was prepare_layout() called?");
        compute_paint_window(virtual_y, layouts, visible_range, scroll, viewport_h, |i| {
            self.span_at(i).map_or(i + 1, |span| span.range.end)
        })
    }

    /// Get the sticky header layout for the current scroll position.
    /// Works for both AllTurns and SingleTurn modes using unified sticky logic.
    /// Returns None if no entries or no sticky header at current position.
    pub fn sticky_layout(&mut self) -> Option<StickyHeaderLayout> {
        if self.entries.is_empty() || self.last_width == 0 || self.viewport_height == 0 {
            return None;
        }

        self.ensure_layout_cache(self.last_width);

        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);

        let sticky =
            compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts);

        if sticky.has_header() {
            Some(sticky)
        } else {
            None
        }
    }

    /// Get cached prompt descriptors (for rendering).
    /// Returns None if cache is not valid.
    pub fn prompt_descriptors(&mut self) -> Option<Vec<PromptDescriptor>> {
        if self.last_width == 0 {
            return None;
        }
        self.ensure_layout_cache(self.last_width);
        self.layout_cache
            .as_ref()
            .map(|c| c.prompt_descriptors.clone())
    }
}

/// Blank rows between two adjacent visible entries: consecutive collapsed tool chrome stacks with no gap.
/// Membership is [`groups::can_join_dense_run`] in collapsed-only mode, so the gap and dense-run
/// boundaries stay in step; a stop-hook-collapsed turn marker therefore never glues to the next row.
fn gap_after_between(prev: &ScrollbackEntry, next: &ScrollbackEntry) -> u16 {
    if groups::can_join_dense_run(prev, /*collapsed_only=*/ true)
        && groups::can_join_dense_run(next, /*collapsed_only=*/ true)
    {
        0
    } else {
        1
    }
}

/// O(log n) via `partition_point` over the cached prefix-sum `virtual_y`, instead of collecting/walking the full
/// history each frame. The aggregated header labels thus still see off-screen members (counts/tense/failures).
/// `run_end` is only called for visible header rows.
pub fn compute_paint_window(
    virtual_y: &[usize],
    layouts: &[EntryLayoutInfo],
    visible_range: Range<usize>,
    scroll: usize,
    viewport_h: usize,
    run_end: impl Fn(usize) -> usize,
) -> (Range<usize>, usize) {
    debug_assert_eq!(virtual_y.len(), layouts.len());
    if visible_range.is_empty() {
        return (visible_range.start..visible_range.start, 0);
    }
    let Some(&base_y) = virtual_y.get(visible_range.start) else {
        return (visible_range.start..visible_range.start, 0);
    };
    let vp_start = base_y + scroll;
    let vp_end = vp_start + viewport_h;
    let Some(range_vy) = virtual_y.get(visible_range.clone()) else {
        return (visible_range.start..visible_range.start, 0);
    };
    let mut first_rel = range_vy.partition_point(|&y| y < vp_start);
    if first_rel > 0 {
        let prev = visible_range.start + first_rel - 1;
        if virtual_y
            .get(prev)
            .zip(layouts.get(prev))
            .is_some_and(|(&vy, layout)| vy + layout.height as usize > vp_start)
        {
            first_rel -= 1;
        }
    }
    let paint_start = visible_range.start + first_rel;
    let mut paint_end = visible_range.start + range_vy.partition_point(|&y| y < vp_end);
    let mut i = paint_start;
    while i < paint_end {
        // Any group header row (verb or truncation) aggregates entries that can sit past the viewport edge; extend so the label walks see them
        if layouts
            .get(i)
            .is_some_and(|layout| layout.height > 0 && layout.is_group_header())
        {
            // The run walk is range-agnostic; keep the window inside the visible range so index remapping downstream stays valid
            paint_end = paint_end.max(run_end(i).min(visible_range.end));
        }
        i += 1;
    }
    let content_y0 = if paint_start < paint_end {
        virtual_y.get(paint_start).map_or(0, |&y| y - base_y)
    } else {
        0
    };
    (paint_start..paint_end, content_y0)
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
