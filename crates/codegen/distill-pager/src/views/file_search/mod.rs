//! @-provider: fuzzy file completion for `@foo/bar` references.
//!
//! # Architecture
//!
//! - [`context`]: parses `@query` tokens from the text and cursor position
//! - [`state`]: owns the fuzzy matcher daemon, results, and dropdown state
//! - [`dropdown`]: renders the dropdown list (a ListPane wrapper)
//! - [`line_viewer`]: centered popup file viewer
//! - [`preview`]: file preview alongside the dropdown (not yet implemented)

pub mod context;
pub mod dropdown;
pub mod line_viewer;
mod state;

pub use context::AtContext;
pub use state::{FileSearchReplacement, FileSearchState};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Build a styled `@path` or `@path:N-M` display line.
///
/// Set `at_prefix` to include the leading `@` (the prompt element chip does) or omit it (the line viewer title bar does).
pub fn styled_file_ref<'a>(
    path: &str,
    line_range: Option<&str>,
    theme: &Theme,
    at_prefix: bool,
) -> Line<'a> {
    let dim = theme.muted();
    let path_style = Style::default().fg(theme.path);
    let num_style = Style::default().fg(theme.gray_bright);

    let mut spans = Vec::new();
    if at_prefix {
        spans.push(Span::styled("@", dim));
    }
    spans.push(Span::styled(path.to_owned(), path_style));
    if let Some(range) = line_range {
        spans.push(Span::styled(":", dim));
        spans.push(Span::styled(range.to_owned(), num_style));
    }
    Line::from(spans)
}
