// Modified for Distill by Samuel Fajreldines, 2026.
//! All colors come from the `Theme` struct. No hardcoded colors elsewhere.
//! The default theme is GrokNight (neutral gray base with TokyoNight accents).
//!
//! ## Color support
//!
//! GrokNight is defined in `Color::Rgb` (truecolor).
//! At startup, [`Theme::current()`] quantizes every color to the terminal's detected capability level via [`Theme::quantized`].
//! Runtime-generated colors (syntax highlighting, blending) are also quantized via [`color_support::quantize`].

pub mod cache;
pub mod color_support;
pub mod env_appearance;
mod grokday;
mod groknight;
pub mod md_style;
pub mod osc11;
mod oscura;
mod rosepine;
pub mod system_appearance;
mod terminal_default;
pub mod tokyonight;

pub use color_support::quantize;
pub use tokyonight::{Theme, pulse_brightness, wave_brightness};

use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    GrokNight = 0,
    GrokDay = 1,
    TokyoNight = 2,
    RosePineMoon = 3,
    OscuraMidnight = 5,
    /// Every bg is `Reset` so the terminal canvas shows through; legible on both polarities without appearance detection.
    /// Hidden and unparseable while `cache::terminal_theme_enabled()` is off.
    Terminal = 6,
    /// Follow system appearance. Disk stores `"auto"`; `cache::CURRENT` holds only the resolved concrete kind. Excluded from [`ALL`].
    Auto = 4,
}

impl ThemeKind {
    /// All theme kinds (including those that may not work on the current terminal).
    pub const ALL: &[ThemeKind] = &[
        ThemeKind::GrokNight,
        ThemeKind::GrokDay,
        ThemeKind::TokyoNight,
        ThemeKind::RosePineMoon,
        ThemeKind::OscuraMidnight,
        ThemeKind::Terminal,
    ];

    /// [`ALL`] minus gated `terminal`. Ignores color capability ([`available()`] filters that). Derived from [`ALL`] so a new theme cannot be omitted.
    pub fn selectable() -> &'static [ThemeKind] {
        if cache::terminal_theme_enabled() {
            Self::ALL
        } else {
            static GATED: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.is_terminal_native())
                    .collect()
            });
            &GATED
        }
    }

    /// Theme kinds available on the current terminal.
    ///
    /// [`selectable()`] minus themes that require truecolor when the terminal does not support it (e.g., macOS Terminal.app is 256-color).
    pub fn available() -> &'static [ThemeKind] {
        if color_support::detect().has_truecolor() {
            return Self::selectable();
        }
        if cache::terminal_theme_enabled() {
            static NO_TRUECOLOR: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.requires_truecolor())
                    .collect()
            });
            &NO_TRUECOLOR
        } else {
            static NO_TRUECOLOR_GATED: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.requires_truecolor() && !kind.is_terminal_native())
                    .collect()
            });
            &NO_TRUECOLOR_GATED
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::GrokNight => "groknight",
            Self::TokyoNight => "tokyonight",
            Self::GrokDay => "grokday",
            Self::RosePineMoon => "rosepine-moon",
            Self::OscuraMidnight => "oscura-midnight",
            Self::Terminal => "terminal",
            Self::Auto => "auto",
        }
    }

    /// TokyoNight's blue-tinted backgrounds lose their character below truecolor; neutral grays survive quantization.
    pub fn requires_truecolor(self) -> bool {
        match self {
            Self::GrokNight => false,
            Self::TokyoNight => true,
            Self::GrokDay => false,
            Self::RosePineMoon => true,
            Self::OscuraMidnight => true,
            // Reset plus named ANSI-16 entries only — nothing to quantize.
            Self::Terminal => false,
            // Auto is resolved to a concrete theme before rendering.
            Self::Auto => false,
        }
    }

    /// Whether this kind paints the terminal-native palette ([`Theme::terminal_default`]) instead of an RGB palette, and so needs the same polarity-safe rendering paths as minimal mode's lock.
    #[must_use]
    pub fn is_terminal_native(self) -> bool {
        self == Self::Terminal
    }

    /// Alternate lowercase spellings accepted by [`from_name`](Self::from_name), excluding [`display_name`](Self::display_name).
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::GrokNight => &["grok-night", "dark"],
            Self::TokyoNight => &["tokyo-night", "tokyo"],
            Self::GrokDay => &["grok-day", "light", "day"],
            Self::RosePineMoon => &["rosepine", "rose-pine", "rose-pine-moon"],
            Self::OscuraMidnight => &["oscura"],
            Self::Terminal => &["terminal-default", "transparent", "native"],
            Self::Auto => &["system"],
        }
    }

    /// Parse a theme name (case-insensitive) against [`display_name`](Self::display_name) and [`aliases`](Self::aliases).
    /// Every conversion from string to `ThemeKind` must go through this function.
    /// While the `terminal` rollout gate is off its names do not parse, so a configured or typed value falls back like any unknown name.
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        let kind = Self::ALL
            .iter()
            .chain(std::iter::once(&Self::Auto))
            .copied()
            .find(|kind| {
                kind.display_name() == lower || kind.aliases().contains(&lower.as_str())
            })?;
        if kind.is_terminal_native() && !cache::terminal_theme_enabled() {
            return None;
        }
        Some(kind)
    }

    /// Whether this is the meta "auto" variant (resolved at runtime).
    #[must_use]
    pub fn is_auto(self) -> bool {
        self == Self::Auto
    }
}

/// `FromStr` wrapper around [`ThemeKind::from_name`].
impl std::str::FromStr for ThemeKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or(())
    }
}

/// Resolve a theme string to its canonical `&'static str` name.
/// Used by both dispatch and registry layers.
pub fn canonical_name(value: &str) -> Option<&'static str> {
    ThemeKind::from_name(value).map(|k| k.display_name())
}

/// Human-friendly display name for a canonical theme value (e.g. `"groknight"` becomes `"Grok Night"`).
/// Falls back to `value` verbatim.
pub fn display_name_for_canonical(value: &str) -> &str {
    match value {
        "auto" => "Auto",
        "groknight" => "Grok Night",
        "grokday" => "Grok Day",
        "tokyonight" => "Tokyo Night",
        "rosepine-moon" => "Rose Pine Moon",
        "terminal" => "Terminal",
        other => other,
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::groknight()
    }
}

