//! The mascot, as the owner's script paints it.
//!
//! `assets/logo/cat-source.sh` draws the cat with truecolor half blocks: one `▀`
//! per terminal cell, the foreground painting the top pixel and the background
//! the bottom one. `assets/logo/cat-px.txt` carries that stream as data (see
//! `tools/cat-art.py`), and this module turns it back into cells — the TUI
//! cannot run a shell script, but it can paint the same picture.
//!
//! The art is a picture, not line art: it is drawn at its own colours and is not
//! shimmered, because a sheen over a photograph of a cat reads as a rendering
//! bug rather than as an animation.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

/// The half-block glyph: upper half foreground, lower half background.
pub const GLYPH: &str = "\u{2580}";

const ART: &str = include_str!("../../../assets/logo/cat-px.txt");

// Mascot silhouette, one exclusive x-range per pixel row. Keep the dark fur
// intact while excluding the original backdrop and the stray marks at its right.
#[rustfmt::skip]
const SILHOUETTE: [(u16, u16); 60] = [
    (0,0), (0,0), (32,35), (30,36), (29,36), (28,37), (28,37), (27,37), (27,38), (26,38),
    (25,39), (25,39), (6,40), (3,40), (3,41), (2,41), (2,42), (2,41), (2,43), (3,43),
    (3,45), (3,45), (3,45), (4,50), (4,50), (5,47), (5,46), (5,50), (6,50), (8,48),
    (7,47), (7,50), (6,50), (7,43), (8,42), (5,41), (4,40), (3,40), (8,41), (7,41),
    (5,42), (5,42), (9,42), (16,43), (16,43), (15,42), (15,44), (14,44), (13,44), (12,44),
    (11,45), (11,45), (11,45), (11,45), (12,44), (12,44), (8,49), (4,50), (1,52), (0,0),
];

fn is_mascot_pixel(x: u16, y: u16) -> bool {
    let Some(&(start, end)) = SILHOUETTE.get(usize::from(y)) else {
        return false;
    };
    let between_ears = match y {
        12 => (10..22).contains(&x),
        13 => (13..20).contains(&x),
        14 => x == 16,
        _ => false,
    };
    (start..end).contains(&x) && !between_ears
}

/// The pixel art, parsed once.
pub struct Cat {
    /// Pixel rows, top to bottom; each row is the pixels left to right.
    pixels: Vec<Vec<Color>>,
}

/// A cell is two pixels tall, so the art is twice as many pixel rows as cell rows.
const PIXELS_PER_CELL: usize = 2;

impl Cat {
    /// The art, parsed on first use. A malformed asset yields an empty cat
    /// rather than a panic: the welcome screen must render on any build.
    pub fn art() -> &'static Self {
        static CAT: OnceLock<Cat> = OnceLock::new();
        CAT.get_or_init(|| Cat { pixels: parse(ART) })
    }

    /// Cell rows: two pixels per cell, so the art is drawn at the size the
    /// script paints it and never resampled.
    pub fn rows(&self) -> u16 {
        (self.pixels.len() / PIXELS_PER_CELL) as u16
    }

    /// Cell columns.
    pub fn width(&self) -> u16 {
        self.pixel_width() as u16
    }

    fn pixel_width(&self) -> usize {
        self.pixels.first().map_or(0, Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    /// The two pixels of one cell: the top one paints the glyph, the bottom one
    /// the rest of the cell.
    fn cell(&self, x: u16, y: u16) -> Option<(Color, Color)> {
        let x = usize::from(x);
        let top = usize::from(y) * PIXELS_PER_CELL;
        let row = |index: usize| self.pixels.get(index)?.get(x).copied();
        Some((row(top)?, row(top + 1)?))
    }

    /// Paints the art into `area`, clipped to it. Cells outside the art keep
    /// whatever was there.
    pub fn render(&self, area: Rect, buf: &mut Buffer, background: Color) {
        let cols = self.width().min(area.width);
        let rows = self.rows().min(area.height);
        for y in 0..rows {
            for x in 0..cols {
                let Some((top, bottom)) = self.cell(x, y) else {
                    continue;
                };
                let top = if is_mascot_pixel(x, y * 2) {
                    top
                } else {
                    background
                };
                let bottom = if is_mascot_pixel(x, y * 2 + 1) {
                    bottom
                } else {
                    background
                };
                let target = &mut buf[(area.x + x, area.y + y)];
                target.set_symbol(if top == background && bottom == background {
                    " "
                } else {
                    GLYPH
                });
                target.set_style(Style::default().fg(top).bg(bottom));
            }
        }
    }
}

/// Parses `cat-px.txt`: `#` lines are comments, every other line is a row of
/// 12-hex-digit cells (top pixel then bottom pixel).
fn parse(art: &str) -> Vec<Vec<Color>> {
    let mut pixels: Vec<Vec<Color>> = Vec::new();
    for line in art.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut top: Vec<Color> = Vec::new();
        let mut bottom: Vec<Color> = Vec::new();
        for cell in line.as_bytes().chunks_exact(12) {
            let (Some(top_rgb), Some(bottom_rgb)) =
                (hex_color(&cell[0..6]), hex_color(&cell[6..12]))
            else {
                return Vec::new();
            };
            top.push(top_rgb);
            bottom.push(bottom_rgb);
        }
        pixels.push(top);
        pixels.push(bottom);
    }
    pixels
}

fn hex_color(hex: &[u8]) -> Option<Color> {
    let text = std::str::from_utf8(hex).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    Some(Color::Rgb(
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asset is the owner's script at its own size: 52 cells wide, 30 rows.
    /// A resampled cat is not this cat, so there is no smaller tier.
    #[test]
    fn the_asset_is_the_scripts_size() {
        let cat = Cat::art();
        assert_eq!(cat.width(), 52, "the script paints 52 cells per row");
        assert_eq!(cat.rows(), 30, "and 30 rows");
        assert!(!cat.is_empty());
    }

    /// A cell is the two pixels the script put in it: the top one under the
    /// half block, the bottom one behind it.
    #[test]
    fn a_cell_carries_its_two_pixels() {
        let cat = Cat::art();
        let (top, bottom) = cat.cell(0, 0).expect("first cell");
        assert_eq!(top, cat.pixels[0][0], "the glyph paints the top pixel");
        assert_eq!(bottom, cat.pixels[1][0], "the background is the bottom one");
        assert!(matches!(top, Color::Rgb(..)));
    }

    #[test]
    fn rendering_uses_theme_background_and_preserves_the_cat() {
        let cat = Cat::art();
        let area = Rect::new(0, 0, cat.width(), cat.rows());
        for background in [Color::Rgb(18, 18, 18), Color::Rgb(245, 245, 245)] {
            let mut buf = Buffer::empty(area);
            cat.render(area, &mut buf, background);
            // Exterior, the gap between the ears, and both yellow fragments.
            for (x, y) in [(0, 0), (51, 2), (50, 8), (15, 6), (0, 29)] {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell ({x},{y})");
                assert_eq!(cell.fg, background);
                assert_eq!(cell.bg, background);
            }
            // Eyes and dark fur retain their original colors in either theme.
            for (x, y) in [(17, 14), (35, 12), (25, 10), (25, 23)] {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), GLYPH);
                assert_eq!((cell.fg, cell.bg), cat.cell(x, y).unwrap());
            }
        }
    }
}
