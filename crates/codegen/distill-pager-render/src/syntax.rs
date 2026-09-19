// Modified for Distill by Samuel Fajreldines, 2026.
//! Provides lazily-initialized `Syntect` instances for code highlighting.
//! Dark themes (GrokNight, TokyoNight) share `grok-night.tmTheme`; GrokDay uses `grok-day.tmTheme` with deepened colors for light backgrounds.
//!
//! ## Terminal-native palette (minimal lock + `terminal` theme)
//!
//! While [`crate::theme::cache::terminal_native_active`] holds (minimal mode's lock, or the `Terminal` kind), syntect still loads a night `.tmTheme`.
//! Its pastel RGB tokens collapse to **White** after naive ANSI-16 quantization, which is invisible on light terminal profiles.
//!
//! On that palette we do **not** detect light/dark. Instead:
//! 1. Near-gray tokens become `Color::Reset` (terminal default fg; always readable).
//! 2. Chromatic tokens map to base ANSI-16 accents (Red/Green/Yellow/Blue/Magenta/Cyan), never White/Black/bright variants.

use std::sync::OnceLock;

pub use distill_markdown::Syntect;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::theme::ThemeKind;

static SYNTECT_GROKNIGHT: OnceLock<Syntect> = OnceLock::new();
static SYNTECT_TOKYONIGHT: OnceLock<Syntect> = OnceLock::new();
static SYNTECT_GROKDAY: OnceLock<Syntect> = OnceLock::new();

