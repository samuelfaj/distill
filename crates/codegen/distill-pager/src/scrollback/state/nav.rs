use super::*;

/// Find the response anchor for a turn: the first AgentMessage of the trailing agent-message run within `range`.
/// Everything else (session events, system messages, empty streaming placeholders) is skipped without breaking the
/// run.
fn response_anchor_in_range(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    range: Range<usize>,
) -> Option<usize> {
    let mut anchor = None;
    for idx in range.rev() {
        // Turn ranges are always in bounds; keep any anchor already found
        let Some((_, entry)) = entries.get_index(idx) else {
            break;
        };
        match &entry.block {
            RenderBlock::AgentMessage(msg) => {
                if !msg.is_empty() {
                    anchor = Some(idx);
                }
            }
            RenderBlock::ToolCall(_)
            | RenderBlock::Thinking(_)
            | RenderBlock::Subagent(_)
            | RenderBlock::BgTask(_) => break,
            _ => {}
        }
    }
    anchor
}

impl ScrollbackState {
    // Turn Navigation

    pub(super) fn rebuild_turns(&mut self) {
        self.turns.clear();
        let mut current_start: Option<usize> = None;

        for (i, entry) in self.entries.values().enumerate() {
            if matches!(entry.block, RenderBlock::UserPrompt(_)) {
                // Close previous turn
                if let Some(start) = current_start {
                    self.turns.push(Turn {
                        prompt_index: start,
                        end_index: i,
                        status: TurnStatus::Completed,
                    });
                }
                current_start = Some(i);
            }
        }

        // Close the last turn
        if let Some(start) = current_start {
            self.turns.push(Turn {
                prompt_index: start,
                end_index: self.entries.len(),
                status: TurnStatus::Running, // Last turn may still be active
            });
        }

        // Update current_turn
        self.current_turn = if self.turns.is_empty() {
            None
        } else if let Some(sel) = self.selected {
            self.turn_containing(sel)
        } else {
            Some(self.turns.len() - 1)
        };
    }

    pub fn turn_containing(&self, entry_index: usize) -> Option<usize> {
        self.turns
            .iter()
            .position(|t| entry_index >= t.prompt_index && entry_index < t.end_index)
    }

    pub fn current_turn(&self) -> Option<usize> {
        self.current_turn
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn turn(&self, index: usize) -> Option<&Turn> {
        self.turns.get(index)
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Iterate over all entries in scrollback order, paired with their `EntryId`.
    pub fn iter_entries(&self) -> impl Iterator<Item = (EntryId, &ScrollbackEntry)> {
        self.entries.iter().map(|(id, entry)| (*id, entry))
    }

    /// Common logic for jumping to an entry within a turn.
    ///
    /// Sets `current_turn`, resets scroll in SingleTurn mode, selects the entry, and scrolls so it sits at the top of the viewport.
    fn activate_entry(&mut self, turn_idx: usize, entry_idx: usize) {
        self.current_turn = Some(turn_idx);
        if self.view_mode == ViewMode::SingleTurn {
            self.scroll_offset = 0;
            self.follow_mode = false;
        }
        self.selected = Some(entry_idx);
        if self.view_mode == ViewMode::AllTurns {
            self.scroll_to_entry_top(entry_idx);
        } else {
            self.ensure_selected_visible(NavDirection::default());
        }
        self.bump_generation();
    }

    /// Common logic for switching to a specific turn (jumps to its prompt).
    fn activate_turn(&mut self, turn_idx: usize) {
        if let Some(prompt_index) = self.turns.get(turn_idx).map(|t| t.prompt_index) {
            self.activate_entry(turn_idx, prompt_index);
        }
    }

    /// Jump directly to a turn by index: select its prompt and scroll it to the viewport top (same behavior as h/l turn navigation, random access).
    /// Returns `false` for an out-of-range index.
    pub fn jump_to_turn(&mut self, turn_idx: usize) -> bool {
        if turn_idx >= self.turns.len() {
            return false;
        }
        self.activate_turn(turn_idx);
        true
    }

    /// Jump to a turn by its prompt's stable [`EntryId`]: resolve to the current index and activate that turn.
    /// Returns `false` if the id no longer exists or isn't a turn's prompt (e.g. removed since capture).
    /// Stable identity means a shifted index can't land on the wrong block.
    pub fn jump_to_entry(&mut self, prompt_id: EntryId) -> bool {
        let Some(entry_idx) = self.index_of_id(prompt_id) else {
            return false;
        };
        let Some(turn_idx) = self.turns.iter().position(|t| t.prompt_index == entry_idx) else {
            return false;
        };
        self.activate_turn(turn_idx);
        true
    }

    /// Navigate to the next turn (l key).
    /// If we're before the first turn (e.g., at system messages), jumps to the first turn.
    /// Otherwise jumps to the next turn.
    pub fn next_turn(&mut self) -> bool {
        // If no turns exist, nothing to do
        if self.turns.is_empty() {
            return false;
        }

        let next = match self.current_turn {
            Some(current) => {
                if current + 1 >= self.turns.len() {
                    // Already at last turn; re-activate it to scroll its prompt to the top (same as h then l would do)
                    self.activate_turn(current);
                    return true;
                }
                current + 1
            }
            None => {
                // Not at any turn (e.g., before first prompt), go to first turn
                0
            }
        };

        self.activate_turn(next);
        true
    }

    /// Navigate to the previous turn (h key). If currently in a turn's response (not on the prompt), jumps to the
    /// current turn's prompt. If already on a prompt, jumps to the previous turn's prompt. If at first turn's prompt
    /// and pre-turn exists with selectable entries, jumps there.
    pub fn prev_turn(&mut self) -> bool {
        // If no turns exist, nothing to do
        if self.turns.is_empty() {
            return false;
        }

        // Check if we're currently in pre-turn (current_turn is None)
        if self.current_turn.is_none() {
            return false; // Already at pre-turn, can't go further back
        }

        let current = self.current_turn.unwrap();
        let current_turn = match self.turns.get(current) {
            Some(t) => t,
            None => return false,
        };

        // Check if we're on the current turn's prompt or somewhere in its response
        // If selected is None, treat as being "on prompt" (go to previous turn)
        let on_prompt = self.selected.is_none() || self.selected == Some(current_turn.prompt_index);

        if !on_prompt {
            // In the turn's response: go to current turn's prompt first
            self.activate_turn(current);
            return true;
        }

        // Already on a prompt: go to previous turn
        if current == 0 {
            // At first turn's prompt: check if pre-turn exists and has selectable entries
            if current_turn.prompt_index > 0 {
                let range = 0..current_turn.prompt_index;
                let first_selectable = self.find_first_selectable_in_range(range);
                if first_selectable.is_none() {
                    return false; // No selectable entries in pre-turn
                }
                self.current_turn = None;
                if self.view_mode == ViewMode::SingleTurn {
                    self.scroll_offset = 0;
                    self.follow_mode = false;
                }
                self.selected = first_selectable;
                if self.view_mode == ViewMode::AllTurns {
                    self.scroll_offset = 0;
                }
                return true;
            }
            return false; // No pre-turn, already at first turn's prompt
        }

        // Go to previous turn's prompt
        self.activate_turn(current - 1);
        true
    }

    /// Estimated (header-free) top scroll offset of an entry, for cheap candidate pruning before the exact sticky-converged computation.
    fn entry_top_estimate(&self, idx: usize) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        let base = *cache.virtual_y.get(range.start)?;
        let y = *cache.virtual_y.get(idx)?;
        Some(y.saturating_sub(base))
    }

    /// Apply a response snap: select the anchor and put its top at the viewport's content top (`target` from `entry_top_scroll_offset`).
    fn snap_to_response(&mut self, turn_idx: usize, entry_idx: usize, target: usize) {
        self.current_turn = Some(turn_idx);
        self.selected = Some(entry_idx);
        self.scroll_offset = target;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Snap the viewport up to the nearest response anchor above. Anchors are computed on demand; only plausible
    /// candidates are measured exactly. Returns `false` without a layout or when no anchor lies above.
    pub fn prev_response(&mut self) -> bool {
        if self.viewport_height == 0 || self.last_width == 0 {
            return false;
        }
        self.ensure_layout_cache(self.last_width);
        for t in (0..self.turns.len()).rev() {
            let Some(turn) = self.turns.get(t) else {
                continue;
            };
            let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
                continue;
            };
            if !self.visible_entry_range().contains(&idx) {
                continue;
            }
            // Headers shrink the exact target by at most the truncated-header cap, so anchors estimated further below can be skipped unmeasured
            // (Viewport-height margins invert on tiny terminal splits.)
            let Some(estimate) = self.entry_top_estimate(idx) else {
                continue;
            };
            if estimate
                >= self
                    .scroll_offset
                    .saturating_add(MAX_TRUNCATED_HEADER_HEIGHT as usize + 1)
            {
                continue;
            }
            if let Some(target) = self.entry_top_scroll_offset(idx) {
                debug_assert!(target <= estimate, "exact target exceeded estimate");
                if target < self.scroll_offset {
                    self.snap_to_response(t, idx, target);
                    return true;
                }
            }
        }
        false
    }

    /// Snap the viewport down to the nearest response anchor below. Returns `false` without a layout or when no
    /// anchor lies below.
    pub fn next_response(&mut self) -> bool {
        if self.viewport_height == 0 || self.last_width == 0 {
            return false;
        }
        self.ensure_layout_cache(self.last_width);
        for t in 0..self.turns.len() {
            let Some(turn) = self.turns.get(t) else {
                continue;
            };
            let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
                continue;
            };
            if !self.visible_entry_range().contains(&idx) {
                continue;
            }
            // The exact target never exceeds the estimate, so anchors estimated at or above the current offset can't land below it
            let Some(estimate) = self.entry_top_estimate(idx) else {
                continue;
            };
            if estimate <= self.scroll_offset {
                continue;
            }
            if let Some(target) = self.entry_top_scroll_offset(idx) {
                debug_assert!(target <= estimate, "exact target exceeded estimate");
                if target > self.scroll_offset {
                    self.snap_to_response(t, idx, target);
                    return true;
                }
            }
        }
        false
    }

