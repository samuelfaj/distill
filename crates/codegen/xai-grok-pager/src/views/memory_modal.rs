//! `/memory` browser modal: view-state, rendering, and input handling.
//!
//! A centered popup with ModalWindow chrome.
//! Horizontally split into a searchable file list (left, ~40%) and a read-only content preview pane (right, ~60%).
//! File list shows all memory files grouped by source (Global, Workspace, Sessions) with session logs in reverse chronological order.
//! Selecting a file loads its content into the preview pane.
//!
//! Layout collapses to single-pane (list only) on narrow terminals (under 64 cols); `Enter` then
//! opens the selected note over the whole content area.
//!
//! `/` enters filter mode (type to search names and note contents, Escape to exit).
//! Dragging over the preview copies the selected text on release.
//! `x` deletes with double-press confirmation: v2 topics and inbox observations via the shell's
//! `x.ai/memory/forget` (tombstone + index + manifest update), legacy session logs by unlink.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crossterm::event::KeyModifiers;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::input::line_editor::{LineEditOutcome, LineEditor};

use crate::render::SafeBuf;
use crate::render::scrollbar::{ScrollbarClickResult, render_scrollbar, scrollbar_click_to_offset};
use crate::scrollback::blocks::markdown_content::MarkdownContent;
use crate::theme::Theme;
use crate::views::drag_select::{
    TextDrag, TextEndpoint, endpoint_at, endpoint_at_clamped, paint_text_drag, text_for_drag,
};
use crate::views::modal_window::{
    self, ModalContentArea, ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut,
};
use xai_grok_shell::extensions::memory::{
    MEMORY_FORGET_MAX_FILE_BYTES, MemoryForgetResponse, MemoryListing, MemoryToggleResponse,
};
use xai_grok_shell::extensions::notification::MemoryDisabledReason;

const SPLIT_MIN_WIDTH: u16 = 64;
const LIST_WIDTH_RATIO: f64 = 0.40;
/// Below this list width the size column is dropped so labels keep room (age only).
const NARROW_LIST_WIDTH: u16 = 48;
const MAX_PREVIEW_BYTES: u64 = 1_048_576;
/// Content search reads run on the UI thread, so they are capped per note and in total.
const MAX_SEARCH_BYTES_PER_NOTE: u64 = 262_144;
const MAX_SEARCH_BYTES_TOTAL: u64 = 8 * 1_048_576;
const NOTICE_MAX_WIDTH: u16 = 72;

// Notices use bold leads rather than `#` headings: heading text takes the theme accent color.
/// Only advertises the actions the shell reported as available in this session.
fn empty_state_markdown(capture_enabled: bool, dream_enabled: bool) -> String {
    let mut text = String::from("**Nothing remembered yet.**\n\n");
    if capture_enabled {
        text.push_str("- Keep working. Notes are saved automatically after each completed turn.\n");
    }
    text.push_str("- `/remember <note>` saves something specific right now.\n");
    if dream_enabled {
        text.push_str("- `/dream` organizes saved notes into topics.\n");
    }
    text.push_str(
        "\nRemote-Code remembers conventions, decisions, and project facts across sessions so you \
         don't have to repeat yourself. Notes live in **workspace** memory for this repository and \
         **global** memory shared across all your projects; each has a generated `MEMORY.md` index \
         that fills in as notes are saved.\n",
    );
    text
}

fn disabled_state_markdown(reason: Option<MemoryDisabledReason>) -> &'static str {
    match reason {
        None | Some(MemoryDisabledReason::SessionToggle) => {
            "\
**Memory is off for this session.** Press **t** to turn it back on.

While off, Grok isn't reading or saving notes; anything already remembered is kept on disk. \
Memory carries conventions, decisions, and project facts between sessions so you don't have \
to repeat yourself."
        }
        Some(MemoryDisabledReason::ConfigOptOut) => {
            "\
**Memory is off** (`[memory] enabled = false` in `config.toml`). Press **t** to turn it on for \
this session.

The toggle lasts for this session only; new sessions follow `config.toml`. Set `enabled = true` \
there (or remove the line) to keep memory on. Anything already remembered is kept on disk."
        }
        Some(MemoryDisabledReason::ProcessDisabled) => {
            "\
**Memory is off for this process.** Start a new session without `--no-memory` or \
`GROK_MEMORY=0` to use it.

Memory was turned off when Remote-Code started, so it can't be turned on here. Anything already \
remembered is kept on disk."
        }
        Some(MemoryDisabledReason::RolloutRestricted) => {
            "\
**Memory is unavailable in this session.** Start a new session to pick up your current settings.

This session's memory settings were pinned when it started, and they disable memory, so it \
can't be turned on here. Press **s** for details."
        }
        Some(MemoryDisabledReason::NotConfigured) => {
            "\
**Memory isn't configured.** Press **s** for details.

No memory storage is set up for this session, so there is nothing to browse or turn on."
        }
        Some(MemoryDisabledReason::Unknown) => {
            "\
**Memory is off for this session.** Press **s** for details.

This session reports a reason this version of Remote-Code doesn't recognize; press **t** to try \
turning it back on."
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryFileEntry {
    pub path: PathBuf,
    /// `"global"`, `"workspace"`, or `"session"`.
    pub source: String,
    pub label: String,
    /// Formatted size and age for the list row's metadata column. Empty for headers.
    pub size_text: String,
    pub age_text: String,
    pub is_header: bool,
    /// Store-generated index, not a note (see `MemoryFileInfo::generated`).
    pub generated: bool,
    pub size_bytes: u64,
}

impl MemoryFileEntry {
    /// Metadata column text; fixed-width fields keep the separator in one column across rows.
    /// Narrow lists drop the size so labels keep room.
    fn meta_text(&self, narrow: bool) -> String {
        if self.is_header {
            return String::new();
        }
        if narrow {
            format!("{:>3}", self.age_text)
        } else {
            format!("{:>8} \u{00B7} {:>3}", self.size_text, self.age_text)
        }
    }

    /// What `x` may remove: v2 topics and inbox observations, or legacy session logs.
    /// Mirrors the store's own rule so the hint never advertises a delete the shell would refuse.
    pub fn is_deletable(&self) -> bool {
        if self.is_header || self.generated || self.size_bytes > MEMORY_FORGET_MAX_FILE_BYTES {
            return false;
        }
        match self.source.as_str() {
            "session" => true,
            "workspace" | "global" => {
                let parent = self.path.parent();
                parent.is_some_and(|p| p.ends_with("topics"))
                    || parent.is_some_and(|p| p.ends_with("observations/_inbox"))
            }
            _ => false,
        }
    }
}

/// Status shown under the file list. Transient messages (copy outcomes) carry a tick countdown
/// like the agent-view toast; the rest stay until the next action replaces them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStatusLine {
    pub text: String,
    pub is_error: bool,
    pub ticks_remaining: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryModalMode {
    Browse,
    /// Filter input is focused: all single-char keys go to the filter.
    /// Only Escape (exit filter) and Enter (no-op / stay) remain active.
    FilterFocused,
    /// The preview has keyboard focus: arrows scroll the note; Esc/Enter return to the list.
    /// When the split is hidden the preview covers the whole content area.
    PreviewFocused,
    ConfirmingDelete {
        idx: usize,
    },
}

/// Left-button press on the preview before the movement threshold promotes a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingPress {
    column: u16,
    row: u16,
    endpoint: TextEndpoint,
}

/// What the modal can read from a note on disk.
enum NoteRead {
    Text(String),
    TooLarge,
    Unreadable,
}

/// Bounded read: the file may grow between a size check and the read.
fn read_note(path: &Path, limit: u64) -> NoteRead {
    let mut bytes = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|file| file.take(limit + 1).read_to_end(&mut bytes));
    if read.is_err() {
        return NoteRead::Unreadable;
    }
    if bytes.len() as u64 > limit {
        return NoteRead::TooLarge;
    }
    match String::from_utf8(bytes) {
        Ok(text) => NoteRead::Text(text),
        Err(_) => NoteRead::Unreadable,
    }
}

#[derive(Debug, Clone)]
pub struct MemoryModalState {
    pub window: ModalWindowState,
    pub entries: Vec<MemoryFileEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub preview_markdown: Option<MarkdownContent>,
    pub preview_scroll: usize,
    pub mode: MemoryModalMode,
    query: LineEditor,
    /// Whether memory is currently enabled for this session.
    pub memory_enabled: bool,
    /// `None` when enabled or when the shell predates the field.
    pub disabled_reason: Option<MemoryDisabledReason>,
    /// Session capabilities from the shell; the empty-state copy only advertises what is on.
    capture_enabled: bool,
    dream_enabled: bool,
    /// BLAKE3 hex of the previewed bytes, sent with a delete so the store only removes what the user saw.
    preview_hash: Option<String>,
    /// Delete awaiting the shell's answer; blocks a second `x` meanwhile.
    pending_delete: Option<PathBuf>,
    /// Flags to restore if an in-flight `t` toggle fails; `Some` also blocks a second `t`.
    pending_toggle: Option<(bool, Option<MemoryDisabledReason>)>,
    pub status: Option<MemoryStatusLine>,
    /// Whether the modal is rendered in fullscreen mode (persisted to config).
    pub fullscreen: bool,
    /// Cached filtered indices.
    /// Recomputed when `query` or `entries` change.
    filtered_cache: Vec<usize>,
    /// Lower-cased note contents for the filter, read once per listing on the first non-empty query.
    /// `None` marks notes that could not be read (or are over the preview cap).
    content_cache: HashMap<PathBuf, Option<String>>,
    /// Scroll the preview to the first query match on the next render (set by `load_preview`).
    preview_jump_pending: bool,
    /// Cached total lines in the preview (for scrollbar rendering).
    preview_total_lines: usize,
    /// Plain text of the wrapped preview lines as last rendered, for drag-select geometry and copy.
    preview_plain_lines: Vec<String>,
    /// Width `preview_plain_lines` was wrapped at; rebuilt when it changes.
    preview_plain_width: u16,
    pending_press: Option<PendingPress>,
    text_drag: Option<TextDrag>,
    /// Whether the last render showed list and preview side by side.
    split_shown: bool,
    /// Hit-test rects for mouse interaction (set during render).
    list_area: Rect,
    preview_area: Rect,
    /// Preview text cells (the preview area minus its scrollbar column).
    preview_text_area: Rect,
    list_scrollbar_area: Option<Rect>,
    preview_scrollbar_area: Option<Rect>,
}

impl MemoryModalState {
    pub fn new(entries: Vec<MemoryFileEntry>) -> Self {
        let filtered_cache = (0..entries.len()).collect();
        let mut state = Self {
            window: ModalWindowState::new(),
            entries,
            selected: 0,
            scroll_offset: 0,
            preview_markdown: None,
            preview_scroll: 0,
            mode: MemoryModalMode::Browse,
            query: LineEditor::default(),
            memory_enabled: true,
            disabled_reason: None,
            capture_enabled: true,
            dream_enabled: true,
            preview_hash: None,
            pending_delete: None,
            pending_toggle: None,
            status: None,
            fullscreen: load_fullscreen_pref(),
            filtered_cache,
            content_cache: HashMap::new(),
            preview_jump_pending: false,
            preview_total_lines: 0,
            preview_plain_lines: Vec::new(),
            preview_plain_width: 0,
            pending_press: None,
            text_drag: None,
            split_shown: true,
            list_area: Rect::default(),
            preview_area: Rect::default(),
            preview_text_area: Rect::default(),
            list_scrollbar_area: None,
            preview_scrollbar_area: None,
        };
        state.advance_past_headers();
        state.load_preview();
        state
    }

    pub fn from_listing(listing: MemoryListing) -> Self {
        let mut state = Self::new(Vec::new());
        state.apply_listing(listing);
        state
    }

    #[cfg(test)]
    fn with_enabled(mut self, enabled: bool, reason: Option<MemoryDisabledReason>) -> Self {
        self.memory_enabled = enabled;
        self.disabled_reason = if enabled { None } else { reason };
        self
    }

    #[cfg(test)]
    fn with_capabilities(mut self, capture_enabled: bool, dream_enabled: bool) -> Self {
        self.capture_enabled = capture_enabled;
        self.dream_enabled = dream_enabled;
        self
    }

    /// Replace the file list and flags with a fresh listing, keeping the window, filter, and fullscreen state.
    pub fn apply_listing(&mut self, listing: MemoryListing) {
        self.memory_enabled = listing.enabled;
        self.disabled_reason = if listing.enabled {
            None
        } else {
            listing.disabled_reason
        };
        self.capture_enabled = listing.capture_enabled;
        self.dream_enabled = listing.dream_enabled;
        self.entries = build_entries(listing.files);
        self.pending_delete = None;
        self.content_cache.clear();
        // A confirm armed against the old list must not carry its index into the new one.
        if matches!(self.mode, MemoryModalMode::ConfirmingDelete { .. }) {
            self.mode = MemoryModalMode::Browse;
        }
        self.invalidate_filter();
        self.selected = 0;
        self.advance_past_headers();
        self.clamp_selected();
        self.load_preview();
    }

