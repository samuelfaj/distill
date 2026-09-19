// Modified for Distill by Samuel Fajreldines, 2026.
//! All colors come from the `Theme` struct; nothing else hardcodes colors.
//!
//! The named constants below match the TokyoNight Night/Storm palette from `distill-pager/src/ui/style.rs`.
//! The `Theme` struct maps these constants to semantic roles.

use ratatui::style::{Color, Modifier, Style};

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

// TokyoNight palette constants (Night/Storm variant).
// Keep in sync with distill-pager TokyoNightNight.
#[allow(dead_code)]
pub mod palette {
    use super::*;
    pub const BG: Color = rgb(26, 27, 38); // #1a1b26 - Night
    pub const BG_DARK: Color = rgb(22, 22, 30); // #16161e
    pub const BG_HIGHLIGHT: Color = rgb(41, 46, 66); // #292e42
    pub const BG_STORM: Color = rgb(36, 40, 59); // #24283b - Storm
    pub const BG_STORM_DARK: Color = rgb(31, 35, 53); // #1f2335
    pub const FG: Color = rgb(192, 202, 245); // #c0caf5
    pub const FG_DARK: Color = rgb(169, 177, 214); // #a9b1d6
    pub const FG_GUTTER: Color = rgb(59, 66, 97); // #3b4261
    pub const COMMENT: Color = rgb(86, 95, 137); // #565f89
    pub const DARK3: Color = rgb(84, 92, 126); // #545c7e
    pub const DARK5: Color = rgb(115, 122, 162); // #737aa2
    pub const BLUE: Color = rgb(122, 162, 247); // #7aa2f7
    pub const BLUE0: Color = rgb(61, 89, 161); // #3d59a1
    pub const BLUE1: Color = rgb(42, 195, 222); // #2ac3de
    pub const CYAN: Color = rgb(125, 207, 255); // #7dcfff
    pub const GREEN: Color = rgb(158, 206, 106); // #9ece6a
    pub const GREEN1: Color = rgb(115, 218, 202); // #73daca
    pub const MAGENTA: Color = rgb(187, 154, 247); // #bb9af7
    pub const ORANGE: Color = rgb(255, 158, 100); // #ff9e64
    pub const PURPLE: Color = rgb(157, 124, 216); // #9d7cd8
    pub const RED: Color = rgb(247, 118, 142); // #f7768e
    pub const RED1: Color = rgb(219, 75, 75); // #db4b4b
    pub const TEAL: Color = rgb(26, 188, 156); // #1abc9c
    pub const YELLOW: Color = rgb(224, 175, 104); // #e0af68
}
use palette::*;

/// Theme for v3 pager rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    // Backgrounds
    pub bg_base: Color,
    pub bg_light: Color,
    pub bg_dark: Color,
    pub bg_highlight: Color,
    pub bg_hover: Color, // Mouse hover row in dropdowns, between bg_highlight and bg_visual
    pub bg_terminal: Color, // For terminal output blocks (currently unused, using bg_dark instead)

    // Accent colors (for vertical lines)
    pub accent_user: Color,
    pub accent_assistant: Color,
    pub accent_thinking: Color,
    pub accent_tool: Color,
    pub accent_system: Color,
    pub accent_error: Color,
    pub accent_success: Color,
    pub accent_running: Color, // For tools that are currently running
    pub accent_skill: Color,   // For skill invocations (slash command skills)

    // Text colors
    pub text_primary: Color,
    pub text_secondary: Color,

    // Gray scale (dim, medium, bright)
    // Every theme defines these three; they provide a consistent hierarchy for secondary/meta text across all themes
    pub gray_dim: Color,    // Dimmest: meta punctuation (`$`, `(+N/-M)`, etc.)
    pub gray: Color,        // Medium: muted text, comments, collapsed content
    pub gray_bright: Color, // Brightest: tool accents, secondary labels

    // Semantic colors
    pub command: Color, // Yellow for shell commands
    pub path: Color,    // Orange for file paths
    pub running: Color, // Cyan for running indicator
    pub warning: Color, // Yellow/amber for warnings

    // Search
    pub fuzzy_accent: Color, // Highlight color for fuzzy search matches

    // Plan mode
    pub accent_plan: Color, // Golden accent for plan mode indicator

    // Context-window overhead category (context info block)
    pub accent_verify: Color, // Violet accent, distinct from plan gold

    // Remember mode
    pub accent_remember: Color, // Green accent for # remember mode

    // Selection
    pub selection_border: Color,
    pub hover_border: Color,
    pub prompt_border: Color,
    pub prompt_border_active: Color,

    // Prompt info
    pub accent_model: Color, // Model name in prompt info line

    // Scrollbar
    pub scrollbar_bg: Color,
    pub scrollbar_fg: Color,

    // Diff colors
    pub diff_delete_bg: Color,
    pub diff_delete_fg: Color,
    pub diff_insert_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_equal_fg: Color,
    pub diff_gutter_fg: Color,

    // Visual selection / dropdown selection background
    pub bg_visual: Color,

    // Paste elements (chip + preview overlay)
    pub paste_bg: Color,
    pub paste_fg: Color,
    pub paste_dim: Color,

    // Markdown rendering colors, used by md_style.rs for headings, code blocks, inline code, links, etc
    // These default to the corresponding top-level theme colors but can be overridden per-theme to customise markdown appearance independently
    pub md_heading_h1: Color,        // H1 headings
    pub md_heading_h1_mod: Modifier, // H1 extra effects
    pub md_heading_h2: Color,        // H2 headings, task unchecked, tables
    pub md_heading_h2_mod: Modifier, // H2 extra effects
    pub md_heading_h3: Color,        // H3 headings, code language tag
    pub md_heading_h3_mod: Modifier, // H3 extra effects
    pub md_heading_h4: Color,        // H4 headings
    pub md_heading_h4_mod: Modifier, // H4 extra effects
    pub md_heading_h5: Color,        // H5 headings, link titles
    pub md_heading_h5_mod: Modifier, // H5 extra effects
    pub md_heading_h6: Color,        // H6 headings
    pub md_heading_h6_mod: Modifier, // H6 extra effects
    pub md_code: Color,              // Inline code, code block delimiters
    pub md_task_checked: Color,      // Task checked
    pub md_task_unchecked: Color,    // Task unchecked
    pub md_muted: Color,             // Blockquotes, list items, rules, links
    pub md_code_bg: Color,           // Code block background
    pub md_text: Color,              // Default body text (plain paragraphs, strong, emphasis)
    pub link_fg: Color,              // Clickable link text color
}