    /// From inside an answer that anchor is the nearest one above, so the indicator only shows when the click has that
    /// answer's top to land on. Cache-only estimate (`&self`, headers ignored) so render can poll it every frame. The
    /// estimate never undershoots the exact target, so a visible indicator always has a real jump behind it.
    pub fn has_response_top_above(&self) -> bool {
        let Some(turn) = self
            .active_turn_for_viewport()
            .and_then(|t| self.turns.get(t))
        else {
            return false;
        };
        let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
            return false;
        };
        self.visible_entry_range().contains(&idx)
            && self
                .entry_top_estimate(idx)
                .is_some_and(|estimate| estimate < self.scroll_offset)
    }

    pub fn set_last_turn_status(&mut self, status: TurnStatus) {
        if let Some(turn) = self.turns.last_mut() {
            turn.status = status;
        }
    }

    // Scrolling

    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows as usize);
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Scroll down by n rows. A scroll that lands at the bottom moved real rows and never engages, so a fast
    /// scroll-down ending there can't re-enter follow by accident.
    pub fn scroll_down(&mut self, rows: u16) {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        let before = self.scroll_offset;
        self.scroll_offset = (self.scroll_offset + rows as usize).min(max_offset);

        // rows > 0 keeps degenerate page/half-page calls (0-row viewports) from engaging follow without a real scroll gesture
        if rows > 0
            && self.scroll_offset == before
            && self.scroll_offset >= max_offset
            && self.pin_reserve_pad == 0
            && self.appearance.scrollback.scroll.follow_by_overscroll
        {
            self.follow_mode = true;
            if self.follow_preserve_scroll && self.pin_reserve_after_turn {
                self.follow_preserve_scroll = false;
                self.release_pin_reserve();
                self.scroll_offset = self.max_scroll_offset();
            }
        }
        self.bump_generation();
    }

    /// Rows to advance for a full-page scroll. Without it, a page moves `viewport_height - 2` rows but only
    /// `viewport_height - header` rows are actually on screen. Always moves at least 1 row so paging never stalls on a
    /// tiny viewport.
    fn page_scroll_rows(&self) -> u16 {
        let header = self.current_header_screen_rows();
        self.viewport_height
            .saturating_sub(header)
            .saturating_sub(2)
            .max(1)
    }

    /// Current sticky header height in screen rows (0 when no header/cache).
    fn current_header_screen_rows(&self) -> u16 {
        // Mirror render_with_sticky_headers: no sticky header is drawn when disabled or in compact prompt mode
        // The whole viewport is then content, so paging must not subtract a header height
        if !self.appearance.scrollback.display.sticky_headers || self.appearance.prompt.compact {
            return 0;
        }
        let Some(cache) = self.layout_cache.as_ref() else {
            return 0;
        };
        let range = self.visible_entry_range();
        if range.is_empty() {
            return 0;
        }
        self.current_sticky_layout(cache, &range)
            .header_screen_rows()
    }

    /// Page up: scroll viewport, then select the topmost selectable on-screen entry.
    pub fn page_up(&mut self) {
        self.scroll_up(self.page_scroll_rows());
        self.select_viewport_edge(/* prefer_top */ true);
    }

    /// Page down: scroll viewport, then select the bottommost selectable on-screen entry.
    pub fn page_down(&mut self) {
        self.scroll_down(self.page_scroll_rows());
        self.select_viewport_edge(/* prefer_top */ false);
    }

    /// Select selectable entry covering the visual top (`prefer_top`) or bottom row; walk inward if needed.
    fn select_viewport_edge(&mut self, prefer_top: bool) {
        if self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        self.ensure_layout_cache(self.last_width);
        let range = self.visible_entry_range();
        if range.is_empty() {
            return;
        }
        let Some((vp_top, vp_bottom)) = self.viewport_virtual_bounds() else {
            return;
        };
        // Empty viewport (bottom == top) has no edge row to anchor on.
        if vp_bottom <= vp_top {
            return;
        }
        let edge_row = if prefer_top { vp_top } else { vp_bottom - 1 };
        let Some(raw_edge) = self.entry_at_virtual_row(edge_row) else {
            return;
        };
        // Clamp into the visible range so `..=` / `..` never panic on inverted bounds.
        let edge_idx = raw_edge.clamp(range.start, range.end - 1);

        let pick = if prefer_top {
            (edge_idx..range.end).find(|&idx| self.is_selectable_in_viewport(idx))
        } else {
            (range.start..=edge_idx)
                .rev()
                .find(|&idx| self.is_selectable_in_viewport(idx))
        };

        if let Some(idx) = pick {
            self.selected = Some(idx);
            self.sync_current_turn();
        }
    }

    fn is_selectable_in_viewport(&self, idx: usize) -> bool {
        if self.is_entry_hidden(idx) || !self.entry_overlaps_viewport(idx) {
            return false;
        }
        self.entries
            .get_index(idx)
            .is_some_and(|(_, e)| e.block.is_selectable())
    }

    /// Half page up (Ctrl-U style).
    pub fn half_page_up(&mut self) {
        self.scroll_up(self.viewport_height / 2);
    }

    /// Half page down (Ctrl-D style).
    pub fn half_page_down(&mut self) {
        self.scroll_down(self.viewport_height / 2);
    }

    pub fn goto_top(&mut self) {
        self.scroll_offset = 0;
        self.follow_mode = false;
        let range = self.visible_entry_range();
        if !range.is_empty() {
            self.selected = self.find_first_selectable_in_range(range);
            self.sync_current_turn();
        }
        self.bump_generation();
    }

    pub fn goto_bottom(&mut self) {
        self.release_pin_reserve();
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = max_offset;
        self.follow_mode = true;
        // Also drop follow-preserve, or its branch in `follow_scroll_to_bottom` keeps holding the offset computed above
        // That offset may come from estimated heights, leaving End or PageDown stuck above or wedged past the measured bottom
        self.follow_preserve_scroll = false;

        let range = self.visible_entry_range();
        if !range.is_empty() {
            self.selected = self.find_last_selectable_in_range(range);
            self.sync_current_turn();
        }
        self.bump_generation();
    }

    pub fn toggle_follow(&mut self) {
        self.follow_mode = !self.follow_mode;
        if self.follow_mode {
            self.goto_bottom();
        }
    }

    /// Enable follow mode and scroll to bottom.
    pub fn enable_follow_mode(&mut self) {
        self.follow_mode = true;
        self.follow_preserve_scroll = false;
        self.goto_bottom();
    }

    /// Enable follow mode without changing scroll position.
    ///
    /// Use this when you've already positioned the viewport (e.g., after `scroll_to_entry_top`) but want new content to auto-scroll.
    pub fn enable_follow(&mut self) {
        self.follow_mode = true;
    }

    /// Enable follow mode, preserving the current scroll position for one frame.
    /// Like `enable_follow`, but also sets `follow_preserve_scroll` so the first `handle_follow_mode` call doesn't override the scroll position.
    /// Use after `scroll_to_entry_top` to keep the entry at the viewport top until new content arrives.
    pub fn enable_follow_with_preserve(&mut self) {
        self.follow_mode = true;
        self.follow_preserve_scroll = true;
        self.follow_preserve_content_generation = self.content_generation;
    }

    /// Pin an entry at the viewport top with real trailing scroll extent.
    pub(crate) fn page_flip_to_entry(&mut self, idx: usize) {
        self.arm_pin_reserve();
        self.scroll_to_entry_top(idx);
        self.pin_reserve_target = Some(self.scroll_offset);
        self.pin_reserve_prompt_id = self.entries.get_index(idx).map(|(id, _)| *id);
        self.compute_total_height_from_cache();
        self.enable_follow_with_preserve();
    }

    /// `page_flip` without a prompt (bash/synthetic): enable follow-with-preserve only. No `page_flip`, with a prompt:
    /// leave scroll and follow unchanged. No `page_flip`, no prompt: still enable follow-with-preserve (there is no
    /// prompt to snap. pre-setting bash/adoption always engaged follow). Always selects `prompt_idx` when present.
    pub fn follow_new_turn(&mut self, prompt_idx: Option<usize>, page_flip: bool) {
        if let Some(idx) = prompt_idx {
            self.set_selected(Some(idx));
        }
        if page_flip {
            if let Some(idx) = prompt_idx {
                self.page_flip_to_entry(idx);
            } else {
                self.release_pin_reserve();
                self.enable_follow_with_preserve();
            }
        } else if prompt_idx.is_none() {
            self.release_pin_reserve();
            self.enable_follow_with_preserve();
        }
    }

    pub fn is_follow_mode(&self) -> bool {
        self.follow_mode
    }

    pub(crate) fn is_follow_preserve_scroll(&self) -> bool {
        self.follow_preserve_scroll
    }

    pub fn has_content_below(&self) -> bool {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.total_height > self.viewport_height as usize && self.scroll_offset < max_offset
    }

    /// Ensure the selected entry is visible in the viewport, using minimal scrolling. large entries that exceed it
    /// always show their top. Only scrolls if the entry is not fully visible. `min_page_fraction`: if the scroll is
    /// smaller than this fraction, use it instead (disabled for large entries and at edges).
    pub(super) fn ensure_selected_visible(&mut self, _direction: NavDirection) {
        let Some(selected_idx) = self.selected else {
            return;
        };

        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return;
        }

        // Get scroll config (cumulative scroll math is usize)
        let base_margin = self.appearance.scrollback.scroll.margin as usize;
        let min_scroll = self
            .appearance
            .scrollback
            .scroll
            .min_scroll_lines(self.viewport_height) as usize;

        // Get visible range; positions are relative to this range
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&selected_idx) {
            return; // Selected entry not in visible range, nothing to do
        }

        // Stay O(viewport): only an off-viewport selection (the path that scrolls) gets a bounded measure around it
        // The scroll branch below re-derives scroll
        // Measuring around an on-viewport selection would jump it (see measure_around_entry)
        self.ensure_layout_cache(self.last_width);
        if !self.entry_overlaps_viewport(selected_idx) {
            self.measure_around_entry(selected_idx, self.last_width);
        }

        // Margin only applies when there's content to show in that direction.
        // At scroll edges, margin would just show empty space.
        let max_scroll = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        let at_top = self.scroll_offset == 0;
        let at_bottom = self.scroll_offset >= max_scroll;
        let top_margin = if at_top { 0 } else { base_margin };
        let bottom_margin = if at_bottom { 0 } else { base_margin };

        let Some(ref cache) = self.layout_cache else {
            return;
        };

        // Compute RELATIVE positions for the visible range.
        // scroll_offset is relative to the start of visible_range, not entry 0.
        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return;
        };
        let Some(&entry_y_abs) = cache.virtual_y.get(selected_idx) else {
            return;
        };
        let Some(entry_info) = cache.entries.get(selected_idx) else {
            return;
        };
        let entry_y = entry_y_abs - base_y;
        let entry_height = entry_info.height;
        let entry_bottom = entry_y + entry_height as usize;

        // Build prompt descriptors relative to visible range (for sticky layout)
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);

        // Compute sticky header at current scroll position
        let sticky =
            compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts);
        let header_height = sticky.header_screen_rows();

        // Content area bounds
        let content_top = self.scroll_offset + header_height as usize;
        let viewport_bottom = self.scroll_offset + self.viewport_height as usize;

        // Effective bounds with margin
        let effective_top = content_top + top_margin;
        let effective_bottom = viewport_bottom.saturating_sub(bottom_margin);

        // Check if entry is fully visible (with margin)
        let is_fully_visible = entry_y >= effective_top && entry_bottom <= effective_bottom;

        if is_fully_visible {
            return; // No scroll needed
        }

        // Check if entry fits in viewport content area
        let content_height = self.viewport_height.saturating_sub(header_height);
        let entry_fits = entry_height <= content_height;

        let old_scroll = self.scroll_offset;

        if entry_y < effective_top {
            // Entry top is clipped (or within margin): scroll up to show top with margin
            let target_scroll = self.find_scroll_for_entry_at_content_top(
                entry_y.saturating_sub(top_margin),
                &relative_prompts,
            );
            self.scroll_offset = target_scroll;
            self.follow_mode = false;
        } else if entry_bottom > effective_bottom {
            // Entry bottom is clipped (or within margin)
            if entry_fits {
                // Entry fits at current header height: try scrolling to show bottom with margin
                let target_scroll =
                    (entry_bottom + bottom_margin).saturating_sub(self.viewport_height as usize);

                // But check: at the new scroll position, will a sticky header appear that clips the entry's top?
                let sticky_at_target =
                    compute_sticky_layout(target_scroll, self.viewport_height, &relative_prompts);
                let header_at_target = sticky_at_target.header_screen_rows();
                let content_top_at_target = target_scroll + header_at_target as usize;

                if entry_y >= content_top_at_target {
                    // Entry top will still be visible after scroll: use this target
                    self.scroll_offset = target_scroll;
                } else {
                    // Sticky header would clip the top: show top instead
                    let target_scroll =
                        self.find_scroll_for_entry_at_content_top(entry_y, &relative_prompts);
                    self.scroll_offset = target_scroll;
                }
            } else {
                // Entry doesn't fit: show top at content area top
                let target_scroll =
                    self.find_scroll_for_entry_at_content_top(entry_y, &relative_prompts);
                self.scroll_offset = target_scroll;
            }
            self.follow_mode = false;
        }

        // Apply minimum scroll: if we scrolled but less than min_scroll, use min_scroll. Only applies when. Entry fits in
        // viewport (large entries must show their top exactly). Won't end at scroll edge (would create empty space).
        let started_at_edge = at_top || at_bottom;
        let new_at_top = self.scroll_offset == 0;
        let new_at_bottom = self.scroll_offset >= max_scroll;
        let ends_at_edge = new_at_top || new_at_bottom;
        if min_scroll > 0
            && entry_fits
            && !started_at_edge
            && !ends_at_edge
            && self.scroll_offset != old_scroll
        {
            let actual_scroll = self.scroll_offset.abs_diff(old_scroll);

            if actual_scroll > 0 && actual_scroll < min_scroll {
                // Scroll more to meet minimum
                let max_scroll = self
                    .total_height
                    .saturating_sub(self.viewport_height as usize);
                if self.scroll_offset > old_scroll {
                    // Scrolling down: add more
                    self.scroll_offset = (old_scroll + min_scroll).min(max_scroll);
                } else {
                    // Scrolling up: subtract more
                    self.scroll_offset = old_scroll.saturating_sub(min_scroll);
                }
            }
        }
    }

    /// Build prompt descriptors relative to a visible range.
    /// Filters to only prompts in the range, adjusts y_virtual to be relative to the range start.
    /// This is needed because scroll_offset is relative to the visible range, not the full entry list.
    pub(super) fn build_relative_prompt_descriptors(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
    ) -> Vec<PromptDescriptor> {
        if visible_range.is_empty() {
            return Vec::new();
        }

        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return Vec::new();
        };

        cache
            .prompt_descriptors
            .iter()
            .filter(|p| visible_range.contains(&p.entry_idx))
            .map(|p| PromptDescriptor {
                entry_idx: p.entry_idx, // Keep absolute index for lookup
                y_virtual: p.y_virtual.saturating_sub(base_y), // Relative Y
                full_height: p.full_height,
                min_height: p.min_height,
                sticky: p.sticky,
            })
            .collect()
    }

    /// Compute the current sticky header layout.
    /// Uses the current scroll_offset and viewport_height.
    /// Requires a valid layout cache; the caller passes it in, which avoids `&mut self`.
    pub(super) fn current_sticky_layout(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
    ) -> StickyHeaderLayout {
        let relative_prompts = self.build_relative_prompt_descriptors(cache, visible_range);
        compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts)
    }

    /// Scroll offset that puts `entry_y` (relative to the visible range's top) at the content-area top, below any
    /// sticky header. Shared by `entry_top_scroll_offset` and the pin reserve's `pin_reserve_prompt_scroll_target`, so
    /// the convergence math lives in one place.
    pub(super) fn sticky_adjusted_entry_top(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
        entry_y: usize,
    ) -> usize {
        let relative_prompts = self.build_relative_prompt_descriptors(cache, visible_range);
        let mut scroll = entry_y;
        for _ in 0..3 {
            let sticky = compute_sticky_layout(scroll, self.viewport_height, &relative_prompts);
            scroll = entry_y.saturating_sub(sticky.header_screen_rows() as usize);
        }
        scroll
    }

    /// Find scroll position that puts entry_y at top of content area.
    ///
    /// Uses relative prompt descriptors and iterates to find a stable position because header height depends on scroll position.
    fn find_scroll_for_entry_at_content_top(
        &self,
        entry_y: usize,
        relative_prompts: &[PromptDescriptor],
    ) -> usize {
        let mut target_scroll = entry_y;

        // Iterate to find fixed point (max 3 iterations as safety bound).
        // Converges quickly because header shrinks monotonically as we scroll up.
        for _ in 0..3 {
            let sticky =
                compute_sticky_layout(target_scroll, self.viewport_height, relative_prompts);
            let header_at_target = sticky.header_screen_rows();
            let content_top_at_target = target_scroll + header_at_target as usize;

            if entry_y >= content_top_at_target {
                break; // Entry would be visible at this scroll position
            }

            // Need to scroll up more to account for header
            target_scroll = entry_y.saturating_sub(header_at_target as usize);
        }

        target_scroll
    }

    /// Scroll to put a specific entry at the top of the viewport.
    /// Unlike ensure_selected_visible (which only scrolls if entry is outside viewport), this always scrolls to position the entry at the top.
    /// Used for 'l' (next turn) navigation where we want the prompt at the very top.
    pub fn scroll_to_entry_top(&mut self, entry_idx: usize) {
        let Some(scroll) = self.entry_top_scroll_offset(entry_idx) else {
            return;
        };
        self.scroll_offset = scroll;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Exact scroll offset that puts `entry_idx` at the top of the viewport's content area, below any sticky header.
    /// Returns `None` without a usable layout. Measures the target entry exactly; does not move the viewport.
    fn entry_top_scroll_offset(&mut self, entry_idx: usize) -> Option<usize> {
        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return None;
        }
        if entry_idx >= self.entries.len() {
            return None;
        }

        // Ensure layout cache is valid
        self.ensure_layout_cache(self.last_width);
        // Measure the target exactly first (settle only re-pins top/bottom, never an arbitrary target)
        // Measuring above is safe: callers consume the post-measure offset
        self.measure_scroll_target(entry_idx, self.last_width);

        let cache = self.layout_cache.as_ref()?;

        // scroll_offset is relative to the start of visible_entry_range, not entry 0.
        let visible_range = self.visible_entry_range();
        let base_y = *cache.virtual_y.get(visible_range.start)?;
        let entry_y_abs = *cache.virtual_y.get(entry_idx)?;
        let entry_y = entry_y_abs - base_y;

        // Account for the sticky header: the content area starts at scroll_offset + header_height, so scroll_offset = entry_y - header
        // The header depends on scroll_offset (sticky headers collapse as you scroll), so the shared helper iterates to a fixed point
        Some(self.sticky_adjusted_entry_top(cache, &visible_range, entry_y))
    }

    pub fn scroll_to_entry_center(&mut self, entry_idx: usize) {
        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        if entry_idx >= self.entries.len() {
            return;
        }
        self.ensure_layout_cache(self.last_width);
        // Measure the target exactly first so the center uses exact offsets; otherwise an estimated off-screen target drifts off-center
        self.measure_scroll_target(entry_idx, self.last_width);
        let Some(ref cache) = self.layout_cache else {
            return;
        };
        let visible_range = self.visible_entry_range();
        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return;
        };
        let Some(&entry_y_abs) = cache.virtual_y.get(entry_idx) else {
            return;
        };
        let entry_y = entry_y_abs - base_y;
        let half_vp = self.viewport_height as usize / 2;
        let target = entry_y.saturating_sub(half_vp);
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);
        let mut scroll = target;
        for _ in 0..3 {
            let sticky = compute_sticky_layout(scroll, self.viewport_height, &relative_prompts);
            let header = sticky.header_screen_rows();
            scroll = entry_y.saturating_sub(half_vp + header as usize);
        }
        self.scroll_offset = scroll;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Select entry `entry_idx` and reveal it: expand a fold and un-truncate its group so it is no longer hidden.
    /// Wrapped or tall entries thus land the match on screen instead of scrolling it past the viewport.
    pub fn reveal_entry_line(&mut self, entry_idx: usize, line_in_entry: usize) {
        if entry_idx >= self.entries.len() {
            return;
        }
        self.set_selected(Some(entry_idx));

        // is_entry_hidden and group-truncation visibility are only meaningful once the layout cache exists. On a cache
        // miss build it first so the unhide below isn't silently skipped is_entry_hidden conservatively reports "visible"
        // when the cache is absent. That would leave a truncated target hidden after the rebuild gate.
        if self.layout_cache.is_none() && self.last_width > 0 {
            self.rebuild_layout();
            self.dirty_heights.clear();
        }

        // Track whether the reveal actually changed what is shown; only then (or when the cache is missing/stale) is the O(history) rebuild needed
        let mut layout_changed = false;

        // Un-truncate the containing group if truncation currently hides it.
        // Captured before the fold below, which would split the collapsed run.
        if self.is_entry_hidden(entry_idx) {
            let group = self.group_range_of(entry_idx, true);
            if let Some((&start_id, _)) = self.entries.get_index(group.start) {
                layout_changed |= self.expanded_groups.insert(start_id);
            }
        }

        // Expand the entry so a collapsed fold doesn't hide the matched line.
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if let Some((id, entry)) = self.entries.get_index_mut(entry_idx)
            && entry.is_foldable()
            && entry.display_mode != DisplayMode::Expanded
        {
            entry.set_display_mode(DisplayMode::Expanded);
            if respect_manual_folds {
                entry.display_mode_pinned = true;
                tracing::debug!(
                    entry_id = id.value(),
                    mode = ?entry.display_mode,
                    "scrollback.fold.pinned"
                );
            }
            // Revealing an expanded run's head re-anchors the run; migrate the expansion key with it (same discipline as the fold path)
            self.rekey_verb_group_expansion(entry_idx);
            layout_changed = true;
        }

        // Rebuild only when the reveal changed display state, the cache is gone, or heights are stale. Holding n/N across
        // already-visible matches only moves the selection. After a real rebuild drop dirty marks so the next frame's
        // incremental path can't snap off the target.
        if layout_changed || self.layout_cache.is_none() || !self.dirty_heights.is_empty() {
            self.rebuild_layout();
            self.dirty_heights.clear();
        } else {
            // rebuild_layout would have refreshed total_height; do the cheap O(visible-range) sum here. Skips only
            // rebuild_layout's per-entry re-estimation.
            self.compute_total_height_from_cache();
        }

        // Center the entry, then nudge toward the matched line within it. The logical line maps through the entry's
        // wrapped output to its rendered-row offset, so a match below a wrapped line isn't left off screen. That offset is
        // clamped to the entry height and the result to max_offset, so the nudge can't park the view past the last entry.
        self.scroll_to_entry_center(entry_idx);
        let max_row_offset = self
            .get_cached_entry_height(entry_idx)
            .unwrap_or(0)
            .saturating_sub(1);
        let header_rows = self
            .layout_cache
            .as_ref()
            .and_then(|cache| cache.entries.get(entry_idx))
            .map_or(0, |info| u16::from(info.is_expanded_verb_header()));
        let row_offset = self
            .rendered_row_offset_within_entry(entry_idx, line_in_entry)
            .saturating_add(header_rows)
            .min(max_row_offset);
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(row_offset as usize)
            .min(max_offset);

        self.bump_generation();
    }

    /// Rendered-row offset of `line_in_entry` within entry `entry_idx` at the current layout width.
    /// Maps the logical line the search index reports through the entry's word-wrapped output to the row actually painted.
    /// Reveal thus scrolls to where the match is rather than to an unwrapped estimate.
    fn rendered_row_offset_within_entry(&self, entry_idx: usize, line_in_entry: usize) -> u16 {
        if self.last_width == 0 {
            return u16::try_from(line_in_entry).unwrap_or(u16::MAX);
        }
        let Some((_, entry)) = self.entries.get_index(entry_idx) else {
            return 0;
        };
        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(self.last_width);
        EntryRenderer::new(entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(self.cwd())
            .rendered_row_of_logical_line(entry_area_width, line_in_entry)
    }

    /// Handle follow mode auto-scroll (call during rendering). Only follow if current turn is "running" (no end-of-turn
    /// marker). In SingleTurn mode, only follow if viewing the running turn. In AllTurns mode, only follow if at the
    /// bottom viewing running content.
    pub fn handle_follow_mode(&mut self) {
        if !self.follow_mode {
            return;
        }

        self.follow_scroll_to_bottom();

        // Auto-select the last selectable entry when following.
        // With follow_auto_select: only move selection when it's already at the tail (tracking new content) or when nothing is selected
        // This prevents overriding the user's selection when they fold or unfold a block in the middle while follow mode is on
        let range = self.visible_entry_range();
        if !range.is_empty() {
            let last_selectable = self.find_last_selectable_in_range(range);
            let should_select = self.selected.is_none()
                || (self.appearance.scrollback.scroll.follow_auto_select
                    && self.selected == last_selectable);
            if should_select {
                // Re-query in case entries were added since we last checked
                let range = self.visible_entry_range();
                self.selected = self.find_last_selectable_in_range(range);
                self.sync_current_turn();
            }
        }
    }

    /// Re-pin the viewport to the bottom for follow mode without touching the selection. Auto-selecting the last entry
    /// there would overwrite the selection while folding, etc. Pins unconditionally; callers must only invoke it when
    /// `follow_mode` is set (both current callers gate on it).
    pub(super) fn follow_scroll_to_bottom(&mut self) {
        debug_assert!(
            self.follow_mode,
            "follow_scroll_to_bottom called outside follow mode"
        );
        // When follow_preserve_scroll is set, keep the current scroll position. No explicit invalidation is needed: any
        // user interaction (scroll, fold, turn nav) sets follow_mode=false, making this unreachable.
        if self.follow_preserve_scroll {
            // Overflow is measured against the unpadded transcript
            // The pad makes max_offset equal the pin pose, so comparing to the padded max would look like overflow on every frame and eat the pin
            let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
            let unpadded_max = unpadded_total.saturating_sub(self.viewport_height as usize);
            let live_pin = if self.pin_reserve_active {
                self.pin_reserve_prompt_scroll_target()
                    .unwrap_or(self.scroll_offset)
            } else {
                self.scroll_offset
            };
            let content_changed = self.pin_reserve_active
                || self.content_generation != self.follow_preserve_content_generation;
            if content_changed && unpadded_max > live_pin && !self.pin_reserve_after_turn {
                self.follow_preserve_scroll = false;
                self.release_pin_reserve();
                self.scroll_offset = self.max_scroll_offset();
            }
            // Otherwise: all new content still fits below the prompt. Stay put.
        } else {
            // Normal follow: scroll to bottom, unconditionally
            // max_scroll_offset() is 0 when the content fits, which also heals an offset left wedged past the end by a shrink
            self.scroll_offset = self.max_scroll_offset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn follow_new_turn_scroll_policies() {
        fn tall_state_with_prompt() -> (ScrollbackState, usize) {
            let mut state = ScrollbackState::new();
            for i in 0..30 {
                state.push_block(agent_block(&format!("filler line {i}")));
            }
            state.push_block(user_block("next question"));
            let prompt_idx = state.len() - 1;
            state.prepare_layout(80, 8);
            (state, prompt_idx)
        }

        let (mut state, prompt_idx) = tall_state_with_prompt();
        state.goto_bottom();
        let bottom = state.scroll_offset();
        state.follow_new_turn(Some(prompt_idx), true);
        assert!(state.is_follow_mode());
        assert!(state.is_follow_preserve_scroll());
        assert_eq!(state.selected(), Some(prompt_idx));
        assert_ne!(state.scroll_offset(), bottom);

        let (mut state, prompt_idx) = tall_state_with_prompt();
        state.goto_bottom();
        let bottom = state.scroll_offset();
        state.follow_new_turn(Some(prompt_idx), false);
        assert!(state.is_follow_mode());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.scroll_offset(), bottom);
        assert_eq!(state.selected(), Some(prompt_idx));

        let (mut state, prompt_idx) = tall_state_with_prompt();
        state.goto_bottom();
        state.scroll_up(10);
        let reading = state.scroll_offset();
        state.follow_new_turn(Some(prompt_idx), false);
        assert!(!state.is_follow_mode());
        assert_eq!(state.scroll_offset(), reading);
        assert_eq!(state.selected(), Some(prompt_idx));

        // No prompt (bash/synthetic): follow is always enabled, with or without page_flip
        let (mut state, _) = tall_state_with_prompt();
        state.goto_bottom();
        state.follow_new_turn(None, true);
        assert!(state.is_follow_mode());
        assert!(state.is_follow_preserve_scroll());

        let (mut state, _) = tall_state_with_prompt();
        state.goto_bottom();
        state.scroll_up(10);
        let reading = state.scroll_offset();
        state.follow_new_turn(None, false);
        assert!(state.is_follow_mode());
        assert!(state.is_follow_preserve_scroll());
        assert_eq!(state.scroll_offset(), reading);
    }

    #[test]
    fn synthetic_turn_replaces_previous_page_flip_reserve() {
        let mut state = ScrollbackState::new();
        for i in 0..30 {
            state.push_block(agent_block(&format!("history {i}")));
        }
        state.push_block(user_block("prompt"));
        let prompt_idx = state.len() - 1;
        state.prepare_layout(80, 8);
        state.follow_new_turn(Some(prompt_idx), true);
        state.note_pin_reserve_turn_finished();
        assert!(state.is_pin_reserve_active());

        state.follow_new_turn(None, false);

        assert!(!state.is_pin_reserve_active());
        assert!(state.is_follow_preserve_scroll());
        state.push_block(tall_agent_block());
        state.prepare_layout(80, 8);
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.scroll_offset(), state.max_scroll_offset());
    }

    #[test]
    fn synthetic_preserve_releases_when_output_overflows_saved_viewport() {
        let mut state = ScrollbackState::new();
        for i in 0..30 {
            state.push_block(agent_block(&format!("history {i}")));
        }
        state.prepare_layout(80, 8);
        state.goto_bottom();
        state.scroll_up(5);
        let saved = state.scroll_offset();
        state.follow_new_turn(None, false);
        assert!(!state.is_pin_reserve_active());

        state.push_block(tall_agent_block());
        state.prepare_layout(80, 8);

        assert!(!state.is_follow_preserve_scroll());
        assert!(state.scroll_offset() > saved);
        assert_eq!(state.scroll_offset(), state.max_scroll_offset());
    }

    #[test]
    fn test_response_anchor_trailing_run_skips_interleaved_messages() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(agent_block("Let me look out loud")); // 1 - mid-turn speak
        state.push_block(tool_block("ls")); // 2
        state.push_block(agent_block("Found it, checking more")); // 3 - mid-turn speak
        state.push_block(tool_block("cat")); // 4
        state.push_block(agent_block("Final answer part 1")); // 5 - trailing run start
        state.push_block(agent_block("Final answer part 2")); // 6
        state.push_block(RenderBlock::system("turn done")); // 7 - doesn't break the run
        state.prepare_layout(80, 6);

        // The anchor is the trailing run's first message, not a mid-turn one
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(5));
    }

    #[test]
    fn test_streaming_response_real_path() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(tool_block("ls")); // 1
        let id = state.start_streaming_agent(); // 2
        state.prepare_layout(80, 6);

        // Empty streaming placeholder doesn't qualify
        assert!(!state.prev_response());

        // Streaming and finishing don't rebuild turns; anchors are computed on demand so nav sees the streamed content anyway
        for i in 1..=10 {
            assert!(state.push_chunk_to_agent(id, &format!("paragraph {i}\n\n")));
        }
        state.finish_running(id);
        state.prepare_layout(80, 6);
        assert!(state.is_follow_mode());
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn test_no_response_anchor_when_turn_ends_in_tool_call() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1"));
        state.push_block(agent_block("Working on it"));
        state.push_block(tool_block("cargo build")); // still running
        state.prepare_layout(80, 6);

        assert!(!state.prev_response());
        assert!(!state.next_response());
    }

    #[test]
    fn test_response_nav_without_layout_is_noop() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1"));
        state.push_block(agent_block("A1"));

        // No layout means no viewport top to compare against
        assert!(!state.prev_response());
        assert!(!state.next_response());
    }

    #[test]
    fn test_response_navigation_walks_anchor_offsets() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(tall_agent_block()); // 1
        state.push_block(user_block("Q2")); // 2
        state.push_block(tool_block("cat")); // 3 - tool-only turn, skipped
        state.push_block(user_block("Q3")); // 4
        state.push_block(tall_agent_block()); // 5
        state.prepare_layout(80, 6);

        // next_response at the follow-mode bottom is a no-op (no anchor top below)
        assert!(state.is_follow_mode());
        assert!(!state.next_response());

        // From the bottom: prev_response snaps to the last response's top
        let before = state.scroll_offset;
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(5));
        assert_eq!(state.current_turn(), Some(2));
        assert!(state.scroll_offset < before);
        assert!(!state.is_follow_mode());
        // The sticky prompt header pushed the target above the raw entry top
        let landed = state.scroll_offset;
        assert!(landed > 0 && landed < state.entry_top_estimate(5).unwrap());

        // From the snapped top, next_response still has nothing strictly below
        assert!(!state.next_response());

        // Exactly at that anchor's top: prev_response walks past the tool-only turn
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(1));
        assert_eq!(state.current_turn(), Some(0));

        // No anchor above the first one
        assert!(!state.prev_response());
        assert_eq!(state.selected(), Some(1));

        // next_response walks back down to the last anchor's top, then has nothing below
        assert!(state.next_response());
        assert_eq!(state.selected(), Some(5));
        assert_eq!(state.scroll_offset, landed);
        assert!(!state.next_response());
    }

    #[test]
    fn test_next_response_snaps_current_turn_from_work_region() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        for i in 0..6 {
            state.push_block(tool_block(&format!("tool {i}"))); // 1-6
        }
        state.push_block(tall_agent_block()); // 7
        state.prepare_layout(80, 6);
        state.scroll_to_entry_top(3);

        // Mid work region: the response top is below the viewport top, so it is a next_response target, and there is nothing above for prev_response
        assert!(!state.prev_response());
        let before = state.scroll_offset;
        assert!(state.next_response());
        assert_eq!(state.selected(), Some(7));
        assert_eq!(state.current_turn(), Some(0));
        assert!(state.scroll_offset > before);

        // Exactly at the only anchor's top: both directions are no-ops
        assert!(!state.next_response());
        assert!(!state.prev_response());
    }

    #[test]
    fn test_prompt_queued_mid_stream_response_reachable() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        let id = state.start_streaming_agent(); // 1
        // Queued prompt closes the streaming turn while its message is empty
        state.push_block(user_block("Q2")); // 2
        state.prepare_layout(80, 6);
        assert!(!state.prev_response());

        // Content streamed into the now non-last turn is reachable mid-stream
        for i in 1..=10 {
            assert!(state.push_chunk_to_agent(id, &format!("paragraph {i}\n\n")));
        }
        state.prepare_layout(80, 6);
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(1));
        assert_eq!(state.current_turn(), Some(0));
    }

    #[test]
    fn test_response_navigation_single_turn_mode() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(agent_block("A1")); // 1
        state.push_block(user_block("Q2")); // 2
        state.push_block(tall_agent_block()); // 3
        state.view_mode = ViewMode::SingleTurn;
        state.prepare_layout(80, 6);

        // Viewing the bottom of the current (last) turn: prev_response snaps its response
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(3));
        assert_eq!(state.current_turn(), Some(1));
        assert!(!state.is_follow_mode());

        // Turn 0's anchor is outside the visible turn: confined, no-op
        assert!(!state.prev_response());
        assert_eq!(state.selected(), Some(3));
    }

    #[test]
    fn test_scroll() {
        let mut state = ScrollbackState::new();
        state.total_height = 100;
        state.viewport_height = 20;

        assert!(state.is_follow_mode());

        state.scroll_up(10);
        assert!(!state.is_follow_mode());
        // scroll_up with offset=0 stays at 0
        assert_eq!(state.scroll_offset, 0);

        state.scroll_offset = 50;
        state.scroll_up(10);
        assert_eq!(state.scroll_offset, 40);

        state.scroll_down(20);
        assert_eq!(state.scroll_offset, 60);

        state.goto_bottom();
        assert_eq!(state.scroll_offset, 80); // 100 - 20
        assert!(state.is_follow_mode());

        state.goto_top();
        assert_eq!(state.scroll_offset, 0);
        assert!(!state.is_follow_mode());
    }

    // The overscroll-to-follow contract, changed from the original double-hit shape. A scroll-down that lands at the
    // bottom (moved real rows) still never engages, preserving the fast-scroll landing protection.

    #[test]
    fn clamped_scroll_down_at_bottom_engages_follow_on_first_event() {
        let mut state = ScrollbackState::new();
        state.total_height = 100;
        state.viewport_height = 20;
        // Park at max_offset without a wheel landing (scrollbar jump path).
        state.set_scroll_offset(80);
        assert!(!state.is_follow_mode());

        // A 0-row call (degenerate page-down on a tiny viewport) is not a scroll gesture and must not engage
        state.scroll_down(0);
        assert!(!state.is_follow_mode());

        state.scroll_down(3);
        assert_eq!(state.scroll_offset, 80);
        assert!(
            state.is_follow_mode(),
            "first fully-clamped scroll-down at the bottom must engage follow"
        );
    }

    #[test]
    fn landing_at_bottom_does_not_engage_follow() {
        let mut state = ScrollbackState::new();
        state.total_height = 100;
        state.viewport_height = 20;
        state.scroll_up(1); // exit follow (offset stays 0)
        state.scroll_offset = 50;

        // Clamped from 50 to 80: real rows moved, so this is a landing, not an overscroll; a fast scroll-down ending at the bottom stays manual
        state.scroll_down(40);
        assert_eq!(state.scroll_offset, 80);
        assert!(!state.is_follow_mode());

        // The next tick moves zero rows: that's the overscroll gesture.
        state.scroll_down(1);
        assert!(state.is_follow_mode());
    }

    #[test]
    fn overscroll_never_engages_follow_when_config_disabled() {
        use crate::appearance::AppearanceConfig;

        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.scroll.follow_by_overscroll = false;
        state.set_appearance(appearance);
        state.total_height = 100;
        state.viewport_height = 20;
        state.set_scroll_offset(80);

        state.scroll_down(1);
        state.scroll_down(1);
        assert!(
            !state.is_follow_mode(),
            "follow_by_overscroll=false must keep clamped scroll-downs inert"
        );
    }

    #[test]
    fn test_page_flip_prompt_stays_at_top() {
        let mut h = ScrollTestHarness::new(80, 20);
        h.push_prompt("old prompt");
        h.push_agent("old response");
        h.send_prompt("new question");
        let prompt_idx = h.state.len() - 1;

        assert!(h.is_follow(), "should be in follow mode");
        assert!(h.is_preserve(), "should have preserve_scroll");
        h.assert_entry_at_top(prompt_idx, "prompt at top after send");
        h.frame();
        h.frame();
        h.assert_entry_at_top(prompt_idx, "prompt stays at top across frames");
        assert!(h.is_preserve(), "preserve still active");
    }

    #[test]
    fn test_page_flip_thinking_streams_below_prompt() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut h = ScrollTestHarness::new(80, 20);
        h.push_prompt("old prompt");
        h.push_agent("old response");
        h.send_prompt("new question");
        let prompt_idx = h.state.len() - 1;

        let think_id = h.push_thinking("initial");
        h.assert_entry_at_top(prompt_idx, "prompt at top after thinking push");
        h.stream_thinking(think_id, " more");
        h.assert_entry_at_top(prompt_idx, "prompt at top after small stream");
    }

    #[test]
    fn test_page_flip_truncated_thinking_stays_preserved() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut h = ScrollTestHarness::new(80, 10);
        h.push_prompt("old");
        h.push_agent("old response");
        h.send_prompt("new question");
        let prompt_idx = h.state.len() - 1;

        let think_id = h.push_thinking("line1");
        for i in 0..20 {
            h.stream_thinking(think_id, &format!("\nline{}", i + 2));
        }
        assert!(
            h.is_preserve(),
            "preserve still active with truncated thinking"
        );
        h.assert_entry_at_top(prompt_idx, "prompt still at top");
    }

    #[test]
    fn test_page_flip_overflow_with_multiple_blocks() {
        let mut h = ScrollTestHarness::new(80, 10);
        h.push_prompt("old");
        h.push_agent("old response");
        h.send_prompt("new question");

        for i in 0..15 {
            h.push_tool(&format!("tool{i}"));
        }
        assert!(!h.is_preserve(), "preserve consumed after many blocks");
        assert!(h.is_follow(), "still in follow mode");
        h.assert_at_bottom("should be at bottom after overflow");
    }

    #[test]
    fn page_flip_overflow_releases_reserve_before_finish_shrink() {
        let mut h = ScrollTestHarness::new(80, 10);
        h.push_prompt("old");
        h.push_agent("old response");
        h.send_prompt("new question");

        let tall_id = h.state.push_block(tall_agent_block());
        let tall_idx = h.state.len() - 1;
        h.frame();
        assert!(!h.is_preserve(), "overflow consumes preserve");
        assert!(
            !h.state.is_pin_reserve_active(),
            "overflow releases reserve"
        );
        let live_tail = h.state.scroll_offset;

        {
            let entry = h.state.entry_mut(tall_idx).expect("response entry");
            entry.block = stub_block("short");
            entry.invalidate_cache();
        }
        h.state.mark_height_dirty(tall_id);
        h.frame();

        assert!(
            h.state.scroll_offset < live_tail,
            "collapsed content moves the real tail up"
        );
        assert!(
            !h.state.is_pin_reserve_active(),
            "finish shrink cannot recreate the reserve"
        );
        h.assert_at_bottom("finish shrink remains at the real tail");
    }

    /// A height shrink above a pinned interjection moves the prompt and its reachable bottom together.
    #[test]
    fn page_flip_pin_tracks_shrink_above_prompt() {
        let mut h = ScrollTestHarness::new(80, 10);
        for i in 0..20 {
            h.push_agent(&format!("history {i}"));
        }
        let tall_id = h.state.push_block(tall_agent_block());
        let tall_idx = h.state.len() - 1;
        h.frame();
        h.send_prompt("interjected follow-up");

        let pin = h.state.scroll_offset;
        assert!(h.is_preserve(), "setup: preserve pin armed");
        assert_eq!(
            pin,
            h.max_offset(),
            "setup: pin pose is the padded bottom ({pin} == {})",
            h.max_offset(),
        );
        assert!(
            pin < h.state.total_height,
            "setup: pin sits on real content ({pin} < {})",
            h.state.total_height,
        );

        {
            let entry = h.state.entry_mut(tall_idx).unwrap();
            entry.block = stub_block("task started");
            entry.display_mode = DisplayMode::Collapsed;
            entry.invalidate_cache();
        }
        h.state.mark_height_dirty(tall_id);
        h.frame();
        assert!(
            h.state.scroll_offset <= h.max_offset(),
            "shrink above the prompt must keep the moved pin reachable ({} <= {})",
            h.state.scroll_offset,
            h.max_offset(),
        );
        assert!(h.is_preserve(), "the prompt referent still exists");
        assert!(h.state.is_pin_reserve_active());

        for i in 0..20 {
            if !h.is_preserve() {
                break;
            }
            h.push_agent(&format!("tail growth {i}"));
        }
        assert!(
            !h.is_preserve(),
            "output below the prompt consumes preserve"
        );
        h.push_agent("task completed");
        h.push_agent("final answer");
        let last_idx = h.state.len() - 1;
        let visible = h.state.visible_entry_range();
        let (window, _) = h
            .state
            .paint_window(visible, h.state.scroll_offset, h.height as usize);
        assert!(
            window.contains(&last_idx),
            "appended rows must be inside the paint window (window {window:?}, last entry {last_idx})"
        );
    }

    /// Explicit bottom-follow gestures must release the page-flip pin before resolving the measured tail.
    #[test]
    fn bottom_gestures_clear_preserve_pin() {
        let mut h = ScrollTestHarness::new(80, 10);
        for i in 0..30 {
            h.push_agent(&format!("row {i}"));
        }
        h.send_prompt("pinned");
        assert!(h.is_preserve(), "setup: pin armed");

        h.state.goto_bottom();
        assert!(!h.is_preserve(), "goto_bottom clears the pin");
        assert!(h.is_follow(), "goto_bottom keeps follow on");
        h.frame();
        h.assert_at_bottom("End lands at the measured bottom");

        h.state.enable_follow_with_preserve();
        assert!(h.is_preserve(), "setup: pin re-armed");
        h.state.enable_follow_mode();
        assert!(!h.is_preserve(), "enable_follow_mode clears the pin");
        h.frame();
        h.assert_at_bottom("follow gesture lands at the measured bottom");
    }

    /// Plain follow (no pin): content shrinking below viewport height must re-clamp the offset to 0.
    /// Otherwise the window is left wedged past the end of the content (same freeze as the preserve variant, minus the pin).
    #[test]
    fn follow_reclamps_when_content_shrinks_below_viewport() {
        let mut state = ScrollbackState::new();
        let tall_id = state.push_block(tall_agent_block());
        for i in 0..3 {
            state.push_block(agent_block(&format!("row {i}")));
        }
        state.prepare_layout(80, 20);
        assert!(state.is_follow_mode(), "fresh state follows");
        assert!(state.scroll_offset > 0, "setup: content must overflow");

        // Shrink the tall entry so everything fits in the viewport.
        {
            let entry = state.entry_mut(0).unwrap();
            entry.block = stub_block("short");
            entry.invalidate_cache();
        }
        state.mark_height_dirty(tall_id);
        state.prepare_layout(80, 20);
        assert_eq!(
            state.scroll_offset, 0,
            "offset re-clamps to 0 once the content fits the viewport"
        );
    }

    #[test]
    fn reveal_expands_collapsed_entry() {
        let mut state = ScrollbackState::new();
        state.appearance.scrollback.scroll.respect_manual_folds = true;
        let id = state.push_block(RenderBlock::execute_with_output(
            "cargo test",
            "line0\nline1\nline2",
            None::<String>,
        ));
        state.collapse_all();
        state.prepare_layout(80, 20);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode(),
            DisplayMode::Collapsed
        );

        state.reveal_entry_line(0, 0);

        assert_eq!(state.selected(), Some(0));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode(),
            DisplayMode::Expanded
        );
        assert!(
            state.get_by_id(id).unwrap().display_mode_pinned,
            "search reveal's forced expand pins the entry"
        );
    }

    #[test]
    fn reveal_ungroups_truncated_entry() {
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        push_tool_calls(&mut state, 6);
        state.prepare_layout(80, 40);

        // Entry 1 starts hidden by group truncation.
        assert_eq!(cached_height_at(&state, 1), 0);

        state.reveal_entry_line(1, 0);

        assert_eq!(state.selected(), Some(1));
        assert!(
            cached_height_at(&state, 1) > 0,
            "revealed entry must no longer be truncated"
        );
    }

    /// Mixed run: a verb-claimed lone read abutting a truncated dense run.
    /// The dense range walk must stop at the claimed read so reveal and Left-collapse key on the truncation header's id, not the read's.
    #[test]
    fn reveal_ungroups_truncated_entry_across_adjacent_verb_fold() {
        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        state.push_block(RenderBlock::read("lone.rs", None));
        push_tool_calls(&mut state, 12);
        state.prepare_layout(80, 40);

        // The read folds on its own; the Others truncate behind entry 1.
        let verb_header = |state: &ScrollbackState, idx: usize| {
            state
                .get_cached_entry_layouts()
                .and_then(|layouts| layouts.get(idx))
                .is_some_and(|info| info.verb_group_header)
        };
        assert!(verb_header(&state, 0));
        assert_eq!(cached_height_at(&state, 2), 0, "dense member truncated");

        state.reveal_entry_line(2, 0);

        assert!(
            cached_height_at(&state, 2) > 0,
            "reveal must key the expansion on the dense run's own header"
        );
        assert!(verb_header(&state, 0), "the read's fold is untouched");
        assert_eq!(cached_height_at(&state, 0), 1);

        // Left from the revealed entry collapses the DENSE group (same bounded range), leaving the read folded
        state.set_selected(Some(2));
        assert!(state.collapse_group_if_expanded());
        state.prepare_layout(80, 40);
        assert_eq!(cached_height_at(&state, 2), 0, "dense run re-truncates");
        assert!(verb_header(&state, 0), "the read's fold is untouched");
    }

    #[test]
    fn reveal_scrolls_target_into_view() {
        let mut state = ScrollbackState::new();
        for i in 0..40 {
            state.push_block(RenderBlock::user_prompt(format!("line {i}")));
        }
        state.prepare_layout(80, 10);
        state.goto_top();
        assert_eq!(state.scroll_offset(), 0);

        state.reveal_entry_line(39, 0);

        assert_eq!(state.selected(), Some(39));
        assert!(
            state.scroll_offset() > 0,
            "revealing a bottom entry scrolls down to it"
        );
    }

    #[test]
    fn reveal_out_of_bounds_is_noop() {
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::user_prompt("only"));
        state.prepare_layout(80, 10);
        let sel = state.selected();
        let scroll = state.scroll_offset();
        let generation = state.generation();

        state.reveal_entry_line(99, 0);

        assert_eq!(state.selected(), sel, "must not change selection");
        assert_eq!(state.scroll_offset(), scroll, "must not scroll");
        assert_eq!(state.generation(), generation, "must not bump generation");
    }

    #[test]
    fn reveal_biases_scroll_toward_matched_line() {
        let mut state = ScrollbackState::new();
        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("pre {i}")));
        }
        // A tall entry mid-list, so the line nudge isn't masked by max_offset.
        let tall_idx = state.len();
        let tall = (0..20)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.push_block(RenderBlock::user_prompt(tall));
        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("post {i}")));
        }
        state.prepare_layout(80, 10);

        state.reveal_entry_line(tall_idx, 0);
        let at_first_line = state.scroll_offset();
        state.reveal_entry_line(tall_idx, 8);
        let at_eighth_line = state.scroll_offset();
        assert!(
            at_eighth_line > at_first_line,
            "a later line within the entry scrolls further down"
        );

        // A line past the entry height clamps to the last row, not beyond.
        state.reveal_entry_line(tall_idx, 9_999);
        let clamped_a = state.scroll_offset();
        state.reveal_entry_line(tall_idx, 999_999);
        let clamped_b = state.scroll_offset();
        assert_eq!(
            clamped_a, clamped_b,
            "any line past the entry height clamps to the same row"
        );
    }

    #[test]
    fn reveal_scrolls_past_wrapped_rows_of_earlier_line() {
        let mut state = ScrollbackState::new();
        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("pre {i}")));
        }
        // A tall entry whose first logical line word-wraps across many rendered rows; its second logical line therefore begins well below row 1
        let tall_idx = state.len();
        let wrapped_first = "word ".repeat(80);
        state.push_block(RenderBlock::user_prompt(format!(
            "{wrapped_first}\nsecond\nthird"
        )));
        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("post {i}")));
        }
        // Narrow width forces the first logical line to wrap into several rows.
        state.prepare_layout(40, 10);

        state.reveal_entry_line(tall_idx, 0);
        let at_first_line = state.scroll_offset();
        state.reveal_entry_line(tall_idx, 1);
        let at_second_line = state.scroll_offset();

        // The second logical line sits past every wrapped row of the first, so the reveal scrolls more than a single row down
        // With the old logical-index nudge the delta would be exactly 1; mapping through the wrapped output makes it the wrapped-row count of line 0
        let delta = at_second_line.saturating_sub(at_first_line);
        assert!(
            delta > 1,
            "revealing the line after a wrapped line must scroll past its \
             wrapped rows (delta {delta} should exceed 1)"
        );
    }

    #[test]
    fn reveal_offsets_expanded_verb_head_past_group_header() {
        crate::appearance::cache::set_group_tool_verbs(true);
        let mut state = ScrollbackState::new();
        for i in 0..20 {
            state.push_block(RenderBlock::agent_message(format!("pre {i}")));
        }
        let head_idx = state.len();
        for i in 0..3 {
            state.push_block(RenderBlock::read(format!("f{i}.rs"), None));
        }
        for i in 0..20 {
            state.push_block(RenderBlock::agent_message(format!("post {i}")));
        }
        state.prepare_layout(80, 6);
        state.set_selected(Some(head_idx));
        assert!(state.toggle_group_expansion());
        state.prepare_layout(80, 6);
        assert!(
            state
                .layout_cache
                .as_ref()
                .and_then(|c| c.entries.get(head_idx))
                .is_some_and(|e| e.is_expanded_verb_header())
        );

        state.scroll_to_entry_center(head_idx);
        let header_row_offset = state.scroll_offset();
        state.reveal_entry_line(head_idx, 0);
        assert_eq!(
            state.scroll_offset(),
            header_row_offset + 1,
            "logical member row 0 starts below the synthetic group header"
        );
    }

    #[test]
    fn reveal_skips_thinking_header_rows_when_mapping_logical_line() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut state = ScrollbackState::new();
        // Expanded Thinking output prepends a non-selectable header and a blank row
        // The header is on by default; set it explicitly so the test stays valid if that default ever changes
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = true;
        state.set_appearance(appearance);

        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("pre {i}")));
        }
        // First reasoning line word-wraps across several rendered rows; a later line follows
        // The search index counts only the selectable body lines (`plain_text_from_output` skips the header)
        // Its logical line 0 is therefore the first reasoning line, not the header
        let think_idx = state.len();
        let wrapped_first = "reason ".repeat(60);
        state.push_block(RenderBlock::thinking(format!(
            "{wrapped_first}\n\nlast reasoning line"
        )));
        for i in 0..30 {
            state.push_block(RenderBlock::user_prompt(format!("post {i}")));
        }
        state.prepare_layout(40, 12);

        // Thinking entries default to Truncated, whose output starts with a "…" ellipsis row
        // Reveal expands the entry first, so measure the Expanded output reveal actually operates on
        // In Expanded mode the index's logical line 0 is the first reasoning line, not the header
        state
            .get_mut(think_idx)
            .expect("thinking entry exists")
            .set_display_mode(DisplayMode::Expanded);

        // Logical line 0 maps past the two non-selectable header rows
        // Thinking has no vpad, so the old "count every hard break" logic returned 0 here
        let row0 = state.rendered_row_offset_within_entry(think_idx, 0);
        assert!(
            row0 >= 2,
            "logical line 0 must skip the 2 non-selectable header rows, got {row0}"
        );
        // The next logical line begins past every wrapped row of line 0 (and the header), not one row after it
        let row1 = state.rendered_row_offset_within_entry(think_idx, 1);
        assert!(
            row1 > row0 + 1,
            "wrapped first line must push the next logical line well below it \
             (row0 {row0}, row1 {row1})"
        );

        // End to end: revealing the later line scrolls further than the first, confirming the reveal path honors the header-skipping wrapped mapping
        state.reveal_entry_line(think_idx, 0);
        let at_first = state.scroll_offset();
        state.reveal_entry_line(think_idx, 1);
        let at_second = state.scroll_offset();
        assert!(
            at_second.saturating_sub(at_first) > 1,
            "revealing the later reasoning line scrolls past the wrapped first line"
        );
    }

    #[test]
    fn reveal_skips_rebuild_for_already_visible_target() {
        let mut state = ScrollbackState::new();
        for i in 0..40 {
            state.push_block(RenderBlock::user_prompt(format!("line {i}")));
        }
        // One render settles the cache and clears dirty heights.
        state.prepare_layout(80, 10);
        assert!(state.layout_cache.is_some());
        assert!(state.dirty_heights.is_empty());

        let before = state.layout_rebuilds;
        // Plain prompts are neither foldable nor group-hidden, so repeated reveals change only the selection; the gate must skip rebuild_layout
        state.reveal_entry_line(39, 0);
        state.reveal_entry_line(20, 0);
        state.reveal_entry_line(39, 0);

        assert_eq!(
            state.layout_rebuilds, before,
            "revealing already-visible entries must not rebuild layout"
        );
        assert_eq!(state.selected(), Some(39), "selection still follows reveal");
        assert!(
            state.scroll_offset() > 0,
            "the view still scrolled to the bottom target"
        );
        // Stronger than scroll > 0: the revealed target actually lands on screen.
        assert!(
            state
                .entry_screen_area(39, Rect::new(0, 0, 80, 10))
                .is_some(),
            "revealed target must be within the visible viewport"
        );
    }

    #[test]
    fn reveal_rebuilds_once_when_expanding_collapsed_entry() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::execute_with_output(
            "cargo test",
            "line0\nline1\nline2",
            None::<String>,
        ));
        state.collapse_all();
        state.prepare_layout(80, 20);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode(),
            DisplayMode::Collapsed
        );

        let before = state.layout_rebuilds;
        state.reveal_entry_line(0, 0);

        assert_eq!(
            state.get_by_id(id).unwrap().display_mode(),
            DisplayMode::Expanded,
            "reveal expands the collapsed entry"
        );
        assert_eq!(
            state.layout_rebuilds,
            before + 1,
            "expanding a fold rebuilds exactly once"
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn reveal_rebuilds_when_heights_are_dirty() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut state = ScrollbackState::new();
        for i in 0..20 {
            state.push_block(RenderBlock::user_prompt(format!("pre {i}")));
        }
        let think_id = state.push_block(RenderBlock::thinking("reasoning"));
        for i in 0..20 {
            state.push_block(RenderBlock::user_prompt(format!("post {i}")));
        }
        state.prepare_layout(80, 10);
        assert!(
            state.dirty_heights.is_empty(),
            "a render settles dirty heights"
        );

        // Streaming content dirties a height but keeps the cache (incremental path), so the next reveal hits the stale-heights arm of the gate
        state.push_chunk_to_thinking(think_id, "\nmore reasoning\nand still more");
        assert!(state.layout_cache.is_some());
        assert!(
            !state.dirty_heights.is_empty(),
            "streaming left a dirty height without a render"
        );

        let before = state.layout_rebuilds;
        // Target a plain prompt so the rebuild is driven solely by the stale heights, not by a display-state change
        state.reveal_entry_line(0, 0);

        assert_eq!(
            state.layout_rebuilds,
            before + 1,
            "stale heights force a rebuild so the reveal lands correctly"
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn reveal_rebuilds_on_group_unhide_then_skips_when_visible() {
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        push_tool_calls(&mut state, 6);
        state.prepare_layout(80, 40);

        // Entry 1 starts hidden by group truncation.
        assert_eq!(cached_height_at(&state, 1), 0);

        let before = state.layout_rebuilds;
        state.reveal_entry_line(1, 0);

        // Un-hiding the truncated group is a display change, so exactly one rebuild
        assert_eq!(
            state.layout_rebuilds,
            before + 1,
            "un-hiding a truncated group rebuilds exactly once"
        );
        assert!(
            cached_height_at(&state, 1) > 0,
            "revealed entry must no longer be truncated"
        );

        // Re-revealing the now-visible entry changes nothing, so it must skip the rebuild
        let after_unhide = state.layout_rebuilds;
        state.reveal_entry_line(1, 0);
        assert_eq!(
            state.layout_rebuilds, after_unhide,
            "re-revealing the already-visible entry must not rebuild"
        );
    }

    #[test]
    fn reveal_unhides_truncated_group_on_cache_miss() {
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance.clone());
        let ids = push_tool_calls(&mut state, 6);
        state.prepare_layout(80, 40);

        // Entry 1 starts hidden by group truncation.
        assert_eq!(cached_height_at(&state, 1), 0);

        // Simulate a reveal that lands on a cache miss: set_appearance nulls the layout cache
        // is_entry_hidden then conservatively reports the truncated target as "visible"
        // Without the pre-unhide cache rebuild the unhide is skipped and the entry stays group-truncated after the reveal
        state.set_appearance(appearance);
        assert!(
            state.layout_cache.is_none(),
            "set_appearance must null the layout cache"
        );

        state.reveal_entry_line(1, 0);

        assert_eq!(state.selected(), Some(1));
        // The unhide block is reveal's only writer of expanded_groups, so the group's start id being present proves the unhide actually ran
        assert!(
            ids.first()
                .is_some_and(|id| state.expanded_groups.contains(id)),
            "reveal on a cache-miss must un-hide the truncated group"
        );
        assert!(
            cached_height_at(&state, 1) > 0,
            "revealed entry must be visible after a cache-miss reveal"
        );
    }

    fn multi_page_scrollback() -> ScrollbackState {
        let mut state = ScrollbackState::new();
        for i in 0..40 {
            state.push_block(agent_block(&format!("message {i}\nline two\nline three")));
        }
        state.prepare_layout(80, 10);
        state
    }

    #[test]
    fn page_up_selects_top_of_viewport() {
        let mut state = multi_page_scrollback();
        state.goto_bottom();
        let at_bottom = state.selected().expect("goto_bottom selects last");
        let offset_before = state.scroll_offset();

        state.page_up();
        state.prepare_layout(80, 10);

        assert!(state.scroll_offset() < offset_before, "viewport moved up");
        let sel = state.selected().expect("page_up selects an entry");
        assert!(sel < at_bottom, "selection moved up with the page");
        assert!(
            state.entry_overlaps_viewport(sel),
            "selection must stay in the new viewport"
        );
        // Top edge: no earlier selectable entry overlaps the viewport.
        for idx in 0..sel {
            assert!(
                !state.entry_overlaps_viewport(idx)
                    || !state
                        .entries
                        .get_index(idx)
                        .is_some_and(|(_, e)| e.block.is_selectable()),
                "entry {idx} is above the page-up selection"
            );
        }
    }

    #[test]
    fn page_down_selects_bottom_of_viewport() {
        let mut state = multi_page_scrollback();
        state.goto_top();
        let at_top = state.selected().expect("goto_top selects first");
        let offset_before = state.scroll_offset();

        state.page_down();
        state.prepare_layout(80, 10);

        assert!(state.scroll_offset() > offset_before, "viewport moved down");
        let sel = state.selected().expect("page_down selects an entry");
        assert!(sel > at_top, "selection moved down with the page");
        assert!(
            state.entry_overlaps_viewport(sel),
            "selection must stay in the new viewport"
        );
        // Bottom edge: no later selectable entry overlaps the viewport.
        for idx in (sel + 1)..state.len() {
            assert!(
                !state.entry_overlaps_viewport(idx)
                    || !state
                        .entries
                        .get_index(idx)
                        .is_some_and(|(_, e)| e.block.is_selectable()),
                "entry {idx} is below the page-down selection"
            );
        }
    }

    #[test]
    fn page_up_selection_matches_visual_top_row() {
        // Mixed heights: after paging, selection must be the entry covering the viewport top row (not merely the lowest-index overlap)
        let mut state = ScrollbackState::new();
        for i in 0..15 {
            state.push_block(agent_block(&format!("short {i}")));
        }
        state.push_block(tall_agent_block());
        for i in 0..15 {
            state.push_block(agent_block(&format!("after {i}")));
        }
        let vp = 8u16;
        state.prepare_layout(80, vp);
        state.goto_bottom();
        state.prepare_layout(80, vp);
        state.page_up();
        state.prepare_layout(80, vp);
        let sel = state.selected().expect("page_up selects");
        let (top, _) = state.viewport_virtual_bounds().unwrap();
        let top_entry = state.entry_at_virtual_row(top).unwrap();
        // Walk inward from the top edge past non-selectables (none here).
        assert_eq!(sel, top_entry, "page_up must select the visual top entry");
        assert!(state.entry_overlaps_viewport(sel));
    }

    #[test]
    fn page_up_then_select_next_does_not_teleport_to_old_selection() {
        let mut state = multi_page_scrollback();
        state.goto_bottom();
        let bottom = state.selected().unwrap();

        state.page_up();
        state.prepare_layout(80, 10);
        let after_page = state.selected().unwrap();
        assert!(after_page < bottom);

        let offset_after_page = state.scroll_offset();
        state.select_next();
        state.prepare_layout(80, 10);

        // Continues from the page-up edge, not the pre-page bottom entry.
        assert!(
            state.selected().unwrap() > after_page,
            "select_next advances from the page-up selection"
        );
        assert!(
            state.selected().unwrap() < bottom,
            "select_next must not jump back to the pre-page bottom entry"
        );
        // Viewport stays near the paged position (no ensure_selected_visible jump).
        let delta = state.scroll_offset().abs_diff(offset_after_page);
        assert!(
            delta <= state.viewport_height as usize,
            "viewport jumped {delta} rows after select_next (page offset was {offset_after_page})"
        );
    }

    /// Regression: a full page-down must not skip the lines that sit behind a sticky prompt header pinned at the viewport top.
    /// The content area is `viewport - header` rows, so a page that advances `viewport - 2` rows jumps `header - 2` lines over the top border.
    /// The page delta has to subtract the header height so the intended 2-row overlap is preserved.
    #[test]
    fn page_down_does_not_skip_lines_behind_sticky_header() {
        let mut h = ScrollTestHarness::new(80, 20);
        // A multi-line prompt so the pinned header is taller than the 2-row overlap
        // A single-line prompt renders as one row plus one gap, exactly the 2-row overlap, which would hide the bug
        // One very tall response follows so there is plenty of room to page through the middle without clamping at the bottom
        h.push_prompt("Q1 line A\nQ1 line B\nQ1 line C");
        let giant: String = (1..=300)
            .map(|i| format!("answer line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.push_agent(&giant);
        h.frame();

        // Start at the top, then page down until the prompt scrolls above the viewport and pins as a sticky header
        // One extra page lands on the stable (fully collapsed) header height
        h.state.goto_top();
        h.frame();
        let mut guard = 0;
        while h.state.current_header_screen_rows() == 0 {
            h.state.page_down();
            h.frame();
            guard += 1;
            assert!(
                guard < 100,
                "expected a sticky header to appear while paging"
            );
        }
        h.state.page_down();
        h.frame();

        // The header must be taller than the 2-row overlap
        // Otherwise the old `viewport - 2` delta would not have skipped anything and the test would not exercise the bug
        let header = h.state.current_header_screen_rows();
        assert!(
            header > 2,
            "test needs a header taller than the overlap to be meaningful, got {header}"
        );
        // There must be a full page of room left below, so the next page-down advances a whole page instead of clamping at the bottom
        assert!(
            h.state.scroll_offset + h.state.viewport_height as usize <= h.max_offset(),
            "test needs a full page of room to page down without clamping"
        );

        let old_bottom_line = h.state.scroll_offset + h.state.viewport_height as usize - 1;
        h.state.page_down();
        h.frame();
        let new_top_content = h.state.scroll_offset + h.state.current_header_screen_rows() as usize;

        assert!(
            new_top_content <= old_bottom_line + 1,
            "page-down skipped lines behind the sticky header: new top content line \
             {new_top_content} > old bottom line {old_bottom_line} + 1"
        );
    }

    /// (`render_with_sticky_headers` falls back to a zero-height layout because `use_sticky` is false.). The whole
    /// viewport is then content, so a page must advance `viewport - 2`. the header height must be gated on the same
    /// flag as the renderer.
    #[test]
    fn page_delta_ignores_header_when_sticky_headers_disabled() {
        let mut h = ScrollTestHarness::new(80, 20);
        // Same setup as the sticky-header test: a multi-line prompt that would pin a header taller than 2 rows when enabled
        // A long response follows with room to page through the middle without clamping at the bottom
        h.push_prompt("Q1 line A\nQ1 line B\nQ1 line C");
        let giant: String = (1..=300)
            .map(|i| format!("answer line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.push_agent(&giant);
        h.frame();

        // Page down (with sticky headers on, the harness default) until the prompt pins as a header
        // That lands on a scroll position where a header genuinely exists
        h.state.goto_top();
        h.frame();
        let mut guard = 0;
        while h.state.current_header_screen_rows() == 0 {
            h.state.page_down();
            h.frame();
            guard += 1;
            assert!(
                guard < 100,
                "expected a sticky header to appear while paging"
            );
        }
        h.state.page_down();
        h.frame();

        // Sanity: with sticky headers on, a header taller than 2 rows is measured here, so gating on the flag actually changes the result below
        let header_on = h.state.current_header_screen_rows();
        assert!(
            header_on > 2,
            "test needs a real header with sticky headers on, got {header_on}"
        );
        // A full page of room remains below, so a page-down will not clamp.
        assert!(
            h.state.scroll_offset + h.state.viewport_height as usize <= h.max_offset(),
            "test needs a full page of room to page down without clamping"
        );

        // Disable sticky headers, mirroring the renderer's `use_sticky` gate.
        // No header is drawn now, so none must be subtracted from the page.
        h.state.appearance.scrollback.display.sticky_headers = false;
        h.frame();

        assert_eq!(
            h.state.current_header_screen_rows(),
            0,
            "no header must be measured when sticky headers are disabled"
        );
        let expected = h.state.viewport_height - 2;
        assert_eq!(
            h.state.page_scroll_rows(),
            expected,
            "page delta must be viewport - 2 (no header subtracted) when sticky headers are off"
        );

        // The observable scroll delta of a page-down is a full viewport - 2.
        let before = h.state.scroll_offset;
        h.state.page_down();
        h.frame();
        assert_eq!(
            h.state.scroll_offset - before,
            expected as usize,
            "page-down should advance a full viewport - 2 with sticky headers off"
        );
    }

    #[test]
    fn response_top_above_tracks_the_answer_being_read() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(tall_agent_block()); // 1
        state.prepare_layout(80, 6);

        // Follow mode parks at the tail of the long answer: its first line is above the viewport, so the indicator has a jump to offer
        assert!(state.is_follow_mode());
        assert!(state.has_response_top_above());

        // Taking the jump (the indicator click) lands on the answer's top; from there there is nothing further up to jump to
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(1));
        assert!(!state.has_response_top_above());
    }

    #[test]
    fn response_top_above_is_false_for_short_answers_and_without_layout() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1"));
        state.push_block(agent_block("short answer"));

        // No layout yet: no viewport top to compare against.
        assert!(!state.has_response_top_above());

        // Fully visible answer: nothing above the viewport top.
        state.prepare_layout(80, 20);
        assert!(!state.has_response_top_above());
    }

    #[test]
    fn response_top_above_works_for_earlier_turns_too() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(tall_agent_block()); // 1
        state.push_block(user_block("Q2")); // 2
        state.push_block(tall_agent_block()); // 3
        state.prepare_layout(80, 6);

        // Park the viewport mid-way through turn 0's answer: the indicator is not reserved for the last turn
        state.goto_top();
        while !state.has_response_top_above() {
            let before = state.scroll_offset();
            state.scroll_down(1);
            assert_ne!(
                state.scroll_offset(),
                before,
                "hit the bottom without ever entering turn 0's answer"
            );
        }
        assert_eq!(state.active_turn_for_viewport(), Some(0));

        // The jump snaps to this answer's top, not the last one's
        assert!(state.prev_response());
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn response_top_above_is_false_while_the_answer_is_still_below() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1")); // 0
        state.push_block(tall_agent_block()); // 1
        state.push_block(user_block("Q2")); // 2
        for i in 0..8 {
            state.push_block(tool_block(&format!("tool {i}"))); // 3..=10
        }
        state.push_block(agent_block("A2")); // 11
        state.prepare_layout(80, 6);

        // Viewport top inside turn 1's tool run: turn 1's answer starts below the top
        // There is no "top of the response" to return to, even though turn 0's answer sits further up
        state.scroll_to_entry_top(8);
        assert_eq!(state.active_turn_for_viewport(), Some(1));
        assert!(!state.has_response_top_above());
    }
}
