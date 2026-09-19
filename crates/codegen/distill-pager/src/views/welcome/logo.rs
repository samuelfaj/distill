// Modified for Distill by Samuel Fajreldines, 2026.
//! The mascot is the Distill cat.
//!
//! The art is the owner's: `assets/logo/cat-source.sh` paints it as a truecolor
//! half-block picture, and [`cat`] draws exactly that (see `tools/cat-art.py`).
//! It comes in two sizes of the same drawing — full, and half sampled
//! nearest-neighbour — and the layout steps the column down when it needs the
//! rows back. On a console that cannot show the blocks (legacy Windows), the
//! ASCII cat stands in, so a mascot is always on screen; below the shortest of
//! them the tier goes `Hidden`.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::render::color::blend_color;
use crate::theme::Theme;

/// The ASCII cat: the stand-in for consoles that cannot show the blocks.
const LOGO: &str = include_str!("../../../assets/logo/logo07.txt");
const LOGO_SMALL: &str = include_str!("../../../assets/logo/logo05.txt");

/// Height at or above which the ASCII cat is shown: 14 cell rows, and the
/// stand-in for windows (and consoles) that cannot hold the picture.
const SMALL_LOGO_MIN_HEIGHT: u16 = 22;
/// Height at or above which the owner's cat is shown.
///
/// The picture is 30 cell rows — the size the script paints, never resampled —
/// so the window needs those rows plus the menu, the draft and the version.
const CAT_MIN_HEIGHT: u16 = 38;

/// Which logo art the stacked column shows.
/// The terminal height picks the tier; the stacked layout steps it down only while the column would not fit beside the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoTier {
    Full,
    Compact,
    Hidden,
}

impl LogoTier {
    pub fn for_height(window_height: u16) -> Self {
        Self::for_height_and_hidden(window_height, logo_hidden())
    }

    /// Takes the legacy-console flag as a parameter so tests can drive it directly.
    fn for_height_and_hidden(window_height: u16, legacy: bool) -> Self {
        // Without the blocks there is only the ASCII cat, whatever the height.
        if window_height >= CAT_MIN_HEIGHT && !legacy {
            Self::Full
        } else if window_height >= SMALL_LOGO_MIN_HEIGHT {
            Self::Compact
        } else {
            Self::Hidden
        }
    }

    fn art(self) -> Option<Art> {
        match self {
            // The picture at the size the script paints it. Nothing resizes it:
            // a smaller cat would not be this cat.
            Self::Full => Some(Art::Cat),
            // The ASCII cat stands in where the picture does not fit: a window
            // too short for 30 rows, or a console without the blocks.
            Self::Compact => Some(Art::Ascii(LOGO)),
            Self::Hidden => None,
        }
    }

    pub fn rows(self) -> u16 {
        self.art().map_or(0, Art::rows)
    }

    /// Cell columns the art occupies.
    pub fn width(self) -> u16 {
        self.art().map_or(0, Art::width)
    }

    /// The next smaller tier; `None` once hidden.
    pub fn step_down(self) -> Option<Self> {
        match self {
            Self::Full => Some(Self::Compact),
            Self::Compact => Some(Self::Hidden),
            Self::Hidden => None,
        }
    }
}

/// The art a window of this height gets, if any.
fn pick_logo(window_height: u16) -> Option<Art> {
    LogoTier::for_height(window_height).art()
}

/// What one tier draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Art {
    /// The owner's picture, at the size its script paints it.
    Cat,
    /// The ASCII cat: the stand-in for windows and consoles without the blocks.
    Ascii(&'static str),
}

impl Art {
    fn rows(self) -> u16 {
        match self {
            Self::Cat => super::cat::Cat::art().rows(),
            Self::Ascii(art) => count_lines(art),
        }
    }

    fn width(self) -> u16 {
        match self {
            Self::Cat => super::cat::Cat::art().width(),
            Self::Ascii(art) => visual_width(art),
        }
    }
}

/// The braille art has no ASCII stand-in; see the module doc.
fn logo_hidden() -> bool {
    crate::glyphs::is_legacy_windows_console()
}

fn non_empty_lines(logo: &str) -> impl Iterator<Item = &str> {
    logo.lines().filter(|l| !l.is_empty())
}