impl Theme {
    /// Truecolor passes through; 256-color maps to nearest index; 16-color maps to ANSI names.
    pub fn quantized(self, level: color_support::ColorLevel) -> Self {
        use color_support::quantize_color;
        let q = |c: ratatui::style::Color| quantize_color(c, level);
        Self {
            bg_base: q(self.bg_base),
            bg_light: q(self.bg_light),
            bg_dark: q(self.bg_dark),
            bg_highlight: q(self.bg_highlight),
            bg_hover: q(self.bg_hover),
            bg_terminal: q(self.bg_terminal),

            accent_user: q(self.accent_user),
            accent_assistant: q(self.accent_assistant),
            accent_thinking: q(self.accent_thinking),
            accent_tool: q(self.accent_tool),
            accent_system: q(self.accent_system),
            accent_error: q(self.accent_error),
            accent_success: q(self.accent_success),
            accent_running: q(self.accent_running),
            accent_skill: q(self.accent_skill),

            text_primary: q(self.text_primary),
            text_secondary: q(self.text_secondary),

            gray_dim: q(self.gray_dim),
            gray: q(self.gray),
            gray_bright: q(self.gray_bright),

            command: q(self.command),
            path: q(self.path),
            running: q(self.running),
            warning: q(self.warning),

            fuzzy_accent: q(self.fuzzy_accent),

            accent_plan: q(self.accent_plan),

            accent_verify: q(self.accent_verify),

            accent_remember: q(self.accent_remember),

            selection_border: q(self.selection_border),
            hover_border: q(self.hover_border),
            prompt_border: q(self.prompt_border),
            prompt_border_active: q(self.prompt_border_active),

            accent_model: q(self.accent_model),

            scrollbar_bg: q(self.scrollbar_bg),
            scrollbar_fg: q(self.scrollbar_fg),

            diff_delete_bg: q(self.diff_delete_bg),
            diff_delete_fg: q(self.diff_delete_fg),
            diff_insert_bg: q(self.diff_insert_bg),
            diff_insert_fg: q(self.diff_insert_fg),
            diff_equal_fg: q(self.diff_equal_fg),
            diff_gutter_fg: q(self.diff_gutter_fg),

            bg_visual: q(self.bg_visual),

            paste_bg: q(self.paste_bg),
            paste_fg: q(self.paste_fg),
            paste_dim: q(self.paste_dim),

            md_heading_h1: q(self.md_heading_h1),
            md_heading_h1_mod: self.md_heading_h1_mod,
            md_heading_h2: q(self.md_heading_h2),
            md_heading_h2_mod: self.md_heading_h2_mod,
            md_heading_h3: q(self.md_heading_h3),
            md_heading_h3_mod: self.md_heading_h3_mod,
            md_heading_h4: q(self.md_heading_h4),
            md_heading_h4_mod: self.md_heading_h4_mod,
            md_heading_h5: q(self.md_heading_h5),
            md_heading_h5_mod: self.md_heading_h5_mod,
            md_heading_h6: q(self.md_heading_h6),
            md_heading_h6_mod: self.md_heading_h6_mod,
            md_code: q(self.md_code),
            md_task_checked: q(self.md_task_checked),
            md_task_unchecked: q(self.md_task_unchecked),
            md_muted: q(self.md_muted),
            md_code_bg: q(self.md_code_bg),
            md_text: q(self.md_text),
            link_fg: q(self.link_fg),
        }
    }

    /// Quantized cached kind. Windows boosts contrast so structural RGB survives display gamma.
    /// Basic or legacy ConHost pins chrome to ANSI names; otherwise every dark RGB collapses onto one ANSI16 slot.
    pub fn current() -> Self {
        let level = color_support::detect();
        if cache::terminal_native_locked() {
            return Self::terminal_default().quantized(level);
        }
        let kind = cache::current_kind();
        // Before polarity adaptations: contrast boost and ANSI16 overrides would paint opaque bgs over `Reset`.
        if kind.is_terminal_native() {
            return Self::terminal().quantized(level);
        }
        let base = match kind {
            ThemeKind::GrokNight => Self::groknight(),
            ThemeKind::TokyoNight => Self::tokyonight(),
            ThemeKind::GrokDay => Self::grokday(),
            ThemeKind::RosePineMoon => Self::rosepine_moon(),
            ThemeKind::OscuraMidnight => Self::oscura_midnight(),
            // Handled by the early return above.
            ThemeKind::Terminal => Self::terminal(),
            // Auto is resolved to a concrete theme before being stored; if reached, fall back to GrokNight
            ThemeKind::Auto => Self::groknight(),
        };
        // Sample polarity before quantizing
        // After quantization `bg_base` may land on a named or indexed entry whose luminance depends on the host palette
        let dark = base.is_dark();
        let adapted = if cfg!(target_os = "windows") {
            base.windows_contrast_boost(dark)
        } else {
            base
        };
        let adapted = adapted.quantized(level);
        // Basic or legacy ConHost below truecolor: naive quantization collapses dark RGB onto Black.
        // `has_color()` is required so `NO_COLOR` is not defeated by named-ANSI repaints of `Reset`.
        if level.has_color()
            && (level == color_support::ColorLevel::Basic
                || (crate::glyphs::is_legacy_windows_console() && !level.has_truecolor()))
        {
            adapted.ansi16_chrome_overrides(dark)
        } else {
            adapted
        }
    }

    pub fn current_kind() -> ThemeKind {
        cache::current_kind()
    }

    /// Whether this theme paints no diff row bands (`diff_*_bg` is `Reset`).
    /// In that case changed diff lines carry a whole-line red/green *foreground* instead of syntax highlighting on a colored band.
    #[must_use]
    pub fn diff_uses_line_fg(&self) -> bool {
        use ratatui::style::Color;
        self.diff_delete_bg == Color::Reset && self.diff_insert_bg == Color::Reset
    }

    /// In-memory only; the event loop emits OSC 12. No-op while the terminal-native lock is engaged.
    pub fn apply_kind(kind: ThemeKind) -> ThemeKind {
        if cache::terminal_native_locked() {
            return cache::current_kind();
        }
        let effective = Self::clamp_to_terminal(kind);
        cache::set(effective);
        effective
    }

