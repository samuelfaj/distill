//! This is the core rendering abstraction for virtualized scrolling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::WidgetRef;
use std::sync::Arc;

/// The trait is object-safe to allow heterogeneous collections.
pub trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);

    /// Height needed at this width in lines.
    ///
    /// This should be efficient (ideally O(1)) as it may be called frequently during scroll position calculations.
    fn desired_height(&self, width: u16) -> u16;
}

// ============================================================================
// Standard Implementations
// ============================================================================

impl Renderable for () {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}
    fn desired_height(&self, _width: u16) -> u16 {
        0
    }
}

impl Renderable for &str {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl Renderable for String {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_str().render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl<'a> Renderable for Span<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

/// Lines render as a single line (no wrapping).
impl<'a> Renderable for Line<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        WidgetRef::render_ref(self, area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

// Paragraph::line_count is unstable in ratatui, so we don't implement Renderable for Paragraph directly
// Users should wrap text in custom types that handle their own height calculation

impl<R: Renderable> Renderable for Option<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(renderable) = self {
            renderable.render(area, buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        if let Some(renderable) = self {
            renderable.desired_height(width)
        } else {
            0
        }
    }
}

impl<R: Renderable> Renderable for Arc<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_ref().render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.as_ref().desired_height(width)
    }
}

impl<R: Renderable + ?Sized> Renderable for Box<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_ref().render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.as_ref().desired_height(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_has_zero_height() {
        assert_eq!(().desired_height(80), 0);
    }

    #[test]
    fn str_has_height_one() {
        assert_eq!("hello".desired_height(80), 1);
    }

    #[test]
    fn string_has_height_one() {
        assert_eq!(String::from("hello").desired_height(80), 1);
    }

    #[test]
    fn line_has_height_one() {
        let line = Line::from("hello");
        assert_eq!(line.desired_height(80), 1);
    }

    #[test]
    fn span_has_height_one() {
        let span = Span::raw("hello");
        assert_eq!(span.desired_height(80), 1);
    }

    #[test]
    fn option_none_has_zero_height() {
        let opt: Option<&str> = None;
        assert_eq!(opt.desired_height(80), 0);
    }

    #[test]
    fn option_some_delegates_height() {
        let opt: Option<&str> = Some("hello");
        assert_eq!(opt.desired_height(80), 1);
    }
}