fn count_lines(logo: &str) -> u16 {
    non_empty_lines(logo).count() as u16
}

fn visual_width(logo: &str) -> u16 {
    non_empty_lines(logo)
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(24) as u16
}

/// Animation phase in seconds since the first render.
/// The phase is wall-clock based so the shimmer speed is independent of the frame rate.
fn anim_phase_secs() -> f32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

/// Shimmer redraw cadence in frames per second.
/// The sweep is slow, so a few fps looks smooth while sparing the long-lived welcome screen from full-rate repaints.
const SHIMMER_FPS: f32 = 12.0;

/// Quantized shimmer frame for the current wall-clock phase.
/// The welcome screen redraws only when this advances, throttling the animation to ~`SHIMMER_FPS` rather than the full event-loop tick rate.
/// The frame is pinned to 0 when the logo is hidden.
pub fn shimmer_frame() -> u64 {
    if logo_hidden() {
        return 0;
    }
    (anim_phase_secs() * SHIMMER_FPS) as u64
}

/// Per-glyph shine opacity in `[0, 1]` at normalized diagonal position `diag` (0 is bottom-left, 1 is top-right) and animation time `secs`.
/// A raised-cosine band sweeps from bottom-left to top-right and parks off-screen between sweeps; a gentle global pulse breathes underneath it.
/// 0 keeps the resting gray, 1 is full bright.
fn shine_opacity(diag: f32, secs: f32) -> f32 {
    const BAND: f32 = 0.38; // half-width of the shine band; wider means a more gradual falloff
    const CYCLE: f32 = 4.0; // seconds for one sweep plus its rest
    const SWEEP_FRAC: f32 = 0.32; // portion of the cycle spent sweeping (~1.3s glint, rest idles)
    const SHINE: f32 = 0.33; // peak shine strength
    const PULSE: f32 = 0.06; // global breathing amount
    const PULSE_SECS: f32 = 5.0; // breathing period

    let p = (secs % CYCLE) / CYCLE;
    let q = (p / SWEEP_FRAC).min(1.0); // parks the band off-screen during the rest
    let band_pos = -BAND + q * (1.0 + 2.0 * BAND);
    let pulse = PULSE * (0.5 - 0.5 * (std::f32::consts::TAU * secs / PULSE_SECS).cos());

    let d = (diag - band_pos).abs();
    let shine = if d < BAND {
        0.5 * (1.0 + (std::f32::consts::PI * d / BAND).cos())
    } else {
        0.0
    };
    (pulse + SHINE * shine).clamp(0.0, 1.0)
}

/// Shimmering spans for the brand line: gold at rest, glinting toward white and
/// bold throughout.
///
/// The home has one title, so it is the one thing on the screen in the accent
/// gold — the meta rows around it stay gray, which is what makes it read as the
/// name of the harness rather than as another line of small print.
pub(crate) fn title_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    shimmer_spans(text, theme.accent_plan, theme.text_primary)
}

/// Shimmering spans for a wordmark that is not the title.
pub(crate) fn wordmark_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    shimmer_spans(text, theme.text_primary, theme.accent_success)
}

