//! Bottom padding, held for one turn, that makes the page-flip pose (the last user prompt at the top of the viewport) a real scroll bottom.
//! Without the pad, clamping the scroll offset to the content height would jump the view to the tail.

use super::ScrollbackState;

impl ScrollbackState {
    #[cfg(test)]
    pub(crate) fn is_pin_reserve_active(&self) -> bool {
        self.pin_reserve_active
    }

    #[cfg(test)]
    pub(crate) fn is_pin_reserve_after_turn(&self) -> bool {
        self.pin_reserve_after_turn
    }

    pub(super) fn arm_pin_reserve(&mut self) {
        self.pin_reserve_active = true;
        self.pin_reserve_after_turn = false;
        // Without a target, the release checks do nothing until positioning captures the pose
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
    }

    /// The armed turn is over, so height changes at finish and the "Worked for…" marker must not consume follow-preserve after this point.
    pub(crate) fn note_pin_reserve_turn_finished(&mut self) {
        if self.pin_reserve_active {
            self.pin_reserve_after_turn = true;
        }
    }

    fn pin_reserve_scroll_target(&self) -> Option<usize> {
        match (
            self.pin_reserve_target,
            self.pin_reserve_prompt_scroll_target(),
        ) {
            (Some(captured), Some(live)) => Some(captured.max(live)),
            (captured, live) => captured.or(live),
        }
    }

    /// Shift the captured pin pose by any height change ABOVE the pinned prompt.
    /// Growth above moves the prompt's virtual_y, so the captured offset (relative to the visible range top) must move with it.
    /// Changes at or below the prompt leave its offset put, so this is a no-op for ordinary streaming, where the response grows below the prompt.
    pub(super) fn shift_pin_reserve_target_for_changes(&mut self, changes: &[(usize, i32)]) {
        if !self.pin_reserve_active {
            return;
        }
        let Some(target) = self.pin_reserve_target else {
            return;
        };
        let Some(prompt_idx) = self.pin_reserve_prompt_index() else {
            return;
        };
        let start = self.visible_entry_range().start;
        let above: i64 = changes
            .iter()
            .filter(|&&(idx, _)| idx >= start && idx < prompt_idx)
            .map(|&(_, d)| d as i64)
            .sum();
        if above != 0 {
            let shifted = (target as i64 + above).max(0) as usize;
            self.pin_reserve_target = Some(shifted);
            if self.follow_mode && self.follow_preserve_scroll {
                self.scroll_offset = (self.scroll_offset as i64 + above).max(0) as usize;
            }
        }
    }

    /// Shift the captured pose by a coordinate change in the retained layout cache.
    pub(super) fn shift_pin_reserve_target_for_layout(
        &mut self,
        before: Option<usize>,
        after: Option<usize>,
    ) {
        if !self.pin_reserve_active {
            return;
        }
        let (Some(before), Some(after), Some(target)) = (before, after, self.pin_reserve_target)
        else {
            return;
        };
        let delta = after as i64 - before as i64;
        self.pin_reserve_target = Some((target as i64 + delta).max(0) as usize);
        if self.follow_mode && self.follow_preserve_scroll {
            self.scroll_offset = (self.scroll_offset as i64 + delta).max(0) as usize;
        }
    }

    /// Replace the captured pose after changing to a different view coordinate space.
    pub(super) fn reset_pin_reserve_target(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        let Some(prompt_idx) = self.pin_reserve_prompt_index() else {
            return;
        };
        let range = self.visible_entry_range();
        if !range.contains(&prompt_idx) || self.last_width == 0 {
            return;
        }
        self.measure_span_and_rebuild(range.start, prompt_idx, self.last_width);
        if let Some(target) = self.pin_reserve_prompt_scroll_target() {
            self.pin_reserve_target = Some(target);
        }
    }