    fn clamp_to_terminal(kind: ThemeKind) -> ThemeKind {
        if kind.requires_truecolor() && !color_support::detect().has_truecolor() {
            ThemeKind::GrokNight
        } else {
            kind
        }
    }

    /// Native ~12-unit RGB steps collapse under Windows display gamma; ConHost needs ~24-32 levels per channel.
    /// Prompt-block bg is asymmetric: a dark step on a light canvas weighs more, so that direction is pushed less.
    fn windows_contrast_boost(self, dark: bool) -> Self {
        use ratatui::style::Color;

        /// Move `color` `amount` levels per channel further from `base`.
        /// Returns `color` unchanged when either side isn't RGB.
        fn push_away(base: Color, color: Color, amount: i16) -> Color {
            let Color::Rgb(br, b_green, bb) = base else {
                return color;
            };
            let Color::Rgb(cr, cg, cb) = color else {
                return color;
            };
            let base_lum = br as i16 + b_green as i16 + bb as i16;
            let color_lum = cr as i16 + cg as i16 + cb as i16;
            let sign: i16 = if color_lum >= base_lum { 1 } else { -1 };
            let nudge = |c: u8| (c as i16 + sign * amount).clamp(0, 255) as u8;
            Color::Rgb(nudge(cr), nudge(cg), nudge(cb))
        }

        let bg = self.bg_base;
        let user_block_push = if dark { 28 } else { 8 };
        Self {
            bg_light: push_away(bg, self.bg_light, user_block_push),
            bg_dark: push_away(bg, self.bg_dark, 16),
            bg_highlight: push_away(bg, self.bg_highlight, 28),
            bg_hover: push_away(bg, self.bg_hover, 16),
            gray_dim: push_away(bg, self.gray_dim, 40),
            selection_border: push_away(bg, self.selection_border, 36),
            prompt_border: push_away(bg, self.prompt_border, 40),
            prompt_border_active: push_away(bg, self.prompt_border_active, 60),
            hover_border: push_away(bg, self.hover_border, 28),
            scrollbar_bg: push_away(bg, self.scrollbar_bg, 16),
            scrollbar_fg: push_away(bg, self.scrollbar_fg, 32),
            bg_visual: push_away(bg, self.bg_visual, 16),
            md_code_bg: push_away(bg, self.md_code_bg, 16),
            ..self
        }
    }

    /// Style for shell command suggestion ghost text (dimmed italic).
    /// Inherits [`Self::dim`]'s polarity-safe rule on terminal-native
    /// palettes instead of painting bright black.
    pub fn ghost_text_style(&self) -> ratatui::style::Style {
        self.dim().add_modifier(ratatui::style::Modifier::ITALIC)
    }

    /// True when `bg_base` reads as dark per BT.709 luminance.
    /// Must be called pre-quantization while `bg_base` is still RGB; named/Reset fall back to "dark" (the default theme polarity).
    pub fn is_dark(&self) -> bool {
        use ratatui::style::Color;
        let (r, g, b) = match self.bg_base {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Indexed(n) => crate::render::color::indexed_to_rgb(n),
            _ => return true,
        };
        crate::theme::osc11::classify_luminance(r, g, b)
            == crate::theme::system_appearance::SystemAppearance::Dark
    }