impl Theme {
    /// TokyoNight Storm theme.
    pub const fn tokyonight() -> Self {
        Self {
            bg_base: BG_STORM,
            bg_light: BG_HIGHLIGHT,
            bg_dark: BG_HIGHLIGHT,
            bg_highlight: BG_HIGHLIGHT,
            bg_hover: rgb(40, 49, 76),
            bg_terminal: BG,

            accent_user: BLUE,
            accent_assistant: MAGENTA,
            accent_thinking: FG_GUTTER,
            accent_tool: DARK5,
            accent_system: BLUE,
            accent_error: RED,
            accent_success: GREEN,
            accent_running: MAGENTA,
            accent_skill: rgb(100, 180, 170), // Muted teal

            text_primary: FG,
            text_secondary: FG_DARK,

            gray_dim: FG_GUTTER,
            gray: COMMENT,
            gray_bright: DARK5,

            command: YELLOW,
            path: ORANGE,
            running: CYAN,
            warning: YELLOW,

            fuzzy_accent: BLUE,

            accent_plan: rgb(230, 180, 50), // #E6B432, golden

            accent_verify: MAGENTA, // #bb9af7: violet (distinct from plan)

            accent_remember: Color::Rgb(139, 195, 74), // #8BC34A, Material Design light green

            selection_border: rgb(58, 72, 115), // #3A4873, muted tokyonight blue
            prompt_border: rgb(60, 75, 120),    // #323E64, dimmer prompt chrome
            prompt_border_active: rgb(75, 92, 140), // #4B5C8C, brighter when focused
            hover_border: rgb(55, 58, 80),

            accent_model: TEAL,

            scrollbar_bg: BG_STORM_DARK,
            scrollbar_fg: BG_HIGHLIGHT,

            diff_delete_bg: rgb(85, 15, 20),
            diff_delete_fg: RED,
            diff_insert_bg: rgb(15, 65, 20),
            diff_insert_fg: GREEN,
            diff_equal_fg: COMMENT,
            diff_gutter_fg: COMMENT,

            bg_visual: rgb(40, 52, 87), // #283457, blue-tinted selection bg

            paste_bg: BG_STORM_DARK,
            paste_fg: FG_DARK,
            paste_dim: FG_GUTTER,
            // paste_bg: BG_HIGHLIGHT,
            // paste_fg: DARK5,
            // paste_dim: COMMENT,
            md_heading_h1: TEAL,
            md_heading_h1_mod: Modifier::BOLD,
            md_heading_h2: BLUE,
            md_heading_h2_mod: Modifier::BOLD,
            md_heading_h3: ORANGE,
            md_heading_h3_mod: Modifier::BOLD,
            md_heading_h4: RED,
            md_heading_h4_mod: Modifier::BOLD,
            md_heading_h5: GREEN,
            md_heading_h5_mod: Modifier::BOLD,
            md_heading_h6: MAGENTA,
            md_heading_h6_mod: Modifier::BOLD,
            md_code: GREEN1,
            md_task_checked: CYAN,
            md_task_unchecked: BLUE,
            md_muted: COMMENT,
            md_code_bg: BG_HIGHLIGHT,
            md_text: FG,
            link_fg: BLUE, // #7aa2f7
        }
    }

    pub const fn fg(&self, color: Color) -> Style {
        Style::new().fg(color)
    }

    /// `Reset` gray uses DIM so contrast stays polarity-safe; RGB themes keep an explicit gray foreground.
    pub const fn muted(&self) -> Style {
        match self.gray {
            Color::Reset => Style::new().add_modifier(Modifier::DIM),
            c => Style::new().fg(c),
        }
    }