/// The sweep itself: same phase and band as the art, so the banner animates as
/// one piece. Falls back to the resting colour wherever the theme cannot express
/// a blend (legacy terminals), which keeps the text readable instead of dimming
/// it.
fn shimmer_spans(text: &str, base: Color, hilite: Color) -> Vec<Span<'static>> {
    let secs = anim_phase_secs();
    let cols = text.chars().count().max(1) as f32;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_color: Option<Color> = None;
    for (col, ch) in text.chars().enumerate() {
        let diag = col as f32 / cols;
        let color = blend_color(base, hilite, shine_opacity(diag, secs)).unwrap_or(base);
        if run_color != Some(color) {
            if let Some(prev) = run_color {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    Style::default().fg(prev).add_modifier(Modifier::BOLD),
                ));
            }
            run_color = Some(color);
        }
        run.push(ch);
    }
    if let Some(prev) = run_color {
        spans.push(Span::styled(
            run,
            Style::default().fg(prev).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// Draws one art into `area`.
fn render_art(area: Rect, buf: &mut Buffer, theme: &Theme, art: Art) {
    match art {
        // The picture is drawn in its own colours, so it is written cell by cell
        // and not shimmered.
        Art::Cat => super::cat::Cat::art().render(area, buf, theme.bg_base),
        Art::Ascii(logo) => render_into(area, buf, theme, logo),
    }
}

fn render_into(area: Rect, buf: &mut Buffer, theme: &Theme, logo: &str) {
    let lines: Vec<&str> = non_empty_lines(logo).collect();
    let rows = lines.len().max(1) as f32;
    let cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let secs = anim_phase_secs();

    // Blend each glyph from the resting gray toward the bright text color by its shine opacity, so a sheen sweeps across the braille art
    // Adjacent glyphs that land on the same blended color share one Span to hold down the per-frame allocation
    let base = theme.gray;
    let hilite = theme.text_primary;
    let logo_lines: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;
            for (col, ch) in line.chars().enumerate() {
                // Sweep along the diagonal from bottom-left to top-right: the coordinate grows as col increases and row decreases
                let diag = (col as f32 + (rows - 1.0 - row as f32)) / (cols + rows);
                let color = blend_color(base, hilite, shine_opacity(diag, secs)).unwrap_or(base);
                if run_color != Some(color) {
                    if let Some(prev) = run_color {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            Style::default().fg(prev),
                        ));
                    }
                    run_color = Some(color);
                }
                run.push(ch);
            }
            if let Some(prev) = run_color {
                spans.push(Span::styled(run, Style::default().fg(prev)));
            }
            Line::from(spans).alignment(Alignment::Center)
        })
        .collect();
    Paragraph::new(logo_lines).render(area, buf);
}

pub fn logo_line_count(window_height: u16) -> u16 {
    LogoTier::for_height(window_height).rows()
}

pub fn logo_visual_width(window_height: u16) -> u16 {
    LogoTier::for_height(window_height).width()
}

pub fn render_logo(area: Rect, buf: &mut Buffer, theme: &Theme, window_height: u16) {
    if let Some(art) = pick_logo(window_height) {
        render_art(area, buf, theme, art);
    }
}

/// Paint the tier the layout reserved rows for, so the art can never outgrow its slot.
pub fn render_logo_tier(area: Rect, buf: &mut Buffer, theme: &Theme, tier: LogoTier) {
    if let Some(art) = tier.art() {
        render_art(area, buf, theme, art);
    }
}

/// The hero box shows the cat beside the menu.
///
/// It uses the half-size tier: the box shares its height with the menu and the
/// info slot, so the full 30-row cat would push the box past the fit gate on
/// every ordinary window. These report and render that art directly, independent
/// of the height-based [`pick_logo`] tiers the stacked layout uses.
/// On a console without the blocks, and when [`logo_hidden`], the ASCII cat
/// stands in — it is the same mascot in fewer rows than the picture needs.
pub fn full_logo_line_count() -> u16 {
    Art::Cat.rows()
}

pub fn full_logo_visual_width() -> u16 {
    if logo_hidden() { 0 } else { Art::Cat.width() }
}

pub fn render_full_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    // The box is beside the menu, so the picture's own fit gate decides whether
    // it is drawn at all; a console without the blocks gets the ASCII cat.
    let art = if logo_hidden() {
        Art::Ascii(LOGO)
    } else {
        Art::Cat
    };
    render_art(area, buf, theme, art);
}

/// Line count of the small logo used in minimal's committed welcome card (0 on a legacy Windows console, where the braille art is suppressed).
pub fn compact_logo_line_count() -> u16 {
    if logo_hidden() {
        0
    } else {
        count_lines(LOGO_SMALL)
    }
}