    /// Apply the shell's answer to a `t` toggle: resync from its listing, or revert the optimistic
    /// flip and surface the failure.
    pub fn apply_toggle_result(&mut self, result: Result<MemoryToggleResponse, String>) {
        let previous = self.pending_toggle.take();
        match result {
            Ok(response) => {
                // The reply's flags are authoritative: a refusal leaves memory where it was.
                self.memory_enabled = response.enabled;
                self.disabled_reason = response.disabled_reason.filter(|_| !response.enabled);
                if let Some(listing) = response.listing {
                    self.apply_listing(listing);
                }
                let requested = previous.map(|(was_enabled, _)| !was_enabled);
                self.status = Some(MemoryStatusLine {
                    text: response.message,
                    is_error: requested.is_some_and(|r| r != response.enabled),
                    ticks_remaining: None,
                });
            }
            Err(message) => {
                if let Some((enabled, reason)) = previous {
                    self.memory_enabled = enabled;
                    self.disabled_reason = reason;
                }
                self.status = Some(MemoryStatusLine {
                    text: message,
                    is_error: true,
                    ticks_remaining: None,
                });
            }
        }
    }

    /// Memory is off but `t` can turn it back on in this session.
    /// `None` (shell predates the field) is assumed toggleable; `Unknown` fails closed.
    pub fn can_enable(&self) -> bool {
        !self.memory_enabled
            && matches!(
                self.disabled_reason,
                None | Some(
                    MemoryDisabledReason::SessionToggle | MemoryDisabledReason::ConfigOptOut
                )
            )
    }

