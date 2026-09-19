//! `ListPaneState` and `ListPane<T>` provide a reusable, scrollable, selectable list component.
//! The state is non-generic and owns only scroll/selection/layout data.
//! Item data lives in an external model and is borrowed via [`ListPaneState::prepare_layout`].
//!
//! Designed for three concrete use cases:
//! - **Tracing pane** (100K+ entries, append-only, NoWrap, follow mode)
//! - **Todo pane** (under 10 items, random mutations, Wrap)
//! - **Background task pane** (under 10 items, random mutations, NoWrap)

mod layout;
mod render;
mod state;

pub use crate::search::QueryKind;
pub use layout::{ListLayoutCache, WrapMode};
pub use render::ListPane;
pub use state::{
    FilterMatcher, InputBarMode, ListFilter, ListMatcher, ListPaneConfig, ListPaneState, MatchMode,
};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;

/// Controls colors for selection highlighting, the input bar, and other framework-level overlays.
/// Match highlights use style inversion (REVERSED modifier) and don't need configurable colors.
/// Items do not need to know about these; the framework applies them in a pass after each item renders.
#[derive(Debug, Clone, Copy)]
pub struct ListPaneStyle {
    /// Background color for the selected item row (cursor line).
    pub selection_bg: Color,

    /// Background color for the visual selection range (not the cursor line).
    /// Kept slightly different from `selection_bg` so the range and the cursor read apart.
    pub visual_select_bg: Color,

    /// Background color for the input bar (search/filter).
    pub input_bar_bg: Color,

    /// Foreground color for the prompt prefix (`/`, `f>`).
    pub input_bar_prompt_fg: Color,

    /// Foreground color for the typed query text.
    pub input_bar_text_fg: Color,

    /// Scrollbar track background color.
    pub scrollbar_bg: Color,

    /// Scrollbar thumb foreground color.
    pub scrollbar_fg: Color,

    /// Corner indicator color (▲ ▼ for scroll position hints).
    pub indicator_fg: Color,

    /// Follow mode indicator color (▶ in bottom-right when following).
    /// Distinct from `indicator_fg` so it's visible against content.
    pub follow_indicator_fg: Color,

    /// "Copied!" toast foreground color.
    pub toast_fg: Color,

    /// When true, the cursor line inside a visual selection uses `visual_select_bg`, so the whole range looks uniform.
    /// The cursor is then distinguished only by the `prefix_cursor` style, not by background.
    /// When false (default), the cursor line always uses `selection_bg`, even within a visual selection.
    pub uniform_visual_bg: bool,

    /// When false, the right-corner scroll indicators (▲/▼) are suppressed. Defaults to `true`.
    /// Panes that draw their own scroll hint set this false (the tasks pane draws the same ▲/▼ centered on dedicated rows).
    pub show_corner_indicators: bool,
}

impl Default for ListPaneStyle {
    fn default() -> Self {
        let theme = crate::theme::Theme::current();
        Self {
            // Defaults come from the theme, whose colors are already quantized
            selection_bg: theme.bg_highlight,
            visual_select_bg: theme.bg_visual,
            input_bar_bg: theme.bg_base,
            input_bar_prompt_fg: theme.command,
            input_bar_text_fg: theme.text_secondary,
            scrollbar_bg: theme.bg_base,
            scrollbar_fg: theme.scrollbar_fg,
            indicator_fg: theme.gray,
            follow_indicator_fg: theme.command,
            toast_fg: theme.accent_user,
            uniform_visual_bg: false,
            show_corner_indicators: true,
        }
    }
}