/// Render the small braille logo (centered) into `area` for minimal's welcome card.
/// No-op when the logo is hidden.
pub fn render_compact_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if !logo_hidden() {
        render_into(area, buf, theme, LOGO_SMALL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tier ladder: the picture at full size, the same picture at half, and
    /// nothing when even that does not fit.
    /// The ladder: the owner's cat when the window holds its 30 rows, the ASCII
    /// stand-in when it does not, nothing when even that does not fit.
    #[test]
    fn logo_sizes_by_height() {
        assert!(
            LogoTier::for_height_and_hidden(SMALL_LOGO_MIN_HEIGHT - 1, false)
                .art()
                .is_none()
        );
        assert_eq!(
            LogoTier::for_height_and_hidden(SMALL_LOGO_MIN_HEIGHT, false).art(),
            Some(Art::Ascii(LOGO))
        );
        assert_eq!(
            LogoTier::for_height_and_hidden(CAT_MIN_HEIGHT - 1, false).art(),
            Some(Art::Ascii(LOGO)),
            "below the picture's height the ASCII cat holds the slot"
        );
        assert_eq!(
            LogoTier::for_height_and_hidden(CAT_MIN_HEIGHT, false).art(),
            Some(Art::Cat)
        );
    }

    /// The cat is the script's size: 52 cells by 30 rows, nothing resampled.
    #[test]
    fn the_cat_keeps_the_scripts_size() {
        assert_eq!(LogoTier::Full.rows(), 30);
        assert_eq!(LogoTier::Full.width(), 52);
        assert_eq!(full_logo_line_count(), 30);
        assert_eq!(full_logo_visual_width(), 52);
    }

    /// A console without the blocks gets the ASCII cat at every height, and the
    /// hero box measures the ASCII cat there instead of the blocks it cannot
    /// draw.
    #[test]
    fn legacy_consoles_get_the_ascii_cat() {
        assert!(
            LogoTier::for_height_and_hidden(SMALL_LOGO_MIN_HEIGHT - 1, true)
                .art()
                .is_none()
        );
        for h in [SMALL_LOGO_MIN_HEIGHT, CAT_MIN_HEIGHT, u16::MAX] {
            assert_eq!(
                LogoTier::for_height_and_hidden(h, true).art(),
                Some(Art::Ascii(LOGO)),
                "height {h}"
            );
        }
    }

    #[test]
    fn shine_opacity_stays_in_unit_range() {
        let mut secs = 0.0;
        while secs < 10.0 {
            for i in 0..=20 {
                let diag = i as f32 / 20.0;
                let op = shine_opacity(diag, secs);
                assert!(
                    (0.0..=1.0).contains(&op),
                    "opacity {op} out of range at diag {diag}, secs {secs}"
                );
            }
            secs += 0.13;
        }
    }

    #[test]
    fn shine_band_sweeps_across() {
        // The brightest point along the diagonal advances from left to right as the sweep progresses through its active phase
        let brightest = |secs: f32| -> f32 {
            (0..=100)
                .map(|i| i as f32 / 100.0)
                .max_by(|a, b| {
                    shine_opacity(*a, secs)
                        .partial_cmp(&shine_opacity(*b, secs))
                        .unwrap()
                })
                .unwrap()
        };
        let early = brightest(0.1);
        let mid = brightest(0.4);
        let late = brightest(0.7);
        assert!(early < mid, "early {early} should precede mid {mid}");
        assert!(mid < late, "mid {mid} should precede late {late}");
    }

    #[test]
    fn shine_rests_dim_between_sweeps() {
        // During the rest phase the band is parked off-screen, so an interior glyph falls back to at most the gentle pulse, never full bright
        let op = shine_opacity(0.5, 6.0); // secs % 4.0 = 2.0, past SWEEP_FRAC, in the rest phase
        assert!(op < 0.2, "resting opacity {op} should stay dim");
    }

    #[test]
    fn the_wordmark_spans_reproduce_the_text_exactly_once() {
        let theme = Theme::current();
        let text = "Distill  ";
        let spans = wordmark_spans(text, &theme);
        assert!(!spans.is_empty(), "the wordmark is never empty");
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, text, "animation must not alter the text");
        assert!(
            spans
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "the wordmark stays bold while it shimmers"
        );
        // Colors follow the theme's text color wherever a blend is expressible.
        assert!(
            spans.iter().any(|s| s.style.fg.is_some()),
            "each span carries a foreground color"
        );
    }

    #[test]
    fn the_wordmark_animation_advances_with_the_wall_clock() {
        // The shimmer phase is wall-clock based, so two renders far enough apart
        // must land on different shimmer frames (this is what drives the
        // welcome screen's throttled repaint).
        let first = shimmer_frame();
        std::thread::sleep(std::time::Duration::from_millis(150));
        let second = shimmer_frame();
        assert!(
            second > first,
            "the shimmer must advance: {first} -> {second}"
        );
    }
}