    /// ANSI16 has two grays; naive distance collapses pastel accents onto that ramp and erases state.
    /// Pin chrome to the four neutrals and accents to hue-preserving slots (bright on dark, normal on light).
    /// Canvas-blending surfaces pin to theme polarity, not `Reset`, or the terminal profile punches holes through them.
    fn ansi16_chrome_overrides(self, dark: bool) -> Self {
        use ratatui::style::Color;
        // Theme polarity canvas: what the body bg should look like
        // Matches the natural quantize result for `bg_base` on both built-in themes
        // The explicit pin gives themes whose bg RGB doesn't quantize cleanly the right polarity anyway
        let canvas_bg = if dark { Color::Black } else { Color::White };
        // One palette step off the canvas: DarkGray (ANSI 8) on black, Gray (ANSI 7 / silver) on white
        // ANSI16 has no slot between these and the canvas, so the elevation reads louder than the truecolor design
        // But it's guaranteed visible on every 16-color terminal, including museum-grade `TERM=ansi` boxes
        let elevated_bg = if dark { Color::DarkGray } else { Color::Gray };
        // Max-contrast fg for focused chrome and assistant body.
        let high_contrast_fg = if dark { Color::White } else { Color::Black };
        // Mid-tone fg for muted labels: silver on black, dark-gray on white
        // This is the higher-contrast of the two muted slots, used for secondary text that still needs to read clearly
        let muted_fg = if dark { Color::Gray } else { Color::DarkGray };
        // Low-contrast fg for genuinely dim chrome (soft modal/picker frames, the unselected `>` prompt indicator)
        // The polarity is the INVERSE of `muted_fg`: DarkGray sits next to Black, Gray (silver) next to White
        // A separate slot keeps dim chrome from reading at the same weight as secondary text
        let dim_fg = if dark { Color::DarkGray } else { Color::Gray };

        // ── Polarity-aware semantic hues ────────────────────────────
        // Normal ANSI hues (idx 1-7) are designed at ~50% luminance and read well on light backgrounds
        // Light variants (idx 9-15) are full saturation and read well on dark backgrounds
        let red = if dark { Color::LightRed } else { Color::Red };
        let green = if dark {
            Color::LightGreen
        } else {
            Color::Green
        };
        let yellow = if dark {
            Color::LightYellow
        } else {
            Color::Yellow
        };
        let blue = if dark { Color::LightBlue } else { Color::Blue };
        let magenta = if dark {
            Color::LightMagenta
        } else {
            Color::Magenta
        };
        let cyan = if dark { Color::LightCyan } else { Color::Cyan };
        Self {
            // ── Elevated surfaces: one step off the canvas ──────────────
            // Hover/highlight/visual-selection rows need to read as a distinct "raised" band against the body
            // Without this every GrokNight bg field quantizes to Color::Black and these become invisible
            bg_light: elevated_bg,
            bg_highlight: elevated_bg,
            bg_hover: elevated_bg,
            bg_visual: elevated_bg,

            // Theme polarity, not Reset: 16-color cannot sunken-blend, and Reset would follow a disagreeing terminal canvas.
            bg_dark: canvas_bg,
            md_code_bg: canvas_bg,
            paste_bg: canvas_bg,
            scrollbar_bg: canvas_bg,

            // Idle frame uses muted_fg: DarkGray (ANSI 8) is near-bg on many palettes and the frame vanishes.
            // Hover stays DarkGray; selection is muted_fg; focus is high_contrast_fg.
            prompt_border: muted_fg,
            prompt_border_active: high_contrast_fg,
            selection_border: muted_fg,
            hover_border: Color::DarkGray,

            // Scrollbar thumb stays visible against the canvas-matched track.
            scrollbar_fg: muted_fg,

            // ── Foreground / text hierarchy ─────────────────────────────
            // Prompt textarea and chrome captions use these directly
            // blend_color cannot mix named ANSI, so they must already be readable slots
            text_primary: high_contrast_fg,
            text_secondary: muted_fg,
            md_text: high_contrast_fg,
            // Selected user-prompt `>` (drives the user selection accent and the OSC 12 cursor color) takes max-contrast fg
            // The selection pops against the canvas: White on dark, Black on light
            accent_user: high_contrast_fg,
            // Two-tier grey: `gray`/`gray_bright` carry secondary text and need readable contrast, so they take `muted_fg`
            // `gray_dim` is for genuinely faded chrome (modal frames, unselected `>` prompt indicator) and takes `dim_fg`
            // Two tiers is the most ANSI16 can express without colliding with the elevated-bg slot
            gray: muted_fg,
            gray_bright: muted_fg,
            gray_dim: dim_fg,

            // ANSI16 has 6 chromatic slots, so sub-hues fold onto the dominant family. Magenta covers the purple/violet accents.
            accent_assistant: magenta,
            accent_thinking: magenta,
            accent_running: magenta,
            accent_verify: magenta,
            // Red family: error states and diff deletes
            accent_error: red,
            diff_delete_fg: red,
            // Green family: success states, remember mode, diff inserts
            accent_success: green,
            accent_remember: green,
            diff_insert_fg: green,
            // Blue family: system messages, skill invocations, fuzzy search matches
            accent_system: blue,
            accent_skill: blue,
            fuzzy_accent: blue,
            // Cyan family: model name and the legacy `running` indicator (distinct from the magenta `accent_running` used for subagents).
            // ANSI16 has no separate teal slot, so the truecolor teal model accent folds onto cyan here.
            accent_model: cyan,
            running: cyan,
            // Yellow family: warning text, plan-mode gold, shell commands, file paths
            // ANSI16 has no orange or gold slot, so warm accents all fold onto yellow
            command: yellow,
            warning: yellow,
            path: yellow,
            accent_plan: yellow,

            // Markdown content: naive Basic quantize lands GrokNight md_code / md_muted / h4-h6 on DarkGray (ANSI 8)
            // Many palettes tune that slot near the background, so inline code and table borders vanish in tmux over ssh
            // `md_muted` uses muted_fg, not dim_fg: format_table also stacks DIM on table borders
            md_heading_h1: cyan,
            md_heading_h2: blue,
            md_heading_h3: magenta,
            md_heading_h4: high_contrast_fg,
            md_heading_h5: muted_fg,
            md_heading_h6: muted_fg,
            md_code: cyan,
            md_muted: muted_fg,
            md_task_checked: green,
            md_task_unchecked: muted_fg,
            link_fg: blue,
            diff_equal_fg: muted_fg,
            diff_gutter_fg: muted_fg,
            ..self
        }
    }
}

/// Gates OSC 112: an unprompted reset makes Ghostty latch the cursor color and stop tracking theme changes. Only undo what we painted.
static CURSOR_COLOR_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// OSC 12 wants RGB even below truecolor, so every variant is resolved back. `Reset` yields `None` so the terminal keeps its profile cursor.
pub fn cursor_color_escape() -> Option<String> {
    let theme = Theme::current();
    let (r, g, b) = crate::render::color::resolve_to_rgb(theme.accent_user)?;
    Some(format!("\x1b]12;rgb:{r:02x}/{g:02x}/{b:02x}\x07"))
}

/// Write [`cursor_color_escape`] inline. Startup only, while no writer thread is live; once the event loop runs, its OSC 12/112 tracker is the single emitter (including screen-mode switches).
pub fn apply_cursor_color() {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    let Some(escape) = cursor_color_escape() else {
        // No RGB accent (terminal-native palette, NO_COLOR): undo our own
        // paint if any, otherwise stay silent (see `CURSOR_COLOR_APPLIED`).
        reset_cursor_color_if_applied();
        return;
    };
    CURSOR_COLOR_APPLIED.store(true, Ordering::Relaxed);
    distill_shared::stderr::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(escape.as_bytes());
        let _ = stderr.flush();
    });
}

/// [`reset_cursor_color`] gated on this session having painted the cursor
/// (see [`CURSOR_COLOR_APPLIED`]), clearing the flag. Atomic swap, so
/// panic- and signal-handler-safe.
pub fn reset_cursor_color_if_applied() {
    if CURSOR_COLOR_APPLIED.swap(false, std::sync::atomic::Ordering::Relaxed) {
        reset_cursor_color();
    }
}

/// The OSC 112 escape that resets the terminal cursor color to its default.
pub const CURSOR_COLOR_RESET_ESCAPE: &str = "\x1b]112\x07";

/// Record that a cursor-color escape went out over the render wire (the event
/// loop's queued emitter, which bypasses [`apply_cursor_color`]), keeping
/// [`CURSOR_COLOR_APPLIED`] truthful for the teardown reset.
pub fn note_cursor_color_on_wire(applied: bool) {
    CURSOR_COLOR_APPLIED.store(applied, std::sync::atomic::Ordering::Relaxed);
}

