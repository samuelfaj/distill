//! ScrollbackState: unified state for the v3 scrollback pane.
//!
//! This combines entries, scroll position, selection, and turn-based navigation into a single state object.

pub mod groups;
mod layout;
mod nav;
mod pin_reserve;
mod selection;
mod timeline;
mod types;
pub mod verb_group;

pub(crate) use layout::ScrollAnchor;
pub use layout::compute_paint_window;
pub use timeline::TimelineEntry;
pub use types::*;

use layout::{LayoutCache, StructuralScrollAnchor};

use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::layout::Rect;

use super::block::{BlockContent, RenderBlock};
use super::blocks::tool::{EditToolCallBlock, ToolCallBlock};
use super::entry::{EntryId, ScrollbackEntry};
use super::layout::HorizontalLayout;
use super::selection::SelectionBox;
use super::sticky::{PromptDescriptor, StickyHeaderLayout, compute_sticky_layout};
use super::types::DisplayMode;
use super::wrappers::EntryRenderer;
use crate::appearance::AppearanceConfig;
use crate::render::Renderable;
use crate::theme::Theme;

/// Lifecycle of a scroll-up warm-up that a resize postponed until the width settles.
/// Settling is measured in frames, not `prepare_layout` calls.
/// One frame prepares layout several times, so a call-based rule would run the warm-up during the very resize that deferred it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DeferredWarmAbove {
    #[default]
    Idle,
    Deferred,
    Armed,
}

/// Unified scrollback state for the v3 pager.
#[derive(Debug)]
pub struct ScrollbackState {
    // Content
    /// All entries in the scrollback, keyed by EntryId for O(1) lookup.
    /// IndexMap preserves insertion order for rendering.
    entries: IndexMap<EntryId, ScrollbackEntry>,

    /// Next entry ID to assign.
    next_id: u64,

    /// Set of currently running entry IDs.
    /// Used for O(running) iteration in tick_running().
    running: HashSet<EntryId>,

    /// Entry IDs whose finish-flash (accent stays bright for [`FINISH_FLASH_DURATION_MS`] after completion) may still be active.
    /// Lets `tick()` check O(flashing) recently-finished entries instead of scanning every entry's `finished_at` on every animation tick.
    /// Pushed by `finish_running_with_time`, drained by `tick()` on expiry.
    flashing: Vec<EntryId>,

    /// Set of entry IDs with potentially stale cached heights.
    /// Used for incremental layout updates: only these entries need height recomputation.
    dirty_heights: HashSet<EntryId>,

    /// Minimal mode only: entry IDs already emitted into the terminal's native scrollback. Keyed by `EntryId` (not a
    /// per-entry flag) so it survives `shift_remove` / `remove_from` reordering for free. A positional index would be
    /// stranded by a below-cursor removal. Empty in the alt-screen / inline modes, which never commit.
    committed: HashSet<EntryId>,

    /// Edits this session opened for a permission prompt. Only these refold when the prompt ends.
    permission_opened: HashSet<EntryId>,

    /// Minimal mode only: lowest entry index that *might* be uncommitted (not yet printed into native scrollback). A
    /// lower-bound perf hint so the per-frame commit pass is O(new) rather than O(history). Contract for every mutation
    /// that shifts entry positions: the cursor must never end up *above* an uncommitted entry's index.
    commit_scan_cursor: usize,

    /// Minimal mode only: a bounded ring of entry IDs committed to native scrollback while folded (collapsed reasoning,
    /// truncated tool output). (Committed terminal text can't be mutated, so expansion is a re-print.). Bounded so a
    /// long session never grows it without limit.
    commit_expand_ring: VecDeque<EntryId>,
    // Scroll. `usize` (not `u16`): a long session can render well past 65 535 rows, so the cumulative scroll position
    // must match `virtual_y` (`Vec<usize>`).
    scroll_offset: usize,

    /// Total content height (cached, updated on render).
    ///
    /// `usize` for the same reason as `scroll_offset`: the summed height of a long session can exceed `u16::MAX`.
    total_height: usize,

    /// Viewport height (set on render).
    /// Stays `u16`; a terminal is never 65 535 rows tall.
    viewport_height: u16,

    /// Whether auto-scroll is enabled (follow new content).
    follow_mode: bool,

    /// When true, handle_follow_mode skips the scroll-to-bottom on the first call, preserving a scroll position set by scroll_to_entry_top.
    /// Cleared after one use.
    /// This lets dispatch_send_prompt position the prompt at the viewport top while still enabling follow for new content.
    follow_preserve_scroll: bool,

    /// Content generation captured when follow-preserve is armed, so synthetic turns distinguish appended output from geometry-only rebuilds.
    follow_preserve_content_generation: u64,

    /// Extra rows under the latest user prompt so a page-flip can keep that prompt at the top.
    /// Independent of follow mode and viewport position so scrolling through history does not
    /// make the page-flip position unreachable. Dropped on an explicit bottom gesture or reset.
    pin_reserve_active: bool,

    /// Rows currently added to `total_height` by [`Self::pin_reserve_active`].
    /// Kept beside the flag so follow-preserve can still detect real overflow against the unpadded content height.
    pin_reserve_pad: usize,

    /// Scroll offset of the page-flip pin, captured when the reserve is armed.
    /// Re-deriving it from the last user prompt after a finish-time rebuild can disagree with the pose we scrolled to and drop the pad.
    /// (Finish-time rebuilds: thinking collapse, the "Worked for…" marker, a group fold.)
    pin_reserve_target: Option<usize>,

    /// Stable id of the prompt the pin targets, captured when armed.
    /// The pin tracks this specific prompt, not "the last user prompt".
    /// A mid-turn interjection therefore cannot move the above/below boundary the pad shift uses.
    pin_reserve_prompt_id: Option<EntryId>,

    /// The turn that armed the pin has finished.
    /// Midstream overflow still chases the tail; after this, a remeasure or terminal marker must not.
    pin_reserve_after_turn: bool,

    // Selection
    /// Currently selected entry index.
    selected: Option<usize>,

    /// Selection box to be rendered by the frame (computed during render).
    /// This is stored here so the frame can render it after the scrollback pane.
    selection_box: Option<SelectionBox>,

    // Turns
    /// Detected turns in the conversation.
    turns: Vec<Turn>,

    /// Index of the currently viewed turn.
    current_turn: Option<usize>,

    /// View mode: all turns or single turn.
    view_mode: ViewMode,

    // Cache. Width change invalidates the entire layout cache and triggers a full recompute of entry heights. Resize
    // events are debounced at the event-loop level so only the final width triggers a rebuild.
    last_width: u16,

    /// Layout cache for navigation (entry heights, prompt descriptors).
    layout_cache: Option<LayoutCache>,

    /// One-shot viewport-top anchor armed by a structural entry mutation (removal/insertion) just before it invalidates the layout cache.
    /// Consumed by the next `prepare_layout`; see [`StructuralScrollAnchor`].
    structural_scroll_anchor: Option<StructuralScrollAnchor>,

    // Sticky modes. Display mode applied to thinking blocks when they finish running. Defaults to `Collapsed`
    // (auto-collapse on finish). Toggled by `expand_all_thinking()` (Ctrl+E) between `Expanded` and `Collapsed`.
    thinking_display_mode: DisplayMode,

    // Animation
    /// Frame tick counter for animations (increments each render tick).
    tick: u64,

    // Appearance
    /// Current appearance configuration (hot-reloadable).
    appearance: AppearanceConfig,

    // Batching
    /// When > 0, `push()` skips `rebuild_turns()` and `invalidate_layout_cache()`.
    /// Call `begin_batch()` before bulk insertions and `end_batch()` after.
    batch_depth: u32,

    /// True when gap_after values may need recomputation (display_mode changed, entries added/removed).
    /// Streaming content mutations (`push_chunk_to_*`) leave this false, enabling an O(1) incremental virtual_y patch instead of an O(n) full rebuild.
    gaps_may_be_dirty: bool,

    /// A synchronous settings rebuild populated the cache, but the next frame must still run the full reserve lifecycle.
    full_settlement_pending: bool,

    warm_above: DeferredWarmAbove,

    /// Last observed [`ffmpeg_available`](crate::inline_media_ffmpeg::ffmpeg_available).
    /// A false-to-true flip (user installs ffmpeg mid-session) must rebuild the layout so reserved heights match the now-full-size posters.
    /// Otherwise a poster paints over the text below its (still banner-sized) reservation.
    ffmpeg_available_snapshot: bool,

    /// Set of group-start EntryIds whose group has been manually expanded by the user (pressing l/Enter on the group header).
    /// When a group's first entry ID is in this set, the fold pass (`groups::apply`) marks its span expanded instead of hiding entries.
    expanded_groups: HashSet<EntryId>,

    // Link map
    /// Monotonically increasing counter, bumped when visible link positions or policy inputs change.
    /// Used by `VisibleLinkMap::is_stale()` to skip rebuilds.
    generation: u64,

    /// Bumped only when entries are added or removed or an entry's content changes.
    /// Never bumped on display toggles (fold/raw/group), appearance, scroll, or viewport.
    /// A stable invalidation key for content-derived caches that must survive view changes, unlike `generation`.
    content_generation: u64,

    /// Test-only count of full `rebuild_layout` calls, so reveal tests can assert the fast path skips the O(history) rebuild for visible matches.
    #[cfg(test)]
    layout_rebuilds: usize,

    /// Height override for the inline-edited entry: measurement reports this instead of the block's natural height.
    /// The layout then reserves room for the live edit textarea.
    /// Cleared when editing ends.
    pub(super) inline_edit_height: Option<(EntryId, u16)>,

    /// Session/worktree cwd (`AgentSession.cwd`) for Expanded tool paths.
    cwd: Option<std::path::PathBuf>,
}