    /// Synchronize the owned page-flip pose after a structural rebuild.
    pub(super) fn settle_pin_reserve_target(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        for _ in 0..2 {
            let Some(target) = self.pin_reserve_prompt_scroll_target() else {
                return;
            };
            self.pin_reserve_target = Some(target);
            if self.follow_mode && self.follow_preserve_scroll {
                self.scroll_offset = target;
            }
            self.compute_total_height_from_cache();
        }
        if !self.follow_mode {
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
    }

    /// Drop reserve ownership when its captured prompt is outside the current visible slice.
    pub(super) fn release_pin_reserve_outside_view(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        let outside = self
            .pin_reserve_prompt_index()
            .is_none_or(|idx| !self.visible_entry_range().contains(&idx));
        if outside {
            self.follow_preserve_scroll = false;
            self.release_pin_reserve();
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
    }

    /// Clear the reserve's flags, target, and prompt id without changing scroll totals.
    pub(super) fn clear_pin_reserve(&mut self) {
        self.pin_reserve_active = false;
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
        self.pin_reserve_after_turn = false;
    }

    /// Index of the prompt the pin targets.
    /// Resolves the stable id captured at arm time so a mid-turn interjection cannot move it.
    /// Falls back to the last user prompt only when no id is stored (e.g. when a resize re-derives the target).
    pub(super) fn pin_reserve_prompt_index(&self) -> Option<usize> {
        match self.pin_reserve_prompt_id {
            Some(id) => self.entries.get_index_of(&id),
            None => self.last_user_prompt_index(),
        }
    }

    /// Release the reserve before an explicit bottom gesture resolves the real tail.
    pub(super) fn release_pin_reserve(&mut self) {
        if !self.pin_reserve_active && self.pin_reserve_pad == 0 {
            return;
        }
        self.clear_pin_reserve();
        if self.layout_cache.is_some() {
            self.compute_total_height_from_cache();
        } else {
            self.total_height = self.total_height.saturating_sub(self.pin_reserve_pad);
            self.pin_reserve_pad = 0;
        }
    }

    pub(super) fn pin_reserve_pad_rows(&self, content_height: usize) -> usize {
        if !self.pin_reserve_active || self.viewport_height == 0 {
            return 0;
        }
        let Some(target) = self.pin_reserve_scroll_target() else {
            return 0;
        };
        // max_offset = content + pad - viewport must be at least `target` so the last user prompt can sit at the top
        target
            .saturating_add(self.viewport_height as usize)
            .saturating_sub(content_height)
    }

    /// Scroll target of the prompt the pin is armed for, adjusted for sticky headers.
    /// Resolving by the captured id keeps a mid-turn interjection or a resize re-derive from retargeting the pad to a later prompt.
    /// Falls back to the last user prompt only when no id is stored.
    pub(super) fn pin_reserve_prompt_scroll_target(&self) -> Option<usize> {
        let idx = self.pin_reserve_prompt_index()?;
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if !range.contains(&idx) {
            return None;
        }
        let base = *cache.virtual_y.get(range.start)?;
        let y = *cache.virtual_y.get(idx)?;
        let entry_y = y.saturating_sub(base);
        // Same sticky-header fixed point as `scroll_to_entry_top`
        // Raw `entry_y` would over-state the pad whenever an earlier prompt is still sticky
        // That would make max_offset > scroll_offset and consume follow-preserve on the next frame
        Some(self.sticky_adjusted_entry_top(cache, &range, entry_y))
    }

    fn last_user_prompt_index(&self) -> Option<usize> {
        if let Some(idx) = self.turns.last().map(|turn| turn.prompt_index)
            && self
                .entries
                .get_index(idx)
                .is_some_and(|(_, entry)| entry.block.is_user_prompt())
        {
            return Some(idx);
        }
        self.entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, (_, entry))| entry.block.is_user_prompt().then_some(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::super::{ScrollbackState, ViewMode};
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::types::DisplayMode;

    fn tall_history_then_prompt() -> (ScrollbackState, usize) {
        let mut state = ScrollbackState::new();
        for i in 0..30 {
            state.push_block(agent_block(&format!("filler line {i}")));
        }
        state.push_block(user_block("next question"));
        let prompt_idx = state.len() - 1;
        state.prepare_layout(80, 8);
        (state, prompt_idx)
    }

    fn two_turn_state() -> (ScrollbackState, usize, usize) {
        let mut state = ScrollbackState::new();
        for i in 0..20 {
            state.push_block(agent_block(&format!("history {i}")));
        }
        state.push_block(user_block("first prompt"));
        let first_prompt = state.len() - 1;
        state.push_block(agent_block("first answer"));
        state.push_block(user_block("second prompt"));
        let second_prompt = state.len() - 1;
        state.push_block(agent_block("second answer"));
        (state, first_prompt, second_prompt)
    }

    fn wrapping_history_then_prompt(width: u16) -> (ScrollbackState, usize) {
        use crate::appearance::AppearanceConfig;

        let mut state = ScrollbackState::new();
        state.set_appearance(AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        });
        state.begin_batch();
        for i in 0..80 {
            state.push_block(agent_block(&format!(
                "msg{i} aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff"
            )));
        }
        state.push_block(user_block("next question"));
        state.end_batch();
        let prompt_idx = state.len() - 1;
        state.prepare_layout(width, 8);
        (state, prompt_idx)
    }

    #[test]
    fn page_flip_small_scroll_does_not_collapse_pad() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        let pin = state.scroll_offset();
        let (_, vh, total) = state.scroll_info();
        assert_eq!(
            pin,
            total.saturating_sub(vh as usize),
            "pin pose must be a real bottom once the pad is in total_height"
        );

        state.scroll_up(2);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        assert_eq!(
            state.scroll_offset(),
            pin.saturating_sub(2),
            "a small scroll must not clamp to the unpadded tail"
        );
    }

    #[test]
    fn page_flip_scroll_down_at_pin_does_not_jump() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let pin = state.scroll_offset();

        state.scroll_up(2);
        state.scroll_down(2);
        assert!(!state.is_follow_mode());
        state.scroll_down(3);
        state.prepare_layout(80, 8);
        assert_eq!(state.scroll_offset(), pin);
        assert!(state.is_pin_reserve_active());
        assert!(
            !state.is_follow_mode(),
            "wheel residuals at the padded bottom must not re-engage follow"
        );

        for _ in 0..20 {
            if state.pin_reserve_pad == 0 {
                break;
            }
            state.push_block(tall_agent_block());
            state.prepare_layout(80, 8);
        }
        assert_eq!(
            state.pin_reserve_pad, 0,
            "response growth must consume the padding"
        );
        state.scroll_down(u16::MAX);
        assert!(!state.is_follow_mode(), "first event reaches the new tail");
        state.note_pin_reserve_turn_finished();
        state.scroll_down(1);
        assert!(
            state.is_follow_mode(),
            "overscroll must re-engage follow after streaming consumes the padding"
        );
        assert!(
            !state.is_follow_preserve_scroll(),
            "re-entering follow after turn completion must release preserve"
        );
        assert!(!state.is_pin_reserve_active());
    }