    /// At least one entry is a note rather than a store-generated index.
    /// v2 scope init always writes both `MEMORY.md` indexes, so a store with no notes still lists two files.
    pub fn has_notes(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| !entry.is_header && !entry.generated)
    }

    /// The list has nothing to browse (disabled, or enabled with no notes yet).
    pub fn shows_notice(&self) -> bool {
        !self.memory_enabled || !self.has_notes()
    }

    /// Remove section headers with no files left under them.
    fn drop_empty_headers(&mut self) {
        let mut keep = Vec::with_capacity(self.entries.len());
        for (i, entry) in self.entries.iter().enumerate() {
            let header_has_files =
                !entry.is_header || self.entries.get(i + 1).is_some_and(|next| !next.is_header);
            keep.push(header_has_files);
        }
        let mut keep = keep.into_iter();
        self.entries.retain(|_| keep.next().unwrap_or(true));
    }

    /// Deletion needs the preview hash as evidence; oversized or unreadable notes offer none.
    fn can_delete_selected(&self) -> bool {
        self.preview_hash.is_some() && self.selected_entry().is_some_and(|e| e.is_deletable())
    }

    /// Apply the shell's answer to the delete request for `path`.
    /// Answers for a path that is no longer pending (modal reopened meanwhile) are ignored.
    pub fn apply_forget_result(
        &mut self,
        path: &str,
        result: Result<MemoryForgetResponse, String>,
    ) {
        if self.pending_delete.as_deref() != Some(Path::new(path)) {
            return;
        }
        self.pending_delete = None;
        self.status = Some(match result {
            Ok(MemoryForgetResponse::Forgotten { .. }) => {
                // The user may have moved on while the delete was in flight; keep their row.
                let selected_path = self.selected_entry().map(|e| e.path.clone());
                let label = self
                    .entries
                    .iter()
                    .position(|e| e.path == Path::new(path))
                    .map(|idx| self.entries.remove(idx).label);
                self.drop_empty_headers();
                self.invalidate_filter();
                if let Some(pos) = selected_path.and_then(|p| {
                    self.filtered_cache
                        .iter()
                        .position(|&i| self.entries.get(i).is_some_and(|e| e.path == p))
                }) {
                    self.selected = pos;
                }
                self.clamp_selected();
                self.load_preview();
                MemoryStatusLine {
                    text: format!("Deleted {}.", label.unwrap_or_default()),
                    is_error: false,
                    ticks_remaining: None,
                }
            }
            Ok(MemoryForgetResponse::Rejected { message, .. }) | Err(message) => MemoryStatusLine {
                text: message,
                is_error: true,
                ticks_remaining: None,
            },
        });
    }

    pub fn filtered_indices(&self) -> &[usize] {
        &self.filtered_cache
    }

    pub fn query(&self) -> &str {
        self.query.text()
    }

    pub fn query_cursor_byte(&self) -> usize {
        self.query.cursor_byte()
    }

    #[cfg(test)]
    fn set_query(&mut self, query: impl Into<String>) {
        self.query.set_text(query);
    }

    #[cfg(test)]
    fn set_query_cursor_byte(&mut self, cursor_byte: usize) -> LineEditOutcome {
        self.query.set_cursor_byte(cursor_byte)
    }

    #[cfg(test)]
    fn query_viewport(&self, width: usize) -> xai_ratatui_textarea::SingleLineViewport {
        self.query.viewport(width)
    }

    fn invalidate_filter(&mut self) {
        let terms = query_terms(self.query());
        if !terms.is_empty() {
            self.fill_content_cache();
        }
        self.filtered_cache = compute_filtered(&self.entries, &terms, &self.content_cache);
    }

    /// Read every listed note not yet cached. Runs on the first non-empty query; later keystrokes
    /// only hit the cache. Notes past the total budget are cached as unreadable (name-only match).
    fn fill_content_cache(&mut self) {
        let mut budget = MAX_SEARCH_BYTES_TOTAL;
        for entry in self.entries.iter().filter(|e| !e.is_header) {
            self.content_cache
                .entry(entry.path.clone())
                .or_insert_with(|| {
                    if budget == 0 {
                        return None;
                    }
                    match read_note(&entry.path, MAX_SEARCH_BYTES_PER_NOTE.min(budget)) {
                        NoteRead::Text(text) => {
                            budget = budget.saturating_sub(text.len() as u64);
                            Some(text.to_lowercase())
                        }
                        NoteRead::TooLarge | NoteRead::Unreadable => None,
                    }
                });
        }
    }

    /// The filter is active but matches nothing.
    fn filter_has_no_matches(&self) -> bool {
        self.filtered_cache.is_empty() && !self.query().is_empty()
    }

    /// Show the outcome of an `Action::MemoryCopy` in the status line (the agent-view toast is
    /// painted under the modal).
    pub fn report_copy(&mut self, delivery: &crate::clipboard::CopyDelivery) {
        self.status = Some(MemoryStatusLine {
            text: delivery.toast_message().into_owned(),
            is_error: !delivery.success(),
            ticks_remaining: Some(delivery.toast_ticks()),
        });
    }

    /// A status message with a countdown is showing (keeps the animation tick alive).
    pub fn status_is_transient(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.ticks_remaining.is_some())
    }

    /// Advance a transient status message by one animation tick.
    /// Returns `true` if it just expired (a redraw is needed to erase it).
    pub fn tick_status(&mut self) -> bool {
        let Some(ticks) = self
            .status
            .as_mut()
            .and_then(|s| s.ticks_remaining.as_mut())
        else {
            return false;
        };
        if *ticks == 0 {
            self.status = None;
            return true;
        }
        *ticks -= 1;
        false
    }

    fn clear_text_drag(&mut self) {
        self.pending_press = None;
        self.text_drag = None;
    }

    fn preview_endpoint_clamped(&self, column: u16, row: u16) -> Option<TextEndpoint> {
        endpoint_at_clamped(
            &self.preview_plain_lines,
            self.preview_text_area,
            self.preview_scroll,
            column,
            row,
        )
    }

    fn scroll_preview_by(&mut self, delta: isize) {
        let max = self
            .preview_total_lines
            .saturating_sub(self.preview_text_area.height as usize);
        self.preview_scroll = self.preview_scroll.saturating_add_signed(delta).min(max);
        // Content moves under a still pointer; drop an unfinished gesture.
        self.clear_text_drag();
    }

    pub fn selected_entry(&self) -> Option<&MemoryFileEntry> {
        self.filtered_cache
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
    }

    /// Advance `selected` past any leading headers to the first selectable entry.
    fn advance_past_headers(&mut self) {
        let filtered = &self.filtered_cache;
        for (i, &orig) in filtered.iter().enumerate() {
            if self.entries.get(orig).is_some_and(|e| !e.is_header) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn select_next(&mut self) {
        if self.advance_next() {
            self.load_preview();
        }
    }

    pub fn select_prev(&mut self) {
        if self.advance_prev() {
            self.load_preview();
        }
    }

    /// Move `selected` forward to the next non-header entry without loading preview.
    /// Returns `true` if the selection changed.
    fn advance_next(&mut self) -> bool {
        let filtered = &self.filtered_cache;
        let mut next = self.selected + 1;
        while next < filtered.len() {
            if filtered
                .get(next)
                .and_then(|&i| self.entries.get(i))
                .is_some_and(|e| !e.is_header)
            {
                self.selected = next;
                return true;
            }
            next += 1;
        }
        false
    }

    /// Move `selected` backward to the previous non-header entry without loading preview.
    /// Returns `true` if the selection changed.
    fn advance_prev(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        let filtered = &self.filtered_cache;
        let mut prev = self.selected - 1;
        loop {
            if filtered
                .get(prev)
                .and_then(|&i| self.entries.get(i))
                .is_some_and(|e| !e.is_header)
            {
                self.selected = prev;
                return true;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
        false
    }

    /// Select the entry at a given filtered index (used by mouse click).
    /// Skips headers.
    /// Returns `true` if the selection changed.
    pub fn select_at(&mut self, filt_idx: usize) -> bool {
        let filtered = &self.filtered_cache;
        if filt_idx >= filtered.len() {
            return false;
        }
        if filtered
            .get(filt_idx)
            .and_then(|&i| self.entries.get(i))
            .is_some_and(|e| e.is_header)
        {
            return false;
        }
        if self.selected == filt_idx {
            return false;
        }
        self.selected = filt_idx;
        self.load_preview();
        true
    }

    pub fn clamp_selected(&mut self) {
        let filtered = &self.filtered_cache;
        if filtered.is_empty() {
            self.selected = 0;
            self.preview_markdown = None;
            return;
        }
        if self.selected >= filtered.len() {
            self.selected = filtered.len() - 1;
        }
        if filtered
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .is_some_and(|e| e.is_header)
        {
            // Try forward.
            for (i, &orig) in filtered.iter().enumerate().skip(self.selected + 1) {
                if self.entries.get(orig).is_some_and(|e| !e.is_header) {
                    self.selected = i;
                    self.load_preview();
                    return;
                }
            }
            // Try backward.
            for (i, &orig) in filtered.iter().enumerate().take(self.selected).rev() {
                if self.entries.get(orig).is_some_and(|e| !e.is_header) {
                    self.selected = i;
                    self.load_preview();
                    return;
                }
            }
        }
        self.load_preview();
    }

    fn load_preview(&mut self) {
        self.preview_scroll = 0;
        self.preview_hash = None;
        self.status = None;
        self.preview_plain_width = 0;
        self.clear_text_drag();
        self.preview_jump_pending = !self.query().is_empty();
        let path = self
            .selected_entry()
            .filter(|e| !e.is_header)
            .map(|e| e.path.clone());
        self.preview_markdown = path.and_then(|path| match read_note(&path, MAX_PREVIEW_BYTES) {
            NoteRead::Text(text) => {
                self.preview_hash = Some(blake3::hash(text.as_bytes()).to_hex().to_string());
                Some(MarkdownContent::new(text))
            }
            NoteRead::TooLarge => Some(MarkdownContent::new("*(File too large to preview)*")),
            NoteRead::Unreadable => None,
        });
    }
}

/// Lower-cased whitespace-separated filter terms; every term must match.
fn query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Line to scroll the preview to for `query`: the first line containing every term, else the first
/// containing the longest term (a short common word alone would pin the preview to the top).
fn first_match_line(lines: &[String], query: &str) -> Option<usize> {
    let terms = query_terms(query);
    let longest = terms.iter().max_by_key(|t| t.len())?;
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    lowered
        .iter()
        .position(|line| terms.iter().all(|t| line.contains(t.as_str())))
        .or_else(|| {
            lowered
                .iter()
                .position(|line| line.contains(longest.as_str()))
        })
}

/// Compute filtered indices from entries and query terms. An entry matches when every term occurs
/// in its label, scope name, or (cached) note contents.
/// Section headers are preserved only when at least one entry in the section matches.
fn compute_filtered(
    entries: &[MemoryFileEntry],
    terms: &[String],
    contents: &HashMap<PathBuf, Option<String>>,
) -> Vec<usize> {
    if terms.is_empty() {
        return (0..entries.len()).collect();
    }
    let matches = |entry: &MemoryFileEntry| {
        let label = entry.label.to_lowercase();
        let source = entry.source.to_lowercase();
        let content = contents.get(&entry.path).and_then(Option::as_deref);
        terms.iter().all(|term| {
            label.contains(term.as_str())
                || source.contains(term.as_str())
                || content.is_some_and(|c| c.contains(term.as_str()))
        })
    };
    let mut result = Vec::new();
    let mut pending_header: Option<usize> = None;
    for (i, entry) in entries.iter().enumerate() {
        if entry.is_header {
            pending_header = Some(i);
        } else if matches(entry) {
            if let Some(h) = pending_header.take() {
                result.push(h);
            }
            result.push(i);
        }
    }
    result
}

pub fn build_entries(
    files: Vec<xai_grok_shell::extensions::notification::MemoryFileInfo>,
) -> Vec<MemoryFileEntry> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut global = Vec::new();
    let mut workspace = Vec::new();
    let mut session = Vec::new();

    for f in files {
        let label = entry_label(&f);
        let bucket = match f.source.as_str() {
            "global" => &mut global,
            "workspace" => &mut workspace,
            _ => &mut session,
        };
        bucket.push(MemoryFileEntry {
            label,
            size_text: crate::util::format_bytes(f.size_bytes),
            age_text: format_modified(f.modified_epoch_secs, now_secs),
            path: f.path.into(),
            source: f.source,
            is_header: false,
            generated: f.generated,
            size_bytes: f.size_bytes,
        });
    }

    // Session logs: reverse chronological (newest first).
    session.reverse();

    let mut entries = Vec::new();
    let mut push_section = |label: &str, items: Vec<MemoryFileEntry>| {
        if !items.is_empty() {
            entries.push(MemoryFileEntry {
                path: PathBuf::new(),
                source: String::new(),
                size_text: String::new(),
                age_text: String::new(),
                label: label.to_string(),
                is_header: true,
                generated: false,
                size_bytes: 0,
            });
            entries.extend(items);
        }
    };
    push_section("Global", global);
    push_section("Workspace", workspace);
    push_section("Sessions", session);
    entries
}

pub fn render_memory_modal(
    buf: &mut Buffer,
    full_area: Rect,
    state: &mut MemoryModalState,
    compact: bool,
) {
    let theme = Theme::current();
    let shortcuts = build_shortcuts(state);

    let modal_config = ModalWindowConfig {
        title: "Memory",
        tabs: None,
        shortcuts: &shortcuts,
        sizing: if state.fullscreen {
            ModalSizing {
                width_pct: 1.0,
                max_width: u16::MAX,
                min_width: 44,
                v_margin: 0,
                h_pad: 2,
                v_pad: 0,
                footer_lines: 2,
            }
        } else {
            ModalSizing {
                width_pct: 0.75,
                max_width: 140,
                min_width: 44,
                v_margin: 3,
                h_pad: 2,
                v_pad: 1,
                footer_lines: 2,
            }
        }
        .with_compact(compact),
        fold_info: None,
    };

    let Some(ModalContentArea {
        content: content_area,
        ..
    }) =
        modal_window::render_modal_window(buf, full_area, &mut state.window, &modal_config, &theme)
    else {
        return;
    };

    if content_area.height < 2 || content_area.width < 10 {
        return;
    }

    if state.shows_notice() {
        // No list or preview: clear the hit-test rects so stale mouse clicks do nothing.
        state.list_area = Rect::default();
        state.preview_area = Rect::default();
        state.preview_text_area = Rect::default();
        state.list_scrollbar_area = None;
        state.preview_scrollbar_area = None;
        let markdown = if state.memory_enabled {
            empty_state_markdown(state.capture_enabled, state.dream_enabled)
        } else {
            disabled_state_markdown(state.disabled_reason).to_owned()
        };
        let status_rows = render_status_line(buf, content_area, state, &theme);
        let notice_area = Rect {
            height: content_area.height.saturating_sub(status_rows),
            ..content_area
        };
        render_notice(buf, notice_area, &markdown, &theme);
        return;
    }

    let split = content_area.width >= SPLIT_MIN_WIDTH;
    state.split_shown = split;
    let preview_only = !split && state.mode == MemoryModalMode::PreviewFocused;

    if preview_only {
        state.list_area = Rect::default();
        state.list_scrollbar_area = None;
        let status_rows = render_status_line(buf, content_area, state, &theme);
        let preview_area = Rect {
            height: content_area.height.saturating_sub(status_rows),
            ..content_area
        };
        state.preview_area = preview_area;
        render_preview(buf, preview_area, state, &theme);
        return;
    }

    let list_width = if split {
        (content_area.width as f64 * LIST_WIDTH_RATIO) as u16
    } else {
        content_area.width
    };

    let list_area = Rect {
        x: content_area.x,
        y: content_area.y,
        width: list_width,
        height: content_area.height,
    };
    state.list_area = list_area;

    render_file_list(buf, list_area, state, &theme);

    if split {
        let preview_x = content_area.x + list_width + 1;
        let preview_width = content_area.width.saturating_sub(list_width + 1);
        if preview_width > 2 {
            let sep_x = content_area.x + list_width;
            let sep_style = Style::default().fg(theme.gray_dim);
            for y in content_area.y..content_area.y + content_area.height {
                if let Some(cell) = buf.cell_mut((sep_x, y)) {
                    cell.set_symbol("\u{2502}");
                    cell.set_style(sep_style);
                }
            }

            let preview_area = Rect {
                x: preview_x,
                y: content_area.y,
                width: preview_width,
                height: content_area.height,
            };
            state.preview_area = preview_area;
            render_preview(buf, preview_area, state, &theme);
        } else {
            state.preview_area = Rect::default();
            state.preview_text_area = Rect::default();
            state.preview_scrollbar_area = None;
        }
    } else {
        state.preview_area = Rect::default();
        state.preview_text_area = Rect::default();
        state.preview_scrollbar_area = None;
    }
}

fn render_notice(buf: &mut Buffer, area: Rect, markdown: &str, theme: &Theme) {
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let width = area.width.min(NOTICE_MAX_WIDTH);
    if width == 0 {
        return;
    }
    let content = MarkdownContent::new(markdown);
    content.with_wrapped_lines(width as usize, |wrapped| {
        for (row, line) in wrapped.lines.iter().take(area.height as usize).enumerate() {
            buf.set_line_safe(area.x, area.y + row as u16, line, width);
        }
    });
}

/// Returns the number of rows used (0 or 1).
fn render_status_line(
    buf: &mut Buffer,
    area: Rect,
    state: &MemoryModalState,
    theme: &Theme,
) -> u16 {
    let Some(status) = state.status.as_ref() else {
        return 0;
    };
    if area.height < 3 {
        return 0;
    }
    let fg = if status.is_error {
        theme.accent_error
    } else {
        theme.accent_user
    };
    let text = crate::render::line_utils::truncate_str(&status.text, area.width as usize);
    buf.set_span(
        area.x,
        area.y + area.height - 1,
        &Span::styled(
            text.as_str(),
            Style::default()
                .fg(fg)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD),
        ),
        area.width,
    );
    1
}

fn render_file_list(buf: &mut Buffer, area: Rect, state: &mut MemoryModalState, theme: &Theme) {
    let search_y = area.y;
    let filter_focused = matches!(state.mode, MemoryModalMode::FilterFocused);
    let viewport = state.query.viewport(area.width as usize);
    if state.query().is_empty() {
        let placeholder = if filter_focused {
            "type to filter..."
        } else {
            "/ to filter..."
        };
        buf.set_span(
            area.x,
            search_y,
            &Span::styled(
                placeholder,
                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
            ),
            area.width,
        );
    } else {
        let leading;
        let visible = if filter_focused {
            state
                .query()
                .get(viewport.visible_byte_range.clone())
                .unwrap_or("")
        } else {
            leading = crate::render::line_utils::truncate_str(state.query(), area.width as usize);
            &leading
        };
        buf.set_span(
            area.x,
            search_y,
            &Span::styled(
                visible,
                Style::default().fg(theme.text_primary).bg(theme.bg_base),
            ),
            area.width,
        );
    }
    if filter_focused {
        let cursor_x = area.x + viewport.cursor_display_column as u16;
        if cursor_x < area.x + area.width
            && let Some(cell) = buf.cell_mut((cursor_x, search_y))
        {
            cell.set_style(theme.block_cursor_over(theme.bg_base));
        }
    }

    let status_rows = render_status_line(buf, area, state, theme);
    let entries_start_y = search_y + 1;
    let available_height = area.height.saturating_sub(1 + status_rows).max(1) as usize;
    if state.selected < state.scroll_offset {
        state.scroll_offset = state.selected;
    }
    if state.selected >= state.scroll_offset + available_height {
        state.scroll_offset = state.selected.saturating_sub(available_height - 1);
    }

    // Compute scrollbar area before borrowing filtered_cache.
    let total_entries = sat_u16(state.filtered_indices().len());
    let sb_area = if total_entries > available_height as u16 && area.width > 4 {
        Some(Rect {
            x: area.x + area.width - 1,
            y: entries_start_y,
            width: 1,
            height: available_height as u16,
        })
    } else {
        None
    };
    state.list_scrollbar_area = sb_area;
    let content_width = if sb_area.is_some() {
        area.width.saturating_sub(2)
    } else {
        area.width
    };

    if state.filter_has_no_matches() {
        render_no_matches(buf, area, entries_start_y, state.query(), theme);
        return;
    }

    // Labels truncate before the metadata column so long names cannot run into it.
    let narrow = content_width < NARROW_LIST_WIDTH;
    let meta_col_w = state
        .entries
        .iter()
        .filter(|e| !e.is_header)
        .map(|e| e.meta_text(narrow).width())
        .max()
        .unwrap_or(0);
    let filtered = state.filtered_indices();
    let end = filtered.len().min(state.scroll_offset + available_height);
    let visible = filtered.get(state.scroll_offset..end).unwrap_or(&[]);

    for (row, &orig_idx) in visible.iter().enumerate() {
        let y = entries_start_y + row as u16;
        if y >= area.y + area.height {
            break;
        }
        let Some(entry) = state.entries.get(orig_idx) else {
            continue;
        };
        let filt_idx = state.scroll_offset + row;
        let is_selected = filt_idx == state.selected;

        if entry.is_header {
            let header_style = Style::default()
                .fg(theme.accent_user)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD);
            let line = Line::from(Span::styled(&entry.label, header_style));
            buf.set_line(area.x, y, &line, content_width);
        } else {
            let bg = if is_selected {
                theme.bg_visual
            } else {
                theme.bg_base
            };
            let label_style = Style::default().fg(theme.text_primary).bg(bg);
            let meta_style = Style::default().fg(theme.gray).bg(bg);

            let row_rect = Rect {
                x: area.x,
                y,
                width: content_width,
                height: 1,
            };
            buf.set_style(row_rect, Style::default().bg(bg));

            // 1 col left pad, 2 col gap before the metadata column, 1 col right pad.
            let max_label_w = (content_width as usize).saturating_sub(meta_col_w + 4);
            let truncated_label: Cow<str> = if entry.label.width() > max_label_w {
                let trunc = truncate_to_width(&entry.label, max_label_w.saturating_sub(1));
                format!("{trunc}\u{2026}").into()
            } else {
                Cow::Borrowed(&entry.label)
            };
            buf.set_span(
                area.x + 1,
                y,
                &Span::styled(truncated_label.as_ref(), label_style),
                content_width.saturating_sub(1),
            );

            if is_selected
                && matches!(state.mode, MemoryModalMode::ConfirmingDelete { idx } if idx == filt_idx)
            {
                let hint = " [x to confirm]";
                let hint_w = hint.len() as u16;
                let hint_x = (area.x + content_width).saturating_sub(hint_w + 1);
                buf.set_span(
                    hint_x,
                    y,
                    &Span::styled(hint, Style::default().fg(theme.accent_error).bg(bg)),
                    hint_w,
                );
            } else {
                let meta = entry.meta_text(narrow);
                let meta_w = meta.width() as u16;
                let meta_x = (area.x + content_width).saturating_sub(meta_w + 1);
                if meta_x > area.x + 1 {
                    buf.set_span(meta_x, y, &Span::styled(meta.as_str(), meta_style), meta_w);
                }
            }

            // Terminal theme (Reset bands): reverse video; no-op on RGB.
            if is_selected {
                buf.set_style(row_rect, theme.selection_overlay());
            }
        }
    }

    // List scrollbar.
    render_scrollbar(
        buf,
        sb_area,
        total_entries,
        sat_u16(available_height),
        sat_u16(state.scroll_offset),
        false,
    );
}

/// Shown under the query row when the filter matches nothing.
fn render_no_matches(buf: &mut Buffer, area: Rect, y: u16, query: &str, theme: &Theme) {
    let width = area.width as usize;
    let style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
    let lines = [
        format!("No notes match \u{201C}{query}\u{201D}"),
        "Backspace clears the filter".to_owned(),
    ];
    for (row, text) in lines.iter().enumerate() {
        let y = y + row as u16;
        if y >= area.y + area.height {
            break;
        }
        let text = crate::render::line_utils::truncate_str(text, width);
        buf.set_span(area.x, y, &Span::styled(text.as_str(), style), area.width);
    }
}