/// Convert syntect style to ratatui foreground-only style, quantized for terminal color support (or polarity-safe under the terminal-native lock).
pub fn syntect_to_ratatui_fg(style: syntect::highlighting::Style) -> Style {
    let fg = syntect_rgb_to_fg(style.foreground.r, style.foreground.g, style.foreground.b);
    let mut out = Style::default().fg(fg);
    use syntect::highlighting::FontStyle;
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Map a syntect RGB triplet to a ratatui foreground color.
///
/// On the terminal-native palette, uses [`polarity_safe_syntax_fg`]; otherwise quantizes via the normal theme color pipeline.
pub fn syntect_rgb_to_fg(r: u8, g: u8, b: u8) -> Color {
    if crate::theme::cache::terminal_native_active() {
        polarity_safe_syntax_fg(r, g, b)
    } else {
        crate::theme::quantize(Color::Rgb(r, g, b))
    }
}

/// Low-chroma tokens use [`Color::Reset`] so host fg keeps contrast on both polarities.
/// Saturated hues map to base ANSI only — never White, Black, or Light*, which vanish on the opposite polarity.
pub fn polarity_safe_syntax_fg(r: u8, g: u8, b: u8) -> Color {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    let chroma = max - min;
    // Night default body (~#c8c8c8) and dim comments are near-gray.
    if chroma < 40 {
        return Color::Reset;
    }
    // Integer HSV hue in degrees [0, 360).
    let (ri, gi, bi) = (r as i32, g as i32, b as i32);
    let h = if max == ri {
        let mut h = (gi - bi) * 60 / chroma;
        if h < 0 {
            h += 360;
        }
        h
    } else if max == gi {
        (bi - ri) * 60 / chroma + 120
    } else {
        (ri - gi) * 60 / chroma + 240
    };
    // Magenta starts at 255° so Tokyo Night purple (#bb9af7, ~261°) lands Magenta rather than Blue; pure blues (~221°) stay Blue
    match h {
        0..30 | 330..=360 => Color::Red,
        30..90 => Color::Yellow,
        90..150 => Color::Green,
        150..210 => Color::Cyan,
        210..255 => Color::Blue,
        _ => Color::Magenta,
    }
}

/// Under the terminal-native lock, syntect tokens are remapped via [`polarity_safe_syntax_fg`].
/// Highlight failure uses `fallback` (typically Reset) so contrast is not lost.
pub fn highlight_line(
    text: &str,
    highlighter: &mut Option<syntect::easy::HighlightLines<'_>>,
    syntect: &Syntect,
    fallback: Style,
) -> Vec<Span<'static>> {
    if let Some(hl) = highlighter.as_mut()
        && let Ok(ranges) = hl.highlight_line(&format!("{text}\n"), &syntect.syntax_set)
    {
        let mut spans = Vec::new();
        for (style, segment) in ranges {
            let mut s = segment.to_owned();
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            if s.is_empty() {
                continue;
            }
            spans.push(Span::styled(s, syntect_to_ratatui_fg(style)));
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(text.to_string(), fallback)]
}

/// Terminal-native lock reports nominal `GrokNight`, so this returns the night theme.
/// Colors are remapped later; do not load a day theme from OS/terminal polarity.
pub fn get_syntect() -> &'static Syntect {
    match crate::theme::Theme::current_kind() {
        ThemeKind::GrokNight
        | ThemeKind::RosePineMoon
        | ThemeKind::OscuraMidnight
        // Terminal remaps every token in `syntect_rgb_to_fg`, so the
        // source palette only has to be a full one — polarity is irrelevant.
        | ThemeKind::Terminal
        | ThemeKind::Auto => SYNTECT_GROKNIGHT
            .get_or_init(|| Syntect::new(include_bytes!("../assets/grok-night.tmTheme"))),
        ThemeKind::TokyoNight => SYNTECT_TOKYONIGHT
            .get_or_init(|| Syntect::new(include_bytes!("../assets/tokyo-night.tmTheme"))),
        ThemeKind::GrokDay => SYNTECT_GROKDAY
            .get_or_init(|| Syntect::new(include_bytes!("../assets/grok-day.tmTheme"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::cache as theme_cache;

    /// Hold the theme test lock so we can flip the terminal-native flag.
    fn with_native_lock<R>(locked: bool, f: impl FnOnce() -> R) -> R {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        theme_cache::reset_for_test();
        theme_cache::set_terminal_native_lock(locked);
        let out = f();
        theme_cache::set_terminal_native_lock(false);
        theme_cache::reset_for_test();
        out
    }

    #[test]
    fn polarity_safe_grays_are_reset() {
        // Night default body / comments.
        assert_eq!(polarity_safe_syntax_fg(0xc8, 0xc8, 0xc8), Color::Reset);
        assert_eq!(polarity_safe_syntax_fg(0x6c, 0x6c, 0x6c), Color::Reset);
        assert_eq!(polarity_safe_syntax_fg(0xb2, 0xb2, 0xb2), Color::Reset);
        assert_eq!(polarity_safe_syntax_fg(0x44, 0x44, 0x44), Color::Reset);
    }

    #[test]
    fn polarity_safe_never_emits_white_or_black() {
        // Common night-theme pastels that naive ANSI16 maps to White.
        let samples = [
            (0xbb, 0x9a, 0xf7), // magenta
            (0x7d, 0xcf, 0xff), // cyan
            (0x7a, 0xa2, 0xf7), // blue
            (0xff, 0x9e, 0x64), // orange
            (0xf7, 0x76, 0x8e), // red
            (0xe0, 0xaf, 0x68), // yellow
            (0x9e, 0xce, 0x6a), // green
            (0xc8, 0xc8, 0xc8), // gray body
        ];
        for (r, g, b) in samples {
            let c = polarity_safe_syntax_fg(r, g, b);
            assert!(
                !matches!(
                    c,
                    Color::White
                        | Color::Black
                        | Color::Gray
                        | Color::DarkGray
                        | Color::LightRed
                        | Color::LightGreen
                        | Color::LightYellow
                        | Color::LightBlue
                        | Color::LightMagenta
                        | Color::LightCyan
                ),
                "polarity-unsafe color for #{r:02x}{g:02x}{b:02x}: {c:?}"
            );
        }
    }

    #[test]
    fn polarity_safe_chromatic_buckets() {
        assert_eq!(polarity_safe_syntax_fg(0xf7, 0x76, 0x8e), Color::Red);
        assert_eq!(polarity_safe_syntax_fg(0xe0, 0xaf, 0x68), Color::Yellow);
        assert_eq!(polarity_safe_syntax_fg(0x9e, 0xce, 0x6a), Color::Yellow); // Lime lands in the yellow bucket
        assert_eq!(polarity_safe_syntax_fg(0x7d, 0xcf, 0xff), Color::Cyan);
        assert_eq!(polarity_safe_syntax_fg(0x7a, 0xa2, 0xf7), Color::Blue);
        assert_eq!(polarity_safe_syntax_fg(0xbb, 0x9a, 0xf7), Color::Magenta);
    }

    #[test]
    fn syntect_rgb_to_fg_uses_polarity_safe_when_locked() {
        with_native_lock(true, || {
            // Pastel that naive quantize would turn White.
            assert_eq!(syntect_rgb_to_fg(0xc8, 0xc8, 0xc8), Color::Reset);
            assert_eq!(syntect_rgb_to_fg(0xbb, 0x9a, 0xf7), Color::Magenta);
        });
    }

    #[test]
    fn highlight_line_fallback_when_no_highlighter() {
        let syn = get_syntect();
        let mut hl = None;
        let fallback = Style::default().fg(Color::Reset);
        let spans = highlight_line("fn main() {}", &mut hl, syn, fallback);
        assert_eq!(spans.len(), 1);
        let [span] = spans.as_slice() else {
            panic!("expected one span: {spans:?}");
        };
        assert_eq!(span.content.as_ref(), "fn main() {}");
        assert_eq!(span.style.fg, Some(Color::Reset));
    }

    #[test]
    fn highlight_line_under_native_lock_avoids_white_tokens() {
        with_native_lock(true, || {
            let syn = get_syntect();
            let mut hl = syn.highlight_lines_for_token("rust");
            let fallback = Style::default().fg(Color::Reset);
            let spans = highlight_line("fn main() { let x = 1; /* c */ }", &mut hl, syn, fallback);
            assert!(!spans.is_empty());
            for span in &spans {
                let fg = span.style.fg;
                assert!(
                    !matches!(fg, Some(Color::White)),
                    "token {:?} painted White under native lock",
                    span.content
                );
            }
        });
    }

    fn markdown_atx_tokens(theme_bytes: &[u8], line: &str) -> Vec<(String, String, (u8, u8, u8))> {
        let syn = Syntect::new(theme_bytes);
        let syntax = syn
            .syntax_set
            .find_syntax_by_extension("md")
            .expect("two-face markdown");
        let mut parse_state = syntect::parsing::ParseState::new(syntax);
        let ops = parse_state
            .parse_line(line, &syn.syntax_set)
            .expect("parse markdown");
        let highlighter = syntect::highlighting::Highlighter::new(&syn.theme);
        let mut stack = syntect::parsing::ScopeStack::new();
        let mut tokens = Vec::new();
        let mut last = 0usize;
        for (i, op) in ops {
            if i > last {
                let style = highlighter.style_for_stack(stack.as_slice());
                tokens.push((
                    line.get(last..i).unwrap_or("").to_owned(),
                    stack.to_string(),
                    (style.foreground.r, style.foreground.g, style.foreground.b),
                ));
            }
            stack.apply(&op).expect("scope op");
            last = i;
        }
        if last < line.len() {
            let style = highlighter.style_for_stack(stack.as_slice());
            tokens.push((
                line.get(last..).unwrap_or("").to_owned(),
                stack.to_string(),
                (style.foreground.r, style.foreground.g, style.foreground.b),
            ));
        }
        tokens
    }

    fn atx_token<'a>(
        tokens: &'a [(String, String, (u8, u8, u8))],
        name: &str,
    ) -> &'a (String, String, (u8, u8, u8)) {
        tokens
            .iter()
            .find(|(text, _, _)| text == name)
            .unwrap_or_else(|| panic!("missing {name:?} token in {tokens:?}"))
    }

    #[test]
    fn markdown_atx_h2_emits_markup_heading_scope_and_theme_color() {
        const LINE: &str = "## Heading\n";
        let night_h2 = (0x61, 0xbd, 0xf2);
        let night = markdown_atx_tokens(include_bytes!("../assets/grok-night.tmTheme"), LINE);
        let (_, scopes, rgb) = atx_token(&night, "Heading");
        assert!(
            scopes
                .split_whitespace()
                .any(|s| s == "markup.heading.2.markdown"),
            "two-face must emit markup.heading.2.markdown, got {scopes}"
        );
        assert_eq!(*rgb, night_h2);
        assert_eq!(atx_token(&night, "##").2, night_h2);

        for (bytes, want) in [
            (
                include_bytes!("../assets/tokyo-night.tmTheme").as_slice(),
                night_h2,
            ),
            (
                include_bytes!("../assets/grok-day.tmTheme").as_slice(),
                (0x2f, 0x64, 0xd2),
            ),
        ] {
            assert_eq!(
                atx_token(&markdown_atx_tokens(bytes, LINE), "Heading").2,
                want
            );
        }
    }
}