/// Test-only access to [`CURSOR_COLOR_APPLIED`] so the OSC 12/112 gating is
/// assertable without capturing the raw stderr fd.
#[cfg(any(test, feature = "test-support"))]
pub fn cursor_color_applied_for_test() -> bool {
    CURSOR_COLOR_APPLIED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test-only setter for [`CURSOR_COLOR_APPLIED`].
#[cfg(any(test, feature = "test-support"))]
pub fn set_cursor_color_applied_for_test(v: bool) {
    CURSOR_COLOR_APPLIED.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Reset the terminal cursor color to the terminal's default via OSC 112.
///
/// Called on shutdown to restore the user's original cursor appearance.
pub fn reset_cursor_color() {
    use std::io::Write;
    distill_shared::stderr::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(CURSOR_COLOR_RESET_ESCAPE.as_bytes());
        let _ = stderr.flush();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `faint()` sits strictly between the background and `gray_dim` on a palette that can blend, and degrades to the DIM
    /// attribute with no hard colour on the bandless terminal palette.
    #[test]
    fn faint_is_between_bg_and_gray_dim_or_dim_when_unblendable() {
        use ratatui::style::{Color, Modifier};
        let luma = |c: Color| match c {
            Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
            other => panic!("expected RGB, got {other:?}"),
        };
        let night = Theme::groknight();
        let faint = night.faint().fg.expect("GrokNight blends to a hard colour");
        assert!(luma(night.bg_base) < luma(faint) && luma(faint) < luma(night.gray_dim));

        let terminal = Theme::terminal_default();
        let style = terminal.faint();
        assert_eq!(style.fg, None, "no hard colour on the bandless palette");
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn from_name_auto() {
        assert_eq!(ThemeKind::from_name("auto"), Some(ThemeKind::Auto));
    }

    #[test]
    fn from_name_system() {
        assert_eq!(ThemeKind::from_name("system"), Some(ThemeKind::Auto));
    }

    #[test]
    fn from_name_auto_case_insensitive() {
        assert_eq!(ThemeKind::from_name("AUTO"), Some(ThemeKind::Auto));
        assert_eq!(ThemeKind::from_name("Auto"), Some(ThemeKind::Auto));
        assert_eq!(ThemeKind::from_name("SYSTEM"), Some(ThemeKind::Auto));
    }

    /// Every alias parses back to its own kind, so no alias is shadowed by another kind's name.
    #[test]
    fn from_name_accepts_every_alias() {
        let _guard = cache::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        cache::reset_for_test();
        cache::set_terminal_theme_enabled(true);
        for kind in ThemeKind::ALL.iter().chain([&ThemeKind::Auto]).copied() {
            for alias in kind.aliases() {
                assert_eq!(ThemeKind::from_name(alias), Some(kind), "alias {alias}");
            }
        }
        cache::reset_for_test();
    }

    #[test]
    fn display_name_auto() {
        assert_eq!(ThemeKind::Auto.display_name(), "auto");
    }

    #[test]
    fn is_auto_returns_true_for_auto() {
        assert!(ThemeKind::Auto.is_auto());
    }

    #[test]
    fn is_auto_returns_false_for_concrete_variants() {
        assert!(!ThemeKind::GrokNight.is_auto());
        assert!(!ThemeKind::GrokDay.is_auto());
        assert!(!ThemeKind::TokyoNight.is_auto());
        assert!(!ThemeKind::RosePineMoon.is_auto());
        assert!(!ThemeKind::OscuraMidnight.is_auto());
    }

    #[test]
    fn all_excludes_auto() {
        assert!(!ThemeKind::ALL.contains(&ThemeKind::Auto));
    }

    #[test]
    fn available_excludes_auto() {
        assert!(!ThemeKind::available().contains(&ThemeKind::Auto));
    }

    /// With the rollout gate off, the `terminal` theme neither parses nor appears in any catalog; on, both come back.
    #[test]
    fn terminal_rollout_gate_hides_and_rejects_the_terminal_theme() {
        // The gate is a process global (test default: on) — serialize with the other theme-global tests and restore via reset.
        let _guard = cache::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        cache::reset_for_test();

        cache::set_terminal_theme_enabled(false);
        for name in ["terminal", "terminal-default", "transparent", "native"] {
            assert_eq!(ThemeKind::from_name(name), None, "{name} must not parse");
        }
        assert!(!ThemeKind::selectable().contains(&ThemeKind::Terminal));
        assert!(!ThemeKind::available().contains(&ThemeKind::Terminal));

        cache::set_terminal_theme_enabled(true);
        assert_eq!(ThemeKind::from_name("terminal"), Some(ThemeKind::Terminal));
        assert!(ThemeKind::selectable().contains(&ThemeKind::Terminal));
        assert!(ThemeKind::available().contains(&ThemeKind::Terminal));

        cache::reset_for_test();
    }

    #[test]
    fn is_dark_classifies_built_in_themes() {
        // Sanity-check the polarity sampler against the theme catalog.
        assert!(Theme::groknight().is_dark());
        assert!(Theme::tokyonight().is_dark());
        assert!(Theme::rosepine_moon().is_dark());
        assert!(Theme::oscura_midnight().is_dark());
        assert!(!Theme::grokday().is_dark());
    }

    #[test]
    fn ansi16_overrides_dark_uses_bright_white_high_contrast() {
        use ratatui::style::Color;
        let t = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t.bg_light, Color::DarkGray);
        assert_eq!(t.bg_highlight, Color::DarkGray);
        // Idle prompt border sits at `muted_fg` (Gray on dark canvas); focused border jumps to max-contrast White
        assert_eq!(t.prompt_border, Color::Gray);
        assert_eq!(t.prompt_border_active, Color::White);
        assert_eq!(t.md_text, Color::White);
        assert_eq!(t.text_primary, Color::White);
        assert_eq!(t.text_secondary, Color::Gray);
        // Two-tier grey: secondary text (`gray`) reads at the muted slot (silver), `gray_dim` at the dim slot (DarkGray)
        // See `ansi16_overrides_gray_hierarchy_collapses_to_two_slots`
        assert_eq!(t.gray, Color::Gray);
        assert_eq!(t.gray_dim, Color::DarkGray);
    }

    #[test]
    fn ansi16_overrides_light_inverts_high_contrast_and_elevated_bg() {
        // Light canvas inverts polarity: elevated bg reads darker (silver step from white), high-contrast fg is Black, muted fg is DarkGray
        // The dim slot (`gray_dim`) flips to silver; see `ansi16_overrides_gray_hierarchy_collapses_to_two_slots`
        use ratatui::style::Color;
        let t = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t.bg_light, Color::Gray);
        assert_eq!(t.bg_highlight, Color::Gray);
        assert_eq!(t.prompt_border, Color::DarkGray);
        assert_eq!(t.prompt_border_active, Color::Black);
        assert_eq!(t.md_text, Color::Black);
        assert_eq!(t.text_primary, Color::Black);
        assert_eq!(t.text_secondary, Color::DarkGray);
        assert_eq!(t.gray, Color::DarkGray);
        assert_eq!(t.gray_dim, Color::Gray);
    }

    #[test]
    fn ansi16_overrides_preserve_bg_base() {
        // `bg_base` belongs to the user's terminal session, not to us: we never overwrite it
        // The polarity-pinned canvas surfaces are tested separately in `ansi16_overrides_canvas_matching_surfaces_use_theme_polarity`
        let base = Theme::groknight();
        let t = base.ansi16_chrome_overrides(true);
        assert_eq!(t.bg_base, base.bg_base);
    }

    #[test]
    fn ansi16_overrides_state_accents_pin_to_polarity_aware_hue() {
        // running / completed / error must read as their hue family even at ANSI16
        // A dark canvas takes bright (Light*) variants, a light canvas normal variants
        // Without these pins the source pastel RGBs collapse onto silver/DarkGray and every state signal becomes the same gray
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.accent_error, Color::LightRed);
        assert_eq!(t_dark.accent_success, Color::LightGreen);
        assert_eq!(t_dark.accent_running, Color::LightMagenta);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.accent_error, Color::Red);
        assert_eq!(t_light.accent_success, Color::Green);
        assert_eq!(t_light.accent_running, Color::Magenta);
    }

    #[test]
    fn ansi16_overrides_md_palette_never_lands_on_dark_gray() {
        // On a dark canvas no md field may land on DarkGray, the slot palettes tune near their background
        use ratatui::style::Color;
        let t = Theme::groknight().ansi16_chrome_overrides(true);
        for (name, c) in [
            ("md_heading_h1", t.md_heading_h1),
            ("md_heading_h2", t.md_heading_h2),
            ("md_heading_h3", t.md_heading_h3),
            ("md_heading_h4", t.md_heading_h4),
            ("md_heading_h5", t.md_heading_h5),
            ("md_heading_h6", t.md_heading_h6),
            ("md_code", t.md_code),
            ("md_muted", t.md_muted),
            ("md_task_checked", t.md_task_checked),
            ("md_task_unchecked", t.md_task_unchecked),
            ("link_fg", t.link_fg),
            ("diff_equal_fg", t.diff_equal_fg),
            ("diff_gutter_fg", t.diff_gutter_fg),
        ] {
            assert_ne!(
                c,
                Color::DarkGray,
                "{name} must not land on DarkGray on a dark canvas \
                 (invisible on palettes that tune ANSI 8 near-bg)"
            );
            assert!(
                !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                "{name} must be a named ANSI16 color, got {c:?}"
            );
        }
    }

    #[test]
    fn ansi16_overrides_md_palette_polarity_aware_hues() {
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.md_code, Color::LightCyan);
        assert_eq!(t_dark.md_muted, Color::Gray);
        assert_eq!(t_dark.md_heading_h1, Color::LightCyan);
        assert_eq!(t_dark.md_heading_h2, Color::LightBlue);
        assert_eq!(t_dark.md_heading_h3, Color::LightMagenta);
        assert_eq!(t_dark.md_heading_h4, Color::White);
        assert_eq!(t_dark.link_fg, Color::LightBlue);
        assert_eq!(t_dark.md_task_checked, Color::LightGreen);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.md_code, Color::Cyan);
        assert_eq!(t_light.md_muted, Color::DarkGray);
        assert_eq!(t_light.md_heading_h1, Color::Cyan);
        assert_eq!(t_light.md_heading_h2, Color::Blue);
        assert_eq!(t_light.md_heading_h3, Color::Magenta);
        assert_eq!(t_light.md_heading_h4, Color::Black);
        assert_eq!(t_light.link_fg, Color::Blue);
        assert_eq!(t_light.md_task_checked, Color::Green);
    }

    #[test]
    fn ansi16_overrides_magenta_family_shares_slot() {
        // assistant turn, mid-stream thinking, running indicator, and context-overhead accent all use a purple/violet hue in truecolor
        // ANSI16 has one magenta slot per polarity, so they all fold onto it together
        // They live in different surfaces so the collision doesn't cause confusion
        use ratatui::style::Color;
        let t = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t.accent_assistant, Color::LightMagenta);
        assert_eq!(t.accent_thinking, Color::LightMagenta);
        assert_eq!(t.accent_running, Color::LightMagenta);
        assert_eq!(t.accent_verify, Color::LightMagenta);
    }

    #[test]
    fn ansi16_overrides_yellow_family_absorbs_orange_and_gold() {
        // ANSI16 has no orange or gold slot, so warm accents (command, warning, path, plan) all fold onto Yellow / LightYellow
        // Preserving the warm hue family matters more than per-accent differentiation the palette cannot represent
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        for f in [
            t_dark.command,
            t_dark.warning,
            t_dark.path,
            t_dark.accent_plan,
        ] {
            assert_eq!(f, Color::LightYellow);
        }

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        for f in [
            t_light.command,
            t_light.warning,
            t_light.path,
            t_light.accent_plan,
        ] {
            assert_eq!(f, Color::Yellow);
        }
    }

    #[test]
    fn ansi16_overrides_cyan_family_absorbs_teal() {
        // ANSI16 has no teal slot; the model teal folds onto cyan.
        // The `running` indicator (legacy cyan, distinct from the magenta `accent_running` used for subagents) also lives here.
        use ratatui::style::Color;
        let t = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t.accent_model, Color::LightCyan);
        assert_eq!(t.running, Color::LightCyan);
    }

    #[test]
    fn ansi16_overrides_blue_family_pins_system_skill_fuzzy() {
        // System messages, skill invocations, and fuzzy-search matches all carry the same blue family in truecolor
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.accent_system, Color::LightBlue);
        assert_eq!(t_dark.accent_skill, Color::LightBlue);
        assert_eq!(t_dark.fuzzy_accent, Color::LightBlue);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.accent_system, Color::Blue);
        assert_eq!(t_light.accent_skill, Color::Blue);
        assert_eq!(t_light.fuzzy_accent, Color::Blue);
    }

    #[test]
    fn ansi16_overrides_diff_fg_uses_polarity_aware_red_green() {
        // Diff add/remove rely on fg color for their primary signal at ANSI16 (the subtle pastel bg tints don't survive quantization)
        // Pin fg to red / green so deletes and inserts stay legible.
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.diff_delete_fg, Color::LightRed);
        assert_eq!(t_dark.diff_insert_fg, Color::LightGreen);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.diff_delete_fg, Color::Red);
        assert_eq!(t_light.diff_insert_fg, Color::Green);
    }

    #[test]
    fn ansi16_overrides_accent_user_uses_high_contrast() {
        // accent_user drives the selected-user-prompt `>` color and the OSC 12 cursor color
        // It's pinned to max-contrast fg in both polarities so the selection always pops: White on a dark canvas, Black on a light one
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.accent_user, Color::White);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.accent_user, Color::Black);
    }

    #[test]
    fn ansi16_overrides_extended_dark_pins_elevated_bg_to_dark_gray() {
        // Without these pins, every dark RGB bg quantizes to Color::Black and the hover/visual/highlight bands collapse onto the canvas
        use ratatui::style::Color;
        let t = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t.bg_hover, Color::DarkGray);
        assert_eq!(t.bg_visual, Color::DarkGray);
    }

    #[test]
    fn ansi16_overrides_extended_light_pins_elevated_bg_to_gray() {
        use ratatui::style::Color;
        let t = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t.bg_hover, Color::Gray);
        assert_eq!(t.bg_visual, Color::Gray);
    }

    #[test]
    fn ansi16_overrides_canvas_matching_surfaces_use_theme_polarity() {
        // Sunken bg, code-block bg, paste chip bg, and scrollbar track must match the theme polarity (Black for dark themes, White for light)
        // Color::Reset would defer to the user's terminal canvas, which can disagree with the theme (e.g. GrokNight on a white-canvas terminal).
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.bg_dark, Color::Black);
        assert_eq!(t_dark.md_code_bg, Color::Black);
        assert_eq!(t_dark.paste_bg, Color::Black);
        assert_eq!(t_dark.scrollbar_bg, Color::Black);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.bg_dark, Color::White);
        assert_eq!(t_light.md_code_bg, Color::White);
        assert_eq!(t_light.paste_bg, Color::White);
        assert_eq!(t_light.scrollbar_bg, Color::White);
    }

    #[test]
    fn ansi16_overrides_border_hierarchy_is_distinct() {
        // Dark: idle/selection share Gray so the frame survives palettes that tune ANSI 8 near-bg. Light: those sit on DarkGray; focus is Black.
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.hover_border, Color::DarkGray);
        assert_eq!(t_dark.prompt_border, Color::Gray);
        assert_eq!(t_dark.selection_border, Color::Gray);
        assert_eq!(t_dark.prompt_border_active, Color::White);
        assert_ne!(t_dark.selection_border, t_dark.prompt_border_active);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.hover_border, Color::DarkGray);
        assert_eq!(t_light.prompt_border, Color::DarkGray);
        assert_eq!(t_light.selection_border, Color::DarkGray);
        assert_eq!(t_light.prompt_border_active, Color::Black);
        assert_ne!(t_light.selection_border, t_light.prompt_border_active);
    }

    #[test]
    fn ansi16_overrides_scrollbar_thumb_visible_against_canvas() {
        // scrollbar_fg must not equal scrollbar_bg or the thumb is invisible against the canvas-pinned track
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.scrollbar_fg, Color::Gray);
        assert_ne!(t_dark.scrollbar_fg, t_dark.scrollbar_bg);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.scrollbar_fg, Color::DarkGray);
        assert_ne!(t_light.scrollbar_fg, t_light.scrollbar_bg);
    }

    #[test]
    fn scrollbar_thumb_contrasts_with_track_in_all_themes() {
        // Thumb must sit away from the canvas vs the track, by >=30 summed-RGB, or follow-mode's 40% blend hides the scrollbar.
        use ratatui::style::Color;
        let lum = |c: Color, field: &str, kind: ThemeKind| -> i32 {
            let Color::Rgb(r, g, b) = c else {
                panic!("{kind:?} {field} must be Color::Rgb, got {c:?}");
            };
            r as i32 + g as i32 + b as i32
        };
        for &kind in ThemeKind::ALL {
            let theme = match kind {
                ThemeKind::GrokNight => Theme::groknight(),
                ThemeKind::GrokDay => Theme::grokday(),
                ThemeKind::TokyoNight => Theme::tokyonight(),
                ThemeKind::RosePineMoon => Theme::rosepine_moon(),
                ThemeKind::OscuraMidnight => Theme::oscura_midnight(),
                // Reset plus named ANSI entries: the scrollbar rides the
                // terminal's own fg/bg contrast, so there is no RGB delta.
                ThemeKind::Terminal => continue,
                ThemeKind::Auto => unreachable!("ALL excludes Auto"),
            };
            let track = lum(theme.scrollbar_bg, "scrollbar_bg", kind);
            let thumb = lum(theme.scrollbar_fg, "scrollbar_fg", kind);
            let delta = thumb - track;
            if theme.is_dark() {
                assert!(
                    delta >= 30,
                    "{kind:?}: thumb (Σ{thumb}) must be ≥30 lighter than \
                     track (Σ{track}) on a dark theme, got Δ{delta}"
                );
            } else {
                assert!(
                    delta <= -30,
                    "{kind:?}: thumb (Σ{thumb}) must be ≥30 darker than \
                     track (Σ{track}) on a light theme, got Δ{delta}"
                );
            }
        }
    }

    #[test]
    fn ansi16_quantize_without_override_collapses_groknight_backgrounds() {
        // Ratchet: naive Basic maps every dark GrokNight bg to Black. If a mid-tone level appears, revisit the override.
        use ratatui::style::Color;
        let q = Theme::groknight().quantized(color_support::ColorLevel::Basic);
        for (name, color) in [
            ("bg_base", q.bg_base),
            ("bg_light", q.bg_light),
            ("bg_dark", q.bg_dark),
            ("bg_highlight", q.bg_highlight),
            ("bg_hover", q.bg_hover),
            ("bg_visual", q.bg_visual),
            ("md_code_bg", q.md_code_bg),
            ("scrollbar_bg", q.scrollbar_bg),
        ] {
            assert_eq!(
                color,
                Color::Black,
                "{name} should collapse to Black without the override"
            );
        }
    }

    #[test]
    fn ansi16_overrides_gray_hierarchy_collapses_to_two_slots() {
        // ANSI16 has two greys. Bright and medium share muted_fg so secondary text stays readable; dim keeps the lower slot.
        use ratatui::style::Color;
        let t_dark = Theme::groknight().ansi16_chrome_overrides(true);
        assert_eq!(t_dark.gray, Color::Gray);
        assert_eq!(t_dark.gray_bright, Color::Gray);
        assert_eq!(t_dark.gray_dim, Color::DarkGray);
        assert_ne!(t_dark.gray, t_dark.gray_dim);

        let t_light = Theme::grokday().ansi16_chrome_overrides(false);
        assert_eq!(t_light.gray, Color::DarkGray);
        assert_eq!(t_light.gray_bright, Color::DarkGray);
        assert_eq!(t_light.gray_dim, Color::Gray);
        assert_ne!(t_light.gray, t_light.gray_dim);
    }

    #[test]
    fn auto_does_not_require_truecolor() {
        assert!(!ThemeKind::Auto.requires_truecolor());
    }

    #[test]
    fn resolve_to_rgb_handles_rgb_indexed_named_and_reset() {
        use crate::render::color::{indexed_to_rgb, resolve_to_rgb};
        use ratatui::style::Color;
        // Truecolor pass-through.
        assert_eq!(resolve_to_rgb(Color::Rgb(12, 34, 56)), Some((12, 34, 56)));
        // Indexed routes through indexed_to_rgb; index 16 is (0, 0, 0), the first cube cell
        assert_eq!(resolve_to_rgb(Color::Indexed(16)), Some(indexed_to_rgb(16)));
        // Each named ANSI variant resolves to indexed_to_rgb(0..=15).
        let named = [
            (Color::Black, 0u8),
            (Color::Red, 1),
            (Color::Green, 2),
            (Color::Yellow, 3),
            (Color::Blue, 4),
            (Color::Magenta, 5),
            (Color::Cyan, 6),
            (Color::Gray, 7),
            (Color::DarkGray, 8),
            (Color::LightRed, 9),
            (Color::LightGreen, 10),
            (Color::LightYellow, 11),
            (Color::LightBlue, 12),
            (Color::LightMagenta, 13),
            (Color::LightCyan, 14),
            (Color::White, 15),
        ];
        for (color, idx) in named {
            assert_eq!(
                resolve_to_rgb(color),
                Some(indexed_to_rgb(idx)),
                "named variant {color:?} should map to indexed_to_rgb({idx})"
            );
        }
        // Reset is the only no-op.
        assert_eq!(resolve_to_rgb(Color::Reset), None);
    }

    #[test]
    fn from_name_concrete_variants_still_work() {
        assert_eq!(
            ThemeKind::from_name("groknight"),
            Some(ThemeKind::GrokNight)
        );
        assert_eq!(ThemeKind::from_name("dark"), Some(ThemeKind::GrokNight));
        assert_eq!(ThemeKind::from_name("grokday"), Some(ThemeKind::GrokDay));
        assert_eq!(ThemeKind::from_name("light"), Some(ThemeKind::GrokDay));
        assert_eq!(
            ThemeKind::from_name("tokyonight"),
            Some(ThemeKind::TokyoNight)
        );
        assert_eq!(
            ThemeKind::from_name("rosepine"),
            Some(ThemeKind::RosePineMoon)
        );
        assert_eq!(
            ThemeKind::from_name("oscura"),
            Some(ThemeKind::OscuraMidnight)
        );
        assert_eq!(
            ThemeKind::from_name("oscura-midnight"),
            Some(ThemeKind::OscuraMidnight)
        );
    }

    /// `FromStr` agrees with `from_name` for all canonicals and aliases.
    #[test]
    fn from_str_matches_from_name_for_all_canonicals() {
        // Mapping `from_name`'s alias matrix into the `FromStr` API.
        let cases = [
            ("auto", ThemeKind::Auto),
            ("system", ThemeKind::Auto),
            ("groknight", ThemeKind::GrokNight),
            ("grok-night", ThemeKind::GrokNight),
            ("dark", ThemeKind::GrokNight),
            ("tokyonight", ThemeKind::TokyoNight),
            ("tokyo-night", ThemeKind::TokyoNight),
            ("tokyo", ThemeKind::TokyoNight),
            ("grokday", ThemeKind::GrokDay),
            ("grok-day", ThemeKind::GrokDay),
            ("light", ThemeKind::GrokDay),
            ("day", ThemeKind::GrokDay),
            ("rosepine", ThemeKind::RosePineMoon),
            ("rose-pine", ThemeKind::RosePineMoon),
            ("rosepine-moon", ThemeKind::RosePineMoon),
            ("rose-pine-moon", ThemeKind::RosePineMoon),
        ];
        for (name, expected) in cases {
            assert_eq!(
                name.parse::<ThemeKind>(),
                Ok(expected),
                "name `{name}` must parse to {expected:?}",
            );
            // Case-insensitive symmetry.
            assert_eq!(
                name.to_uppercase().parse::<ThemeKind>(),
                Ok(expected),
                "name `{name}` (upper) must parse to {expected:?}",
            );
        }
        assert_eq!("nonexistent".parse::<ThemeKind>(), Err(()));
        assert_eq!("".parse::<ThemeKind>(), Err(()));
    }

    /// OSC 12/112 gating on the terminal theme (Reset accent): apply never
    /// marks the cursor as painted, and the swap-gated reset fires only when
    /// this session actually painted it — clearing the flag exactly once.
    #[test]
    fn cursor_color_gating_stays_silent_on_reset_accent() {
        let _guard = cache::pin_theme();
        cache::set(ThemeKind::Terminal);

        // Pure terminal-theme session: apply must not latch the flag.
        set_cursor_color_applied_for_test(false);
        apply_cursor_color();
        assert!(
            !cursor_color_applied_for_test(),
            "Reset accent must never mark the cursor as painted"
        );

        // Reset-accent undoes a prior OSC 12 exactly once. This leg emits one OSC 112 to fd 2; the harness does not capture it.
        set_cursor_color_applied_for_test(true);
        apply_cursor_color();
        assert!(
            !cursor_color_applied_for_test(),
            "apply on a Reset accent must clear a previously-painted flag"
        );
    }
}