    #[test]
    fn page_flip_padding_survives_scrolling_away_and_back() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        let initial_pin = state.scroll_offset();

        let mut target = initial_pin;
        for _ in 0..80 {
            state.page_up();
            state.prepare_layout(20, 8);
            assert!(
                state.is_pin_reserve_active(),
                "scrolling into history must keep the page-flip padding"
            );
            target = state
                .pin_reserve_prompt_scroll_target()
                .expect("measured prompt target");
            if target != initial_pin || state.scroll_offset() == 0 {
                break;
            }
        }
        assert_ne!(
            target, initial_pin,
            "settling wrapped history must move the prompt target"
        );
        state.set_scroll_offset(usize::MAX);
        state.prepare_layout(20, 8);
        assert_eq!(
            state.scroll_offset(),
            target,
            "scrolling back down must restore the measured prompt-at-top position"
        );
        assert!(state.is_pin_reserve_active());
    }

    #[test]
    fn direct_scroll_away_keeps_page_flip_padding() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let pin = state.scroll_offset();

        state.set_scroll_offset(0);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());

        state.set_scroll_offset(pin);
        state.prepare_layout(80, 8);
        assert_eq!(state.scroll_offset(), pin);
        assert!(state.is_pin_reserve_active());
    }

    #[test]
    fn page_flip_scroll_after_turn_complete_does_not_jump() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);

        let think_id = state.push_block(RenderBlock::thinking("line1"));
        if let Some(entry) = state.entries.get_mut(&think_id) {
            entry.is_running = true;
            entry.set_display_mode(DisplayMode::Truncated);
        }
        state.running.insert(think_id);
        state.prepare_layout(80, 8);
        for i in 0..8 {
            state.push_chunk_to_thinking(think_id, &format!("\nline{}", i + 2));
            state.prepare_layout(80, 8);
        }

        state.scroll_up(2);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        let mid = state.scroll_offset();

        state.finish_running(think_id);
        state.note_pin_reserve_turn_finished();
        state.push_block(RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(std::time::Duration::from_secs(1)),
            },
        ));
        state.prepare_layout(80, 8);
        assert!(
            state.is_pin_reserve_active(),
            "completing the turn must not drop the pad"
        );
        assert_eq!(
            state.scroll_offset(),
            mid,
            "finish must not snap a midstream scroll to the tail"
        );

        state.scroll_up(2);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        assert_eq!(
            state.scroll_offset(),
            mid.saturating_sub(2),
            "scroll after complete must not clamp to the unpadded tail"
        );
    }

    #[test]
    fn page_flip_stays_pinned_when_short_answer_finishes_idle() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let pin = state.scroll_offset();

        let think_id = state.push_block(RenderBlock::thinking("line1"));
        if let Some(entry) = state.entries.get_mut(&think_id) {
            entry.is_running = true;
            entry.set_display_mode(DisplayMode::Truncated);
        }
        state.running.insert(think_id);
        state.prepare_layout(80, 8);
        state.push_chunk_to_thinking(think_id, "\nline2");
        state.prepare_layout(80, 8);
        assert!(state.is_follow_preserve_scroll());
        assert!(state.is_pin_reserve_active());

        state.finish_running(think_id);
        state.note_pin_reserve_turn_finished();
        state.push_block(RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(std::time::Duration::from_secs(2)),
            },
        ));
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        assert_eq!(
            state.scroll_offset(),
            pin,
            "an idle finish must keep the last user prompt at the top"
        );
    }

    #[test]
    fn post_turn_visibility_invalidation_cannot_strand_viewport_in_padding() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);

        let think_id = state.push_block(RenderBlock::thinking(
            (0..20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        state.prepare_layout(80, 8);
        state.finish_running(think_id);
        state.note_pin_reserve_turn_finished();
        state.prepare_layout(80, 8);

        crate::appearance::cache::set_show_thinking_blocks(false);
        state.invalidate_heights();
        state.prepare_layout(80, 8);

        assert!(state.scroll_offset <= state.max_scroll_offset());
        assert!(
            !state.is_follow_preserve_scroll() || state.is_pin_reserve_active(),
            "preserve cannot outlive the reserve after a global height invalidation"
        );
        crate::appearance::cache::set_show_thinking_blocks(true);
    }

    #[test]
    fn post_turn_visibility_shrink_above_prompt_uses_settled_target() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let (mut state, prompt_idx) = tall_history_then_prompt();
        let prompt_id = *state.entries.get_index(prompt_idx).expect("prompt entry").0;
        state.insert_block_before(
            prompt_id,
            RenderBlock::thinking(
                (0..20)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );
        state.follow_new_turn(Some(prompt_idx + 1), true);
        state.prepare_layout(80, 8);
        state.note_pin_reserve_turn_finished();

        crate::appearance::cache::set_show_thinking_blocks(false);
        state.invalidate_heights();
        state.prepare_layout(80, 8);

        assert!(state.is_pin_reserve_active());
        assert!(state.is_follow_preserve_scroll());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("settled prompt target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset, target);
        crate::appearance::cache::set_show_thinking_blocks(true);
    }

    #[test]
    fn manual_visibility_invalidation_clamps_after_target_shrinks() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let (mut state, prompt_idx) = tall_history_then_prompt();
        let prompt_id = *state.entries.get_index(prompt_idx).expect("prompt entry").0;
        state.insert_block_before(
            prompt_id,
            RenderBlock::thinking(
                (0..20)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );
        state.follow_new_turn(Some(prompt_idx + 1), true);
        state.prepare_layout(80, 8);
        state.scroll_up(1);
        state.set_scroll_offset(state.max_scroll_offset());

        crate::appearance::cache::set_show_thinking_blocks(false);
        state.invalidate_heights();
        state.prepare_layout(80, 8);

        assert!(!state.is_follow_mode());
        assert!(state.scroll_offset <= state.max_scroll_offset());
        crate::appearance::cache::set_show_thinking_blocks(true);
    }

    #[test]
    fn page_flip_arms_while_scrolled_up_despite_stale_target() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        // Simulate arming while reading history with a stale prior target.
        state.scroll_offset = 0;
        state.follow_mode = false;
        state.pin_reserve_target = Some(999);

        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        assert!(
            state.is_pin_reserve_active(),
            "arming while scrolled up must not disarm the reserve"
        );
        let pin = state.scroll_offset();
        let (_, vh, total) = state.scroll_info();
        assert_eq!(
            pin,
            total.saturating_sub(vh as usize),
            "pin pose must be a real bottom, not an offset past the unpadded tail"
        );

        state.scroll_up(2);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        assert_eq!(
            state.scroll_offset(),
            pin.saturating_sub(2),
            "a scroll after arming-while-scrolled-up must not clamp to the tail"
        );
    }

    #[test]
    fn reserve_padding_uses_captured_exact_target() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        let live = state
            .pin_reserve_prompt_scroll_target()
            .expect("live prompt target");
        let content = state.total_height.saturating_sub(state.pin_reserve_pad);
        state.pin_reserve_target = Some(live + 10);

        assert_eq!(
            state.pin_reserve_pad_rows(content),
            live.saturating_add(10)
                .saturating_add(state.viewport_height as usize)
                .saturating_sub(content),
            "estimate-only geometry must not replace the captured exact target"
        );
    }

    #[test]
    fn shift_pin_reserve_target_tracks_only_above_prompt_changes() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let base = state.pin_reserve_target.expect("armed pose");
        let start = state.visible_entry_range().start;

        state.shift_pin_reserve_target_for_changes(&[(prompt_idx, 5), (prompt_idx + 1, 9)]);
        assert_eq!(
            state.pin_reserve_target,
            Some(base),
            "below-prompt change is a no-op"
        );

        state.shift_pin_reserve_target_for_changes(&[(start, 3)]);
        assert_eq!(
            state.pin_reserve_target,
            Some(base + 3),
            "above-prompt growth shifts down"
        );
        assert_eq!(
            state.scroll_offset,
            base + 3,
            "the viewport follows the pinned prompt's new row"
        );

        state.shift_pin_reserve_target_for_changes(&[(start, -2)]);
        assert_eq!(
            state.pin_reserve_target,
            Some(base + 1),
            "above-prompt shrink shifts up"
        );
        assert_eq!(
            state.scroll_offset,
            base + 1,
            "the viewport remains aligned with the shifted prompt"
        );
    }

    #[test]
    fn page_flip_tracks_height_changes_above_prompt() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let before = state.scroll_offset;
        let entry_id = *state.entries.get_index(0).expect("history entry").0;
        let Some(before_height) = state
            .layout_cache
            .as_ref()
            .expect("layout cache")
            .entries
            .first()
            .map(|e| e.height)
        else {
            panic!("expected a cached history entry");
        };
        {
            let entry = state.entry_mut(0).expect("history entry");
            entry.block = tall_agent_block();
            entry.invalidate_cache();
        }
        state.dirty_heights.insert(entry_id);
        state.prepare_layout(80, 8);

        let Some(after_height) = state
            .layout_cache
            .as_ref()
            .expect("layout cache")
            .entries
            .first()
            .map(|e| e.height)
        else {
            panic!("expected a cached history entry");
        };
        assert!(
            after_height > before_height,
            "fixture must grow above the prompt"
        );
        assert!(state.scroll_offset > before);
        assert_eq!(
            state.scroll_offset,
            state
                .pin_reserve_prompt_scroll_target()
                .expect("prompt target"),
            "the viewport must remain aligned with the prompt after content above it grows"
        );
    }

    #[test]
    fn structural_height_change_above_prompt_shifts_target_once() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let before = state.pin_reserve_target.expect("captured target");
        let entry_id = *state.entries.get_index(0).expect("history entry").0;
        let Some(old_height) = state
            .layout_cache
            .as_ref()
            .expect("layout")
            .entries
            .first()
            .map(|e| e.height)
        else {
            panic!("expected a cached history entry");
        };
        {
            let entry = state.entry_mut(0).expect("history entry");
            entry.block = tall_agent_block();
            entry.invalidate_cache();
        }
        state.dirty_heights.insert(entry_id);
        state.gaps_may_be_dirty = true;

        state.prepare_layout(80, 8);

        let Some(new_height) = state
            .layout_cache
            .as_ref()
            .expect("layout")
            .entries
            .first()
            .map(|e| e.height)
        else {
            panic!("expected a cached history entry");
        };
        let delta = new_height as usize - old_height as usize;
        assert!(delta > 0, "fixture must grow above the prompt");
        assert_eq!(state.pin_reserve_target, Some(before + delta));
    }

    #[test]
    fn streaming_fast_path_keeps_pin_while_reading_history() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        state.scroll_up(2);
        let entry_id = *state.entries.get_index(0).expect("history entry").0;
        {
            let entry = state.entry_mut(0).expect("history entry");
            entry.block = tall_agent_block();
            entry.invalidate_cache();
        }
        state.dirty_heights.insert(entry_id);

        state.prepare_layout(80, 8);

        assert!(state.is_pin_reserve_active());
        assert!(state.pin_reserve_pad > 0);
    }

    #[test]
    fn removed_captured_prompt_releases_reserve_without_retargeting() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let prompt_id = state.pin_reserve_prompt_id.expect("captured prompt id");
        state.push_block(user_block("later prompt"));

        assert!(state.remove_entry(prompt_id));
        state.prepare_layout(80, 8);

        assert!(!state.is_pin_reserve_active());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.pin_reserve_pad, 0);
        assert_eq!(state.scroll_offset, state.max_scroll_offset());
    }

    #[test]
    fn removed_captured_prompt_clears_reserve_while_scrolled_away() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let prompt_id = state.pin_reserve_prompt_id.expect("captured prompt id");
        state.scroll_up(5);
        let reading = state.scroll_offset;

        assert!(state.remove_entry(prompt_id));
        state.prepare_layout(80, 8);

        assert!(!state.is_pin_reserve_active());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.pin_reserve_pad, 0);
        assert_eq!(state.scroll_offset, reading.min(state.max_scroll_offset()));
    }

    #[test]
    fn pin_reserve_prompt_index_survives_interjection() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);

        // An interjection becomes the last user prompt but must not retarget the reserve.
        state.push_block(user_block("interjection"));
        let interjection_idx = state.len() - 1;
        state.turns.clear();

        assert_eq!(
            state.last_user_prompt_index(),
            Some(interjection_idx),
            "pre-fix boundary would follow the interjection"
        );
        assert_eq!(
            state.pin_reserve_prompt_index(),
            Some(prompt_idx),
            "the pin tracks its armed prompt via the captured id, not the last user prompt"
        );
    }

    #[test]
    fn page_flip_resize_refreshes_target_after_exact_measurement() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(80);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        let before = state.pin_reserve_target.expect("wide target");

        state.prepare_layout(20, 8);

        assert!(state.is_pin_reserve_active());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("exact narrow target");
        assert!(target > before, "narrow wrapping must move the target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset(), target);
        let (_, vh, total) = state.scroll_info();
        assert_eq!(target, total.saturating_sub(vh as usize));
    }

    #[test]
    fn resize_preserves_synthetic_turn_viewport() {
        let (mut state, _) = wrapping_history_then_prompt(20);
        state.goto_bottom();
        state.scroll_up(30);
        state.follow_new_turn(None, false);
        assert!(!state.is_pin_reserve_active());
        let kept_offset = state.scroll_offset;

        state.prepare_layout(80, 8);

        assert!(state.is_follow_preserve_scroll());
        assert!(!state.is_pin_reserve_active());
        assert_eq!(
            state.scroll_offset,
            kept_offset.min(state.max_scroll_offset())
        );
    }

    #[test]
    fn height_only_resize_clamps_synthetic_turn_viewport() {
        let (mut state, _) = wrapping_history_then_prompt(20);
        state.goto_bottom();
        state.scroll_up(5);
        state.follow_new_turn(None, false);
        assert!(!state.is_pin_reserve_active());

        state.prepare_layout(20, 30);

        assert!(state.is_follow_preserve_scroll());
        assert!(!state.is_pin_reserve_active());
        assert!(state.scroll_offset <= state.max_scroll_offset());
    }

    #[test]
    fn same_width_rebuild_preserves_synthetic_turn_viewport() {
        let (mut state, _) = wrapping_history_then_prompt(20);
        state.goto_bottom();
        state.scroll_up(30);
        let reading = state.scroll_offset;
        state.follow_new_turn(None, false);
        assert!(!state.is_pin_reserve_active());

        state.invalidate_layout_cache();
        state.prepare_layout(20, 8);

        assert!(state.is_follow_preserve_scroll());
        assert_eq!(state.scroll_offset, reading);
    }

    #[test]
    fn new_prompt_page_flip_uses_current_single_turn_coordinates() {
        let (mut state, _first_prompt, second_prompt) = two_turn_state();
        state.prepare_layout(80, 8);
        state.set_selected(Some(0));
        state.set_view_mode(ViewMode::SingleTurn);

        state.follow_new_turn(Some(second_prompt), true);

        assert_eq!(state.current_turn, Some(1));
        assert!(state.is_pin_reserve_active());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("new-turn target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset, target);
    }

    #[test]
    fn single_turn_view_rebases_owned_page_flip_reserve() {
        let (mut state, _first_prompt, second_prompt) = two_turn_state();
        state.prepare_layout(80, 8);
        state.follow_new_turn(Some(second_prompt), true);
        state.set_selected(Some(second_prompt));
        state.set_view_mode(ViewMode::SingleTurn);

        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("single-turn target");
        assert!(state.is_pin_reserve_active());
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset, target);
        assert_eq!(state.max_scroll_offset(), target);
    }

    #[test]
    fn single_turn_view_releases_foreign_page_flip_reserve() {
        let (mut state, first_prompt, second_prompt) = two_turn_state();
        state.prepare_layout(80, 8);
        state.follow_new_turn(Some(first_prompt), true);
        state.set_selected(Some(second_prompt));
        state.set_view_mode(ViewMode::SingleTurn);

        state.prepare_layout(80, 8);

        assert!(!state.is_pin_reserve_active());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.pin_reserve_pad, 0);
    }

    #[test]
    fn single_turn_foreign_reserve_releases_with_width_change() {
        let (mut state, first_prompt, second_prompt) = two_turn_state();
        state.prepare_layout(80, 8);
        state.follow_new_turn(Some(first_prompt), true);
        state.set_selected(Some(second_prompt));
        state.set_view_mode(ViewMode::SingleTurn);

        state.prepare_layout(40, 8);

        assert!(!state.is_pin_reserve_active());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.pin_reserve_pad, 0);
    }

    #[test]
    fn single_turn_foreign_reserve_releases_with_dirty_height() {
        let (mut state, first_prompt, second_prompt) = two_turn_state();
        state.prepare_layout(80, 8);
        state.follow_new_turn(Some(first_prompt), true);
        state.set_selected(Some(second_prompt));
        state.set_view_mode(ViewMode::SingleTurn);
        let dirty_id = *state
            .entries
            .get_index(second_prompt)
            .expect("second prompt")
            .0;
        state.dirty_heights.insert(dirty_id);

        state.prepare_layout(80, 8);

        assert!(!state.is_pin_reserve_active());
        assert!(!state.is_follow_preserve_scroll());
        assert_eq!(state.pin_reserve_pad, 0);
    }

    #[test]
    fn same_width_rebuild_keeps_settled_prompt_target() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        let first_id = *state.entries.get_index(0).expect("history entry").0;

        assert!(state.remove_entry(first_id));
        state.prepare_layout(20, 8);

        assert!(state.is_pin_reserve_active());
        assert!(state.is_follow_preserve_scroll());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("settled prompt target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset, target);
    }

    #[test]
    fn same_width_rebuild_updates_target_while_scrolled_away() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        state.scroll_up(10);
        let (top_idx, rows_into_span) = state.viewport_top_anchor_point().expect("viewport anchor");
        let top_id = *state.entries.get_index(top_idx).expect("top entry").0;
        let first_id = *state.entries.get_index(0).expect("history entry").0;

        assert!(state.remove_entry(first_id));
        state.prepare_layout(20, 8);

        assert!(state.is_pin_reserve_active());
        assert!(!state.is_follow_mode());
        let (new_top_idx, new_rows_into_span) =
            state.viewport_top_anchor_point().expect("restored anchor");
        assert_eq!(
            *state.entries.get_index(new_top_idx).expect("top entry").0,
            top_id
        );
        assert_eq!(new_rows_into_span, rows_into_span);
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("settled prompt target");
        assert_eq!(state.pin_reserve_target, Some(target));
    }

    #[test]
    fn targeted_dirty_resize_uses_new_width_target() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        let entry_id = *state.entries.get_index(0).expect("history entry").0;
        state.dirty_heights.insert(entry_id);
        state.layout_cache = None;

        state.prepare_layout(80, 8);

        assert!(state.is_pin_reserve_active());
        assert!(state.is_follow_preserve_scroll());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("new-width target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.scroll_offset, target);
    }

    #[test]
    fn resize_while_reading_clamps_after_target_shrinks() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);
        state.scroll_up(1);
        state.set_scroll_offset(state.max_scroll_offset());

        state.prepare_layout(80, 8);

        assert!(!state.is_follow_mode());
        assert!(state.scroll_offset <= state.max_scroll_offset());
    }

    #[test]
    fn resize_reset_uses_exact_new_width_target() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(20);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(20, 8);

        state.prepare_layout(80, 8);

        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("exact new-width target");
        assert_eq!(state.pin_reserve_target, Some(target));
        assert_eq!(state.max_scroll_offset(), target);
    }

    #[test]
    fn resize_while_reading_history_refreshes_pin_without_moving_viewport() {
        let (mut state, prompt_idx) = wrapping_history_then_prompt(80);
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        state.goto_top();

        state.prepare_layout(20, 8);

        assert_eq!(state.scroll_offset(), 0);
        assert!(state.is_pin_reserve_active());
        let target = state
            .pin_reserve_prompt_scroll_target()
            .expect("exact narrow target");
        assert_eq!(state.pin_reserve_target, Some(target));
        let (_, vh, total) = state.scroll_info();
        assert_eq!(target, total.saturating_sub(vh as usize));

        state.set_scroll_offset(usize::MAX);
        state.prepare_layout(20, 8);
        assert_eq!(state.scroll_offset(), target);
        assert!(state.is_pin_reserve_active());
    }

    #[test]
    fn goto_bottom_releases_pin_reserve() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());
        let pinned = state.scroll_offset();

        state.goto_bottom();
        assert!(
            !state.is_pin_reserve_active(),
            "an explicit bottom gesture must drop the page-flip pin"
        );
        let (_, vh, total) = state.scroll_info();
        assert_eq!(
            state.scroll_offset(),
            total.saturating_sub(vh as usize),
            "End must land on the real (unpadded) tail"
        );
        assert!(
            state.scroll_offset() < pinned,
            "the released tail sits below the padded pin pose"
        );
    }

    #[test]
    fn clear_drops_pin_reserve() {
        let (mut state, prompt_idx) = tall_history_then_prompt();
        state.follow_new_turn(Some(prompt_idx), true);
        state.prepare_layout(80, 8);
        assert!(state.is_pin_reserve_active());

        state.clear();
        assert!(!state.is_pin_reserve_active());
        state.push_block(user_block("reopened"));
        state.push_block(agent_block("answer"));
        state.prepare_layout(80, 8);
        assert!(!state.is_pin_reserve_active());
        let (_, vh, total) = state.scroll_info();
        assert!(
            total <= vh as usize,
            "reopening must not keep leftover bottom pad (total={total}, vh={vh})"
        );
    }
}