fn render_preview(buf: &mut Buffer, area: Rect, state: &mut MemoryModalState, theme: &Theme) {
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let Some(markdown) = state.preview_markdown.as_ref() else {
        state.preview_total_lines = 0;
        state.preview_scrollbar_area = None;
        state.preview_text_area = Rect::default();
        state.preview_plain_lines.clear();
        state.preview_plain_width = 0;
        // With a filter that matches nothing the list already explains; an empty pane is clearer.
        if !state.filter_has_no_matches() {
            let msg = "No file selected";
            let style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
            let cy = area.y + area.height / 2;
            let cx = area.x + area.width.saturating_sub(msg.width() as u16) / 2;
            buf.set_span(cx, cy, &Span::styled(msg, style), area.width);
        }
        return;
    };

    // Reserve scrollbar column if needed (computed after first pass).
    // We do a two-pass approach: first compute total at full width to decide if scrollbar is needed, then re-compute at narrowed width if so
    let full_width = area.width as usize;
    if full_width == 0 {
        return;
    }

    let total_at_full = markdown.with_wrapped_lines(full_width, |w| w.lines.len());
    let visible = area.height as usize;

    let (content_width, sb_area) = if total_at_full > visible && area.width > 4 {
        let sb = Rect {
            x: area.x + area.width - 1,
            y: area.y,
            width: 1,
            height: area.height,
        };
        (full_width.saturating_sub(2), Some(sb))
    } else {
        (full_width, None)
    };

    // Plain text of the wrapped lines drives drag-select and the jump to the first match.
    if state.preview_plain_width != content_width as u16 {
        state.preview_plain_lines = markdown.with_wrapped_lines(content_width, |wrapped| {
            wrapped
                .lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        });
        state.preview_plain_width = content_width as u16;
        state.pending_press = None;
        state.text_drag = None;
    }
    let total = state.preview_plain_lines.len();

    if state.preview_jump_pending {
        state.preview_jump_pending = false;
        if let Some(line) = first_match_line(&state.preview_plain_lines, state.query.text()) {
            state.preview_scroll = line;
        }
    }

    state.preview_total_lines = total;
    state.preview_scrollbar_area = sb_area;
    state.preview_text_area = Rect {
        width: content_width as u16,
        ..area
    };
    state.preview_scroll = state.preview_scroll.min(total.saturating_sub(visible));
    let scroll = state.preview_scroll;

    markdown.with_wrapped_lines(content_width, |wrapped| {
        for (row, line_idx) in (scroll..total.min(scroll + visible)).enumerate() {
            let y = area.y + row as u16;
            if let Some(line) = wrapped.lines.get(line_idx) {
                buf.set_line_safe(area.x, y, line, content_width as u16);
            }
        }
    });

    if let Some(drag) = state.text_drag {
        paint_text_drag(
            drag,
            &state.preview_plain_lines,
            state.preview_text_area,
            scroll,
            buf,
            theme,
        );
    }

    render_scrollbar(
        buf,
        sb_area,
        sat_u16(total),
        sat_u16(visible),
        sat_u16(scroll),
        false,
    );
}

pub fn handle_memory_key(state: &mut MemoryModalState, key: &KeyEvent) -> InputOutcome {
    if key.kind == KeyEventKind::Release {
        return InputOutcome::Unchanged;
    }

    // ConfirmingDelete has an "any key cancel" contract; handle it first so no other handler can silently swallow the key
    if let MemoryModalMode::ConfirmingDelete { idx } = state.mode {
        state.mode = MemoryModalMode::Browse;
        state.status = None;
        if key.code == KeyCode::Char('x')
            && let Some(&orig_idx) = state.filtered_indices().get(idx)
            && let Some(entry) = state.entries.get(orig_idx)
            && let Some(hash) = state.preview_hash.clone()
        {
            // The row stays until the shell confirms; the store may refuse.
            let path = entry.path.clone();
            state.pending_delete = Some(path.clone());
            state.status = Some(MemoryStatusLine {
                text: format!("Deleting {}…", entry.label),
                is_error: false,
                ticks_remaining: None,
            });
            return InputOutcome::Action(Action::MemoryForget {
                path: path.to_string_lossy().into_owned(),
                expected_content_hash: hash,
            });
        }
        return InputOutcome::Changed;
    }

    match state.mode {
        MemoryModalMode::ConfirmingDelete { .. } => unreachable!("handled above"),
        MemoryModalMode::FilterFocused => handle_filter_focused(state, key),
        MemoryModalMode::PreviewFocused => handle_preview_focused(state, key),
        MemoryModalMode::Browse => handle_browse(state, key),
    }
}

/// Keys while the preview has focus: scroll the note; Esc or Enter return to the list.
fn handle_preview_focused(state: &mut MemoryModalState, key: &KeyEvent) -> InputOutcome {
    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.fullscreen = !state.fullscreen;
        return InputOutcome::Action(Action::PersistMemoryFullscreen(state.fullscreen));
    }
    let page = state.preview_text_area.height.max(1) as isize;
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            state.mode = MemoryModalMode::Browse;
            InputOutcome::Changed
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.scroll_preview_by(1);
            InputOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll_preview_by(-1);
            InputOutcome::Changed
        }
        KeyCode::PageDown => {
            state.scroll_preview_by(page);
            InputOutcome::Changed
        }
        KeyCode::PageUp => {
            state.scroll_preview_by(-page);
            InputOutcome::Changed
        }
        KeyCode::Home => {
            state.scroll_preview_by(isize::MIN);
            InputOutcome::Changed
        }
        KeyCode::End => {
            state.scroll_preview_by(isize::MAX);
            InputOutcome::Changed
        }
        _ => InputOutcome::Unchanged,
    }
}

pub fn handle_memory_paste(state: &mut MemoryModalState, text: &str) -> InputOutcome {
    if state.mode != MemoryModalMode::FilterFocused {
        return InputOutcome::Unchanged;
    }
    let outcome = state.query.insert_paste(text);
    finish_filter_edit(state, outcome)
}

/// Saturating cast from `usize` to `u16` (caps at `u16::MAX`).
fn sat_u16(v: usize) -> u16 {
    v.min(u16::MAX as usize) as u16
}

/// After a list scrollbar jump, select the first visible non-header entry.
/// `render_file_list`'s scroll-clamping then doesn't snap `scroll_offset` back to the old selection.
fn select_first_visible(state: &mut MemoryModalState) {
    let filtered = state.filtered_indices();
    let visible_start = state.scroll_offset;
    let visible_end = filtered
        .len()
        .min(visible_start + state.list_area.height.saturating_sub(1) as usize);
    for (i, &orig) in filtered
        .iter()
        .enumerate()
        .take(visible_end)
        .skip(visible_start)
    {
        if state.entries.get(orig).is_some_and(|e| !e.is_header) {
            state.selected = i;
            state.load_preview();
            return;
        }
    }
}

/// Apply a scrollbar click/drag at `screen_row` within `sb_area`, updating `offset`.
fn apply_scrollbar_jump(
    screen_row: u16,
    sb_area: Rect,
    total_lines: u16,
    viewport_lines: u16,
    offset: &mut usize,
) {
    let cell_index = screen_row.saturating_sub(sb_area.y);
    let result = scrollbar_click_to_offset(cell_index, sb_area.height, total_lines, viewport_lines);
    let max_scroll = (total_lines as usize).saturating_sub(viewport_lines as usize);
    match result {
        ScrollbarClickResult::Top => *offset = 0,
        ScrollbarClickResult::Bottom => *offset = max_scroll,
        ScrollbarClickResult::Offset(o) => *offset = o.min(max_scroll),
    }
}

/// Handle mouse events for the memory modal content area.
pub fn handle_memory_mouse(
    state: &mut MemoryModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> InputOutcome {
    if state.shows_notice() {
        return InputOutcome::Unchanged;
    }
    // Mouse input can move the selection and reload the preview hash; a pending confirm must
    // not survive that, or the next `x` would pair the old row with the new hash.
    if matches!(state.mode, MemoryModalMode::ConfirmingDelete { .. }) {
        state.mode = MemoryModalMode::Browse;
        state.status = None;
    }
    let in_rect = |r: Rect| -> bool {
        r.width > 0
            && r.height > 0
            && column >= r.x
            && column < r.x + r.width
            && row >= r.y
            && row < r.y + r.height
    };

    let on_list = in_rect(state.list_area);
    let on_preview = in_rect(state.preview_area);
    let on_preview_text = in_rect(state.preview_text_area);
    let on_list_sb = state.list_scrollbar_area.is_some_and(&in_rect);
    let on_preview_sb = state.preview_scrollbar_area.is_some_and(&in_rect);

    match kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            if on_preview_text || state.text_drag.is_some() || state.pending_press.is_some() {
                return handle_preview_drag(state, kind, column, row);
            }
            state.clear_text_drag();
            // Click/drag on list scrollbar: jump-scroll and select nearest entry
            if on_list_sb {
                let sb = state.list_scrollbar_area.unwrap();
                let total = sat_u16(state.filtered_indices().len());
                let visible = state.list_area.height.saturating_sub(1);
                apply_scrollbar_jump(row, sb, total, visible, &mut state.scroll_offset);
                select_first_visible(state);
                return InputOutcome::Changed;
            }

            // Click/drag on preview scrollbar: jump-scroll
            if on_preview_sb {
                let sb = state.preview_scrollbar_area.unwrap();
                let total = sat_u16(state.preview_total_lines);
                let visible = state.preview_area.height;
                apply_scrollbar_jump(row, sb, total, visible, &mut state.preview_scroll);
                return InputOutcome::Changed;
            }

            // Click on a file list row to select it (not on drag).
            if matches!(kind, MouseEventKind::Down(_)) && on_list {
                let entries_start_y = state.list_area.y + 1;
                if row >= entries_start_y {
                    let clicked_row = (row - entries_start_y) as usize;
                    let filt_idx = state.scroll_offset + clicked_row;
                    if state.select_at(filt_idx) {
                        return InputOutcome::Changed;
                    }
                }
            }

            InputOutcome::Unchanged
        }

        MouseEventKind::ScrollDown => {
            if on_list || on_list_sb {
                let mut moved = false;
                for _ in 0..3 {
                    moved |= state.advance_next();
                }
                if moved {
                    state.load_preview();
                }
                return InputOutcome::Changed;
            }
            if on_preview || on_preview_sb {
                state.scroll_preview_by(3);
                return InputOutcome::Changed;
            }
            InputOutcome::Unchanged
        }

        MouseEventKind::ScrollUp => {
            if on_list || on_list_sb {
                let mut moved = false;
                for _ in 0..3 {
                    moved |= state.advance_prev();
                }
                if moved {
                    state.load_preview();
                }
                return InputOutcome::Changed;
            }
            if on_preview || on_preview_sb {
                state.scroll_preview_by(-3);
                return InputOutcome::Changed;
            }
            InputOutcome::Unchanged
        }

        MouseEventKind::Up(MouseButton::Left) => finish_preview_drag(state, column, row),

        MouseEventKind::Moved => {
            // A bare Moved with an active drag is a lost Up: finish it so the band does not
            // linger without copying (same rule as the usage modal).
            if state.text_drag.is_some() {
                return finish_preview_drag(state, column, row);
            }
            InputOutcome::Unchanged
        }

        MouseEventKind::Down(_) => {
            // A non-left press ends a stuck drag.
            if state.text_drag.take().is_some() || state.pending_press.take().is_some() {
                InputOutcome::Changed
            } else {
                InputOutcome::Unchanged
            }
        }

        _ => InputOutcome::Unchanged,
    }
}

/// Left press / drag over the preview text. A press is held until a 1-cell move promotes it to a
/// drag (same threshold as scrollback); the drag auto-scrolls when the pointer leaves the pane.
fn handle_preview_drag(
    state: &mut MemoryModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> InputOutcome {
    let rect = state.preview_text_area;
    if matches!(kind, MouseEventKind::Down(_)) {
        let Some(endpoint) = endpoint_at(
            &state.preview_plain_lines,
            rect,
            state.preview_scroll,
            column,
            row,
        ) else {
            return InputOutcome::Unchanged;
        };
        state.pending_press = Some(PendingPress {
            column,
            row,
            endpoint,
        });
        state.text_drag = None;
        return InputOutcome::Changed;
    }
    if let Some(pending) = state.pending_press {
        if pending.column == column && pending.row == row {
            return InputOutcome::Unchanged;
        }
        state.pending_press = None;
        state.text_drag = Some(TextDrag {
            anchor: pending.endpoint,
            head: pending.endpoint,
        });
    }
    if state.text_drag.is_none() {
        return InputOutcome::Unchanged;
    }
    if rect.width == 0 || rect.height == 0 {
        state.clear_text_drag();
        return InputOutcome::Changed;
    }
    if row < rect.y {
        state.preview_scroll = state.preview_scroll.saturating_sub(1);
    } else if row >= rect.y.saturating_add(rect.height) {
        let max = state
            .preview_total_lines
            .saturating_sub(rect.height as usize);
        state.preview_scroll = state.preview_scroll.saturating_add(1).min(max);
    }
    let head = state.preview_endpoint_clamped(column, row);
    if let (Some(head), Some(drag)) = (head, state.text_drag.as_mut()) {
        drag.head = head;
    }
    InputOutcome::Changed
}

/// Mouse-up (or a lost Up) over the preview: a non-empty drag copies its text.
fn finish_preview_drag(state: &mut MemoryModalState, column: u16, row: u16) -> InputOutcome {
    if state.pending_press.take().is_some() {
        return InputOutcome::Changed;
    }
    let Some(mut drag) = state.text_drag.take() else {
        return InputOutcome::Unchanged;
    };
    if let Some(head) = state.preview_endpoint_clamped(column, row) {
        drag.head = head;
    }
    if drag.is_non_empty()
        && let Some(text) = text_for_drag(
            drag,
            &state.preview_plain_lines,
            state.preview_text_area.width,
        )
    {
        return InputOutcome::Action(Action::MemoryCopy { text });
    }
    InputOutcome::Changed
}

/// Keys while the filter input is focused: all chars go to the filter, only Escape exits filter mode.
/// Arrow keys still navigate the list.
fn handle_filter_focused(state: &mut MemoryModalState, key: &KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Esc => {
            state.mode = MemoryModalMode::Browse;
            InputOutcome::Changed
        }
        KeyCode::Down => {
            state.select_next();
            InputOutcome::Changed
        }
        KeyCode::Up => {
            state.select_prev();
            InputOutcome::Changed
        }
        _ => {
            let outcome = state.query.handle_key(key);

            finish_filter_edit(state, outcome)
        }
    }
}