impl Default for ScrollbackState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollbackState {
    /// Create a new empty state.
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
            next_id: 1, // Start at 1 so 0 can be a sentinel
            running: HashSet::new(),
            flashing: Vec::new(),
            dirty_heights: HashSet::new(),
            committed: HashSet::new(),
            permission_opened: HashSet::new(),
            commit_scan_cursor: 0,
            commit_expand_ring: VecDeque::new(),
            scroll_offset: 0,
            total_height: 0,
            viewport_height: 0,
            follow_mode: true,
            follow_preserve_scroll: false,
            follow_preserve_content_generation: 0,
            pin_reserve_active: false,
            pin_reserve_pad: 0,
            pin_reserve_target: None,
            pin_reserve_prompt_id: None,
            pin_reserve_after_turn: false,
            selected: None,
            selection_box: None,
            turns: Vec::new(),
            current_turn: None,
            view_mode: ViewMode::AllTurns,
            last_width: 0,
            layout_cache: None,
            structural_scroll_anchor: None,
            thinking_display_mode: DisplayMode::Collapsed,
            tick: 0,
            appearance: AppearanceConfig::default(),
            batch_depth: 0,
            gaps_may_be_dirty: false,
            full_settlement_pending: false,
            warm_above: DeferredWarmAbove::Idle,
            ffmpeg_available_snapshot: false,
            expanded_groups: HashSet::new(),
            generation: 0,
            content_generation: 0,
            #[cfg(test)]
            layout_rebuilds: 0,
            inline_edit_height: None,
            cwd: None,
        }
    }

    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Update session cwd; invalidates cwd-dependent paint, layout, and link maps.
    pub fn set_cwd(&mut self, cwd: Option<std::path::PathBuf>) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.dirty_heights = self.entries.keys().copied().collect();
        self.layout_cache = None;
        self.gaps_may_be_dirty = true;
        self.bump_generation();
    }

    /// Create empty continuation state with shared appearance, preferences, and `EntryId` space.
    /// Reconnect replay uses it while pre-outage content is stashed; cross-swap ids may dangle but cannot alias.
    /// Generations advance so equality-cached consumers observe the content swap.
    pub fn fresh_continuation(&self) -> Self {
        let mut fresh = Self::new();
        fresh.next_id = self.next_id;
        fresh.appearance = self.appearance.clone();
        fresh.thinking_display_mode = self.thinking_display_mode;
        fresh.view_mode = self.view_mode;
        fresh.follow_mode = self.follow_mode;
        fresh.cwd = self.cwd.clone();
        fresh.generation = self.generation.wrapping_add(1);
        fresh.content_generation = self.content_generation.wrapping_add(1);
        fresh
    }

    /// Lowest `EntryId` value a future [`push`](Self::push) may assign.
    pub(crate) fn id_floor(&self) -> u64 {
        self.next_id
    }

    /// Ensure future `EntryId`s are allocated at or above `floor`.
    /// Called when a stashed state is swapped back in after a [`fresh_continuation`](Self::fresh_continuation) sibling allocated ids.
    /// Ids handed out by the discarded sibling are then never reused.
    pub(crate) fn raise_id_floor(&mut self, floor: u64) {
        self.next_id = self.next_id.max(floor);
    }

    /// Advance the invalidation generations strictly past a discarded [`fresh_continuation`](Self::fresh_continuation) sibling's.
    /// Caches keyed on counter equality (link map, search index) that last saw the sibling then cannot mistake this state for it after a restore swap.
    pub(crate) fn raise_invalidation_floor(&mut self, sibling: (u64, u64)) {
        self.generation = self.generation.max(sibling.0);
        self.content_generation = self.content_generation.max(sibling.1);
        self.bump_content_generation();
    }

    /// The invalidation-generation pair, for [`Self::raise_invalidation_floor`].
    pub(crate) fn invalidation_generations(&self) -> (u64, u64) {
        (self.generation, self.content_generation)
    }

    /// Whether a [`begin_batch`](Self::begin_batch) is currently open.
    pub(crate) fn in_batch(&self) -> bool {
        self.batch_depth > 0
    }

    /// Append all entries from `tail` (a `fresh_continuation` sibling of this state) after the existing content. Used
    /// by the cursor-found reconnect reload: nothing was replayed, so the pre-outage transcript is kept. Only the
    /// post-cursor live tail that accumulated in the staging state is attached below it.
    pub(crate) fn append_entries_from(&mut self, tail: ScrollbackState) {
        debug_assert!(
            tail.next_id >= self.next_id,
            "append_entries_from requires a fresh_continuation sibling (shared id space)"
        );
        self.entries.extend(tail.entries);
        self.running.extend(tail.running);
        self.dirty_heights.extend(tail.dirty_heights);
        // Carry the tail's committed frontier: with a per-entry flag this traveled with the entry
        // As an id-set it must be merged explicitly so already-committed tail blocks are not re-emitted after the reload
        self.committed.extend(tail.committed);
        self.permission_opened.extend(tail.permission_opened);
        self.expanded_groups.extend(tail.expanded_groups);
        self.next_id = self.next_id.max(tail.next_id);
        // The tail (live during the window) is what equality-cached consumers last saw; the merged state must read as newer than both halves
        self.generation = self.generation.max(tail.generation);
        self.content_generation = self.content_generation.max(tail.content_generation);
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
    }

    /// Update the appearance configuration.
    pub fn set_appearance(&mut self, appearance: AppearanceConfig) {
        crate::render::bidi::set_enabled(appearance.scrollback.display.rtl_bidi);
        self.appearance = appearance;
        // Invalidate caches since appearance affects rendering
        self.layout_cache = None;
        self.gaps_may_be_dirty = true;
        // Mark all entries as having dirty heights
        self.dirty_heights = self.entries.keys().copied().collect();
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.bump_generation();
    }

    /// Remeasure every entry when a process-wide visibility flag flips (e.g. show thinking blocks) without changing `AppearanceConfig`.
    /// Refreshes the cache synchronously for settings callers and leaves full-dirty state for the next frame's reserve lifecycle settlement.
    pub fn invalidate_heights(&mut self) {
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.rebuild_layout();
        self.dirty_heights = self.entries.keys().copied().collect();
        self.gaps_may_be_dirty = true;
        self.full_settlement_pending = true;
        self.bump_generation();
    }

    /// Mark entry `id` structurally dirty: height re-measure plus gap/fold recompute on the next `prepare_layout`.
    /// For in-place block swaps that can change fold membership (e.g. a tool refinement changing its verb-group kind).
    /// A plain cache invalidation is not enough there.
    pub fn mark_structurally_dirty(&mut self, id: EntryId) {
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;
    }

    /// Get current appearance config.
    pub fn appearance(&self) -> &AppearanceConfig {
        &self.appearance
    }

    /// Current animation tick value (for spinner frame selection, etc.).
    pub fn animation_tick(&self) -> u64 {
        self.tick
    }

    /// A redraw is requested only when an animated entry (running wave accent or unexpired finish-flash) is actually
    /// inside the viewport window. Redrawing an otherwise static screen at ~30fps is pure waste: the frame diff would
    /// be empty.
    pub fn tick(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);

        let mut needs_redraw = !self.running.is_empty() && self.any_running_in_viewport();

        // Finish-flash: O(flashing) over recently-finished entries, not O(entries) over the whole scrollback. Emit one
        // final redraw when a flash expires so the accent repaints in its static state. Otherwise the last-painted bright
        // frame would linger until the next event.
        if !self.flashing.is_empty() {
            let flash_dur = FINISH_FLASH_DURATION_MS as u128;
            let mut still_flashing = std::mem::take(&mut self.flashing);
            still_flashing.retain(|id| {
                let Some(idx) = self.entries.get_index_of(id) else {
                    // The entry was removed (rewind/clear); nothing to repaint
                    return false;
                };
                let active = self
                    .entries
                    .get_index(idx)
                    .and_then(|(_, e)| e.finished_at)
                    .is_some_and(|t| t.elapsed().as_millis() < flash_dur);
                // Redraw while the flash animates and once when it expires, but only if the entry can be seen
                if self.entry_index_in_viewport(idx) {
                    needs_redraw = true;
                }
                active
            });
            self.flashing = still_flashing;
        }

        needs_redraw
    }

    /// Whether any entry is still marked running (visible or not).
    pub fn has_running_entries(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether any running entry is inside the current viewport window.
    /// Conservative: with no layout yet (before the first draw), every entry counts as visible.
    fn any_running_in_viewport(&self) -> bool {
        self.running.iter().any(|id| {
            self.entries
                .get_index_of(id)
                .is_some_and(|idx| self.entry_index_in_viewport(idx))
        })
    }

    /// Get the current tick counter (for animation synchronization).
    pub fn tick_count(&self) -> u64 {
        self.tick
    }

    /// Check if animation ticks are needed. Off-screen running entries don't need ticks. Finish-flashes deliberately do
    /// not demand ticks: they animate opportunistically while ticks flow for other reasons.
    pub fn needs_animation(&self) -> bool {
        !self.running.is_empty() && self.any_running_in_viewport()
    }

    /// Get the current animation tick value.
    pub fn current_tick(&self) -> u64 {
        self.tick
    }

    // Link map generation

    /// Current link-map generation.
    /// Incremented when positions or link-policy inputs change and invalidate the visible link map.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Bump the generation counter (marks the current `VisibleLinkMap` stale).
    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Generation that moves only when the entry set or an entry's content changes.
    /// It does not move on fold/raw/group display toggles, appearance, scroll, or viewport.
    /// Stable invalidation key for content-derived caches (e.g. a search index) that must not rebuild on view changes, unlike `generation`.
    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    /// Bump `content_generation` (and `generation` with it, since content changes also move visible links).
    /// Display-only changes (fold/raw/group), appearance, scroll, and viewport call `bump_generation` directly so `content_generation` stays put.
    fn bump_content_generation(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
        self.bump_generation();
    }
    // Batching

    /// Begin a batch of insertions.
    /// While batching, `push()` skips `rebuild_turns()` and `invalidate_layout_cache()`.
    /// Call `end_batch()` when done to run them once.
    pub fn begin_batch(&mut self) {
        self.batch_depth += 1;
    }

    /// End a batch.
    /// Runs the deferred `rebuild_turns()` and `invalidate_layout_cache()` once for all insertions.
    pub fn end_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth == 0 {
            self.rebuild_turns();
            self.invalidate_layout_cache();
        }
    }

    // Content Management

    /// Add an entry, assigning it a unique ID.
    ///
    /// Returns the assigned EntryId which can be used to access this entry later.
    pub fn push(&mut self, entry: ScrollbackEntry) -> EntryId {
        let id = EntryId::new(self.next_id);
        self.next_id += 1;

        let mut entry = entry;
        entry.id = id;

        self.apply_edit_default_display_mode(&mut entry);

        // Track if this entry is running
        if entry.is_running {
            self.running.insert(id);
        }

        self.entries.insert(id, entry);
        if self.batch_depth == 0 {
            self.rebuild_turns();
            // Try to extend the cache incrementally. If extension isn't possible (no cache yet, or cache out of sync), fall
            // back to the full invalidation.
            let new_idx = self.entries.len() - 1;
            if !self.extend_layout_cache_with_new_entry(new_idx) {
                self.gaps_may_be_dirty = true;
                self.invalidate_layout_cache();
            } else if self.appearance.scrollback.display.group_max_visible > 0
                || crate::appearance::cache::load_group_tool_verbs()
            {
                // A groupable, collapsed new entry may extend a group past the truncation threshold, or start or grow a foldable verb-group run
                // The verb fold is gated on `group_tool_verbs`, independent of the truncation threshold
                // Mark both dirty so prepare_layout Case 2 fires (Case 3 doesn't check gaps_may_be_dirty)
                if let Some((_, new_e)) = self.entries.get_index(new_idx)
                    && new_e.block.is_groupable()
                    && new_e.display_mode == DisplayMode::Collapsed
                {
                    self.gaps_may_be_dirty = true;
                    self.dirty_heights.insert(id);
                }
            }
            // Successful extend: gaps were updated inline, so gaps_may_be_dirty deliberately stays unset
            // Setting it would force the next streaming chunk's Case 2 path to do a full virtual_y rebuild
        } else {
            // In batch mode: defer to end_batch's full rebuild for safety.
            // (The cache is bulk-rebuilt once when the batch ends.)
            self.gaps_may_be_dirty = true;
            self.layout_cache = None;
        }
        self.bump_content_generation();
        id
    }

    /// Add a block (convenience wrapper).
    ///
    /// Returns the assigned EntryId.
    pub fn push_block(&mut self, block: RenderBlock) -> EntryId {
        self.push(ScrollbackEntry::new(block))
    }

    /// Add a finalized block positioned immediately before the entry `anchor`, instead of at the end. `anchor` must not
    /// already be committed. A terminal's native scrollback is append-only. Inserting above a block already printed
    /// there would emit the new block below content that logically follows it.
    pub fn insert_block_before(&mut self, anchor: EntryId, block: RenderBlock) -> EntryId {
        let Some(index) = self.entries.get_index_of(&anchor) else {
            return self.push_block(block);
        };
        debug_assert!(
            !self.committed.contains(&anchor),
            "insert_block_before: anchor {anchor:?} is already committed — the inserted \
             block would print out of order in native scrollback"
        );

        // Anchor the viewport top before the insertion shifts indices.
        self.arm_structural_scroll_anchor();
        let id = EntryId::new(self.next_id);
        self.next_id += 1;
        let mut entry = ScrollbackEntry::new(block);
        entry.id = id;
        self.apply_edit_default_display_mode(&mut entry);
        if entry.is_running {
            self.running.insert(id);
        }
        self.entries.shift_insert(index, id, entry);

        if let Some(selected) = self.selected.as_mut()
            && *selected >= index
        {
            *selected += 1;
        }
        self.commit_scan_cursor = self.commit_scan_cursor.min(index);

        if self.batch_depth == 0 {
            self.rebuild_turns();
        }
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        id
    }

    /// Fresh Edit entries at the block's Collapsed default adopt the state-owned materialize policy.
    /// An explicit non-Collapsed mode survives.
    /// An explicit Collapsed is indistinguishable from the default and may be upgraded by the effective expanded default.
    fn apply_edit_default_display_mode(&self, entry: &mut ScrollbackEntry) {
        if let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block
            && entry.display_mode == DisplayMode::Collapsed
        {
            entry.display_mode = edit_default_display_mode(
                self.appearance
                    .scrollback
                    .blocks
                    .edit
                    .effective_expanded(crate::appearance::cache::load_collapsed_edit_blocks()),
                edit,
            );
        }
    }

    /// Remove an entry by EntryId. No-op if the id is not present. Used by the cancel-with-restore flow to undo the
    /// user prompt block that was pushed at turn start. Returns `true` if an entry was removed.
    pub fn remove_entry(&mut self, id: EntryId) -> bool {
        // Capture the index before the removal shifts everything after it down.
        let Some(removed_index) = self.entries.get_index_of(&id) else {
            return false;
        };
        // Anchor the viewport top before the removal shifts indices, then re-point it at the survivor if this removal deleted its entry
        self.arm_structural_scroll_anchor();
        self.entries.shift_remove(&id);
        self.migrate_structural_anchor_past_removal(id, removed_index);
        self.running.remove(&id);
        self.dirty_heights.remove(&id);
        self.committed.remove(&id);
        self.permission_opened.remove(&id);
        self.expanded_groups.remove(&id);
        if let Some(sel) = self.selected
            && sel >= self.entries.len()
        {
            self.selected = self.entries.len().checked_sub(1);
        }
        // Clamping alone is not enough here; see the cursor's contract
        if removed_index < self.commit_scan_cursor {
            self.commit_scan_cursor -= 1;
        }
        self.commit_scan_cursor = self.commit_scan_cursor.min(self.entries.len());
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        true
    }

    pub fn remove_from(&mut self, index: usize) -> Vec<ScrollbackEntry> {
        // Anchor a viewport parked above the cut; pruned below if the anchored entry itself is in the removed tail
        self.arm_structural_scroll_anchor();
        let mut removed = Vec::new();
        while self.entries.len() > index {
            if let Some((id, entry)) = self.entries.pop() {
                self.running.remove(&id);
                self.dirty_heights.remove(&id);
                self.committed.remove(&id);
                self.permission_opened.remove(&id);
                self.expanded_groups.remove(&id);
                removed.push(entry);
            }
        }
        removed.reverse();
        self.prune_dead_structural_anchor();
        if let Some(sel) = self.selected
            && sel >= self.entries.len()
        {
            self.selected = self.entries.len().checked_sub(1);
        }
        // Clamp the minimal-mode commit cursor: `remove_from` (rewind / trailing auth-error strip) pops the tail
        // That could otherwise leave the cursor past the end and silently skip future commits
        self.commit_scan_cursor = self.commit_scan_cursor.min(self.entries.len());
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        removed
    }

    /// Preferred streaming append: writes the chunk, invalidates the render cache, and marks height dirty.
    /// Returns false if the entry is missing or is not an agent message.
    pub fn push_chunk_to_agent(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::AgentMessage(ref mut msg) = entry.block
        {
            msg.push_chunk(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a chunk to an agent message entry without rendering markdown yet.
    pub fn push_chunk_to_agent_deferred(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::AgentMessage(ref mut msg) = entry.block
        {
            msg.push_chunk_deferred(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push streaming output to an execute block entry. Returns true if successful, false if the entry doesn't exist or
    /// isn't an execute block.
    pub fn set_execute_output(&mut self, id: EntryId, output: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::ToolCall(ToolCallBlock::Execute(ref mut exec)) = entry.block
        {
            // Replace output entirely: grok-shell sends the full accumulated buffer each tick
            // The shell now sends clean output (no ANSI codes) when the client sets x.ai/bashOutputNoColor: true, so no stripping is needed
            exec.output = Some(output.to_string());
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a text chunk to a thinking block entry.
    /// Similar to `push_chunk_to_agent()`, this handles all necessary cache invalidation for streaming thinking content.
    /// Returns true if successful, false if the entry doesn't exist or isn't a thinking block.
    pub fn push_chunk_to_thinking(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::Thinking(ref mut block) = entry.block
        {
            block.push_chunk(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a chunk to a thinking entry without rendering markdown yet.
    pub fn push_chunk_to_thinking_deferred(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::Thinking(ref mut block) = entry.block
        {
            block.push_chunk_deferred(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Append incremental output delta to an execute tool call entry. Used when the shell sends incremental
    /// `output_delta` instead of full buffers. Delegates to `push_chunk_to_execute` which handles cache invalidation.
    /// Returns true if successful, false if the entry doesn't exist or isn't an execute block.
    pub fn append_execute_output(&mut self, id: EntryId, delta: &str) -> bool {
        self.push_chunk_to_execute(id, delta)
    }

    /// Push an output chunk to an execute tool call entry.
    /// Similar to `push_chunk_to_agent()`, this handles all necessary cache invalidation for streaming command output.
    /// Returns true if successful, false if the entry doesn't exist or isn't an execute block.
    pub fn push_chunk_to_execute(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::ToolCall(ToolCallBlock::Execute(ref mut block)) = entry.block
        {
            block.push_output(chunk);
            entry.invalidate_cache();
            // Always mark dirty: word wrap may change line count even for same-line appends
            // HashSet dedup makes this cheap when called repeatedly.
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Mark an entry's height as dirty, requiring recomputation on next prepare_layout().
    ///
    /// Use this when you modify an entry's content directly (e.g., via get_by_id_mut()) and the change might affect its rendered height.
    pub fn mark_height_dirty(&mut self, id: EntryId) {
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;
        self.bump_content_generation();
    }

    /// Set (or clear) the inline-edit height override for an entry.
    /// Marks affected entries height-dirty; no-op when unchanged (called per frame).
    pub fn set_inline_edit_height(&mut self, override_h: Option<(EntryId, u16)>) {
        if self.inline_edit_height == override_h {
            return;
        }
        if let Some((old_id, _)) = self.inline_edit_height {
            self.mark_height_dirty(old_id);
        }
        if let Some((new_id, _)) = override_h {
            self.mark_height_dirty(new_id);
        }
        self.inline_edit_height = override_h;
    }

    /// Current inline-edit height override, if any.
    pub fn inline_edit_height(&self) -> Option<(EntryId, u16)> {
        self.inline_edit_height
    }

    /// Get number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.running.clear();
        self.flashing.clear();
        self.dirty_heights.clear();
        self.committed.clear();
        self.permission_opened.clear();
        self.expanded_groups.clear();
        // Note: we don't reset next_id to avoid ID reuse
        self.selected = None;
        self.turns.clear();
        self.current_turn = None;
        self.scroll_offset = 0;
        self.pin_reserve_active = false;
        self.pin_reserve_pad = 0;
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
        self.pin_reserve_after_turn = false;
        self.commit_scan_cursor = 0;
        self.commit_expand_ring.clear();
        self.invalidate_layout_cache();
        self.bump_content_generation();
    }

    // These support `crate::minimal`'s commit pipeline (print finalized blocks into native scrollback)
    // The authoritative state is the `committed` id-set; `commit_scan_cursor` is only a lower-bound hint to keep the per-frame scan O(new)
    // Reached from the minimal crate via `minimal_api::{is_committed, mark_committed, commit_scan_cursor, …}`

    /// Lowest entry index that may still be uncommitted (minimal-mode hint).
    pub(crate) fn commit_scan_cursor(&self) -> usize {
        self.commit_scan_cursor
    }

    /// Advance the commit scan cursor, clamped to the current entry count.
    pub(crate) fn set_commit_scan_cursor(&mut self, cursor: usize) {
        self.commit_scan_cursor = cursor.min(self.entries.len());
    }

    /// Whether the entry `id` was already emitted into native scrollback.
    pub(crate) fn is_committed(&self, id: EntryId) -> bool {
        self.committed.contains(&id)
    }

    /// Mark the entry at `index` as committed to native scrollback.
    /// No-op if the index is out of range.
    pub(crate) fn mark_committed(&mut self, index: usize) {
        if let Some((&id, _)) = self.entries.get_index(index) {
            self.committed.insert(id);
        }
    }

    /// Maximum number of folded-commit IDs retained for `Ctrl+E` / `/expand`.
    const EXPAND_RING_CAP: usize = 256;

    /// Record that the entry `id` was committed to native scrollback in a folded display mode (collapsed reasoning / truncated tool output).
    /// `Ctrl+E` / `/expand` can then re-print it in full.
    /// Bounded: the oldest entry is dropped once the ring is full.
    pub(crate) fn record_committed_for_expand(&mut self, id: EntryId) {
        self.commit_expand_ring.push_back(id);
        while self.commit_expand_ring.len() > Self::EXPAND_RING_CAP {
            self.commit_expand_ring.pop_front();
        }
    }

    /// Pop the most-recently committed folded entry whose entry still exists, for `Ctrl+E` / `/expand` to re-print fully.
    /// Returns `None` when nothing folded remains to expand.
    /// Stale IDs (entries removed by rewind / clear) are skipped.
    pub(crate) fn take_expandable_committed(&mut self) -> Option<EntryId> {
        while let Some(id) = self.commit_expand_ring.pop_back() {
            if self.entries.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    /// Get entry by index.
    pub fn get(&self, index: usize) -> Option<&ScrollbackEntry> {
        self.entries.get_index(index).map(|(_, v)| v)
    }

    /// Get entry by index mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut ScrollbackEntry> {
        self.entries.get_index_mut(index).map(|(_, v)| v)
    }

    /// Get the last entry.
    pub fn last(&self) -> Option<&ScrollbackEntry> {
        self.entries.last().map(|(_, v)| v)
    }

    #[cfg(test)]
    pub(crate) fn session_events(&self) -> Vec<super::blocks::SessionEvent> {
        self.entries
            .values()
            .filter_map(|entry| match &entry.block {
                RenderBlock::SessionEvent(block) => Some(block.event.clone()),
                _ => None,
            })
            .collect()
    }

    /// Get the last entry mutably.
    pub fn last_mut(&mut self) -> Option<&mut ScrollbackEntry> {
        self.entries.last_mut().map(|(_, v)| v)
    }

    /// Mark the last entry as running.
    /// When entering running state (`running = true`), also starts timing on tool call blocks via `ToolCallBlock::start_timing()`.
    /// This is the single point where block timing begins; constructors default to `started_at = None`.
    pub fn set_last_running(&mut self, running: bool) {
        if let Some(id) = self.entries.last().map(|(_, entry)| entry.id) {
            self.set_entry_running(id, running);
        }
    }

    pub fn set_entry_running(&mut self, id: EntryId, running: bool) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let was_running = entry.is_running;
        entry.is_running = running;
        entry.invalidate_cache();
        if running && !was_running {
            if let RenderBlock::ToolCall(ref mut tc) = entry.block {
                tc.start_timing();
            }
            self.running.insert(entry.id);
        } else if !running && was_running {
            self.running.remove(&entry.id);
        }
    }

    /// Get entry by ID.
    /// O(1) average via IndexMap.
    /// Returns None if the entry doesn't exist (was removed or ID is invalid).
    pub fn get_by_id(&self, id: EntryId) -> Option<&ScrollbackEntry> {
        self.entries.get(&id)
    }

    /// Get entry by ID mutably.
    /// O(1) average via IndexMap.
    /// Returns None if the entry doesn't exist (was removed or ID is invalid).
    pub fn get_by_id_mut(&mut self, id: EntryId) -> Option<&mut ScrollbackEntry> {
        self.entries.get_mut(&id)
    }

    /// The escalation fires only on that rising edge, so a user's collapse of an already-untrusted block sticks. A row
    /// already awaiting a permission then opens once if the new Edit has hunks, unless the fold is pinned. Stamps
    /// `started_at` on the new block and invalidates the entry's render cache.
    pub(crate) fn replace_tool_block(
        &mut self,
        entry_id: EntryId,
        mut block: RenderBlock,
        started_at: Option<Instant>,
    ) -> bool {
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        let expanded_by_default = self
            .appearance
            .scrollback
            .blocks
            .edit
            .effective_expanded(crate::appearance::cache::load_collapsed_edit_blocks());
        let Some(entry) = self.entries.get_mut(&entry_id) else {
            return false;
        };
        if let RenderBlock::ToolCall(new_tc) = &mut block
            && let Some(t) = started_at
        {
            new_tc.set_started_at(t);
        }
        let kind_changed = verb_group::verb_group_kind_changed(&entry.block, &block);
        let (is_same_kind, untrusted_rising) = match (&entry.block, &block) {
            (
                RenderBlock::ToolCall(ToolCallBlock::Edit(old)),
                RenderBlock::ToolCall(ToolCallBlock::Edit(new)),
            ) => (
                true,
                new.is_success() && new.summary_untrusted && !old.summary_untrusted,
            ),
            (RenderBlock::ToolCall(old), RenderBlock::ToolCall(new)) => (
                std::mem::discriminant(old) == std::mem::discriminant(new),
                false,
            ),
            _ => (false, false),
        };
        entry.block = block;
        if is_same_kind {
            if untrusted_rising && !(respect_manual_folds && entry.display_mode_pinned) {
                entry.display_mode = DisplayMode::Expanded;
            }
        } else if respect_manual_folds && entry.display_mode_pinned {
            tracing::debug!(
                entry_id = entry_id.value(),
                would_be_mode = ?entry.block.default_display_mode(),
                "scrollback.finish.pin_suppressed_override"
            );
        } else {
            entry.display_mode = match &entry.block {
                RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) => {
                    edit_default_display_mode(expanded_by_default, edit)
                }
                block => block.default_display_mode(),
            };
        }
        let pending = entry.is_pending_user_input;
        entry.invalidate_cache();
        if kind_changed {
            self.mark_structurally_dirty(entry_id);
        }
        // The first pending mark often lands on the eager Other placeholder, before
        // hunks exist. Open once when that Edit arrives, unless the user pinned a fold.
        if pending {
            self.open_permission_edit(entry_id);
        }
        true
    }

    /// Entries still sitting on their old policy default re-materialize under the new one, so the toggle is visible on
    /// the existing transcript. An explicit pager.toml `expanded_by_default` makes both defaults equal, so the walk
    /// naturally no-ops.
    pub fn apply_collapsed_edit_blocks_flip(&mut self, old_flag: bool, new_flag: bool) {
        let edit_cfg = &self.appearance.scrollback.blocks.edit;
        let old_expanded = edit_cfg.effective_expanded(old_flag);
        let new_expanded = edit_cfg.effective_expanded(new_flag);
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if old_expanded != new_expanded {
            for entry in self.entries.values_mut() {
                let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
                    continue;
                };
                if respect_manual_folds && entry.display_mode_pinned {
                    continue;
                }
                if entry.display_mode == edit_default_display_mode(old_expanded, edit) {
                    entry.display_mode = edit_default_display_mode(new_expanded, edit);
                }
            }
        }
        self.clear_group_expansion();
        self.invalidate_heights();
    }

    /// Get the index of an entry by its ID.
    /// O(1) average via IndexMap.
    pub fn index_of_id(&self, id: EntryId) -> Option<usize> {
        self.entries.get_index_of(&id)
    }

    /// Capture a width-stable bookmark of the viewport-top content, to re-pin it after a resize/re-wrap (the `/jump` capture-and-restore).
    /// `None` when there's no layout to anchor to.
    pub(crate) fn capture_scroll_bookmark(&self) -> Option<ScrollAnchor> {
        self.capture_scroll_anchor()
    }

    /// Re-pin the viewport to a bookmark from [`Self::capture_scroll_bookmark`].
    pub(crate) fn restore_scroll_bookmark(&mut self, bookmark: ScrollAnchor) {
        self.restore_scroll_anchor(bookmark);
    }

    /// Mark an entry as finished (no longer running).
    ///
    /// If the entry has been running for less than `MIN_RUNNING_DURATION_MS`, the finish is deferred so the animated "running" state is visible.
    pub fn finish_running(&mut self, id: EntryId) {
        self.finish_running_with_time(id, None);
    }

    /// Mark every running entry finished. `finish_turn` alone would leave them animating forever.
    pub(crate) fn finish_all_running(&mut self) {
        let ids: Vec<EntryId> = self.running.iter().copied().collect();
        for id in ids {
            self.finish_running(id);
        }
    }

    /// Mark an entry as no longer running, with optional thinking time.
    ///
    /// For thinking blocks, the thinking_time_ms will be displayed in collapsed mode.
    pub fn finish_running_with_time(&mut self, id: EntryId, thinking_time_ms: Option<i64>) {
        self.running.remove(&id);
        // Track the finish-flash window so `tick()` checks O(flashing) entries instead of scanning the whole scrollback per tick
        if self.entries.contains_key(&id) && !self.flashing.contains(&id) {
            self.flashing.push(id);
        }
        let thinking_mode = self.thinking_display_mode;
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if let Some(entry) = self.get_by_id_mut(id) {
            entry.is_running = false;
            entry.finished_at = Some(Instant::now());
            // Finish streaming renderers (final safety re-render)
            match &mut entry.block {
                RenderBlock::AgentMessage(msg) => msg.finish(),
                RenderBlock::Thinking(thinking) => {
                    // finish() freezes the local started_at timer into elapsed_time_ms
                    // Only use server time as a fallback when no local timer exists (e.g., during replay)
                    thinking.finish();
                    if thinking.elapsed_time_ms().is_none()
                        && let Some(time_ms) = thinking_time_ms
                    {
                        thinking.set_elapsed_time_ms(Some(time_ms));
                    }
                }
                RenderBlock::ToolCall(ToolCallBlock::Execute(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Read(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Search(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::ListDir(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::WebFetch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::WebSearch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::MemorySearch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::SentMessage(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Other(b)) => b.finish(),
                _ => {}
            }
            // Let the block decide what display mode to adopt on finish. For thinking blocks, use the sticky
            // `thinking_display_mode` so Ctrl+E is respected across the session. Exception: an already-Expanded thinking block
            // keeps its mode. Entries the user manually folded (pinned) keep their mode.
            if respect_manual_folds && entry.display_mode_pinned {
                let would_be_mode = if matches!(entry.block, RenderBlock::Thinking(_)) {
                    (entry.display_mode != DisplayMode::Expanded).then_some(thinking_mode)
                } else {
                    entry.block.finished_display_mode()
                };
                if let Some(would_be_mode) = would_be_mode {
                    tracing::debug!(
                        entry_id = id.value(),
                        ?would_be_mode,
                        "scrollback.finish.pin_suppressed_override"
                    );
                }
            } else if matches!(entry.block, RenderBlock::Thinking(_)) {
                if entry.display_mode != DisplayMode::Expanded {
                    entry.display_mode = thinking_mode;
                }
            } else if let Some(mode) = entry.block.finished_display_mode() {
                // Collapsed stays folded; finish must not snap-open
                if entry.display_mode != DisplayMode::Collapsed {
                    entry.display_mode = mode;
                }
            }
            entry.invalidate_cache();
        }
        // Mark height dirty since collapsed mode has different height. display_mode may have changed, so gaps need recomputation.
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;

        self.bump_content_generation();
    }

    /// Mark an entry as awaiting (or no longer awaiting) user input. Returns `false` if the entry doesn't exist or
    /// already had the requested value.
    pub fn set_pending_user_input(&mut self, id: EntryId, pending: bool) -> bool {
        let Some(entry) = self.get_by_id_mut(id) else {
            return false;
        };
        if entry.is_pending_user_input == pending {
            return false;
        }
        entry.is_pending_user_input = pending;
        // No `invalidate_cache()`: the cached output is identical, only the post-pass bullet styling differs frame to frame
        // The flip is structural though: `run_step` keeps pending rows out of verb-group runs
        // An already-folded run must re-run its folds to surface the prompt row (and refold once it resolves)
        self.mark_structurally_dirty(id);
        true
    }

    /// Open a permission edit once. A pinned fold is a user choice and must stick
    /// across the per-frame pending clear/re-mark.
    pub(crate) fn open_permission_edit(&mut self, id: EntryId) {
        if self.permission_opened.contains(&id) {
            return;
        }
        let Some(entry) = self.get_by_id_mut(id) else {
            return;
        };
        if entry.display_mode_pinned {
            return;
        }
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return;
        };
        if edit.hunks.is_empty() || entry.display_mode == DisplayMode::Expanded {
            return;
        }
        entry.display_mode = DisplayMode::Expanded;
        entry.invalidate_cache();
        self.permission_opened.insert(id);
        self.mark_structurally_dirty(id);
    }

    /// Put a permission-opened edit back to its default fold once the prompt is gone.
    /// A pinned fold is a user choice and is left alone.
    pub(crate) fn close_permission_edit(&mut self, id: EntryId) {
        if !self.permission_opened.remove(&id) {
            return;
        }
        let expanded_by_default = self
            .appearance
            .scrollback
            .blocks
            .edit
            .effective_expanded(crate::appearance::cache::load_collapsed_edit_blocks());
        let Some(entry) = self.get_by_id_mut(id) else {
            return;
        };
        if entry.display_mode_pinned {
            return;
        }
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return;
        };
        let mode = edit_default_display_mode(expanded_by_default, edit);
        if entry.display_mode != mode {
            entry.display_mode = mode;
            entry.invalidate_cache();
            self.mark_structurally_dirty(id);
        }
    }

    /// Clear the pending-user-input flag from every entry.
    ///
    /// Called by `AgentView` before re-syncing flags from the current permission/question queues so stale marks don't linger.
    pub fn clear_all_pending_user_input(&mut self) {
        // Real transitions are structural (mirrors `set_pending_user_input`): un-flagging returns a row to verb-group
        // membership. This clear is the only path back to false when a resolved permission simply stops being re-marked by
        // the next sync.
        let flagged: Vec<EntryId> = self
            .entries
            .iter()
            .filter_map(|(id, e)| e.is_pending_user_input.then_some(*id))
            .collect();
        for id in flagged {
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.is_pending_user_input = false;
            }
            self.mark_structurally_dirty(id);
        }
    }

    pub(crate) fn pending_user_input_ids(&self) -> std::collections::HashSet<EntryId> {
        self.entries
            .iter()
            .filter_map(|(id, entry)| entry.is_pending_user_input.then_some(*id))
            .collect()
    }

    /// Whether any entry is currently flagged as awaiting user input.
    ///
    /// Used by the animation driver: a flagged entry needs ticks even when nothing is `running` (the tool is paused on the user, not the model).
    pub fn has_pending_user_input(&self) -> bool {
        self.entries.values().any(|e| e.is_pending_user_input)
    }

    /// Invalidate all running entries for re-render.
    /// Call this periodically (e.g., every second) to update dynamic content like "[Running for Xs]" timers.
    /// This is O(running_count), not O(total).
    pub fn tick_running(&mut self) {
        let running_ids: Vec<EntryId> = self.running.iter().copied().collect();
        for id in running_ids {
            if let Some(entry) = self.get_by_id_mut(id) {
                entry.invalidate_cache();
            }
        }
    }

    /// Start a new streaming agent message.
    /// Creates an empty AgentMessageBlock in streaming mode and returns its EntryId.
    /// Use `get_by_id_mut()` to access the entry and push chunks.
    pub fn start_streaming_agent(&mut self) -> EntryId {
        let block = RenderBlock::agent_message_streaming();
        let entry = ScrollbackEntry::running(block);
        self.push(entry)
    }

    /// Get an entry by index (immutable).
    pub fn entry(&self, index: usize) -> Option<&ScrollbackEntry> {
        self.entries.get_index(index).map(|(_, v)| v)
    }

    /// Iterate over all entries mutably.
    pub fn entries_mut(&mut self) -> indexmap::map::ValuesMut<'_, EntryId, ScrollbackEntry> {
        self.entries.values_mut()
    }

    // Widget Helpers (used by ScrollbackPane widget)

    /// The one pre-render mutation point: refresh viewport size, rebuild the layout cache if needed, then apply follow-mode scroll.
    /// Callers must not mutate layout elsewhere or heights and scroll position drift from what is painted.
    pub fn prepare_layout(&mut self, width: u16, height: u16) -> bool {
        // Mid-session ffmpeg install: invalidate cached banner-sized reservations.
        if !self.ffmpeg_available_snapshot && crate::inline_media_ffmpeg::ffmpeg_available() {
            self.ffmpeg_available_snapshot = true;
            self.gaps_may_be_dirty = true;
            self.invalidate_layout_cache();
        }

        // Bump generation when viewport dimensions change: screen coordinates of visible links shift, so the VisibleLinkMap must be rebuilt
        if height != self.viewport_height || width != self.last_width {
            self.bump_generation();
        }

        // Update viewport height
        self.viewport_height = height;
        self.release_pin_reserve_outside_view();

        // Take an armed StructuralScrollAnchor unconditionally so it never outlives the first layout pass after its mutation
        // Only the same-width full rebuild below applies it
        let structural_anchor = self.structural_scroll_anchor.take();

        // Case 1: Cache missing or width changed, full rebuild
        if self.layout_cache.is_none() || width != self.last_width || self.full_settlement_pending {
            // A width change re-wraps every entry. The absolute wrapped-row scroll_offset would then point at different
            // content after the rebuild (the resize jump). Anchoring is intentionally limited to the not-following path.
            // Follow mode (including the follow_preserve_scroll page-flip) re-pins each frame, so it needs no anchor.
            let scroll_anchor =
                if width != self.last_width && !self.follow_mode && self.scroll_offset > 0 {
                    self.capture_scroll_anchor()
                } else {
                    None
                };

            let width_changed = width != self.last_width;
            let resized = width_changed && self.last_width != 0;
            let pre_rebuild_pin = self.pin_reserve_target;
            let has_targeted_dirty =
                !self.dirty_heights.is_empty() && self.dirty_heights.len() < self.entries.len();
            if width_changed {
                for entry in self.entries.values_mut() {
                    entry.invalidate_width_caches();
                }
                self.last_width = width;
            }
            if self.full_settlement_pending {
                self.layout_cache = None;
                self.full_settlement_pending = false;
            }
            // Full rebuild produces cheap height ESTIMATES for every entry.
            self.ensure_layout_cache(width);
            if resized
                && self.follow_mode
                && self.follow_preserve_scroll
                && self.pin_reserve_active
                && let Some(target) = self.pin_reserve_prompt_scroll_target()
            {
                self.scroll_offset = target;
            }
            self.compute_total_height_from_cache();
            // Re-pin the anchored content to the viewport top now that virtual_y is rebuilt at the new width (before settle clamps / re-pins to it)
            if let Some(anchor) = scroll_anchor {
                self.restore_scroll_anchor(anchor);
            } else if !width_changed {
                // Same-width rebuild forced by a structural mutation: re-pin the pre-mutation viewport-top content by stable EntryId
                // On a width change the anchor is dropped instead; its row offset is meaningless after a re-wrap
                self.apply_structural_scroll_anchor(structural_anchor, width);
            }
            self.fixup_hidden_selection();
            if self.follow_mode && !self.follow_preserve_scroll {
                self.handle_follow_mode();
            }
            // Page-flip preserve defers release until its authoritative target is restored/reset.
            self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Defer);
            if resized {
                self.reset_pin_reserve_target();
                self.compute_total_height_from_cache();
                if self.follow_mode
                    && self.follow_preserve_scroll
                    && self.pin_reserve_active
                    && let Some(target) = self.pin_reserve_target
                {
                    self.scroll_offset = target;
                } else {
                    self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
                }
            } else if self.pin_reserve_active {
                self.compute_total_height_from_cache();
                self.settle_pin_reserve_target();
            }
            let pin_entry_gone = self.pin_reserve_active
                && self.pin_reserve_prompt_id.is_some()
                && self.pin_reserve_prompt_index().is_none();
            if pin_entry_gone {
                let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
                let was_following = self.follow_mode;
                self.follow_preserve_scroll = false;
                self.clear_pin_reserve();
                self.total_height = unpadded_total;
                self.pin_reserve_pad = 0;
                if was_following {
                    self.scroll_offset = self.max_scroll_offset();
                } else {
                    self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
                }
            } else if !resized
                && (has_targeted_dirty || self.pin_reserve_after_turn)
                && self.follow_mode
                && self.follow_preserve_scroll
            {
                let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
                let shrink_target = if has_targeted_dirty {
                    pre_rebuild_pin
                } else {
                    self.pin_reserve_target
                };
                if shrink_target.is_some_and(|target| target >= unpadded_total) {
                    self.follow_preserve_scroll = false;
                    self.clear_pin_reserve();
                    self.total_height = unpadded_total;
                    self.pin_reserve_pad = 0;
                    self.scroll_offset = self.max_scroll_offset();
                }
            }
            self.handle_follow_mode();
            // Pre-measure a few pages above the bottom so the first scroll-up is glitch-free (no-op unless bottom-pinned)
            // Warming three off-screen pages per drag event, only to throw them away at the next width, profiled as the largest cost of a resize
            // Hence the deferral
            if resized {
                self.warm_above = DeferredWarmAbove::Deferred;
            } else {
                self.warm_above = DeferredWarmAbove::Idle;
                self.warm_measure_pages_above(width);
            }
            self.dirty_heights.clear();
            self.gaps_may_be_dirty = false;
            return true;
        }

        // Case 2: Some entries have dirty heights, incremental update
        if !self.dirty_heights.is_empty() {
            // Viewport-top identity before heights change
            // Case 2 retains the cache (no insert/remove), so the plain index stays valid for the duration of this call
            let top_anchor = self.viewport_top_anchor_point();
            let pin_before = self.pin_reserve_prompt_scroll_target();
            let changes = self.update_dirty_entry_heights(width);
            self.dirty_heights.clear();

            if !changes.is_empty() {
                if self.gaps_may_be_dirty {
                    // Structural change (fold/expand/add/remove): full rebuild
                    self.rebuild_virtual_y_from_heights();
                    let pin_after = self.pin_reserve_prompt_scroll_target();
                    self.shift_pin_reserve_target_for_layout(pin_before, pin_after);
                    self.gaps_may_be_dirty = false;
                    self.compute_total_height_from_cache();
                    self.fixup_hidden_selection();
                } else {
                    // Fast path (streaming): only heights changed, gaps are stable.
                    // Patch virtual_y in O(n-k) where k is the earliest dirty index.
                    // For streaming (dirty entry at end), this is O(1).
                    self.shift_pin_reserve_target_for_changes(&changes);
                    let total_delta = self.patch_virtual_y_for_dirty(&changes);
                    // Streamed growth must shrink the reserve rather than inflate max_offset.
                    let content = self.total_height.saturating_sub(self.pin_reserve_pad);
                    let new_content = (content as i64 + total_delta as i64).max(0) as usize;
                    self.pin_reserve_pad = self.pin_reserve_pad_rows(new_content);
                    self.total_height = new_content.saturating_add(self.pin_reserve_pad);
                }
            } else if self.gaps_may_be_dirty {
                // Heights didn't change, but structural state is dirty (e.g., a new entry was pushed that extends a group needing truncation)
                // Must rebuild to apply group truncation even though heights are stable.
                self.rebuild_virtual_y_from_heights();
                let pin_after = self.pin_reserve_prompt_scroll_target();
                self.shift_pin_reserve_target_for_layout(pin_before, pin_after);
                self.gaps_may_be_dirty = false;
                self.compute_total_height_from_cache();
                self.fixup_hidden_selection();
            } else {
                // No heights changed, but visible range may have shifted
                self.compute_total_height_from_cache();
            }

            // Re-pin the pre-change viewport-top row: geometry above it may have shifted virtual_y while the absolute scroll_offset stayed put
            // (This is an exact no-op for changes at/below the top.)
            if let Some((entry_idx, rows_into_span)) = top_anchor {
                self.repin_viewport_top_to_entry(entry_idx, rows_into_span);
            }
            self.handle_follow_mode();
            // A scroll/content change may have brought estimated entries into view (e.g. streaming while scrolled up); measure them exactly.
            self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Evaluate);
            self.run_pending_warm_above(width);
            return !changes.is_empty();
        }

        // Case 3: Nothing structurally changed, but total_height still depends on visible_entry_range()
        // That range can change between renders (view mode switch, turn navigation).
        // Recompute unconditionally; it's just summing a slice
        self.compute_total_height_from_cache();
        // Handle follow mode even when nothing changed structurally.
        // Needed after fold_selected_impl clears dirty_heights: the fold leaves the cache clean
        // Follow/preserve state may still need to react to the new total_height (e.g., consume preserve on overflow)
        if self.follow_mode {
            self.handle_follow_mode();
            if self.follow_preserve_scroll && !self.pin_reserve_active {
                self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
            }
        }
        // Scroll-up (no dirty heights) reveals estimated off-screen entries; this is the on-demand measurement path for plain scrolling
        self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Evaluate);
        self.run_pending_warm_above(width);
        false
    }

    /// Mark the start of a frame that will draw this scrollback.
    /// Hosts must call this once per frame; it is the only signal of a frame boundary [`DeferredWarmAbove`] has.
    pub fn begin_frame(&mut self) {
        if self.warm_above == DeferredWarmAbove::Deferred {
            self.warm_above = DeferredWarmAbove::Armed;
        }
    }

    fn run_pending_warm_above(&mut self, width: u16) {
        if self.warm_above == DeferredWarmAbove::Armed {
            self.warm_above = DeferredWarmAbove::Idle;
            self.warm_measure_pages_above(width);
        }
    }

    /// Invalidate caches if width changed.
    pub fn invalidate_if_width_changed(&mut self, width: u16) {
        if width != self.last_width {
            for entry in self.entries.values_mut() {
                entry.invalidate_cache();
            }
            self.last_width = width;
            self.layout_cache = None;
            self.gaps_may_be_dirty = true;
            // Mark all heights as dirty
            self.dirty_heights = self.entries.keys().copied().collect();
        }
    }

    /// Get current scroll offset.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn capture_viewport_snapshot(&self) -> ViewportSnapshot {
        ViewportSnapshot {
            scroll_offset: self.scroll_offset,
            follow_mode: self.follow_mode,
            follow_preserve_scroll: self.follow_preserve_scroll,
            follow_preserve_content_generation: self.follow_preserve_content_generation,
            viewport_height: self.viewport_height,
            last_width: self.last_width,
            selected: self.selected,
            current_turn: self.current_turn,
            view_mode: self.view_mode,
            total_height: self.total_height,
        }
    }

    pub fn restore_viewport_snapshot(&mut self, snap: ViewportSnapshot) {
        self.scroll_offset = snap.scroll_offset;
        self.follow_mode = snap.follow_mode;
        self.follow_preserve_scroll = snap.follow_preserve_scroll;
        self.follow_preserve_content_generation = snap.follow_preserve_content_generation;
        self.viewport_height = snap.viewport_height;
        self.last_width = snap.last_width;
        self.selected = snap.selected;
        self.current_turn = snap.current_turn;
        self.view_mode = snap.view_mode;
        self.invalidate_layout_cache();
    }

    /// Set viewport height.
    pub fn set_viewport_height(&mut self, height: u16) {
        self.viewport_height = height;
    }

    /// Set total content height.
    pub fn set_total_height(&mut self, height: usize) {
        self.total_height = height;
    }

    /// Set the scroll offset directly (e.g., from a scrollbar click).
    ///
    /// Clamps to `[0, max_offset]` and disables follow mode since the user is explicitly positioning the viewport.
    pub fn set_scroll_offset(&mut self, offset: usize) {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = offset.min(max_offset);
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Get mutable reference to an entry.
    pub fn entry_mut(&mut self, index: usize) -> Option<&mut ScrollbackEntry> {
        self.entries.get_index_mut(index).map(|(_, v)| v)
    }

    /// Whether group truncation replaces or hides this entry's content at the current layout (a "N more" header, or a hidden member at height 0).
    /// Either way the entry's own content is not what's on screen.
    pub fn entry_content_hidden_by_group(&self, idx: usize) -> bool {
        let Some(cache) = self.layout_cache.as_ref() else {
            return false;
        };
        let Some(info) = cache.entries.get(idx) else {
            return false;
        };
        (info.is_group_header() && !info.is_expanded_verb_header()) || info.height == 0
    }

    /// Whether entry `idx` overlaps the current viewport (cached offsets + the current scroll).
    /// A visible entry is already exact (the prior settle covers the visible window), so callers can skip re-measuring it.
    fn entry_overlaps_viewport(&self, idx: usize) -> bool {
        if !self.visible_entry_range().contains(&idx) {
            return false;
        }
        let Some((top, bottom)) = self.viewport_virtual_bounds() else {
            return false;
        };
        let Some(cache) = self.layout_cache.as_ref() else {
            return false;
        };
        let Some(&entry_top) = cache.virtual_y.get(idx) else {
            return false;
        };
        let Some(info) = cache.entries.get(idx) else {
            return false;
        };
        let entry_bottom = entry_top + info.height as usize;
        entry_top < bottom && entry_bottom > top
    }
}

/// Display mode a freshly materialized Edit block adopts (fresh `push`, or a kind upgrade in `replace_tool_block`).
/// Failed edits collapse; summaries the one-liner can't truthfully compress expand. Otherwise the effective
/// expanded default (`EditBlockConfig::effective_expanded`) decides.
fn edit_default_display_mode(expanded_by_default: bool, edit: &EditToolCallBlock) -> DisplayMode {
    if edit.is_success() && (edit.summary_untrusted || expanded_by_default) {
        DisplayMode::Expanded
    } else {
        DisplayMode::Collapsed
    }
}

#[cfg(test)]
pub(super) mod test_util {
    use super::*;
    use ratatui::style::Color;

    pub(super) fn stub_block(text: &str) -> RenderBlock {
        RenderBlock::stub(text, Color::Blue)
    }

    pub(super) fn user_block(text: &str) -> RenderBlock {
        RenderBlock::user_prompt(text)
    }

    pub(super) fn tool_block(summary: &str) -> RenderBlock {
        RenderBlock::tool_call("Execute", summary, true)
    }

    pub(super) fn agent_block(text: &str) -> RenderBlock {
        RenderBlock::agent_message(text)
    }

    pub(super) fn tall_agent_block() -> RenderBlock {
        let text = (1..=10)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        RenderBlock::agent_message(text)
    }

    /// Helper: create a collapsed groupable stub block.
    pub(super) fn collapsed_groupable(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::new(RenderBlock::stub(text, Color::Blue))
            .with_display_mode(DisplayMode::Collapsed)
    }

    /// Helper: push N collapsed tool calls and return their IDs.
    pub(super) fn push_tool_calls(state: &mut ScrollbackState, n: usize) -> Vec<EntryId> {
        (0..n)
            .map(|i| state.push_block(RenderBlock::tool_call(format!("Tool{i}"), "info", true)))
            .collect()
    }

    /// Helper: get the group_header_count for entry at index `idx`.
    pub(super) fn header_count_at(state: &mut ScrollbackState, idx: usize) -> u16 {
        state.prepare_layout(80, 40);
        state
            .layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx))
            .map(|e| e.group_header_count)
            .unwrap_or(0)
    }

    /// Helper: get cached height for entry at index `idx`.
    pub(super) fn cached_height_at(state: &ScrollbackState, idx: usize) -> u16 {
        state
            .layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx))
            .map(|e| e.height)
            .unwrap_or(u16::MAX)
    }

    pub(super) struct ScrollTestHarness {
        pub(super) state: ScrollbackState,
        pub(super) width: u16,
        pub(super) height: u16,
    }

    impl ScrollTestHarness {
        pub(super) fn new(width: u16, height: u16) -> Self {
            let mut state = ScrollbackState::new();
            let mut appearance = crate::appearance::AppearanceConfig::default();
            appearance.scrollback.blocks.prompt.vpad = false;
            state.set_appearance(appearance);
            Self {
                state,
                width,
                height,
            }
        }

        pub(super) fn frame(&mut self) {
            self.state.prepare_layout(self.width, self.height);
        }

        pub(super) fn push_prompt(&mut self, text: &str) -> EntryId {
            let id = self.state.push_block(RenderBlock::user_prompt(text));
            self.frame();
            id
        }

        pub(super) fn push_thinking(&mut self, initial_text: &str) -> EntryId {
            let id = self.state.push_block(RenderBlock::thinking(initial_text));
            if let Some(entry) = self.state.entries.get_mut(&id) {
                entry.is_running = true;
                entry.set_display_mode(DisplayMode::Truncated);
            }
            self.state.running.insert(id);
            self.frame();
            id
        }

        pub(super) fn push_tool(&mut self, text: &str) -> EntryId {
            let id = self.state.push(collapsed_groupable(text));
            self.frame();
            id
        }

        pub(super) fn push_agent(&mut self, text: &str) -> EntryId {
            let id = self.state.push_block(RenderBlock::agent_message(text));
            self.frame();
            id
        }

        pub(super) fn stream_thinking(&mut self, id: EntryId, chunk: &str) {
            self.state.push_chunk_to_thinking(id, chunk);
            self.frame();
        }

        pub(super) fn select(&mut self, idx: usize) {
            self.state.set_selected(Some(idx));
        }

        /// Send with page-flip on (default product behavior).
        pub(super) fn send_prompt(&mut self, text: &str) -> EntryId {
            let id = self.state.push_block(RenderBlock::user_prompt(text));
            let prompt_idx = self.state.len().saturating_sub(1);
            self.state.follow_new_turn(Some(prompt_idx), true);
            self.frame();
            id
        }

        pub(super) fn toggle_fold(&mut self) {
            self.state.toggle_fold_selected();
        }

        #[allow(dead_code)]
        pub(super) fn scroll_offset(&self) -> usize {
            self.state.scroll_offset
        }
        pub(super) fn max_offset(&self) -> usize {
            self.state
                .total_height
                .saturating_sub(self.state.viewport_height as usize)
        }
        pub(super) fn is_follow(&self) -> bool {
            self.state.follow_mode
        }
        pub(super) fn is_preserve(&self) -> bool {
            self.state.follow_preserve_scroll
        }

        pub(super) fn assert_entry_at_top(&self, idx: usize, msg: &str) {
            let cache = self
                .state
                .layout_cache
                .as_ref()
                .expect("cache must be valid");
            let range = self.state.visible_entry_range();
            let Some(&base_y) = cache.virtual_y.get(range.start) else {
                panic!("virtual_y missing range start {}", range.start);
            };
            let Some(&entry_y_abs) = cache.virtual_y.get(idx) else {
                panic!("virtual_y missing entry {idx}");
            };
            let entry_y = entry_y_abs - base_y;
            assert_eq!(
                entry_y, self.state.scroll_offset,
                "{msg}: entry {idx} at vy={entry_y} should be at scroll_offset={}",
                self.state.scroll_offset
            );
        }

        pub(super) fn assert_at_bottom(&self, msg: &str) {
            let max = self.max_offset();
            assert_eq!(
                self.state.scroll_offset, max,
                "{msg}: scroll_offset={} should be at max_offset={max}",
                self.state.scroll_offset
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_empty_state() {
        let state = ScrollbackState::new();
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);
        assert_eq!(state.turn_count(), 0);
    }

    /// State with an explicit pager.toml-shaped `expanded_by_default` override; it is flag-independent (the `Some` wins over the cache).
    fn edit_state(expanded_by_default: bool) -> ScrollbackState {
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.edit.expanded_by_default = Some(expanded_by_default);
        state.set_appearance(appearance);
        state
    }

    fn edit_block(block: EditToolCallBlock) -> RenderBlock {
        RenderBlock::ToolCall(ToolCallBlock::Edit(block))
    }

    /// `push` owns the Edit materialize policy.
    /// It covers the explicit `expanded_by_default` shape override, the untrusted-summary escape, error collapse, and survival of an explicit mode.
    #[test]
    fn push_applies_edit_materialize_policy() {
        let ok = || EditToolCallBlock::new("f.rs", vec![]);

        let mut state = edit_state(false);
        let id = state.push_block(edit_block(ok()));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
        let id = state.push_block(edit_block(ok().with_untrusted_summary()));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "untrusted summaries expand even with an explicit collapse override"
        );

        let mut state = edit_state(true);
        let id = state.push_block(edit_block(ok()));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );
        let id = state.push_block(edit_block(ok().with_error("boom")));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "failed edits collapse regardless of the expanded override"
        );

        let mut state = edit_state(false);
        let id = state
            .push(ScrollbackEntry::new(edit_block(ok())).with_display_mode(DisplayMode::Expanded));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "an explicitly set mode survives push"
        );
    }

    /// `replace_tool_block` applies the materialize policy on a genuine kind transition and preserves the current mode on Edit-to-Edit swaps.
    /// (A user's manual expand survives refinement/completion.)
    #[test]
    fn replace_tool_block_edit_policy() {
        let mut state = edit_state(false);
        let id = state.push_block(RenderBlock::tool_call("Other", "pending", true));
        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("f.rs", vec![])),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "kind transition adopts the policy default"
        );

        // User opens the one-liner; the Edit-to-Edit completion swap keeps it.
        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("f.rs", vec![])),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "Edit-to-Edit swap must preserve the user's mode"
        );

        // Kind transition with the opt-in on lands Expanded.
        let mut state = edit_state(true);
        let id = state.push_block(RenderBlock::tool_call("Other", "pending", true));
        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("f.rs", vec![])),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );

        assert!(
            !state.replace_tool_block(
                EntryId::new(9999),
                edit_block(EditToolCallBlock::new("f.rs", vec![])),
                None
            ),
            "missing entry reports false"
        );
    }

    #[test]
    fn pending_permission_edit_expands() {
        let mut state = edit_state(false);
        let id = state.push_block(edit_block(EditToolCallBlock::new(
            "config.toml",
            vec![vec![]],
        )));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
        state.open_permission_edit(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );

        {
            let entry = state.get_by_id_mut(id).unwrap();
            entry.set_display_mode(DisplayMode::Collapsed);
            entry.display_mode_pinned = true;
        }
        state.open_permission_edit(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
    }

    #[test]
    fn permission_open_is_once_and_refolds_only_that_row() {
        let mut state = edit_state(false);
        let id = state.push_block(edit_block(EditToolCallBlock::new(
            "config.toml",
            vec![vec![]],
        )));
        state.set_pending_user_input(id, true);
        state.open_permission_edit(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );

        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Collapsed);
        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("config.toml", vec![vec![]],)),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );

        state.set_pending_user_input(id, false);
        state.close_permission_edit(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );

        let other = state.push_block(edit_block(EditToolCallBlock::new("other.rs", vec![vec![]])));
        state
            .get_by_id_mut(other)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        state.close_permission_edit(other);
        assert_eq!(
            state.get_by_id(other).unwrap().display_mode,
            DisplayMode::Expanded
        );

        state.open_permission_edit(id);
        let mut tail = state.fresh_continuation();
        tail.permission_opened.insert(id);
        state.append_entries_from(tail);
        assert!(state.permission_opened.contains(&id));
    }

    /// A permission can mark the eager Other placeholder before the Edit (and its hunks) exist.
    /// The later refine must open the row; a pinned fold must still stick.
    #[test]
    fn pending_other_refine_to_edit_expands() {
        let mut state = edit_state(false);
        let id = state.push_block(RenderBlock::tool_call("Other", "pending", true));
        assert!(state.set_pending_user_input(id, true));
        state.open_permission_edit(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "placeholder is not an Edit yet"
        );

        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("config.toml", vec![vec![]],)),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "Other→Edit refine while a permission is pending must open"
        );

        {
            let entry = state.get_by_id_mut(id).unwrap();
            entry.set_display_mode(DisplayMode::Collapsed);
            entry.display_mode_pinned = true;
        }
        assert!(state.replace_tool_block(
            id,
            edit_block(EditToolCallBlock::new("config.toml", vec![vec![]],)),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "a pinned fold survives a later replace while the prompt is up"
        );
    }

    /// The untrusted rising edge overrides the Edit-to-Edit preserve rule.
    /// Once a later Diff reveals a multi-file call, the collapsed one-liner lies and the entry must open.
    /// Steady-state untrusted swaps keep a user's collapse.
    #[test]
    fn replace_tool_block_untrusted_rising_edge_expands() {
        let untrusted = || EditToolCallBlock::new("f.rs", vec![]).with_untrusted_summary();

        let mut state = edit_state(false);
        let id = state.push_block(edit_block(EditToolCallBlock::new("f.rs", vec![])));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
        assert!(state.replace_tool_block(id, edit_block(untrusted()), None));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "trusted-to-untrusted swap must escalate to Expanded"
        );

        // User collapses the untrusted block; untrusted-to-untrusted is not a rising edge, so the gesture sticks
        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Collapsed);
        assert!(state.replace_tool_block(id, edit_block(untrusted()), None));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "untrusted-to-untrusted swap must preserve the user's collapse"
        );
    }

    /// Same-kind Execute-to-Execute must keep a mid-run user expand (progress ticks rebuild the row through
    /// `replace_tool_block` without pinning). Completion (`replace` then `finish_running`) must not snap the expand
    /// shut; a user-collapsed. Execute must not auto-open.
    #[test]
    fn replace_tool_block_execute_same_kind_preserves_mode() {
        use crate::scrollback::blocks::tool::ExecuteToolCallBlock;

        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::tool_call("Other", "pending", true));
        state.set_last_running(true);
        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        assert!(state.replace_tool_block(id, RenderBlock::execute("cargo build"), None));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "Other→Execute must reset even if the placeholder was expanded"
        );

        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        assert!(
            !state.get_by_id(id).unwrap().display_mode_pinned,
            "→ expand without respect_manual_folds must not pin"
        );
        assert!(state.replace_tool_block(
            id,
            RenderBlock::execute_with_output("cargo build", "Compiling…\n", None::<String>),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "Execute→Execute progress must keep the user's expand"
        );

        assert!(state.replace_tool_block(
            id,
            RenderBlock::execute_with_output(
                "cargo build",
                "Compiling…\nFinished\n",
                None::<String>
            ),
            None
        ));
        state.finish_running(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "completion must not snap a user-expanded Execute shut"
        );

        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::execute("cargo test"));
        state.set_last_running(true);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
        assert!(state.replace_tool_block(
            id,
            RenderBlock::execute_with_output("cargo test", "running 1 test\n", None::<String>),
            None
        ));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "Execute→Execute progress must not auto-open a collapsed Execute"
        );

        // Search (`finished_display_mode()` returns `None`): a same-kind expand survives replace and completion
        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::search("todo", 0, vec![]));
        state.set_last_running(true);
        state
            .get_by_id_mut(id)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        assert!(state.replace_tool_block(id, RenderBlock::search("todo", 3, vec![]), None));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "Search→Search must keep a user expand"
        );
        state.finish_running(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "Search completion must not snap a user expand shut"
        );

        // bash_mode: Truncated progress stays Truncated (no snap-open); finish still expands
        let bash = |output: Option<&str>| {
            let mut b = ExecuteToolCallBlock::new("pytest");
            b.bash_mode = true;
            if let Some(o) = output {
                b = b.with_output(o);
            }
            RenderBlock::ToolCall(ToolCallBlock::Execute(b))
        };
        let mut state = ScrollbackState::new();
        let id = state.push_block(bash(None));
        state.set_last_running(true);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Truncated
        );
        assert!(state.replace_tool_block(id, bash(Some("running 1 test\n")), None));
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Truncated,
            "bash Execute→Execute progress must not snap-open Truncated"
        );
        assert!(state.replace_tool_block(id, bash(Some("lots\nof\noutput\n")), None));
        state.finish_running(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "bash finish must still expand from preserved Truncated"
        );
    }

    /// With the pager.toml shape keys unset (the shipped default), the shell-owned `collapsed_edit_blocks` flag decides
    /// materialization. On means the collapsed one-liner, off means the legacy expanded diff. Untrusted summaries still
    /// escape the collapse. Spawned thread: the cache is a sticky thread-local seeded explicitly here.
    #[test]
    fn push_defaults_follow_collapsed_edit_blocks_flag_when_shape_unset() {
        std::thread::spawn(|| {
            let ok = || EditToolCallBlock::new("f.rs", vec![]);

            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut state = ScrollbackState::new();
            let id = state.push_block(edit_block(ok()));
            assert_eq!(
                state.get_by_id(id).unwrap().display_mode,
                DisplayMode::Collapsed,
                "flag on collapses fresh Edits"
            );
            let id = state.push_block(edit_block(ok().with_untrusted_summary()));
            assert_eq!(
                state.get_by_id(id).unwrap().display_mode,
                DisplayMode::Expanded,
                "untrusted summaries expand even with the flag on"
            );

            crate::appearance::cache::set_collapsed_edit_blocks(false);
            let mut state = ScrollbackState::new();
            let id = state.push_block(edit_block(ok()));
            assert_eq!(
                state.get_by_id(id).unwrap().display_mode,
                DisplayMode::Expanded,
                "flag off keeps the legacy expanded-diff default"
            );

            // An explicit pager.toml shape beats the flag in both directions.
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut state = edit_state(true);
            let id = state.push_block(edit_block(ok()));
            assert_eq!(
                state.get_by_id(id).unwrap().display_mode,
                DisplayMode::Expanded,
                "explicit expanded_by_default = true must beat the flag"
            );
        })
        .join()
        .unwrap();
    }

    /// A live flag flip re-materializes only entries still on their old policy default.
    /// A user gesture away from that default survives, and an explicit pager.toml shape makes the walk a no-op.
    #[test]
    fn collapsed_edit_blocks_flip_rematerializes_only_default_entries() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(false);
            let mut state = ScrollbackState::new();
            let plain = state.push_block(edit_block(EditToolCallBlock::new("a.rs", vec![])));
            let failed = state.push_block(edit_block(
                EditToolCallBlock::new("b.rs", vec![]).with_error("boom"),
            ));
            assert_eq!(
                state.get_by_id(plain).unwrap().display_mode,
                DisplayMode::Expanded
            );
            // User opens the failed edit, a gesture away from its flag-independent Collapsed default
            state
                .get_by_id_mut(failed)
                .unwrap()
                .set_display_mode(DisplayMode::Expanded);

            crate::appearance::cache::set_collapsed_edit_blocks(true);
            state.apply_collapsed_edit_blocks_flip(false, true);
            assert_eq!(
                state.get_by_id(plain).unwrap().display_mode,
                DisplayMode::Collapsed,
                "entry on the old default must follow the flip"
            );
            assert_eq!(
                state.get_by_id(failed).unwrap().display_mode,
                DisplayMode::Expanded,
                "user gesture must survive the flip"
            );

            // Explicit shape override: both effective defaults are equal, so the flip leaves the entry alone
            let mut state = edit_state(true);
            let id = state.push_block(edit_block(EditToolCallBlock::new("c.rs", vec![])));
            state.apply_collapsed_edit_blocks_flip(true, false);
            assert_eq!(
                state.get_by_id(id).unwrap().display_mode,
                DisplayMode::Expanded,
                "explicit expanded_by_default pins the default across flips"
            );
        })
        .join()
        .unwrap();
    }

    /// A finished user `!` command expands to its full output; a Collapsed entry keeps its fold (no snap-open at completion).
    #[test]
    fn bash_execute_expands_on_finish_unless_user_collapsed() {
        use crate::scrollback::blocks::tool::{ExecuteToolCallBlock, ToolCallBlock};

        let bash_block = || {
            let mut b = ExecuteToolCallBlock::new("pytest");
            b.bash_mode = true;
            RenderBlock::ToolCall(ToolCallBlock::Execute(
                b.with_output("lots\nof\ntest\noutput\nlines\nhere\n"),
            ))
        };

        // Untouched streaming block: Truncated becomes Expanded on finish
        let mut state = ScrollbackState::new();
        let id = state.push_block(bash_block());
        state.set_last_running(true);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Truncated,
            "user bash streams truncated"
        );
        state.finish_running(id);
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "finished user bash must show full output"
        );

        // User collapsed mid-run: stays collapsed at completion.
        let id2 = state.push_block(bash_block());
        state.set_last_running(true);
        if let Some(e) = state.get_by_id_mut(id2) {
            e.display_mode = DisplayMode::Collapsed; // manual fold
        }
        state.finish_running(id2);
        assert_eq!(
            state.get_by_id(id2).unwrap().display_mode,
            DisplayMode::Collapsed,
            "a mid-run manual collapse must not snap open on finish"
        );
    }

    // ── Animation gating: off-screen running entries must not redraw ────

    /// A running entry scrolled far out of the viewport must not demand animation ticks or redraws; the wave accent can't be seen there.
    /// 30fps redraws of a static screen were the dominant idle-CPU cost (e.g. a background task left running while reading elsewhere).
    #[test]
    fn offscreen_running_entry_needs_no_animation() {
        let mut state = ScrollbackState::new();
        let first = state.push_block(stub_block("running-entry"));
        state.set_last_running(true);
        assert_eq!(state.index_of_id(first), Some(0));
        for i in 0..200 {
            state.push_block(stub_block(&format!("filler {i}")));
        }

        // Before any layout exists, stay conservative: animate.
        assert!(
            state.needs_animation(),
            "no layout yet — must conservatively animate"
        );

        // Bottom-pinned viewport (follow mode default): entry 0 is far above.
        state.prepare_layout(80, 10);
        assert!(
            state.scroll_offset() > 0,
            "follow mode pins the viewport to the bottom"
        );
        assert!(
            !state.needs_animation(),
            "running entry far above the viewport must not demand ticks"
        );
        assert!(
            !state.tick(),
            "tick with only off-screen running entries must not redraw"
        );

        // Scrolled back to the top: the running entry is visible again.
        state.scroll_up(u16::MAX);
        state.prepare_layout(80, 10);
        assert_eq!(state.scroll_offset(), 0, "scrolled to the very top");
        assert!(
            state.needs_animation(),
            "running entry inside the viewport demands ticks again"
        );
        assert!(state.tick(), "visible running entry redraws per tick");
    }

    /// `finish_running` tracks the finish-flash in the O(flashing) list.
    /// Ticks keep flowing (and redraw) while the flash is active, emit one final repaint on expiry, then the list drains and animation stops.
    #[test]
    fn finish_flash_is_tracked_and_drained() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(stub_block("tool"));
        state.set_last_running(true);
        state.prepare_layout(80, 10);
        assert!(state.needs_animation());

        state.finish_running(id);
        assert!(state.running.is_empty());
        assert_eq!(state.flashing.len(), 1, "finish tracked for the flash");
        assert!(
            !state.needs_animation(),
            "flash alone must not demand ticks (compat: no self-driven metronome)"
        );
        assert!(state.tick(), "flash animates while ticks flow anyway");

        // Sleep past FINISH_FLASH_DURATION_MS: the next tick repaints once (restoring the static accent) and drains the tracking list
        std::thread::sleep(std::time::Duration::from_millis(
            FINISH_FLASH_DURATION_MS + 50,
        ));
        assert!(state.tick(), "one final repaint when the flash expires");
        assert!(state.flashing.is_empty(), "expired flash is drained");
        assert!(!state.needs_animation(), "nothing left to animate");
        assert!(!state.tick(), "and ticks stop redrawing");
    }

    /// Rewound/removed entries can't strand ids in the flash list.
    #[test]
    fn finish_flash_drops_removed_entries() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(stub_block("tool"));
        state.set_last_running(true);
        state.finish_running(id);
        assert_eq!(state.flashing.len(), 1);
        state.remove_entry(id);
        state.tick();
        assert!(
            state.flashing.is_empty(),
            "removed entry dropped from flash"
        );
    }

    // ── Off-screen render-cache eviction ─────────────────────────────────

    /// Entries far outside the viewport lose their cached render output (the dominant long-session allocation).
    /// Entries in the keep zone and the layout (heights/scroll) are untouched.
    /// Evicted entries re-render on demand, so a follow-up sweep after re-caching finds them again.
    #[test]
    fn evict_offscreen_render_caches_sweeps_far_entries_only() {
        let mut state = ScrollbackState::new();
        let n = 400usize;
        for i in 0..n {
            state.push_block(stub_block(&format!("entry {i}")));
        }
        // Bottom-pinned viewport: the measurement window is near the tail.
        state.prepare_layout(80, 10);
        let max_offset_before = state.max_scroll_offset();

        // Populate every entry's render cache (as a fully-scrolled-through live session would)
        let appearance = state.appearance().clone();
        for (_, entry) in state.entries.iter() {
            entry.ensure_cached(78, &appearance, false, None);
        }

        let evicted = state.evict_offscreen_render_caches();
        assert!(
            evicted > 0,
            "far-above entries must be swept (got {evicted})"
        );
        // The keep zone is the measurement window plus EVICT_KEEP_MARGIN_ENTRIES on each side; everything else (the bulk of 400 entries) is swept
        assert!(
            evicted >= n - (EVICT_KEEP_MARGIN_ENTRIES + MEASURE_MARGIN_ENTRIES + 20),
            "sweep must reclaim the bulk of off-screen entries (got {evicted})"
        );

        // Layout untouched: same heights, same scroll geometry.
        state.prepare_layout(80, 10);
        assert_eq!(
            state.max_scroll_offset(),
            max_offset_before,
            "eviction must not change layout geometry"
        );

        // A second sweep with nothing re-rendered finds nothing to do.
        assert_eq!(state.evict_offscreen_render_caches(), 0);
    }

    #[test]
    fn test_push_and_selection() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("one"));
        state.push_block(stub_block("two"));
        state.push_block(stub_block("three"));

        assert_eq!(state.len(), 3);
        assert_eq!(state.selected(), None);

        state.select_next();
        assert_eq!(state.selected(), Some(0));

        state.select_next();
        assert_eq!(state.selected(), Some(1));

        state.select_prev();
        assert_eq!(state.selected(), Some(0));
    }

    /// `fresh_continuation` shares the id space with its source.
    /// `append_entries_from` merges a sibling's entries below the existing content with ids (and the running set) intact.
    #[test]
    fn fresh_continuation_and_append_share_id_space() {
        let mut original = ScrollbackState::new();
        let kept = original.push_block(stub_block("kept"));

        let mut staging = original.fresh_continuation();
        assert!(staging.is_empty());
        let tail_id = staging.push_block(stub_block("tail"));
        assert_ne!(tail_id, kept, "continuation must not reuse existing ids");
        staging.set_last_running(true);

        original.append_entries_from(staging);
        assert_eq!(original.len(), 2);
        assert_eq!(original.index_of_id(kept), Some(0));
        assert_eq!(original.index_of_id(tail_id), Some(1));
        assert!(
            original.get_by_id(tail_id).unwrap().is_running,
            "running state survives the merge"
        );
        let next = original.push_block(stub_block("after"));
        assert!(
            next != kept && next != tail_id,
            "post-merge allocation continues past both id ranges"
        );
    }

    /// `raise_id_floor` prevents a restored stash from re-issuing ids a discarded continuation sibling already handed out.
    #[test]
    fn raise_id_floor_skips_ids_allocated_by_discarded_sibling() {
        let mut stash = ScrollbackState::new();
        stash.push_block(stub_block("old"));

        let mut discarded = stash.fresh_continuation();
        let sibling_id = discarded.push_block(stub_block("partial replay"));

        stash.raise_id_floor(discarded.id_floor());
        let new_id = stash.push_block(stub_block("new"));
        assert_ne!(
            new_id, sibling_id,
            "restored state must not alias ids the sibling allocated"
        );
    }

    /// The invalidation generations never regress across a continuation swap, a failure restore, or a merge.
    /// Consumers (link map, search index) cache them and compare by equality, so a regressed-equal counter would read stale state as fresh.
    #[test]
    fn continuation_swaps_never_regress_invalidation_generations() {
        let mut original = ScrollbackState::new();
        original.push_block(stub_block("kept"));
        let orig = original.invalidation_generations();

        // Swap-in (begin window): staging reads as newer than the source.
        let staging = original.fresh_continuation();
        let staged = staging.invalidation_generations();
        assert!(staged.0 > orig.0 && staged.1 > orig.1);

        // Failure restore: the stash advances past the discarded staging.
        original.raise_invalidation_floor(staged);
        let restored = original.invalidation_generations();
        assert!(restored.0 > staged.0 && restored.1 > staged.1);

        // Merge: the kept stash advances past the consumed tail.
        let mut base = ScrollbackState::new();
        base.push_block(stub_block("kept"));
        let mut tail = base.fresh_continuation();
        tail.push_block(stub_block("tail"));
        let tail_gens = tail.invalidation_generations();
        base.append_entries_from(tail);
        let merged = base.invalidation_generations();
        assert!(merged.0 > tail_gens.0 && merged.1 > tail_gens.1);
    }

    /// User view preferences survive a continuation swap, matching [`clear`](ScrollbackState::clear): a reload must not reset them.
    #[test]
    fn fresh_continuation_preserves_view_preferences() {
        let mut original = ScrollbackState::new();
        original.thinking_display_mode = DisplayMode::Expanded;
        original.view_mode = ViewMode::SingleTurn;
        original.follow_mode = false;

        let fresh = original.fresh_continuation();
        assert_eq!(fresh.thinking_display_mode, DisplayMode::Expanded);
        assert_eq!(fresh.view_mode, ViewMode::SingleTurn);
        assert!(!fresh.follow_mode);
    }

    #[test]
    fn test_turn_detection() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Hello"));
        state.push_block(stub_block("Response 1"));
        state.push_block(stub_block("Response 2"));
        state.push_block(user_block("Next question"));
        state.push_block(stub_block("Response 3"));

        assert_eq!(state.turn_count(), 2);

        let turn0 = state.turn(0).unwrap();
        assert_eq!(turn0.prompt_index, 0);
        assert_eq!(turn0.end_index, 3);
        assert_eq!(turn0.len(), 3);

        let turn1 = state.turn(1).unwrap();
        assert_eq!(turn1.prompt_index, 3);
        assert_eq!(turn1.end_index, 5);
        assert_eq!(turn1.len(), 2);
    }

    #[test]
    fn test_turn_navigation() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1"));
        state.push_block(stub_block("A1"));
        state.push_block(user_block("Q2"));
        state.push_block(stub_block("A2"));

        assert_eq!(state.current_turn(), Some(1)); // Last turn by default

        assert!(state.prev_turn());
        assert_eq!(state.current_turn(), Some(0));
        assert_eq!(state.selected(), Some(0)); // Jumped to turn 0's prompt

        assert!(state.next_turn());
        assert_eq!(state.current_turn(), Some(1));
        assert_eq!(state.selected(), Some(2)); // Jumped to turn 1's prompt

        // At last turn, l re-activates it (scrolls prompt to top)
        assert!(state.next_turn());
        assert_eq!(state.current_turn(), Some(1));
        assert_eq!(state.selected(), Some(2)); // Still on turn 1's prompt
    }

    #[test]
    fn test_pinned_prompt_index() {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Question 1"));
        state.push_block(stub_block("Answer 1"));
        state.push_block(user_block("Question 2"));
        state.push_block(stub_block("Answer 2"));

        state.set_viewport_height(20);
        state.set_total_height(50);

        // Select first prompt (turn 0)
        state.set_selected(Some(0));
        assert_eq!(state.current_turn(), Some(0));

        // At scroll_offset = 0, no pinning
        assert_eq!(state.scroll_offset, 0);
        assert_eq!(state.pinned_prompt_index(), None);

        // At scroll_offset = 1, pinning starts
        state.scroll_down(1);
        assert_eq!(state.scroll_offset, 1);
        assert_eq!(state.pinned_prompt_index(), Some(0));

        // Scroll more, still pinned
        state.scroll_down(10);
        assert_eq!(state.pinned_prompt_index(), Some(0));

        // Go back to top, no pinning
        state.goto_top();
        assert_eq!(state.pinned_prompt_index(), None);
    }

    #[test]
    fn test_cached_height_respects_appearance_vpad() {
        use crate::appearance::AppearanceConfig;

        // Create state with vpad=false for prompt blocks
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.prompt.vpad = false;
        state.set_appearance(appearance);

        // Push a user prompt (1 line of content)
        state.push_block(user_block("Hello"));

        // Prepare layout to populate the cache
        state.prepare_layout(80, 20);

        // The cached height is 1 (content only, no vpad)
        // With vpad=true (default), it would be 3 (1 content row plus 2 vpad rows)
        let cached_height = state.get_cached_entry_height(0).unwrap();
        assert_eq!(
            cached_height, 1,
            "Cached height should be 1 (no vpad) but got {}",
            cached_height
        );
    }

    #[test]
    fn test_cached_height_with_default_appearance_has_vpad() {
        // Default appearance has vpad=true for prompt blocks
        let mut state = ScrollbackState::new();

        // Push a user prompt (1 line of content)
        state.push_block(user_block("Hello"));

        // Prepare layout to populate the cache
        state.prepare_layout(80, 20);

        // The cached height is 3 (1 content row plus 2 vpad rows)
        let cached_height = state.get_cached_entry_height(0).unwrap();
        assert_eq!(
            cached_height, 3,
            "Cached height should be 3 (with vpad) but got {}",
            cached_height
        );
    }

    #[test]
    fn test_push_chunk_to_agent() {
        let mut state = ScrollbackState::new();

        // Push an empty agent message (streaming mode)
        let id = state.push_block(RenderBlock::agent_message_streaming());

        // Initially empty
        if let Some(entry) = state.get_by_id(id)
            && let RenderBlock::AgentMessage(msg) = &entry.block
        {
            assert!(msg.text().is_empty());
        }

        // Push a chunk
        assert!(state.push_chunk_to_agent(id, "Hello "));
        assert!(state.push_chunk_to_agent(id, "world!"));

        // Verify content was appended
        if let Some(entry) = state.get_by_id(id) {
            if let RenderBlock::AgentMessage(msg) = &entry.block {
                assert_eq!(msg.text(), "Hello world!");
            } else {
                panic!("Expected agent message");
            }
        } else {
            panic!("Entry not found");
        }

        // Verify the entry is marked as having dirty height
        assert!(
            state.dirty_heights.contains(&id),
            "Entry should be in dirty_heights after push_chunk"
        );
    }

    #[test]
    fn test_push_chunk_to_nonexistent_entry() {
        let mut state = ScrollbackState::new();

        // Try to push to a non-existent entry
        let fake_id = EntryId::new(999);
        assert!(!state.push_chunk_to_agent(fake_id, "test"));
    }

    #[test]
    fn test_push_chunk_to_wrong_type() {
        let mut state = ScrollbackState::new();

        // Push a user prompt (not an agent message)
        let id = state.push_block(RenderBlock::user_prompt("Hello"));

        // Pushing a chunk to it fails silently
        assert!(!state.push_chunk_to_agent(id, "test"));
    }

    #[test]
    fn test_dirty_heights_cleared_after_prepare_layout() {
        let mut state = ScrollbackState::new();

        // Push an agent message and prepare initial layout
        let id = state.push_block(RenderBlock::agent_message_streaming());
        state.prepare_layout(80, 20);

        // Verify dirty_heights is clear after prepare_layout
        assert!(
            state.dirty_heights.is_empty(),
            "dirty_heights should be empty after prepare_layout"
        );

        // Pushing a chunk marks the height dirty
        state.push_chunk_to_agent(id, "Hello");
        assert!(
            state.dirty_heights.contains(&id),
            "Entry should be dirty after push_chunk"
        );

        // Preparing layout again clears dirty_heights
        state.prepare_layout(80, 20);
        assert!(
            state.dirty_heights.is_empty(),
            "dirty_heights should be empty after second prepare_layout"
        );
    }

    #[test]
    fn test_content_generation_bumps_on_content_changes() {
        let mut state = ScrollbackState::new();
        let mut last = state.content_generation();

        let id = state.push_block(stub_block("one"));
        assert!(state.content_generation() > last, "push bumps");
        last = state.content_generation();

        let agent = state.start_streaming_agent();
        assert!(state.content_generation() > last, "streaming push bumps");
        last = state.content_generation();

        state.push_chunk_to_agent(agent, "hello");
        assert!(state.content_generation() > last, "streamed chunk bumps");
        last = state.content_generation();

        state.remove_entry(id);
        assert!(state.content_generation() > last, "remove_entry bumps");
        last = state.content_generation();

        state.clear();
        assert!(state.content_generation() > last, "clear bumps");
    }

    #[test]
    fn test_content_generation_unchanged_by_display_toggles() {
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::execute_with_output(
            "cargo test",
            "output",
            None::<String>,
        ));
        state.prepare_layout(80, 10);
        state.set_selected(Some(0));

        let content_gen = state.content_generation();
        let link_gen = state.generation();

        // Fold and raw-mode change display, not the searchable corpus: the link-map generation moves, content_generation must hold steady
        state.collapse_all();
        state.expand_all();
        state.toggle_raw_selected();

        assert_eq!(
            state.content_generation(),
            content_gen,
            "display toggles must not change content_generation"
        );
        assert!(
            state.generation() > link_gen,
            "display toggles still bump the link-map generation"
        );
    }

    #[test]
    fn test_content_generation_unchanged_by_scroll_and_resize() {
        let mut state = ScrollbackState::new();
        for i in 0..50 {
            state.push_block(stub_block(&format!("entry {i}")));
        }
        state.prepare_layout(80, 10);

        let content_gen = state.content_generation();
        let link_gen = state.generation();

        // Scroll/resize moves the viewport, not the corpus: link-map generation moves, content_generation must not.
        state.scroll_up(3);
        state.scroll_down(2);
        state.goto_top();
        state.goto_bottom();
        state.set_scroll_offset(1);
        state.prepare_layout(100, 12);

        assert_eq!(
            state.content_generation(),
            content_gen,
            "scroll/resize must not change content_generation"
        );
        assert!(
            state.generation() > link_gen,
            "scroll/resize should still bump the link-map generation"
        );
    }

    #[test]
    fn test_iter_entries_yields_all_in_order() {
        let mut state = ScrollbackState::new();
        let id0 = state.push_block(stub_block("zero"));
        let id1 = state.push_block(stub_block("one"));
        let id2 = state.push_block(stub_block("two"));

        let ids: Vec<EntryId> = state.iter_entries().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![id0, id1, id2]);

        for (id, entry) in state.iter_entries() {
            assert_eq!(entry.id, id, "each entry is paired with its own id");
        }
        assert_eq!(state.iter_entries().count(), 3);
    }

    #[test]
    fn test_get_by_id_is_o1() {
        let mut state = ScrollbackState::new();

        // Push several entries
        let id1 = state.push_block(stub_block("one"));
        let id2 = state.push_block(stub_block("two"));
        let id3 = state.push_block(stub_block("three"));

        // Verify O(1) lookup by ID works (not testing performance, just correctness)
        assert!(state.get_by_id(id1).is_some());
        assert!(state.get_by_id(id2).is_some());
        assert!(state.get_by_id(id3).is_some());

        // Get index of each ID
        assert_eq!(state.index_of_id(id1), Some(0));
        assert_eq!(state.index_of_id(id2), Some(1));
        assert_eq!(state.index_of_id(id3), Some(2));
    }

    /// Total height computed via `compute_total_height_from_cache` after the next prepare_layout must include the newly pushed entry.
    #[test]
    fn test_push_then_prepare_layout_updates_total_height() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("a"));
        state.prepare_layout(80, 20);
        let total_before = state.total_height;

        state.push_block(stub_block("b"));
        // No prepare_layout yet: total_height is stale (matches old behavior)
        // The next prepare_layout must reconcile.
        state.prepare_layout(80, 20);
        assert!(
            state.total_height > total_before,
            "total_height must include the new entry after the next prepare_layout: \
             before={total_before}, after={}",
            state.total_height
        );
    }

    /// In batch mode, the cache is still nullified and the bulk rebuild happens at end_batch.
    #[test]
    fn test_push_in_batch_still_nullifies_cache() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("a"));
        state.prepare_layout(80, 20);
        assert!(state.layout_cache.is_some());

        state.begin_batch();
        state.push_block(stub_block("b"));
        assert!(
            state.layout_cache.is_none(),
            "batch mode should null the cache for safety"
        );
        state.end_batch();

        // After end_batch, the next prepare_layout rebuilds the cache.
        state.prepare_layout(80, 20);
        assert!(state.layout_cache.is_some());
        assert_eq!(state.layout_cache.as_ref().unwrap().entries.len(), 2);
    }

    /// Pushing into an empty state (no cache yet) falls through to invalidate_layout_cache (a no-op, since the cache is already None).
    /// It must not crash trying to extend a nonexistent cache.
    #[test]
    fn test_push_into_empty_state_with_no_cache() {
        let mut state = ScrollbackState::new();
        // Cache is None initially.
        assert!(state.layout_cache.is_none());

        // Push without ever calling prepare_layout. The extend fails gracefully (returns false); invalidate is a no-op.
        let _id = state.push_block(stub_block("a"));
        assert!(state.layout_cache.is_none());
        // gaps_may_be_dirty is set by the fallback path.
        assert!(state.gaps_may_be_dirty);

        // First prepare_layout builds the cache from scratch.
        state.prepare_layout(80, 20);
        assert!(state.layout_cache.is_some());
        assert_eq!(state.layout_cache.as_ref().unwrap().entries.len(), 1);
    }

    #[test]
    fn test_mark_height_dirty() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(stub_block("test"));

        // Clear dirty heights
        state.prepare_layout(80, 20);
        assert!(state.dirty_heights.is_empty());

        // Manually mark as dirty
        state.mark_height_dirty(id);
        assert!(state.dirty_heights.contains(&id));
    }

    #[test]
    fn insert_block_before_positions_and_keeps_ids_unique() {
        let mut state = ScrollbackState::new();
        let a = state.push_block(stub_block("a"));
        let c = state.push_block(stub_block("c"));

        let b = state.insert_block_before(c, stub_block("b"));

        assert_eq!(state.len(), 3);
        assert_eq!(state.index_of_id(a), Some(0));
        assert_eq!(state.index_of_id(b), Some(1));
        assert_eq!(state.index_of_id(c), Some(2));
        assert_ne!(b, a);
        assert_ne!(b, c);
    }

    #[test]
    fn insert_block_before_falls_back_to_push_when_the_anchor_is_gone() {
        let mut state = ScrollbackState::new();
        let a = state.push_block(stub_block("a"));
        assert!(state.remove_entry(a));

        let id = state.insert_block_before(a, stub_block("late"));
        assert_eq!(state.len(), 1);
        assert_eq!(state.index_of_id(id), Some(0));
    }

    #[test]
    fn insert_block_before_keeps_the_selection_on_its_entry() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("a"));
        let anchor = state.push_block(stub_block("b"));
        state.set_selected(Some(1)); // "b"

        state.insert_block_before(anchor, stub_block("inserted"));

        assert_eq!(state.index_of_id(anchor), Some(2));
        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn insert_block_before_never_strands_the_entry_below_the_commit_frontier() {
        // The shape minimal produces: a committed prefix, the cursor parked at the first uncommitted entry, and a block anchored above that entry
        let mut state = ScrollbackState::new();
        let a = state.push_block(stub_block("a"));
        let b = state.push_block(stub_block("b"));
        let anchor = state.push(ScrollbackEntry::running(stub_block("running tool")));
        state.mark_committed(0);
        state.mark_committed(1);
        state.set_commit_scan_cursor(2);

        let inserted = state.insert_block_before(anchor, stub_block("inserted"));

        assert_eq!(state.index_of_id(inserted), Some(2));
        assert!(
            state.commit_scan_cursor() <= 2,
            "cursor must be pulled back to (at most) the insertion point, got {}",
            state.commit_scan_cursor()
        );
        assert!(state.is_committed(a));
        assert!(state.is_committed(b));
        assert!(!state.is_committed(inserted));
        assert!(!state.is_committed(anchor));
    }

    #[test]
    #[should_panic(expected = "already committed")]
    fn insert_block_before_rejects_an_already_committed_anchor() {
        let mut state = ScrollbackState::new();
        let anchor = state.push_block(stub_block("printed"));
        state.mark_committed(0);
        state.insert_block_before(anchor, stub_block("too late"));
    }

    #[test]
    fn insert_block_before_rebuilds_turn_indices() {
        // Turns are positional, so a mid-list insert must rebuild them.
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::user_prompt("turn one"));
        let anchor = state.push_block(stub_block("work"));
        state.push_block(RenderBlock::user_prompt("turn two"));

        state.insert_block_before(anchor, stub_block("inserted"));

        // turn one = [0, 3), turn two = [3, 4)
        assert_eq!(state.turn_containing(0), Some(0));
        assert_eq!(state.turn_containing(1), Some(0));
        assert_eq!(state.turn_containing(2), Some(0));
        assert_eq!(state.turn_containing(3), Some(1));
    }

    #[test]
    fn set_pending_user_input_toggles_flag_and_reports_change() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(stub_block("waiting"));

        // The first flip from the default false to true reports a change
        assert!(state.set_pending_user_input(id, true));
        assert!(state.get_by_id(id).unwrap().is_pending_user_input);
        assert!(state.has_pending_user_input());

        // Repeated set with the same value is a no-op.
        assert!(!state.set_pending_user_input(id, true));

        // Flip back, then clear-all wipes the mark.
        assert!(state.set_pending_user_input(id, false));
        assert!(!state.has_pending_user_input());

        state.set_pending_user_input(id, true);
        state.clear_all_pending_user_input();
        assert!(!state.has_pending_user_input());
        assert!(!state.get_by_id(id).unwrap().is_pending_user_input);

        // Unknown id is silently a no-op (returns false, no panic).
        let missing = EntryId::new(99999);
        assert!(!state.set_pending_user_input(missing, true));
    }

    #[test]
    fn mark_completed_clears_pending_user_input() {
        // A finishing tool must drop its pending mark
        // Otherwise a tool that completes between two render frames would keep pulsing forever after AgentView's next sync clears the queue entry
        let mut entry = ScrollbackEntry::running(stub_block("tool"));
        entry.is_pending_user_input = true;

        entry.mark_completed();

        assert!(!entry.is_running);
        assert!(!entry.is_pending_user_input);
    }

    #[test]
    fn user_expanded_running_thinking_stays_expanded_on_finish() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::thinking_streaming());
        state.set_last_running(true);
        state.push_chunk_to_thinking(id, "deep thoughts");
        state.prepare_layout(80, 40);

        state.set_selected(Some(0));
        state.toggle_fold_selected();
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );

        state.finish_running(id);

        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );
    }

    #[test]
    fn untouched_running_thinking_collapses_on_finish() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::thinking_streaming());
        state.set_last_running(true);
        state.push_chunk_to_thinking(id, "deep thoughts");
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Truncated
        );

        state.finish_running(id);

        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
    }

    #[test]
    fn running_thinking_toggled_back_to_truncated_collapses_on_finish() {
        let mut state = ScrollbackState::new();
        let id = state.push_block(RenderBlock::thinking_streaming());
        state.set_last_running(true);
        state.push_chunk_to_thinking(id, "deep thoughts");
        state.prepare_layout(80, 40);

        state.set_selected(Some(0));
        state.toggle_fold_selected();
        state.toggle_fold_selected();
        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Truncated
        );

        state.finish_running(id);

        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed
        );
    }

    #[test]
    fn sticky_expanded_mode_still_expands_untouched_thinking_on_finish() {
        let mut state = ScrollbackState::new();
        let done = state.push_block(RenderBlock::thinking("earlier thoughts"));
        state.get_by_id_mut(done).unwrap().display_mode = DisplayMode::Collapsed;
        state.expand_all_thinking();

        let id = state.push_block(RenderBlock::thinking_streaming());
        state.set_last_running(true);
        state.push_chunk_to_thinking(id, "deep thoughts");

        state.finish_running(id);

        assert_eq!(
            state.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded
        );
    }

    fn long_wrap_text() -> String {
        "word ".repeat(80)
    }

    fn snapshot_fixture() -> ScrollbackState {
        let mut state = ScrollbackState::new();
        state.push_block(user_block("Q1"));
        state.push_block(agent_block(&long_wrap_text()));
        state.push_block(user_block("Q2"));
        state.push_block(agent_block(&long_wrap_text()));
        state
    }

    #[test]
    fn viewport_snapshot_restore_roundtrip_after_guest_mutate() {
        let mut state = snapshot_fixture();
        const W0: u16 = 80;
        const H0: u16 = 20;
        state.prepare_layout(W0, H0);
        state.follow_mode = false;
        state.follow_preserve_scroll = true;
        state.follow_preserve_content_generation = 17;
        state.set_selected(Some(0));
        state.set_scroll_offset(3);
        state.view_mode = ViewMode::SingleTurn;
        state.current_turn = Some(0);
        state.prepare_layout(W0, H0);

        let snap = state.capture_viewport_snapshot();
        let expected_offset = snap.scroll_offset;
        let expected_follow = snap.follow_mode;
        let expected_preserve = snap.follow_preserve_scroll;
        let expected_preserve_generation = snap.follow_preserve_content_generation;
        let expected_vh = snap.viewport_height;
        let expected_lw = snap.last_width;
        let expected_sel = snap.selected;
        let expected_turn = snap.current_turn;
        let expected_mode = snap.view_mode;

        state.enable_follow_mode();
        state.view_mode = ViewMode::AllTurns;
        assert!(state.prepare_layout(40, 8));
        assert!(state.layout_cache.is_some());
        assert_eq!(state.layout_cache.as_ref().unwrap().width, 40);

        state.restore_viewport_snapshot(snap);

        assert_eq!(state.scroll_offset, expected_offset);
        assert_eq!(state.follow_mode, expected_follow);
        assert_eq!(state.follow_preserve_scroll, expected_preserve);
        assert_eq!(
            state.follow_preserve_content_generation,
            expected_preserve_generation
        );
        assert_eq!(state.viewport_height, expected_vh);
        assert_eq!(state.last_width, expected_lw);
        assert_eq!(state.selected, expected_sel);
        assert_eq!(state.current_turn, expected_turn);
        assert_eq!(state.view_mode, expected_mode);
        assert!(state.layout_cache.is_none());

        assert!(state.prepare_layout(W0, H0));
        assert_eq!(state.layout_cache.as_ref().unwrap().width, W0);
    }

    #[test]
    fn restore_invalidates_stale_peek_width_cache_before_full_prepare() {
        let mut state = snapshot_fixture();
        const W0: u16 = 80;
        const W1: u16 = 40;
        const H: u16 = 20;

        assert!(state.prepare_layout(W0, H));
        assert_eq!(state.last_width, W0);
        let snap = state.capture_viewport_snapshot();
        assert_eq!(snap.last_width, W0);

        assert!(state.prepare_layout(W1, H));
        assert_eq!(state.last_width, W1);
        assert_eq!(state.layout_cache.as_ref().unwrap().width, W1);
        let Some(peek_height) = state
            .layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(1))
            .map(|e| e.height)
        else {
            panic!("expected cached entry 1");
        };

        state.restore_viewport_snapshot(snap);
        assert_eq!(state.last_width, W0);
        assert!(state.layout_cache.is_none());

        assert!(
            state.prepare_layout(W0, H),
            "restore must force Case 1 full rebuild at restored width"
        );
        let cache = state.layout_cache.as_ref().unwrap();
        assert_eq!(cache.width, W0);
        assert_ne!(
            cache.entries.get(1).map(|e| e.height),
            Some(peek_height),
            "heights must be recomputed for W0, not left at W1 wrap"
        );
    }

    #[test]
    fn prepare_layout_width_change_is_case1_height_only_is_not() {
        let mut state = snapshot_fixture();
        assert!(state.prepare_layout(80, 20));
        assert!(
            !state.prepare_layout(80, 20),
            "stable WxH with clean cache is Case 3"
        );
        assert!(
            !state.prepare_layout(80, 12),
            "height-only change is not Case 1"
        );
        assert_eq!(state.last_width, 80);
        assert_eq!(state.layout_cache.as_ref().unwrap().width, 80);
        assert!(
            !state.prepare_layout(80, 12),
            "stable width after height-only stays Case 3"
        );
        assert!(state.prepare_layout(50, 12), "width change is Case 1");
        assert_eq!(state.layout_cache.as_ref().unwrap().width, 50);
        assert!(
            !state.prepare_layout(50, 12),
            "stable width after Case 1 is Case 3"
        );
    }

    #[test]
    fn restore_reverts_follow_autoselect_and_current_turn() {
        let mut state = snapshot_fixture();
        state.prepare_layout(80, 20);
        state.follow_mode = false;
        state.set_selected(Some(0));
        assert_eq!(state.current_turn(), Some(0));
        state.set_scroll_offset(2);

        let snap = state.capture_viewport_snapshot();
        assert_eq!(snap.selected, Some(0));
        assert_eq!(snap.current_turn, Some(0));
        assert!(!snap.follow_mode);

        state.enable_follow_mode();
        state.prepare_layout(80, 20);
        assert!(state.is_follow_mode());
        assert_ne!(state.selected(), Some(0));
        assert_eq!(state.current_turn(), Some(1));

        state.restore_viewport_snapshot(snap);
        assert!(!state.is_follow_mode());
        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.current_turn(), Some(0));
        assert_eq!(state.scroll_offset(), 2);
    }
}