    /// [`Self::muted`] for caption text painted over colored chrome (borders, header rails).
    /// The bandless palette's `muted()` carries no fg, so text patched over a border cell would inherit that cell's fg — pin `Reset` (the terminal default) in that case.
    pub const fn muted_over_chrome(&self) -> Style {
        match self.gray {
            Color::Reset => Style::new().add_modifier(Modifier::DIM).fg(Color::Reset),
            c => Style::new().fg(c),
        }
    }

    /// Style for OSC 8 hyperlink overlay text.
    pub fn link_style(&self) -> Style {
        Style::new()
            .fg(self.link_fg)
            .add_modifier(ratatui::style::Modifier::UNDERLINED)
    }

    /// Get a style with dim text (gray_dim, dimmest).
    ///
    /// Same Reset-to-DIM rule as [`Self::muted`]: bandless palettes retarget `gray_dim` to bright black for decoration, so content stays on the polarity-safe DIM path and only direct `fg(gray_dim)` sites pick up the bright black.
    pub const fn dim(&self) -> Style {
        if self.is_bandless() || matches!(self.gray_dim, Color::Reset) {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            Style::new().fg(self.gray_dim)
        }
    }

    /// One step fainter than [`Self::dim`]: `gray_dim` blended 0.66 toward `bg_base`, for text that must be present but read after
    /// every label. Falls back to [`Self::dim`] on palettes that cannot blend.
    pub fn faint(&self) -> Style {
        // Lands on the design system's `fg_gutter` token for the GrokNight palette
        const BLEND: f32 = 0.66;
        crate::render::color::blend_color(self.bg_base, self.gray_dim, BLEND)
            .map_or(self.dim(), |c| Style::new().fg(c))
    }

    pub const fn primary(&self) -> Style {
        Style::new().fg(self.text_primary)
    }

    /// Whether this is the bandless terminal-native palette: every band slot is `Reset` so the terminal's own canvas shows through.
    /// The canonical predicate for "reverse video / decoration fallback instead of a color band" — key every such branch off this, not off individual slots.
    pub const fn is_bandless(&self) -> bool {
        matches!(self.bg_visual, Color::Reset)
    }

    /// Bandless palette: both slots are `Reset` and composite to nothing, so reverse video keeps the cursor visible.
    pub const fn block_cursor_over(&self, surface: Color) -> Style {
        if self.is_bandless() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(surface).bg(self.text_primary)
        }
    }

    /// Patch over the already-rendered row. Bandless uses reverse video: a bright-black band can sit too close to default fg.
    pub const fn selection_overlay(&self) -> Style {
        if self.is_bandless() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.bg_visual)
        }
    }

    /// Hover analog of [`Self::selection_overlay`], keyed off `bg_hover`.
    /// On the bandless palette hover and selection share reverse video (they shared the same band before); the cursor row stays distinguishable by its marker/bold.
    /// Dropdowns and chips use this. Inline terminal rows use [`Self::row_hover_bg`] instead.
    pub const fn hover_overlay(&self) -> Style {
        if self.is_bandless() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.bg_hover)
        }
    }

    /// Dim row background for hover inside the terminal itself (scrollback tool
    /// rows, the queue pane, the dock). Half-blend of `bg_base` toward `bg_dark`,
    /// softer than the dropdown `bg_hover` token. Named ANSI and Reset palettes
    /// cannot blend, so fall back to `bg_hover` (Reset on the terminal theme: no band).
    pub fn row_hover_bg(&self) -> Color {
        crate::render::color::blend_color(self.bg_base, self.bg_dark, 0.5).unwrap_or(self.bg_hover)
    }

    /// Hairline fg for panel dividers/borders that RGB themes draw in the `bg_highlight` tone.
    /// On the bandless palette that would be a full-brightness line, so decoration falls back to bright black (`gray_dim`).
    pub const fn panel_border_fg(&self) -> Color {
        if self.is_bandless() {
            self.gray_dim
        } else {
            self.bg_highlight
        }
    }

    pub const fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

/// Per-row phase offset (`wave_rows` per cycle) so the wave is smooth regardless of block height. Brightness in [0.0, 1.0].
pub fn wave_brightness(tick: u64, row: u16, wave_rows: u16, speed: f32) -> f32 {
    use std::f32::consts::PI;

    let rows_per_wave = wave_rows.max(1) as f32;
    let phase = (row as f32 / rows_per_wave) * 2.0 * PI;

    let t = tick as f32 * speed;

    // sin²(t + phase) gives smooth 0-1 oscillation
    let sin_val = (t + phase).sin();
    sin_val * sin_val
}

/// Shared-tick pulse in [0.0, 1.0]. `sin²` has period π, so one cycle is `π / (speed * fps)` seconds.
pub fn pulse_brightness(tick: u64, speed: f32) -> f32 {
    let t = tick as f32 * speed;
    let sin_val = t.sin();
    sin_val * sin_val
}