fn finish_filter_edit(state: &mut MemoryModalState, outcome: LineEditOutcome) -> InputOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            state.invalidate_filter();
            state.clamp_selected();
            InputOutcome::Changed
        }
        LineEditOutcome::CursorChanged | LineEditOutcome::HandledNoChange => InputOutcome::Changed,
        LineEditOutcome::Unhandled => InputOutcome::Unchanged,
    }
}

/// Keys in normal browse mode: single-char hotkeys active, `/` enters filter mode.
fn handle_browse(state: &mut MemoryModalState, key: &KeyEvent) -> InputOutcome {
    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.fullscreen = !state.fullscreen;
        return InputOutcome::Action(Action::PersistMemoryFullscreen(state.fullscreen));
    }
    // A notice hides the list; only the toggle may act on it (nav/copy/delete would hit hidden entries).
    if state.shows_notice() && key.code != KeyCode::Char('t') {
        return InputOutcome::Unchanged;
    }
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            state.select_next();
            InputOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.select_prev();
            InputOutcome::Changed
        }
        KeyCode::PageDown => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_next();
            }
            if moved {
                state.load_preview();
            }
            InputOutcome::Changed
        }
        KeyCode::PageUp => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_prev();
            }
            if moved {
                state.load_preview();
            }
            InputOutcome::Changed
        }
        KeyCode::Char('x') if key.modifiers.is_empty() => {
            if state.pending_delete.is_some() {
                return InputOutcome::Unchanged;
            }
            let Some(entry) = state.selected_entry().filter(|e| e.is_deletable()) else {
                return InputOutcome::Unchanged;
            };
            if state.preview_hash.is_none() {
                state.status = Some(MemoryStatusLine {
                    text: "Can't delete: this note couldn't be read for verification.".to_owned(),
                    is_error: true,
                    ticks_remaining: None,
                });
                return InputOutcome::Changed;
            }
            let scope = match entry.source.as_str() {
                "session" => "session logs".to_owned(),
                source => format!("{source} memory"),
            };
            state.status = Some(MemoryStatusLine {
                text: format!(
                    "Delete {} from {scope}? Dream may re-derive it from future sessions. x confirm · any other key cancels",
                    entry.label
                ),
                is_error: false,
                ticks_remaining: None,
            });
            state.mode = MemoryModalMode::ConfirmingDelete {
                idx: state.selected,
            };
            InputOutcome::Changed
        }
        KeyCode::Char('y') if key.modifiers.is_empty() => {
            let path = state
                .selected_entry()
                .filter(|e| !e.is_header)
                .map(|e| e.path.to_string_lossy().into_owned());
            match path {
                Some(text) => InputOutcome::Action(Action::MemoryCopy { text }),
                None => InputOutcome::Unchanged,
            }
        }
        KeyCode::Enter => {
            if state.preview_markdown.is_none() {
                return InputOutcome::Unchanged;
            }
            state.mode = MemoryModalMode::PreviewFocused;
            InputOutcome::Changed
        }
        KeyCode::Char('t') if key.modifiers.is_empty() => {
            if state.pending_toggle.is_some() || !(state.memory_enabled || state.can_enable()) {
                return InputOutcome::Unchanged;
            }
            // Optimistic flip; the reply's listing resyncs, or `apply_toggle_result` reverts.
            state.pending_toggle = Some((state.memory_enabled, state.disabled_reason));
            let enabled = !state.memory_enabled;
            state.memory_enabled = enabled;
            state.disabled_reason = (!enabled).then_some(MemoryDisabledReason::SessionToggle);
            InputOutcome::Action(Action::MemoryToggle { enabled })
        }
        // `i` aliases `/` (vim-nav "press i to search").
        KeyCode::Char('/') | KeyCode::Char('i') if key.modifiers.is_empty() => {
            state.mode = MemoryModalMode::FilterFocused;
            InputOutcome::Changed
        }
        KeyCode::Backspace => {
            if state.query.delete_last_grapheme() == LineEditOutcome::TextChanged {
                state.invalidate_filter();
                state.clamp_selected();
                InputOutcome::Changed
            } else {
                InputOutcome::Unchanged
            }
        }
        _ => InputOutcome::Unchanged,
    }
}

fn build_shortcuts(state: &MemoryModalState) -> Vec<Shortcut<'static>> {
    let plain = |label: &'static str| Shortcut {
        label,
        clickable: false,
        id: 0,
    };
    let fullscreen_label = if state.fullscreen {
        "^F normal"
    } else {
        "^F fullscreen"
    };
    match &state.mode {
        MemoryModalMode::Browse if !state.memory_enabled => {
            let mut shortcuts = Vec::new();
            if state.can_enable() {
                shortcuts.push(plain("t turn on"));
            }
            shortcuts.push(plain(fullscreen_label));
            shortcuts.push(plain("Esc close"));
            shortcuts
        }
        MemoryModalMode::Browse if !state.has_notes() => {
            vec![
                plain("t turn off"),
                plain(fullscreen_label),
                plain("Esc close"),
            ]
        }
        MemoryModalMode::PreviewFocused => vec![
            plain("\u{2191}/\u{2193} scroll"),
            plain("drag to copy"),
            plain(if state.split_shown {
                "Esc list"
            } else {
                "Esc back"
            }),
            plain(fullscreen_label),
        ],
        MemoryModalMode::Browse => {
            let mut shortcuts = vec![
                plain("\u{2191}/\u{2193} nav"),
                plain("/ search"),
                plain(if state.split_shown {
                    "Enter read"
                } else {
                    "Enter open"
                }),
                plain("y copy path"),
            ];
            if state.can_delete_selected() {
                shortcuts.push(plain("x delete"));
            }
            shortcuts.extend([
                plain("t turn off"),
                plain(fullscreen_label),
                plain("Esc close"),
            ]);
            // Browse is nav mode (filter inactive), so append `i search` last (matching the shared pickers)
            modal_window::push_vim_nav_search_hint(&mut shortcuts, false);
            shortcuts
        }
        MemoryModalMode::FilterFocused => vec![
            Shortcut {
                label: "type to filter",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc exit filter",
                clickable: false,
                id: 0,
            },
        ],
        MemoryModalMode::ConfirmingDelete { .. } => vec![
            Shortcut {
                label: "x confirm delete",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "any key cancel",
                clickable: false,
                id: 0,
            },
        ],
    }
}

/// Truncate a string to fit within `max_width` display columns.
/// Delegates to `render::line_utils::byte_offset_at_width` to avoid duplicating the Unicode-width scanning logic.
fn truncate_to_width(s: &str, max_width: usize) -> &str {
    let offset = crate::render::line_utils::byte_offset_at_width(s, max_width);
    s.get(..offset).unwrap_or("")
}

fn file_label(path: &str) -> String {
    let p = std::path::Path::new(path);
    p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Title from the shell when it has one, else a readable form of a v2 inbox key
/// (`<session>__t000038-000040__n001.md` → `observation, turns 38–40 (#2)`), else the filename.
fn entry_label(f: &xai_grok_shell::extensions::notification::MemoryFileInfo) -> String {
    if let Some(title) = f.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        return title.to_owned();
    }
    let name = file_label(&f.path);
    observation_key_label(&name).unwrap_or(name)
}

fn observation_key_label(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".md")?;
    let mut parts = stem.split("__");
    let _session = parts.next()?;
    let range = parts.next()?.strip_prefix('t')?;
    let ordinal: usize = parts.next()?.strip_prefix('n')?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (from, through) = range.split_once('-')?;
    let (from, through): (u64, u64) = (from.parse().ok()?, through.parse().ok()?);
    let turns = if from == through {
        format!("turn {from}")
    } else {
        format!("turns {from}\u{2013}{through}")
    };
    Some(format!("observation, {turns} (#{})", ordinal + 1))
}

/// Relative age in at most 3 columns (`<1m`, `27m`, `5h`, `99d`, `52w`, `99y`); no suffix, the
/// metadata column position says what it is.
fn format_modified(epoch_secs: Option<u64>, now_secs: u64) -> String {
    let Some(modified) = epoch_secs else {
        return "\u{2014}".to_string();
    };
    let delta = now_secs.saturating_sub(modified);
    if delta < 60 {
        return "<1m".to_string();
    }
    if delta < 3600 {
        return format!("{}m", delta / 60);
    }
    if delta < 86400 {
        return format!("{}h", delta / 3600);
    }
    // Units step up so the result stays within 3 columns.
    let days = delta / 86400;
    if days < 100 {
        return format!("{days}d");
    }
    if days < 365 {
        return format!("{}w", days / 7);
    }
    format!("{}y", days / 365)
}