/// Items are owned by the model, not the view. The view borrows them through `&[T]` in
/// [`ListPaneState::prepare_layout`] and [`ListPane::new`].
pub trait ListItem {
    /// The styled content to display: one logical line of text.
    /// The framework handles wrapping (Wrap mode) and truncation (NoWrap mode) based on this content. Return a reference to a stored `Line`.
    /// Default returns an empty `Line` (signals "use custom `render()`").
    fn content(&self) -> &Line<'_> {
        static EMPTY: std::sync::LazyLock<Line<'static>> = std::sync::LazyLock::new(Line::default);
        &EMPTY
    }

    /// Optional prefix column (checkbox, spinner, timestamp, etc.).
    /// Rendered in a fixed-width column at the left edge; in Wrap mode, continuation lines are indented by the prefix width.
    /// Returned by value since prefixes are small and often constructed dynamically (spinner frame, elapsed timer, checkbox toggle).
    fn prefix(&self) -> Option<Line<'_>> {
        None
    }

    /// Prefix for items inside the visual selection range but not on the cursor line.
    /// Default falls back to `prefix()`.
    fn prefix_in_selection(&self) -> Option<Line<'_>> {
        self.prefix()
    }

    /// Prefix for the cursor line (the focused/active item).
    /// Default falls back to `prefix()`.
    fn prefix_cursor(&self) -> Option<Line<'_>> {
        self.prefix()
    }

    /// Optional full-width background color for this item.
    /// When `Some(color)`, the framework fills the entire item row(s) with it before rendering content. Used for code blocks in markdown viewers.
    fn background(&self) -> Option<Color> {
        None
    }

    // Custom rendering API (escape hatch)

    /// Override this only when the content/prefix model doesn't fit; with the content-based API, leave it as the default no-op.
    /// The framework calls this only when `content()` returns an empty Line.
    fn render(&self, _area: Rect, _buf: &mut Buffer, _selected: bool, _focused: bool) {}

    /// Height in visual lines at the given `width` when soft-wrapping. Must be at least 1.
    /// In `NoWrap` mode the pane ignores this and uses a height of 1.
    /// The default computes from [`content()`] and [`prefix()`]; override only with a custom [`render()`].
    fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 1;
        }
        let prefix_w = self.prefix().map(|p| line_display_width(&p)).unwrap_or(0);
        let content_w = line_display_width(self.content());
        if content_w == 0 {
            return 1;
        }
        let text_area = (width as usize).saturating_sub(prefix_w);
        if text_area == 0 {
            return 1;
        }
        // Use the actual word-wrap line count via textwrap, not character-count division. The cheap
        // ceil(chars/width) estimate underestimates because word-aware wrapping produces more lines when
        // words can't fit at line boundaries. Uses the same FirstFit options as the rendering pipeline.
        let flat: String = self
            .content()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let opts = textwrap::Options::new(text_area)
            .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
            .break_words(true);
        (textwrap::wrap(&flat, opts).len() as u16).max(1)
    }

    /// Stable identity that survives insertions, removals, and reordering.
    /// Must be unique within the list. Used so that selection state persists across mutations without index arithmetic.
    fn stable_id(&self) -> u64;

    /// Whether this item can be selected. Return `false` for separator rows.
    fn is_selectable(&self) -> bool {
        true
    }

    /// Source line number for goto-line (`:N`) navigation.
    fn goto_line_number(&self) -> Option<usize> {
        None
    }

    /// Whether this item needs periodic tick updates (e.g. elapsed timer).
    fn needs_tick(&self) -> bool {
        false
    }

    /// Plain text for search/filter matching.
    fn search_text(&self) -> &str {
        ""
    }

    /// Column offset where `search_text()` content begins in the rendered output.
    /// The framework uses this to position match highlights.
    /// Default derives from the [`prefix()`] display width; override only for a custom [`render()`] with a non-standard layout.
    fn search_text_col_offset(&self) -> u16 {
        self.prefix()
            .map(|p| line_display_width(&p) as u16)
            .unwrap_or(0)
    }

    /// Text to copy when `y` is pressed.
    /// Default extracts plain text from `content()`. Override for items that use custom `render()` with empty `content()`.
    fn copy_text(&self) -> String {
        self.content()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }
}

/// Compute the display width of a ratatui `Line` (sum of span display widths).
pub(crate) fn line_display_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}
