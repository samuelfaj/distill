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
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let cols = self.width().min(area.width);
        let rows = self.rows().min(area.height);
        for y in 0..rows {
            for x in 0..cols {
                let Some((top, bottom)) = self.cell(x, y) else {
                    continue;
                };
                let target = &mut buf[(area.x + x, area.y + y)];
                target.set_symbol(GLYPH);
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

    /// Painting writes the block, its colours, and nothing else: an empty cell
    /// left behind would punch a hole in the picture.
    #[test]
    fn rendering_paints_every_cell_of_the_grid() {
        let cat = Cat::art();
        let area = Rect::new(0, 0, cat.width(), cat.rows());
        let mut buf = Buffer::empty(area);
        cat.render(area, &mut buf);
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), GLYPH, "cell ({x},{y})");
                let (top, bottom) = cat.cell(x, y).expect("pixel pair");
                assert_eq!(cell.fg, top, "foreground of ({x},{y})");
                assert_eq!(cell.bg, bottom, "background of ({x},{y})");
            }
        }
    }
}