fn load_fullscreen_pref() -> bool {
    let path =
        xai_grok_tools::util::grok_home::grok_home().join(xai_grok_config::USER_CONFIG_FILENAME);
    let Some(doc) = crate::config_toml_edit::read_config_document_for_edit(&path) else {
        return false;
    };
    doc.get("hints")
        .and_then(|h| h.get("memory_modal_fullscreen"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_modified_relative() {
        let now = 1_700_000_000u64;
        assert_eq!(format_modified(None, now), "\u{2014}");
        assert_eq!(format_modified(Some(now + 5), now), "<1m");
        assert_eq!(format_modified(Some(now - 30), now), "<1m");
        assert_eq!(format_modified(Some(now - 120), now), "2m");
        assert_eq!(format_modified(Some(now - 7200), now), "2h");
        assert_eq!(format_modified(Some(now - 172800), now), "2d");
        // Never wider than 3 columns, so the padded metadata column stays aligned.
        for (secs, expected) in [
            (99 * 86400, "99d"),
            (100 * 86400, "14w"),
            (364 * 86400, "52w"),
            (365 * 86400, "1y"),
            (40 * 365 * 86400, "40y"),
        ] {
            let text = format_modified(Some(now - secs), now);
            assert_eq!(text, expected);
            assert!(text.chars().count() <= 3, "{text}");
        }
    }

    #[test]
    fn observation_labels_prefer_title_then_readable_key() {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let info = |path: &str, title: Option<&str>| MemoryFileInfo {
            path: path.into(),
            source: "workspace".into(),
            size_bytes: 1,
            modified_epoch_secs: None,
            generated: false,
            title: title.map(str::to_owned),
        };
        let key =
            "/s/observations/_inbox/01a0a22b-d97d-7d13-9332-d88f922274b4__t000038-000040__n001.md";
        assert_eq!(
            entry_label(&info(key, Some("memory-v2 /memory command"))),
            "memory-v2 /memory command"
        );
        assert_eq!(
            entry_label(&info(key, Some("  "))),
            "observation, turns 38\u{2013}40 (#2)"
        );
        assert_eq!(
            entry_label(&info(
                "/s/observations/_inbox/x__t000007-000007__n000.md",
                None
            )),
            "observation, turn 7 (#1)"
        );
        assert_eq!(entry_label(&info("/s/topics/anyrun.md", None)), "anyrun.md");
    }

    #[test]
    fn long_labels_stop_short_of_the_metadata_column() {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let mut state = MemoryModalState::new(build_entries(vec![
            MemoryFileInfo {
                path: "/s/topics/a-very-long-topic-name-that-keeps-going-and-going-forever.md"
                    .into(),
                source: "workspace".into(),
                size_bytes: 13_900,
                modified_epoch_secs: None,
                generated: false,
                title: None,
            },
            MemoryFileInfo {
                path: "/s/topics/short.md".into(),
                source: "workspace".into(),
                size_bytes: 254,
                modified_epoch_secs: None,
                generated: false,
                title: None,
            },
        ]));
        let mut buf = Buffer::empty(Rect::new(0, 0, 50, 6));
        let theme = Theme::current();
        render_file_list(&mut buf, Rect::new(0, 0, 50, 6), &mut state, &theme);
        let rows: Vec<String> = buffer_text(&buf).lines().map(str::to_owned).collect();
        let long_row = rows.iter().find(|r| r.contains("a-very-long")).unwrap();
        let short_row = rows.iter().find(|r| r.contains("short.md")).unwrap();
        // Same separator column on both rows, and a gap before it on the truncated row.
        let dot = |r: &str| r.chars().position(|c| c == '\u{00B7}').unwrap();
        assert_eq!(dot(long_row), dot(short_row));
        assert!(long_row.contains("\u{2026}  "), "{long_row:?}");
    }

    #[test]
    fn file_label_extracts_filename() {
        assert_eq!(file_label("/home/user/.grok/memory/MEMORY.md"), "MEMORY.md");
        assert_eq!(
            file_label("/workspace/.grok/memory/sessions/2026-01-15-fix-bug.md"),
            "2026-01-15-fix-bug.md"
        );
    }

    #[test]
    fn build_entries_groups_by_source() {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;

        let files = vec![
            MemoryFileInfo {
                path: "/global/MEMORY.md".into(),
                source: "global".into(),
                size_bytes: 100,
                modified_epoch_secs: Some(1_700_000_000),
                generated: false,
                title: None,
            },
            MemoryFileInfo {
                path: "/workspace/MEMORY.md".into(),
                source: "workspace".into(),
                size_bytes: 200,
                modified_epoch_secs: Some(1_700_000_000),
                generated: false,
                title: None,
            },
            MemoryFileInfo {
                path: "/sessions/log1.md".into(),
                source: "session".into(),
                size_bytes: 50,
                modified_epoch_secs: None,
                generated: false,
                title: None,
            },
        ];

        let entries = build_entries(files);
        assert_eq!(entries.len(), 6);
        let Some(e0) = entries.first() else {
            panic!("expected entries");
        };
        assert!(e0.is_header);
        assert_eq!(e0.label, "Global");
        let Some(e1) = entries.get(1) else {
            panic!("expected workspace entry");
        };
        assert!(!e1.is_header);
        let Some(e2) = entries.get(2) else {
            panic!("expected workspace header");
        };
        assert!(e2.is_header);
        assert_eq!(e2.label, "Workspace");
        let Some(e4) = entries.get(4) else {
            panic!("expected sessions header");
        };
        assert!(e4.is_header);
        assert_eq!(e4.label, "Sessions");
    }

    #[test]
    fn new_selects_first_non_header() {
        let entries = build_test_entries();
        let state = MemoryModalState::new(entries);
        // Index 0 is a header, so selection starts at index 1
        assert_eq!(state.selected, 1);
        let sel = state.selected_entry().unwrap();
        assert!(!sel.is_header);
    }

    #[test]
    fn filtered_indices_preserves_headers_for_matching_entries() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.set_query("memory");
        state.invalidate_filter();

        let indices = state.filtered_indices();
        assert_eq!(indices, &[0, 1]);
    }

    #[test]
    fn filter_matches_note_contents_and_requires_all_terms() {
        let (_dir, mut state) = v2_store_state();
        let labels = |state: &MemoryModalState| {
            state
                .filtered_indices()
                .iter()
                .filter_map(|&i| state.entries.get(i))
                .filter(|e| !e.is_header)
                .map(|e| e.label.clone())
                .collect::<Vec<_>>()
        };
        // "notes" occurs only in anyrun.md's body, not in any label.
        state.set_query("NOTES");
        state.invalidate_filter();
        assert_eq!(labels(&state), ["anyrun.md"]);
        // Every term must match, across label and content.
        state.set_query("anyrun notes");
        state.invalidate_filter();
        assert_eq!(labels(&state), ["anyrun.md"]);
        state.set_query("zulu notes");
        state.invalidate_filter();
        assert!(labels(&state).is_empty());
        assert!(state.filter_has_no_matches());
        state.set_query("");
        state.invalidate_filter();
        assert_eq!(labels(&state).len(), 4);
    }

    #[test]
    fn no_match_filter_explains_itself_instead_of_no_file_selected() {
        let (_dir, mut state) = v2_store_state();
        state.set_query("xyzzy");
        state.invalidate_filter();
        state.clamp_selected();
        let text = render_full(&mut state);
        assert!(
            text.contains("No notes match \u{201C}xyzzy\u{201D}"),
            "{text}"
        );
        assert!(text.contains("Backspace clears the filter"), "{text}");
        assert!(!text.contains("No file selected"), "{text}");
    }

    #[test]
    fn copy_keys_emit_memory_copy_and_the_outcome_lands_in_the_status_line() {
        let (dir, mut state) = v2_store_state();
        select_label(&mut state, "anyrun.md");
        let expected = dir
            .path()
            .join("workspace/topics/anyrun.md")
            .to_string_lossy()
            .into_owned();
        match handle_memory_key(&mut state, &plain_key('y')) {
            InputOutcome::Action(Action::MemoryCopy { text }) => assert_eq!(text, expected),
            other => panic!("unexpected outcome {other:?}"),
        }
        state.report_copy(&crate::clipboard::CopyDelivery::File {
            path: PathBuf::from("/tmp/last-copy.txt"),
        });
        let status = state.status.clone().expect("status line set");
        assert!(
            status.text.starts_with("Clipboard unreachable"),
            "{status:?}"
        );
        assert!(!status.is_error);
        assert!(render_full(&mut state).contains("Clipboard unreachable"));
        // Copy messages expire like a toast; other status text does not tick.
        let ticks = status.ticks_remaining.expect("copy message is transient");
        for _ in 0..ticks {
            assert!(!state.tick_status());
        }
        assert!(state.tick_status());
        assert!(state.status.is_none());
        state.status = Some(MemoryStatusLine {
            text: "Deleting…".into(),
            is_error: false,
            ticks_remaining: None,
        });
        assert!(!state.tick_status());
        assert!(state.status.is_some());
    }

    /// One topic with 60 numbered one-line paragraphs (rendered rows alternate text and blank);
    /// paragraph 10 contains "the", paragraph 40 contains "the needle".
    fn long_note_state() -> (tempfile::TempDir, MemoryModalState) {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("topics")).unwrap();
        let body: String = (1..=60)
            .map(|i| match i {
                10 => "line 10 states the obvious\n\n".to_owned(),
                40 => "line 40 has the needle\n\n".to_owned(),
                _ => format!("line {i}\n\n"),
            })
            .collect();
        let path = dir.path().join("topics/long.md");
        std::fs::write(&path, &body).unwrap();
        let state = MemoryModalState::new(build_entries(vec![MemoryFileInfo {
            path: path.to_string_lossy().into_owned(),
            source: "workspace".into(),
            size_bytes: body.len() as u64,
            modified_epoch_secs: None,
            generated: false,
            title: None,
        }]));
        (dir, state)
    }

    fn render_at(state: &mut MemoryModalState, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render_memory_modal(&mut buf, area, state, false);
        buffer_text(&buf)
    }

    #[test]
    fn filter_scrolls_preview_to_first_match() {
        let (_dir, mut state) = long_note_state();
        let text = render_full(&mut state);
        assert!(
            text.contains("line 1\n") || text.contains("line 1 "),
            "{text}"
        );
        assert!(!text.contains("needle"), "{text}");

        // A common first term must not pin the jump to its first occurrence (line 10).
        state.set_query("the needle");
        state.invalidate_filter();
        state.clamp_selected();
        let text = render_full(&mut state);
        assert!(text.contains("line 40 has the needle"), "{text}");
        assert_eq!(state.preview_scroll, 78);
    }

    #[test]
    fn enter_focuses_preview_for_keyboard_scrolling_and_esc_returns() {
        let (_dir, mut state) = long_note_state();
        render_full(&mut state);
        assert!(matches!(
            handle_memory_key(
                &mut state,
                &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputOutcome::Changed
        ));
        assert_eq!(state.mode, MemoryModalMode::PreviewFocused);
        let before = state.selected;
        handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        assert_eq!(state.preview_scroll, 2);
        assert_eq!(state.selected, before, "list selection is untouched");
        handle_memory_key(&mut state, &KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(render_full(&mut state).contains("line 60"));
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert!(labels.contains(&"drag to copy"), "{labels:?}");
        assert!(labels.contains(&"Esc list"), "{labels:?}");
        handle_memory_key(&mut state, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state.mode, MemoryModalMode::Browse);
    }

    #[test]
    fn narrow_modal_hides_preview_until_enter_opens_it_full_width() {
        let (_dir, mut state) = long_note_state();
        // 70 columns: the modal content is under SPLIT_MIN_WIDTH, so only the list shows.
        let text = render_at(&mut state, 70, 30);
        assert!(text.contains("long.md"), "{text}");
        assert!(!text.contains("line 2"), "{text}");
        assert!(!state.split_shown);
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert!(labels.contains(&"Enter open"), "{labels:?}");

        handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let text = render_at(&mut state, 70, 30);
        assert!(text.contains("line 2"), "{text}");
        assert!(!text.contains("long.md"), "{text}");
        assert_eq!(state.list_area, Rect::default());
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert!(labels.contains(&"Esc back"), "{labels:?}");

        handle_memory_key(&mut state, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let text = render_at(&mut state, 70, 30);
        assert!(text.contains("long.md"), "{text}");
    }

    #[test]
    fn narrow_list_drops_the_size_column() {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let info = MemoryFileInfo {
            path: "/s/topics/anyrun.md".into(),
            source: "workspace".into(),
            size_bytes: 13_900,
            modified_epoch_secs: Some(0),
            generated: false,
            title: None,
        };
        let mut state = MemoryModalState::new(build_entries(vec![info.clone()]));
        let theme = Theme::current();
        let wide = Rect::new(0, 0, 60, 4);
        let mut buf = Buffer::empty(wide);
        render_file_list(&mut buf, wide, &mut state, &theme);
        let text = buffer_text(&buf);
        assert!(text.contains("13.6 KB \u{00B7}"), "{text}");

        let mut state = MemoryModalState::new(build_entries(vec![info]));
        let narrow = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(narrow);
        render_file_list(&mut buf, narrow, &mut state, &theme);
        let text = buffer_text(&buf);
        assert!(!text.contains("KB"), "{text}");
        assert!(!text.contains('\u{00B7}'), "{text}");
        let row = text.lines().find(|l| l.contains("anyrun.md")).unwrap();
        assert!(row.trim_end().ends_with('y'), "{row:?}");
    }

    #[test]
    fn dragging_over_the_preview_copies_the_selected_text() {
        let (_dir, mut state) = long_note_state();
        render_full(&mut state);
        let area = state.preview_text_area;
        assert!(area.width > 10 && area.height > 3, "{area:?}");
        // Press on "line 2" (row 2), drag past the end of "line 3" (row 4), release.
        let down = MouseEventKind::Down(MouseButton::Left);
        assert!(matches!(
            handle_memory_mouse(&mut state, down, area.x, area.y + 2),
            InputOutcome::Changed
        ));
        // A press without movement is not a drag.
        assert!(matches!(
            handle_memory_mouse(
                &mut state,
                MouseEventKind::Drag(MouseButton::Left),
                area.x,
                area.y + 2
            ),
            InputOutcome::Unchanged
        ));
        handle_memory_mouse(
            &mut state,
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 6,
            area.y + 4,
        );
        assert!(state.text_drag.is_some());
        match handle_memory_mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.x + 6,
            area.y + 4,
        ) {
            InputOutcome::Action(Action::MemoryCopy { text }) => {
                assert_eq!(text, "line 2\n\nline 3")
            }
            other => panic!("unexpected outcome {other:?}"),
        }
        assert!(state.text_drag.is_none());

        // A plain click on the preview copies nothing and does not move the list selection.
        let before = state.selected;
        handle_memory_mouse(&mut state, down, area.x, area.y);
        assert!(matches!(
            handle_memory_mouse(
                &mut state,
                MouseEventKind::Up(MouseButton::Left),
                area.x,
                area.y
            ),
            InputOutcome::Changed
        ));
        assert_eq!(state.selected, before);
    }

    /// Generated index, one topic, one inbox observation, all on disk.
    fn v2_store_state() -> (tempfile::TempDir, MemoryModalState) {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(ws.join("topics")).unwrap();
        std::fs::create_dir_all(ws.join("observations/_inbox")).unwrap();
        let files = [
            ("workspace/MEMORY.md", "# index\n", true),
            ("workspace/topics/anyrun.md", "# Anyrun\n\nnotes\n", false),
            (
                "workspace/observations/_inbox/2026-09-14-x.md",
                "obs\n",
                false,
            ),
            ("workspace/topics/zulu.md", "# Zulu\n", false),
        ];
        let infos = files
            .iter()
            .map(|(rel, body, generated)| {
                let path = dir.path().join(rel);
                std::fs::write(&path, body).unwrap();
                MemoryFileInfo {
                    path: path.to_string_lossy().into_owned(),
                    source: "workspace".into(),
                    size_bytes: body.len() as u64,
                    modified_epoch_secs: None,
                    generated: *generated,
                    title: None,
                }
            })
            .collect();
        let state = MemoryModalState::new(build_entries(infos));
        (dir, state)
    }

    fn select_label(state: &mut MemoryModalState, label: &str) {
        let idx = state
            .filtered_indices()
            .iter()
            .position(|&i| state.entries.get(i).is_some_and(|e| e.label == label))
            .expect("label present");
        state.selected = idx;
        state.load_preview();
    }

    #[test]
    fn v2_deletability_follows_store_layout() {
        let (_dir, state) = v2_store_state();
        let by_label = |label: &str| {
            state
                .entries
                .iter()
                .find(|e| e.label == label)
                .map(|e| e.is_deletable())
                .unwrap()
        };
        assert!(!by_label("MEMORY.md"), "generated index is protected");
        assert!(by_label("anyrun.md"));
        assert!(by_label("2026-09-14-x.md"));

        // Legacy MEMORY.md is user content but still not deletable: it is not a topic or observation.
        let legacy = MemoryFileEntry {
            path: PathBuf::from("/legacy/MEMORY.md"),
            source: "global".into(),
            label: "MEMORY.md".into(),
            size_text: String::new(),
            age_text: String::new(),
            is_header: false,
            generated: false,
            size_bytes: 0,
        };
        assert!(!legacy.is_deletable());

        // The shell refuses to hash notes above the store cap, so the modal must not offer them.
        let oversized = MemoryFileEntry {
            path: PathBuf::from("/store/topics/big.md"),
            source: "global".into(),
            label: "big.md".into(),
            size_bytes: MEMORY_FORGET_MAX_FILE_BYTES + 1,
            ..legacy
        };
        assert!(!oversized.is_deletable());
    }

    /// Two-press `x` sends the previewed bytes' hash to the shell and keeps the row until it answers.
    #[test]
    fn confirmed_delete_sends_forget_and_applies_reply() {
        let (dir, mut state) = v2_store_state();
        select_label(&mut state, "anyrun.md");
        let topic_path = dir.path().join("workspace/topics/anyrun.md");
        let expected_hash = blake3::hash(b"# Anyrun\n\nnotes\n").to_hex().to_string();

        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert!(labels.contains(&"x delete"));

        // Any key other than `x` cancels the confirmation.
        handle_memory_key(&mut state, &plain_key('x'));
        assert!(matches!(
            state.mode,
            MemoryModalMode::ConfirmingDelete { .. }
        ));
        handle_memory_key(&mut state, &plain_key('j'));
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert!(state.status.is_none());

        // So does any mouse input, which could otherwise move the selection under the confirm.
        handle_memory_key(&mut state, &plain_key('x'));
        handle_memory_mouse(&mut state, MouseEventKind::ScrollDown, 0, 0);
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert!(state.status.is_none());

        handle_memory_key(&mut state, &plain_key('x'));
        assert!(state.status.as_ref().unwrap().text.contains("anyrun.md"));
        let outcome = handle_memory_key(&mut state, &plain_key('x'));
        let InputOutcome::Action(Action::MemoryForget {
            path,
            expected_content_hash,
        }) = outcome
        else {
            panic!("expected MemoryForget action, got {outcome:?}");
        };
        assert_eq!(Path::new(&path), topic_path);
        assert_eq!(expected_content_hash, expected_hash);
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert!(state.entries.iter().any(|e| e.label == "anyrun.md"));
        // A second `x` while the request is in flight is ignored.
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('x')),
            InputOutcome::Unchanged
        ));

        // A rejection keeps the row and surfaces the shell's message.
        state.apply_forget_result(
            &path,
            Ok(MemoryForgetResponse::Rejected {
                reason: xai_grok_shell::extensions::memory::MemoryForgetRejection::DreamRunning,
                message: "Dream is organizing memory right now.".into(),
            }),
        );
        assert!(state.entries.iter().any(|e| e.label == "anyrun.md"));
        assert!(state.status.as_ref().is_some_and(|s| s.is_error));

        // Retry and succeed: the row goes away, and a selection moved during the round trip stays put.
        handle_memory_key(&mut state, &plain_key('x'));
        handle_memory_key(&mut state, &plain_key('x'));
        select_label(&mut state, "2026-09-14-x.md");
        state.apply_forget_result(
            &path,
            Ok(MemoryForgetResponse::Forgotten {
                was_already_forgotten: false,
            }),
        );
        assert!(!state.entries.iter().any(|e| e.label == "anyrun.md"));
        assert_eq!(
            state.selected_entry().map(|e| e.label.as_str()),
            Some("2026-09-14-x.md")
        );
        assert!(state.status.as_ref().is_some_and(|s| !s.is_error));

        // A reply for a path that is not pending is ignored.
        state.apply_forget_result(
            "/elsewhere.md",
            Ok(MemoryForgetResponse::Forgotten {
                was_already_forgotten: false,
            }),
        );
        assert_eq!(state.entries.iter().filter(|e| !e.is_header).count(), 3);
    }

    #[test]
    fn deleting_last_file_in_section_drops_its_header() {
        let mut state = MemoryModalState::new(build_test_entries());
        state.entries.retain(|e| e.label != "session-log.md");
        state.drop_empty_headers();
        let labels: Vec<&str> = state.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Global", "MEMORY.md"]);
    }

    /// `i` aliases `/` without modifiers: from Browse it enters FilterFocused exactly like `/` (vim-nav "press i to search").
    #[test]
    fn i_key_enters_filter_like_slash() {
        let mut state = MemoryModalState::new(build_test_entries());
        assert_eq!(state.mode, MemoryModalMode::Browse);
        let i = crossterm::event::KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(matches!(
            handle_memory_key(&mut state, &i),
            InputOutcome::Changed
        ));
        assert_eq!(state.mode, MemoryModalMode::FilterFocused);
    }

    /// The `modifiers.is_empty()` guard: Ctrl+i / Alt+i must NOT enter filter.
    #[test]
    fn modified_i_does_not_enter_filter() {
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let mut state = MemoryModalState::new(build_test_entries());
            let k = crossterm::event::KeyEvent::new(KeyCode::Char('i'), mods);
            assert!(matches!(
                handle_memory_key(&mut state, &k),
                InputOutcome::Unchanged
            ));
            assert_eq!(state.mode, MemoryModalMode::Browse);
        }
    }

    /// Wiring check: the Browse footer carries the shared `i search` hint under vim nav mode.
    #[test]
    fn browse_footer_advertises_i_search_under_vim() {
        crate::appearance::cache::set_vim_mode(true);
        let vim = build_shortcuts(&MemoryModalState::new(build_test_entries()));
        assert!(
            vim.iter().any(|s| s.label == "i search"),
            "vim-mode Browse footer must advertise `i search`"
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn render_full(state: &mut MemoryModalState) -> String {
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_memory_modal(&mut buf, area, state, false);
        buffer_text(&buf)
    }

    fn plain_key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn empty_enabled_state_renders_onboarding_copy() {
        // A fresh v2 store is not an empty list: scope init writes both MEMORY.md indexes.
        let manifest = |scope: &str| xai_grok_shell::extensions::notification::MemoryFileInfo {
            path: format!("/store/{scope}/MEMORY.md"),
            source: scope.to_string(),
            size_bytes: 183,
            modified_epoch_secs: None,
            generated: true,
            title: None,
        };
        let mut state = MemoryModalState::new(build_entries(vec![
            manifest("global"),
            manifest("workspace"),
        ]));
        assert!(!state.has_notes());
        let text = render_full(&mut state);
        assert!(text.contains("Nothing remembered yet"), "{text}");
        assert!(text.contains("/remember"), "{text}");
        assert!(text.contains("/dream"), "{text}");
        assert!(text.contains("saved automatically"), "{text}");
        assert!(!text.contains("No file selected"), "{text}");
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["t turn off", "^F fullscreen", "Esc close"]);

        // Capture and manual Dream can be pinned off; the copy must not promise them.
        let mut restricted = MemoryModalState::new(build_entries(vec![manifest("global")]))
            .with_capabilities(false, false);
        let text = render_full(&mut restricted);
        assert!(text.contains("/remember"), "{text}");
        assert!(!text.contains("/dream"), "{text}");
        assert!(!text.contains("saved automatically"), "{text}");

        // One real note brings the two-pane browser (indexes included) back.
        let mut with_topic = MemoryModalState::new(build_entries(vec![
            manifest("global"),
            manifest("workspace"),
            xai_grok_shell::extensions::notification::MemoryFileInfo {
                path: "/store/workspace/topics/anyrun.md".into(),
                source: "workspace".into(),
                size_bytes: 900,
                modified_epoch_secs: None,
                generated: false,
                title: None,
            },
        ]));
        assert!(with_topic.has_notes());
        let text = render_full(&mut with_topic);
        assert!(!text.contains("Nothing remembered yet"), "{text}");
        assert!(text.contains("anyrun.md"), "{text}");

        // Legacy MEMORY.md is user content, not a generated index; the shell leaves `generated` unset.
        let mut legacy = MemoryModalState::new(build_entries(vec![
            xai_grok_shell::extensions::notification::MemoryFileInfo {
                path: "/legacy/global/MEMORY.md".into(),
                source: "global".into(),
                size_bytes: 2048,
                modified_epoch_secs: None,
                generated: false,
                title: None,
            },
        ]));
        assert!(legacy.has_notes());
        assert!(!render_full(&mut legacy).contains("Nothing remembered yet"));
    }

    #[test]
    fn disabled_state_renders_turn_on_call_to_action() {
        let mut state = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::SessionToggle));
        let text = render_full(&mut state);
        assert!(text.contains("Memory is off for this session"), "{text}");
        assert!(text.contains("Press t to turn it back on"), "{text}");
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["t turn on", "^F fullscreen", "Esc close"]);
    }

    #[test]
    fn config_opt_out_state_offers_session_toggle_and_names_the_config() {
        let mut state = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::ConfigOptOut));
        let text = render_full(&mut state);
        assert!(text.contains("[memory] enabled = false"), "{text}");
        assert!(text.contains("this session only"), "{text}");
        assert!(state.can_enable());
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["t turn on", "^F fullscreen", "Esc close"]);
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Action(Action::MemoryToggle { enabled: true })
        ));
    }

    #[test]
    fn process_disabled_state_offers_no_toggle() {
        let mut state = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::ProcessDisabled));
        let text = render_full(&mut state);
        assert!(text.contains("Memory is off for this process"), "{text}");
        assert!(text.contains("--no-memory"), "{text}");
        assert!(!state.can_enable());
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["^F fullscreen", "Esc close"]);
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Unchanged
        ));
    }

    #[test]
    fn rollout_restricted_state_offers_no_toggle() {
        let mut state = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::RolloutRestricted));
        let text = render_full(&mut state);
        assert!(
            text.contains("Memory is unavailable in this session"),
            "{text}"
        );
        let labels: Vec<&str> = build_shortcuts(&state).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["^F fullscreen", "Esc close"]);
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Unchanged
        ));

        // A reason from a newer shell fails closed: no toggle offered.
        let unknown = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::Unknown));
        assert!(!unknown.can_enable());
    }

    #[test]
    fn t_from_disabled_modal_toggles_and_resyncs_from_reply() {
        use xai_grok_shell::extensions::notification::MemoryFileInfo;
        let mut state = MemoryModalState::new(Vec::new())
            .with_enabled(false, Some(MemoryDisabledReason::SessionToggle));
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Action(Action::MemoryToggle { enabled: true })
        ));
        assert!(state.memory_enabled);

        // The reply's listing replaces the empty list without closing the modal, and drops any
        // delete confirmation armed against the old list.
        state.mode = MemoryModalMode::ConfirmingDelete { idx: 0 };
        state.apply_toggle_result(Ok(MemoryToggleResponse {
            message: "Memory enabled for this session.".into(),
            enabled: true,
            disabled_reason: None,
            listing: Some(MemoryListing {
                files: vec![MemoryFileInfo {
                    path: "/store/global/topics/rust.md".into(),
                    source: "global".into(),
                    size_bytes: 12,
                    modified_epoch_secs: None,
                    generated: false,
                    title: None,
                }],
                enabled: true,
                disabled_reason: None,
                capture_enabled: true,
                dream_enabled: true,
            }),
        }));
        assert!(!state.shows_notice());
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert_eq!(
            state.selected_entry().map(|e| e.label.as_str()),
            Some("rust.md")
        );
        assert_eq!(
            state.status.as_ref().map(|s| s.text.as_str()),
            Some("Memory enabled for this session.")
        );

        // A refusal (Ok, state unchanged) with no listing still corrects the optimistic flip and
        // renders as an error, since the result differs from what was requested.
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Action(Action::MemoryToggle { enabled: false })
        ));
        state.apply_toggle_result(Ok(MemoryToggleResponse {
            message: "Memory is already enabled.".into(),
            enabled: true,
            disabled_reason: None,
            listing: None,
        }));
        assert!(state.memory_enabled);
        assert!(state.status.as_ref().is_some_and(|s| s.is_error));
        assert!(!state.shows_notice());

        // A second `t` is ignored while the first is in flight; a failure reverts the optimistic flip.
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Action(Action::MemoryToggle { enabled: false })
        ));
        assert!(!state.memory_enabled);
        assert!(matches!(
            handle_memory_key(&mut state, &plain_key('t')),
            InputOutcome::Unchanged
        ));
        state.apply_toggle_result(Err("Couldn't change memory state.".into()));
        assert!(state.memory_enabled);
        assert!(state.status.as_ref().is_some_and(|s| s.is_error));
        assert!(!state.shows_notice());
    }

    #[test]
    fn t_round_trip_with_entries_stays_open() {
        let mut state = MemoryModalState::new(build_test_entries());
        let off = handle_memory_key(&mut state, &plain_key('t'));
        assert!(matches!(
            off,
            InputOutcome::Action(Action::MemoryToggle { enabled: false })
        ));
        assert!(!state.memory_enabled);
        assert!(state.shows_notice());

        // The hidden list must not accept hotkeys: `x` on a session entry would otherwise arm delete.
        state.selected = 3;
        for c in ['x', 'y', 'j', '/'] {
            assert!(matches!(
                handle_memory_key(&mut state, &plain_key(c)),
                InputOutcome::Unchanged
            ));
        }
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert!(matches!(
            handle_memory_mouse(&mut state, MouseEventKind::ScrollDown, 1, 1),
            InputOutcome::Unchanged
        ));

        // Off replies still list the files, so turning back on reveals them without a refetch.
        state.apply_toggle_result(Ok(MemoryToggleResponse {
            message: "Memory disabled for this session.".into(),
            enabled: false,
            disabled_reason: Some(MemoryDisabledReason::SessionToggle),
            listing: Some(MemoryListing {
                files: vec![xai_grok_shell::extensions::notification::MemoryFileInfo {
                    path: "/store/global/topics/rust.md".into(),
                    source: "global".into(),
                    size_bytes: 12,
                    modified_epoch_secs: None,
                    generated: false,
                    title: None,
                }],
                enabled: false,
                disabled_reason: Some(MemoryDisabledReason::SessionToggle),
                capture_enabled: true,
                dream_enabled: true,
            }),
        }));
        assert!(state.shows_notice());
        let on = handle_memory_key(&mut state, &plain_key('t'));
        assert!(matches!(
            on,
            InputOutcome::Action(Action::MemoryToggle { enabled: true })
        ));
        assert!(state.memory_enabled);
        assert!(!state.shows_notice());
    }

    #[test]
    fn truncate_to_width_handles_ascii() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("", 5), "");
    }

    #[test]
    fn truncate_to_width_handles_multibyte() {
        // CJK characters are 2 columns wide.
        let s = "\u{4F60}\u{597D}world"; // 你好world: 2+2+5 = 9 cols
        assert_eq!(truncate_to_width(s, 4), "\u{4F60}\u{597D}"); // Both CJK chars fit exactly in 4 columns
        assert_eq!(truncate_to_width(s, 3), "\u{4F60}"); // 2 cols; the next char adds 2 and exceeds 3
        assert_eq!(truncate_to_width(s, 9), s); // All 9 columns fit
    }

    #[test]
    fn cached_filter_updates_on_invalidate() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        assert_eq!(state.filtered_indices().len(), 4); // all entries

        state.set_query("session");
        state.invalidate_filter();
        // Only the Sessions header and session-log.md match
        assert_eq!(state.filtered_indices().len(), 2);

        state.set_query("");
        state.invalidate_filter();
        assert_eq!(state.filtered_indices().len(), 4);
    }

    #[test]
    fn select_at_skips_headers_and_updates() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        // Initial selection is index 1 (first non-header).
        assert_eq!(state.selected, 1);

        // Select the session entry at filtered index 3.
        assert!(state.select_at(3));
        assert_eq!(state.selected, 3);
        let sel = state.selected_entry().unwrap();
        assert_eq!(sel.label, "session-log.md");

        // Selecting a header fails
        assert!(!state.select_at(0));
        assert_eq!(state.selected, 3);

        // Selecting the already-selected index returns false
        assert!(!state.select_at(3));
    }

    #[test]
    fn select_at_out_of_bounds() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        assert!(!state.select_at(100));
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn mouse_click_selects_file() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        // Simulate a rendered list area.
        state.list_area = Rect::new(5, 10, 30, 20);
        state.scroll_offset = 0;

        // Click on row corresponding to filtered index 3 (entries_start_y = 11, row 3 is y=14)
        let result = handle_memory_mouse(
            &mut state,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            14,
        );
        assert!(matches!(result, InputOutcome::Changed));
        assert_eq!(state.selected, 3);
    }

    #[test]
    fn mouse_click_on_header_unchanged() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.list_area = Rect::new(5, 10, 30, 20);
        state.scroll_offset = 0;

        // Click on row 0 (header "Global" at y=11).
        let result = handle_memory_mouse(
            &mut state,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            11,
        );
        assert!(matches!(result, InputOutcome::Unchanged));
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn mouse_scroll_up_down_on_list() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.list_area = Rect::new(0, 0, 30, 10);

        // Scroll down on the list area.
        let result = handle_memory_mouse(&mut state, MouseEventKind::ScrollDown, 5, 3);
        assert!(matches!(result, InputOutcome::Changed));

        // Scroll up on the list area.
        let result = handle_memory_mouse(&mut state, MouseEventKind::ScrollUp, 5, 3);
        assert!(matches!(result, InputOutcome::Changed));
    }

    #[test]
    fn mouse_scroll_on_preview() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.preview_area = Rect::new(40, 0, 30, 10);
        state.preview_total_lines = 100;
        state.preview_scroll = 5;

        // Scroll down on the preview area.
        let result = handle_memory_mouse(&mut state, MouseEventKind::ScrollDown, 50, 3);
        assert!(matches!(result, InputOutcome::Changed));
        assert_eq!(state.preview_scroll, 8);

        // Scroll up on the preview area.
        let result = handle_memory_mouse(&mut state, MouseEventKind::ScrollUp, 50, 3);
        assert!(matches!(result, InputOutcome::Changed));
        assert_eq!(state.preview_scroll, 5);
    }

    #[test]
    fn mouse_outside_both_panes_unchanged() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.list_area = Rect::new(0, 0, 30, 10);
        state.preview_area = Rect::new(40, 0, 30, 10);

        // Click outside both areas.
        let result = handle_memory_mouse(
            &mut state,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            35,
            5,
        );
        assert!(matches!(result, InputOutcome::Unchanged));
    }

    #[test]
    fn ctrl_d_u_no_longer_scrolls_preview() {
        let entries = build_test_entries();
        let mut state = MemoryModalState::new(entries);
        state.preview_scroll = 5;

        // Ctrl+D does NOT scroll the preview (removed hotkey)
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let result = handle_memory_key(&mut state, &key);
        assert!(matches!(result, InputOutcome::Unchanged));
        assert_eq!(state.preview_scroll, 5);

        // Ctrl+U does NOT scroll the preview either (removed hotkey)
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        let result = handle_memory_key(&mut state, &key);
        assert!(matches!(result, InputOutcome::Unchanged));
        assert_eq!(state.preview_scroll, 5);
    }

    #[test]
    fn filter_text_changes_recompute_preview_but_cursor_moves_do_not() {
        let mut state = MemoryModalState::new(build_test_entries());
        state.mode = MemoryModalMode::FilterFocused;
        state.preview_scroll = 7;

        let outcome = handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "m");
        assert_eq!(state.preview_scroll, 0);
        let filtered = state.filtered_indices().to_vec();

        state.preview_scroll = 7;
        let outcome = handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "m");
        assert_eq!(state.query_cursor_byte(), 0);
        assert_eq!(state.filtered_indices(), filtered);
        assert_eq!(state.preview_scroll, 7);
    }

    #[test]
    fn filter_paste_recomputes_once_and_consumes_empty_input() {
        let mut state = MemoryModalState::new(build_test_entries());
        state.mode = MemoryModalMode::FilterFocused;
        state.preview_scroll = 7;
        let outcome = handle_memory_paste(&mut state, "mem\r\n");
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "mem");
        assert_eq!(state.preview_scroll, 0);

        state.preview_scroll = 7;
        let outcome = handle_memory_paste(&mut state, "\r\n");
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "mem");
        assert_eq!(state.preview_scroll, 7);

        state.mode = MemoryModalMode::Browse;
        let outcome = handle_memory_paste(&mut state, "ignored");
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(state.query(), "mem");
    }

    #[test]
    fn filter_escape_preserves_query_and_enter_stays_focused() {
        let mut state = MemoryModalState::new(build_test_entries());
        state.mode = MemoryModalMode::FilterFocused;
        state.set_query("memory");

        let outcome = handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(state.mode, MemoryModalMode::FilterFocused);
        assert_eq!(state.query(), "memory");

        let outcome =
            handle_memory_key(&mut state, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.mode, MemoryModalMode::Browse);
        assert_eq!(state.query(), "memory");
    }

    #[test]
    fn filter_uses_canonical_word_and_grapheme_editing() {
        for key in [
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        ] {
            let mut state = MemoryModalState::new(build_test_entries());
            state.mode = MemoryModalMode::FilterFocused;
            state.set_query("hello-world");
            let outcome = handle_memory_key(&mut state, &key);
            assert!(matches!(outcome, InputOutcome::Changed));
            assert_eq!(state.query(), "hello-world");
            assert_eq!(state.query_cursor_byte(), "hello-".len());
        }

        let grapheme = "👩🏽\u{200d}💻";
        let mut state = MemoryModalState::new(build_test_entries());
        state.mode = MemoryModalMode::FilterFocused;
        state.set_query(format!("a{grapheme}b"));
        let _ = state.set_query_cursor_byte(1);
        let outcome = handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "ab");
        assert_eq!(state.query_cursor_byte(), 1);
    }

    #[test]
    fn browse_backspace_deletes_trailing_grapheme_independent_of_cursor_and_modifiers() {
        let mut state = MemoryModalState::new(build_test_entries());
        let grapheme = "👩🏽\u{200d}💻";
        state.set_query(format!("a{grapheme}"));
        let _ = state.set_query_cursor_byte(0);
        let outcome = handle_memory_key(
            &mut state,
            &KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.query(), "a");
        assert_eq!(state.query_cursor_byte(), 1);
    }

    #[test]
    fn filter_render_keeps_unicode_query_and_cursor_visible() {
        let mut state = MemoryModalState::new(build_test_entries());
        state.mode = MemoryModalMode::FilterFocused;
        let grapheme = "👩🏽\u{200d}💻";
        let text = format!("123456789012中e\u{301}{grapheme}z");
        state.set_query(&text);
        let _ = state.set_query_cursor_byte(text.len() - 1);
        let area = Rect::new(0, 0, 12, 3);
        let theme = Theme::current();
        let mut buffer = Buffer::empty(area);
        let viewport = state.query_viewport(area.width as usize);
        let Some(visible) = state.query().get(viewport.visible_byte_range.clone()) else {
            panic!("viewport out of range");
        };
        assert!(visible.contains('中'));
        assert!(visible.contains("e\u{301}"));
        assert!(visible.contains(grapheme));

        render_file_list(&mut buffer, area, &mut state, &theme);
        let cursor_x = viewport.cursor_display_column as u16;
        assert_eq!(
            buffer.cell((cursor_x, 0)).map(|c| c.bg),
            Some(theme.text_primary)
        );
    }

    #[test]
    fn apply_scrollbar_jump_edges() {
        let mut offset = 50;
        // Top click gives offset 0
        apply_scrollbar_jump(10, Rect::new(0, 10, 1, 20), 100, 10, &mut offset);
        assert_eq!(offset, 0);

        // Bottom click gives max offset
        apply_scrollbar_jump(29, Rect::new(0, 10, 1, 20), 100, 10, &mut offset);
        assert_eq!(offset, 90);
    }

    fn build_test_entries() -> Vec<MemoryFileEntry> {
        vec![
            MemoryFileEntry {
                path: PathBuf::new(),
                source: String::new(),
                size_text: String::new(),
                age_text: String::new(),
                label: "Global".to_string(),
                is_header: true,
                generated: false,
                size_bytes: 0,
            },
            MemoryFileEntry {
                path: PathBuf::from("/test/MEMORY.md"),
                source: "global".into(),
                size_text: "1KB".into(),
                age_text: "1d".into(),
                label: "MEMORY.md".to_string(),
                is_header: false,
                generated: false,
                size_bytes: 0,
            },
            MemoryFileEntry {
                path: PathBuf::new(),
                source: String::new(),
                size_text: String::new(),
                age_text: String::new(),
                label: "Sessions".to_string(),
                is_header: true,
                generated: false,
                size_bytes: 0,
            },
            MemoryFileEntry {
                path: PathBuf::from("/test/session.md"),
                source: "session".into(),
                size_text: "500B".into(),
                age_text: "2d".into(),
                label: "session-log.md".to_string(),
                is_header: false,
                generated: false,
                size_bytes: 0,
            },
        ]
    }
}
