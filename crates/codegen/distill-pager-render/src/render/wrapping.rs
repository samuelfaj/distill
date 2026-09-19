// Modified for Distill by Samuel Fajreldines, 2026.
//! Provides word-aware wrapping that preserves styled spans and tracks soft vs hard line breaks (via joiners).

use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::ops::Range;
use textwrap::Options;
use unicode_width::UnicodeWidthChar;

use super::line_utils::{fit_line_to_width, push_owned_lines};

/// Compute byte ranges for wrapped lines without trailing whitespace.
pub(crate) fn wrap_ranges_trim<'a, O>(text: &str, width_or_options: O) -> Vec<Range<usize>>
where
    O: Into<Options<'a>>,
{
    let opts = width_or_options.into();
    let mut lines: Vec<Range<usize>> = Vec::new();
    for line in textwrap::wrap(text, opts).iter() {
        match line {
            Cow::Borrowed(slice) => {
                let start = unsafe { slice.as_ptr().offset_from(text.as_ptr()) as usize };
                let end = start + slice.len();
                lines.push(start..end);
            }
            Cow::Owned(_) => panic!("wrap_ranges_trim: unexpected owned string"),
        }
    }
    lines
}

/// A segment of a match highlight on a single wrapped row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSegment {
    /// Wrapped row index (0-based).
    pub row: usize,
    /// Start display column within that row (0-based, unicode-width aware).
    pub col_start: usize,
    /// End display column (exclusive).
    pub col_end: usize,
}

/// One segment per wrapped row the match overlaps. Columns are display width, not byte offsets.
pub fn byte_range_to_row_cols(
    text: &str,
    wrap_ranges: &[Range<usize>],
    match_range: Range<usize>,
) -> Vec<HighlightSegment> {
    let mut segments = Vec::new();
    for (row, wr) in wrap_ranges.iter().enumerate() {
        // Intersect match_range with this row's byte range.
        let start = match_range.start.max(wr.start);
        let end = match_range.end.min(wr.end);
        if start < end {
            // Convert byte offsets within this row to display columns.
            let Some(row_text) = text.get(wr.start..wr.end) else {
                continue;
            };
            let col_start = byte_offset_to_display_col(row_text, start - wr.start);
            let col_end = byte_offset_to_display_col(row_text, end - wr.start);
            segments.push(HighlightSegment {
                row,
                col_start,
                col_end,
            });
        }
    }
    segments
}

/// Convert a byte offset within `text` to the corresponding display column.
pub(crate) fn byte_offset_to_display_col(text: &str, byte_offset: usize) -> usize {
    let mut col = 0usize;
    let mut bytes = 0usize;
    for ch in text.chars() {
        if bytes >= byte_offset {
            break;
        }
        bytes += ch.len_utf8();
        col += ch.width().unwrap_or(0);
    }
    col
}

/// Breakpoints must match visual rendering. FirstFit is greedy, so single-pass equals the two-stage wrap; a non-greedy change must mirror it.
#[allow(clippy::single_range_in_vec_init)] // Intentional: the single range is the full text, no wrapping
pub fn wrap_byte_ranges_matching(text: &str, width: usize) -> Vec<Range<usize>> {
    if width == 0 || text.is_empty() {
        return vec![0..text.len()];
    }

    // Must match the Options used by word_wrap_line_with_joiners (via RtOptions).
    // Blockquote lines get a subsequent_indent equal to the prefix, which reduces the effective width for continuation lines
    // Mirror that here so search-highlight breakpoints stay in sync with the visual rendering
    let bq_len = blockquote_prefix_len(text);
    let subsequent_width = if bq_len > 0 && bq_len < text.len() {
        match text.get(..bq_len) {
            Some(prefix) => {
                let prefix_display_width = prefix
                    .chars()
                    .map(|c| c.width().unwrap_or(0))
                    .sum::<usize>();
                width.saturating_sub(prefix_display_width).max(1)
            }
            None => width,
        }
    } else {
        width
    };

    // First line wraps at full width; remainder at reduced width for blockquotes.
    let first_line_opts = textwrap::Options::new(width)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
        .word_splitter(textwrap::WordSplitter::HyphenSplitter)
        .break_words(true);
    let first_ranges = wrap_ranges_trim(text, first_line_opts);
    let Some(first) = first_ranges.first() else {
        return vec![0..text.len()];
    };

    let mut ranges = vec![first.clone()];
    let mut base = first.end;

    // Skip whitespace at the wrap boundary (mirrors word_wrap_line_with_joiners).
    let skip = match text.get(base..) {
        Some(rest) => rest.chars().take_while(|c| *c == ' ').count(),
        None => 0,
    };
    base = base.saturating_add(skip);

    if base < text.len() {
        let Some(remainder) = text.get(base..) else {
            return ranges;
        };
        let rem_opts = textwrap::Options::new(subsequent_width)
            .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
            .word_splitter(textwrap::WordSplitter::HyphenSplitter)
            .break_words(true);
        for r in wrap_ranges_trim(remainder, rem_opts) {
            ranges.push((r.start + base)..(r.end + base));
        }
    }

    ranges
}

/// Wrapping options with initial/subsequent indent support.
#[derive(Debug, Clone)]
pub struct RtOptions<'a> {
    /// The width in columns at which the text will be wrapped.
    pub width: usize,
    pub line_ending: textwrap::LineEnding,
    pub initial_indent: Line<'a>,
    pub subsequent_indent: Line<'a>,
    /// Allow long words to be broken if they cannot fit on a line.
    pub break_words: bool,
    pub wrap_algorithm: textwrap::WrapAlgorithm,
    /// The line breaking algorithm to use.
    pub word_separator: textwrap::WordSeparator,
    pub word_splitter: textwrap::WordSplitter,
}

impl From<usize> for RtOptions<'_> {
    fn from(width: usize) -> Self {
        RtOptions::new(width)
    }
}

#[allow(dead_code)]
impl<'a> RtOptions<'a> {
    pub fn new(width: usize) -> Self {
        RtOptions {
            width,
            line_ending: textwrap::LineEnding::LF,
            initial_indent: Line::default(),
            subsequent_indent: Line::default(),
            break_words: true,
            word_separator: textwrap::WordSeparator::new(),
            wrap_algorithm: textwrap::WrapAlgorithm::FirstFit,
            word_splitter: textwrap::WordSplitter::HyphenSplitter,
        }
    }

    pub fn line_ending(self, line_ending: textwrap::LineEnding) -> Self {
        RtOptions {
            line_ending,
            ..self
        }
    }

    pub fn width(self, width: usize) -> Self {
        RtOptions { width, ..self }
    }

    pub fn initial_indent(self, initial_indent: Line<'a>) -> Self {
        RtOptions {
            initial_indent,
            ..self
        }
    }

    pub fn subsequent_indent(self, subsequent_indent: Line<'a>) -> Self {
        RtOptions {
            subsequent_indent,
            ..self
        }
    }

    pub fn break_words(self, break_words: bool) -> Self {
        RtOptions {
            break_words,
            ..self
        }
    }

    pub fn word_separator(self, word_separator: textwrap::WordSeparator) -> RtOptions<'a> {
        RtOptions {
            word_separator,
            ..self
        }
    }

    pub fn wrap_algorithm(self, wrap_algorithm: textwrap::WrapAlgorithm) -> RtOptions<'a> {
        RtOptions {
            wrap_algorithm,
            ..self
        }
    }

    pub fn word_splitter(self, word_splitter: textwrap::WordSplitter) -> RtOptions<'a> {
        RtOptions {
            word_splitter,
            ..self
        }
    }
}

/// Wrap a single line, preserving styles.
#[must_use]
pub fn word_wrap_line<'a, O>(line: &'a Line<'a>, width_or_options: O) -> Vec<Line<'a>>
where
    O: Into<RtOptions<'a>>,
{
    let (lines, _joiners) = word_wrap_line_with_joiners(line, width_or_options);
    lines
}

fn flatten_line_and_bounds<'a>(
    line: &'a Line<'a>,
) -> (String, Vec<(Range<usize>, ratatui::style::Style)>) {
    let mut flat = String::new();
    let mut span_bounds = Vec::new();
    let mut acc = 0usize;
    for s in &line.spans {
        let text = s.content.as_ref();
        let start = acc;
        flat.push_str(text);
        acc += text.len();
        span_bounds.push((start..acc, s.style));
    }
    (flat, span_bounds)
}

fn build_wrapped_line_from_range<'a>(
    indent: Line<'a>,
    original: &'a Line<'a>,
    span_bounds: &[(Range<usize>, ratatui::style::Style)],
    range: &Range<usize>,
    cursor: &mut usize,
) -> Line<'a> {
    let mut out = indent.style(original.style);
    let sliced = slice_line_spans(original, span_bounds, range, cursor);
    let mut spans = out.spans;
    spans.append(
        &mut sliced
            .spans
            .into_iter()
            .map(|s| s.patch_style(original.style))
            .collect(),
    );
    out.spans = spans;
    out
}

/// Never wrap tables: wrapping destroys column alignment. A leading `│` is a blockquote unless an interior `│` separates cells.
fn is_table_line(line: &Line<'_>) -> bool {
    let mut chars = line.spans.iter().flat_map(|s| s.content.chars());
    match chars.next() {
        // Box-drawing border characters (horizontal lines, corners, junctions): these are unambiguously table borders, never blockquote prefixes
        Some(ch)
            if ('\u{2500}'..='\u{257F}').contains(&ch) && ch != '\u{2502}' && ch != '\u{2503}' =>
        {
            true
        }
        // │ (U+2502) at line start is a table content row only if │ appears after the leading prefix region
        // Blockquote prefixes are only │ and spaces
        Some('\u{2502}') => {
            let mut in_prefix = true;
            for ch in chars {
                if in_prefix && (ch == '\u{2502}' || ch == ' ') {
                    continue;
                }
                in_prefix = false;
                if ch == '\u{2502}' {
                    return true;
                }
            }
            false
        }
        // ASCII table borders (TableBorders::ASCII uses '+' and '|')
        Some('|') => true,
        _ => false,
    }
}

/// Leading `│ ` sequences, else 0. Must agree with the selection layer's style-aware twin, which depends on re-injected prefix spans.
fn blockquote_prefix_len(flat: &str) -> usize {
    const BAR_BYTES: usize = '\u{2502}'.len_utf8(); // 3
    let mut len = 0;
    let mut chars = flat.chars();
    while let Some('\u{2502}') = chars.next() {
        if chars.next() == Some(' ') {
            len += BAR_BYTES + 1;
        } else {
            break;
        }
    }
    len
}

/// First line is `None`. Continuation joiners are the exact substring skipped at the wrap boundary (often spaces, possibly empty).
pub fn word_wrap_line_with_joiners<'a, O>(
    line: &'a Line<'a>,
    width_or_options: O,
) -> (Vec<Line<'a>>, Vec<Option<String>>)
where
    O: Into<RtOptions<'a>>,
{
    let mut rt_opts: RtOptions<'a> = width_or_options.into();

    // Never wrap tables. Pad to content width: terminal clipping desyncs on wider-than-measured glyphs and strands a ghost cell.
    if is_table_line(line) {
        let fitted = fit_line_to_width(line.clone(), rt_opts.width);
        return (vec![fitted], vec![None]);
    }

    let (flat, span_bounds) = flatten_line_and_bounds(line);

    // Blockquote lines start with │ (U+2502) prefix(es)
    // Set subsequent_indent to the prefix so continuation lines repeat the quote marker
    // Skip if the caller already set a subsequent_indent (caller intent wins).
    let bq_len = blockquote_prefix_len(&flat);
    if bq_len > 0 && bq_len < flat.len() && rt_opts.subsequent_indent.width() == 0 {
        let prefix = slice_line_spans(line, &span_bounds, &(0..bq_len), &mut 0);
        rt_opts = rt_opts.subsequent_indent(prefix);
    }

    let opts = Options::new(rt_opts.width)
        .line_ending(rt_opts.line_ending)
        .break_words(rt_opts.break_words)
        .wrap_algorithm(rt_opts.wrap_algorithm)
        .word_separator(rt_opts.word_separator)
        .word_splitter(rt_opts.word_splitter);

    let mut out: Vec<Line<'a>> = Vec::new();
    let mut joiners: Vec<Option<String>> = Vec::new();

    // The first output line uses the initial indent and a reduced available width.
    let initial_width_available = opts
        .width
        .saturating_sub(rt_opts.initial_indent.width())
        .max(1);
    let initial_wrapped = wrap_ranges_trim(&flat, opts.clone().width(initial_width_available));
    let Some(first_line_range) = initial_wrapped.first() else {
        out.push(rt_opts.initial_indent.clone());
        joiners.push(None);
        return (out, joiners);
    };

    // Rows are emitted in increasing byte order, so each row resumes the span scan where the previous one stopped instead of rescanning from 0
    // This shared cursor (see `slice_line_spans`) is what keeps wrapping one huge line linear
    let mut span_cursor = 0usize;

    let first_line = build_wrapped_line_from_range(
        rt_opts.initial_indent.clone(),
        line,
        &span_bounds,
        first_line_range,
        &mut span_cursor,
    );
    out.push(first_line);
    joiners.push(None);

    // Wrap the remainder using subsequent indent width.
    let mut base = first_line_range.end;
    let skip_leading_spaces = match flat.get(base..) {
        Some(rest) => rest.chars().take_while(|c| *c == ' ').count(),
        None => 0,
    };
    let joiner_end = base.saturating_add(skip_leading_spaces);
    let joiner_first = match flat.get(base..joiner_end) {
        Some(s) => s.to_string(),
        None => String::new(),
    };
    base = joiner_end;

    let subsequent_width_available = opts
        .width
        .saturating_sub(rt_opts.subsequent_indent.width())
        .max(1);
    let Some(remaining) = flat.get(base..) else {
        return (out, joiners);
    };
    let remaining_wrapped = wrap_ranges_trim(remaining, opts.width(subsequent_width_available));

    let mut prev_end = 0usize;
    for (i, r) in remaining_wrapped.iter().enumerate() {
        if r.is_empty() {
            continue;
        }

        let joiner = if i == 0 {
            joiner_first.clone()
        } else {
            match remaining.get(prev_end..r.start) {
                Some(s) => s.to_string(),
                None => String::new(),
            }
        };
        prev_end = r.end;

        let offset_range = (r.start + base)..(r.end + base);
        let subsequent_line = build_wrapped_line_from_range(
            rt_opts.subsequent_indent.clone(),
            line,
            &span_bounds,
            &offset_range,
            &mut span_cursor,
        );
        out.push(subsequent_line);
        joiners.push(Some(joiner));
    }

    (out, joiners)
}

/// Wrap a sequence of lines, applying the initial indent only to the very first output line.
/// All later wrapped pieces use the subsequent indent.
#[allow(private_bounds)]
pub fn word_wrap_lines<'a, I, O, L>(lines: I, width_or_options: O) -> Vec<Line<'static>>
where
    I: IntoIterator<Item = L>,
    L: IntoLineInput<'a>,
    O: Into<RtOptions<'a>>,
{
    let base_opts: RtOptions<'a> = width_or_options.into();
    let mut out: Vec<Line<'static>> = Vec::new();

    for (idx, line) in lines.into_iter().enumerate() {
        let line_input = line.into_line_input();
        let opts = if idx == 0 {
            base_opts.clone()
        } else {
            let mut o = base_opts.clone();
            let sub = o.subsequent_indent.clone();
            o = o.initial_indent(sub);
            o
        };
        let wrapped = word_wrap_line(line_input.as_ref(), opts);
        push_owned_lines(&wrapped, &mut out);
    }

    out
}

/// Like `word_wrap_lines`, but also returns a parallel vector of soft-wrap joiners.
#[allow(private_bounds)]
pub fn word_wrap_lines_with_joiners<'a, I, O, L>(
    lines: I,
    width_or_options: O,
) -> (Vec<Line<'static>>, Vec<Option<String>>)
where
    I: IntoIterator<Item = L>,
    L: IntoLineInput<'a>,
    O: Into<RtOptions<'a>>,
{
    let base_opts: RtOptions<'a> = width_or_options.into();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut joiners: Vec<Option<String>> = Vec::new();

    for (idx, line) in lines.into_iter().enumerate() {
        let line_input = line.into_line_input();
        let opts = if idx == 0 {
            base_opts.clone()
        } else {
            let mut o = base_opts.clone();
            let sub = o.subsequent_indent.clone();
            o = o.initial_indent(sub);
            o
        };

        let (wrapped, wrapped_joiners) = word_wrap_line_with_joiners(line_input.as_ref(), opts);
        for (l, j) in wrapped.into_iter().zip(wrapped_joiners) {
            out.push(super::line_utils::line_to_static(&l));
            joiners.push(j);
        }
    }

    (out, joiners)
}

/// Utilities to allow wrapping either borrowed or owned lines.
#[derive(Debug)]
enum LineInput<'a> {
    Borrowed(&'a Line<'a>),
    Owned(Line<'a>),
}

impl<'a> LineInput<'a> {
    fn as_ref(&self) -> &Line<'a> {
        match self {
            LineInput::Borrowed(line) => line,
            LineInput::Owned(line) => line,
        }
    }
}

/// This trait makes it easier to pass whatever we need into word_wrap_lines.
trait IntoLineInput<'a> {
    fn into_line_input(self) -> LineInput<'a>;
}

impl<'a> IntoLineInput<'a> for &'a Line<'a> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Borrowed(self)
    }
}

impl<'a> IntoLineInput<'a> for &'a mut Line<'a> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Borrowed(self)
    }
}

impl<'a> IntoLineInput<'a> for Line<'a> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(self)
    }
}

impl<'a> IntoLineInput<'a> for String {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(Line::from(self))
    }
}

impl<'a> IntoLineInput<'a> for &'a str {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(Line::from(self))
    }
}

impl<'a> IntoLineInput<'a> for Cow<'a, str> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(Line::from(self))
    }
}

impl<'a> IntoLineInput<'a> for Span<'a> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(Line::from(self))
    }
}

impl<'a> IntoLineInput<'a> for Vec<Span<'a>> {
    fn into_line_input(self) -> LineInput<'a> {
        LineInput::Owned(Line::from(self))
    }
}

/// Monotonic cursor so wrapping is O(rows + spans), not O(rows × spans) — that quadratic spun the render thread on one huge line.
/// Does not rewind: a later range must not start before a previous one.
fn slice_line_spans<'a>(
    original: &'a Line<'a>,
    span_bounds: &[(Range<usize>, ratatui::style::Style)],
    range: &Range<usize>,
    cursor: &mut usize,
) -> Line<'a> {
    let start_byte = range.start;
    let end_byte = range.end;

    // Spans ending at or before this row's start are done for every later row too (ranges are contiguous and queries monotonic)
    // So advance the shared cursor past them once and never revisit them
    while span_bounds
        .get(*cursor)
        .is_some_and(|(r, _)| r.end <= start_byte)
    {
        *cursor += 1;
    }

    let mut acc: Vec<Span<'a>> = Vec::new();
    for (i, (r, style)) in span_bounds.iter().enumerate().skip(*cursor) {
        let s = r.start;
        let e = r.end;
        if s >= end_byte {
            break;
        }
        let seg_start = start_byte.max(s);
        let seg_end = end_byte.min(e);
        if seg_end > seg_start {
            let local_start = seg_start - s;
            let local_end = seg_end - s;
            let Some(content) = original.spans.get(i).map(|s| s.content.as_ref()) else {
                continue;
            };
            let Some(slice) = content.get(local_start..local_end) else {
                continue;
            };
            acc.push(Span {
                style: *style,
                content: Cow::Borrowed(slice),
            });
        }
        if e >= end_byte {
            break;
        }
    }
    Line {
        style: original.style,
        alignment: original.alignment,
        spans: acc,
    }
}

/// Word-wrap a header line with hanging indent.
///
/// The first span stays on line 1; remaining content wraps with `extra_indent + prefix_width` hanging indent on continuation lines.
pub fn wrap_header_hanging(
    header: Line<'static>,
    width: usize,
    extra_indent: usize,
) -> Vec<Line<'static>> {
    if header.spans.is_empty() {
        return vec![header];
    }

    let Some(prefix_span) = header.spans.first().cloned() else {
        return vec![header];
    };
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix_span.content.as_ref());
    let total_indent = extra_indent + prefix_width;
    let wrap_width = width.saturating_sub(total_indent);

    if wrap_width == 0 || header.spans.len() < 2 {
        return word_wrap_lines(std::iter::once(header), width);
    }

    let Some(rest) = header.spans.get(1..) else {
        return word_wrap_lines(std::iter::once(header), width);
    };
    let content_line = Line::from(rest.to_vec());
    let mut wrapped = word_wrap_lines(std::iter::once(content_line), wrap_width);

    if let Some(first) = wrapped.first_mut() {
        first.spans.insert(0, prefix_span);
    }
    if total_indent > 0 {
        let indent_str: String = " ".repeat(total_indent);
        for line in wrapped.iter_mut().skip(1) {
            line.spans
                .insert(0, ratatui::text::Span::raw(indent_str.clone()));
        }
    }

    wrapped
}

/// Continuation lines indent to a fixed column (usually the bullet width), not under the content after the prefix.
pub fn wrap_header_flush(header: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let wrap_width = width.saturating_sub(indent);
    let mut wrapped = word_wrap_lines(std::iter::once(header), wrap_width);
    if indent > 0 {
        let indent_str: String = " ".repeat(indent);
        for line in wrapped.iter_mut().skip(1) {
            line.spans
                .insert(0, ratatui::text::Span::raw(indent_str.clone()));
        }
    }
    wrapped
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Stylize;

    fn concat_line(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn nth_line<'a, 'b>(out: &'a [Line<'b>], i: usize) -> &'a Line<'b> {
        let Some(line) = out.get(i) else {
            panic!("expected line {i}, got {} lines", out.len());
        };
        line
    }

    #[test]
    fn trivial_unstyled_no_indents_wide_width() {
        let line = Line::from("hello");
        let out = word_wrap_line(&line, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(concat_line(nth_line(&out, 0)), "hello");
    }

    #[test]
    fn simple_unstyled_wrap_narrow_width() {
        let line = Line::from("hello world");
        let out = word_wrap_line(&line, 5);
        assert_eq!(out.len(), 2);
        assert_eq!(concat_line(nth_line(&out, 0)), "hello");
        assert_eq!(concat_line(nth_line(&out, 1)), "world");
    }

    #[test]
    fn simple_styled_wrap_preserves_styles() {
        let line = Line::from(vec!["hello ".red(), "world".into()]);
        let out = word_wrap_line(&line, 6);
        assert_eq!(out.len(), 2);
        // First line carries the red style
        assert_eq!(concat_line(nth_line(&out, 0)), "hello");
        let first = nth_line(&out, 0);
        assert_eq!(first.spans.len(), 1);
        let Some(span) = first.spans.first() else {
            panic!("expected a span on first line");
        };
        assert_eq!(span.style.fg, Some(Color::Red));
        // Second line is unstyled
        assert_eq!(concat_line(nth_line(&out, 1)), "world");
        let second = nth_line(&out, 1);
        assert_eq!(second.spans.len(), 1);
        let Some(span) = second.spans.first() else {
            panic!("expected a span on second line");
        };
        assert_eq!(span.style.fg, None);
    }
    #[test]
    fn real_markdown_link_underline_does_not_leak_after_wrap() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Modifier;
        use ratatui::widgets::Widget;

        // Use the real theme's markdown style to produce spans.
        let md_style = crate::theme::md_style::style();
        let md = "uses [Buildkite](https://buildkite.com/) here\n";
        let (out, _) = distill_markdown::render_markdown_ratatui_full(md, md_style, true, None);
        assert!(!out.lines.is_empty());

        // Wrap at narrow width.
        let (wrapped, _joiners) = word_wrap_lines_with_joiners(out.lines.clone(), 20);

        // Render to buffer.
        let area = Rect::new(0, 0, 20, wrapped.len() as u16);
        let mut buf = Buffer::empty(area);
        for (row, wline) in wrapped.iter().enumerate() {
            let row_area = Rect::new(0, row as u16, 20, 1);
            wline.clone().render(row_area, &mut buf);
        }

        // The real theme uses UNDERLINED for link_text.
        let mut underlined_chars = String::new();
        for row in 0..buf.area().height {
            for col in 0..buf.area().width {
                let cell = buf.cell((col, row)).unwrap();
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    underlined_chars.push_str(cell.symbol());
                }
            }
        }
        assert_eq!(
            underlined_chars.trim(),
            "Buildkite",
            "UNDERLINED should only cover 'Buildkite', got {:?}",
            underlined_chars,
        );
    }
    #[test]
    fn multi_link_paragraph_underline_boundaries() {
        use crate::render::safe_buf::SafeBuf;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Modifier;

        let md_style = crate::theme::md_style::style();
        let md = "This project uses [Bazel](https://bazel.build/) as its build system, \
            with [uv](https://docs.astral.sh/uv/) for Python package management. \
            The Rust crates are built with [Cargo](https://doc.rust-lang.org/cargo/). \
            For container builds, we use [Podman](https://podman.io/) and deploy to \
            [Kubernetes](https://kubernetes.io/) clusters. Source code is hosted on \
            [GitHub](https://github.com/) and CI/CD runs via \
            [Buildkite](https://buildkite.com/).\n";
        let (out, _) = distill_markdown::render_markdown_ratatui_full(md, md_style, true, None);
        let labels = [
            "Bazel",
            "uv",
            "Cargo",
            "Podman",
            "Kubernetes",
            "GitHub",
            "Buildkite",
        ];

        // Try multiple widths to catch wrapping-dependent issues.
        for width in [40u16, 50, 60, 70, 80] {
            let (wrapped, _) = word_wrap_lines_with_joiners(out.lines.clone(), width as usize);
            let area = Rect::new(0, 0, width, wrapped.len() as u16);
            let mut buf = Buffer::empty(area);
            for (row, wline) in wrapped.iter().enumerate() {
                buf.set_line_safe(0, row as u16, wline, width);
            }

            let mut underlined = String::new();
            for row in 0..buf.area().height {
                for col in 0..buf.area().width {
                    let cell = buf.cell((col, row)).unwrap();
                    if cell.modifier.contains(Modifier::UNDERLINED) {
                        underlined.push_str(cell.symbol());
                    }
                }
            }

            // Dump span details for debugging.
            for (i, wline) in wrapped.iter().enumerate() {
                if !wline.style.add_modifier.is_empty() {
                    panic!(
                        "width={width}: wrapped line {i} has Line-level modifier {:?}",
                        wline.style.add_modifier,
                    );
                }
                for span in &wline.spans {
                    let has_ul = span.style.add_modifier.contains(Modifier::UNDERLINED);
                    if has_ul {
                        let content = span.content.as_ref();
                        for ch in content.chars() {
                            if !labels.iter().any(|l| l.contains(ch)) && !ch.is_whitespace() {
                                panic!(
                                    "width={width}: line {i} has underlined span {:?} \
                                     containing non-label char {:?}",
                                    content, ch,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn with_initial_and_subsequent_indents() {
        let opts = RtOptions::new(8)
            .initial_indent(Line::from("- "))
            .subsequent_indent(Line::from("  "));
        let line = Line::from("hello world foo");
        let out = word_wrap_line(&line, opts);
        assert!(concat_line(nth_line(&out, 0)).starts_with("- "));
        assert!(concat_line(nth_line(&out, 1)).starts_with("  "));
        assert!(concat_line(nth_line(&out, 2)).starts_with("  "));
        assert_eq!(concat_line(nth_line(&out, 0)), "- hello");
        assert_eq!(concat_line(nth_line(&out, 1)), "  world");
        assert_eq!(concat_line(nth_line(&out, 2)), "  foo");
    }

    #[test]
    fn empty_input_yields_single_empty_line() {
        let line = Line::from("");
        let out = word_wrap_line(&line, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(concat_line(nth_line(&out, 0)), "");
    }

    #[test]
    fn leading_spaces_preserved_on_first_line() {
        let line = Line::from("   hello");
        let out = word_wrap_line(&line, 8);
        assert_eq!(out.len(), 1);
        assert_eq!(concat_line(nth_line(&out, 0)), "   hello");
    }

    #[test]
    fn multiple_spaces_between_words_dont_start_next_line_with_spaces() {
        let line = Line::from("hello   world");
        let out = word_wrap_line(&line, 8);
        assert_eq!(out.len(), 2);
        assert_eq!(concat_line(nth_line(&out, 0)), "hello");
        assert_eq!(concat_line(nth_line(&out, 1)), "world");
    }

    #[test]
    fn break_words_false_allows_overflow_for_long_word() {
        let opts = RtOptions::new(5).break_words(false);
        let line = Line::from("supercalifragilistic");
        let out = word_wrap_line(&line, opts);
        assert_eq!(out.len(), 1);
        assert_eq!(concat_line(nth_line(&out, 0)), "supercalifragilistic");
    }

    #[test]
    fn hyphen_splitter_breaks_at_hyphen() {
        let line = Line::from("hello-world");
        let out = word_wrap_line(&line, 7);
        assert_eq!(out.len(), 2);
        assert_eq!(concat_line(nth_line(&out, 0)), "hello-");
        assert_eq!(concat_line(nth_line(&out, 1)), "world");
    }

    #[test]
    fn wrap_line_with_joiners_matches_word_wrap_line_output() {
        let opts = RtOptions::new(8)
            .initial_indent(Line::from("- "))
            .subsequent_indent(Line::from("  "));
        let line = Line::from(vec!["hello ".red(), "world".into()]);

        let out = word_wrap_line(&line, opts.clone());
        let (with_joiners, joiners) = word_wrap_line_with_joiners(&line, opts);

        let out_strs: Vec<_> = out.iter().map(concat_line).collect();
        let with_joiners_strs: Vec<_> = with_joiners.iter().map(concat_line).collect();
        assert_eq!(with_joiners_strs, out_strs);
        assert_eq!(joiners.len(), with_joiners.len());
        assert_eq!(
            joiners.first().cloned().unwrap_or(Some("x".to_string())),
            None
        );
    }

    #[test]
    fn wrap_line_with_joiners_includes_skipped_spaces() {
        let line = Line::from("hello   world");
        let (wrapped, joiners) = word_wrap_line_with_joiners(&line, 8);

        let strs: Vec<_> = wrapped.iter().map(concat_line).collect();
        assert_eq!(strs, vec!["hello", "world"]);
        assert_eq!(joiners, vec![None, Some("   ".to_string())]);
    }

    #[test]
    fn wrap_line_with_joiners_uses_empty_joiner_for_mid_word_split() {
        let line = Line::from("abcd");
        let (wrapped, joiners) = word_wrap_line_with_joiners(&line, 2);

        let strs: Vec<_> = wrapped.iter().map(concat_line).collect();
        assert_eq!(strs, vec!["ab", "cd"]);
        assert_eq!(joiners, vec![None, Some("".to_string())]);
    }

    #[test]
    fn wrap_lines_with_joiners_marks_hard_breaks_between_input_lines() {
        let (wrapped, joiners) =
            word_wrap_lines_with_joiners([Line::from("hello world"), Line::from("foo bar")], 5);

        let strs: Vec<_> = wrapped.iter().map(concat_line).collect();
        assert_eq!(strs, vec!["hello", "world", "foo", "bar"]);
        assert_eq!(
            joiners,
            vec![None, Some(" ".to_string()), None, Some(" ".to_string())]
        );
    }

    #[test]
    fn wrap_lines_accepts_str_slices() {
        let lines = ["hello world", "goodnight moon"];
        let out = word_wrap_lines(lines, 12);
        let rendered: Vec<String> = out.iter().map(concat_line).collect();
        assert_eq!(rendered, vec!["hello world", "goodnight", "moon"]);
    }

    #[test]
    fn wide_unicode_wraps_by_display_width() {
        let line = Line::from("😀😀😀");
        let out = word_wrap_line(&line, 4);
        assert_eq!(out.len(), 2);
        assert_eq!(concat_line(nth_line(&out, 0)), "😀😀");
        assert_eq!(concat_line(nth_line(&out, 1)), "😀");
    }

    #[test]
    fn styled_split_within_span_preserves_style() {
        let line = Line::from(vec!["abcd".red()]);
        let out = word_wrap_line(&line, 2);
        assert_eq!(out.len(), 2);
        let first = nth_line(&out, 0);
        let second = nth_line(&out, 1);
        assert_eq!(first.spans.len(), 1);
        assert_eq!(second.spans.len(), 1);
        let Some(span0) = first.spans.first() else {
            panic!("expected a span on first line");
        };
        let Some(span1) = second.spans.first() else {
            panic!("expected a span on second line");
        };
        assert_eq!(span0.style.fg, Some(Color::Red));
        assert_eq!(span1.style.fg, Some(Color::Red));
        assert_eq!(concat_line(first), "ab");
        assert_eq!(concat_line(second), "cd");
    }

    /// The monotonic `cursor` fast path must produce byte-for-byte the same slices as a naive rescan-from-0.
    /// The ranges walked are increasing and non-overlapping, sometimes gapped, exactly what wrapping issues.
    /// This is the correctness guard for the O(rows×spans) to O(rows+spans) optimization.
    #[test]
    fn slice_line_spans_cursor_matches_naive_over_monotonic_ranges() {
        use ratatui::style::{Color, Style};

        // Contiguous styled spans, like markdown output of a long paragraph.
        let mut spans = Vec::new();
        for i in 0..300 {
            let c = match i % 3 {
                0 => Color::Red,
                1 => Color::Green,
                _ => Color::Blue,
            };
            spans.push(Span::styled(format!("tok{i}-"), Style::default().fg(c)));
        }
        let line = Line::from(spans);
        let (_flat, span_bounds) = flatten_line_and_bounds(&line);
        let total = span_bounds.last().map(|(r, _)| r.end).unwrap_or(0);

        // Naive reference: rescan from index 0 for every range (old O(R*S) path).
        fn naive(
            original: &Line<'_>,
            span_bounds: &[(Range<usize>, ratatui::style::Style)],
            range: &Range<usize>,
        ) -> Vec<(String, Option<ratatui::style::Color>)> {
            let mut out = Vec::new();
            for (i, (r, style)) in span_bounds.iter().enumerate() {
                if r.end <= range.start {
                    continue;
                }
                if r.start >= range.end {
                    break;
                }
                let seg_start = range.start.max(r.start);
                let seg_end = range.end.min(r.end);
                if seg_end > seg_start {
                    let Some(content) = original.spans.get(i).map(|s| s.content.as_ref()) else {
                        continue;
                    };
                    let Some(slice) = content.get(seg_start - r.start..seg_end - r.start) else {
                        continue;
                    };
                    out.push((slice.to_string(), style.fg));
                }
            }
            out
        }

        // Walk increasing, non-overlapping ranges (some adjacent, some gapped) reusing one cursor, exactly the access pattern wrapping produces
        let mut cursor = 0usize;
        let mut pos = 0usize;
        let mut step = 7usize;
        while pos < total {
            let end = (pos + step).min(total);
            let range = pos..end;

            let got = slice_line_spans(&line, &span_bounds, &range, &mut cursor);
            let got_simple: Vec<(String, Option<Color>)> = got
                .spans
                .iter()
                .map(|s| (s.content.to_string(), s.style.fg))
                .collect();

            assert_eq!(
                got_simple,
                naive(&line, &span_bounds, &range),
                "cursor slice diverged from naive at range {range:?}",
            );

            // Advance with an occasional 1-byte gap and a varying step.
            pos = end + (step % 2);
            step = (step % 9) + 3;
        }
    }

    /// End-to-end guard that the cursor is threaded correctly through `word_wrap_line_with_joiners`.
    /// A line with hundreds of styled spans wrapped into many rows must preserve every word, in order, with its original colour.
    #[test]
    fn many_spans_many_rows_wrap_preserves_text_and_style() {
        use ratatui::style::{Color, Style};

        let words: Vec<String> = (0..400).map(|i| format!("word{i} ")).collect();
        let spans: Vec<Span> = words
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let c = if i % 2 == 0 { Color::Red } else { Color::Blue };
                Span::styled(w.clone(), Style::default().fg(c))
            })
            .collect();
        let line = Line::from(spans);

        let out = word_wrap_line(&line, 24);
        assert!(
            out.len() > 30,
            "expected many wrapped rows, got {}",
            out.len()
        );

        // Every word survives in order
        // Soft-wrap trims the boundary space per row (it lives in the joiner), so split each row then flatten
        // Concatenating row texts directly would fuse the boundary words
        let got_words: Vec<String> = out
            .iter()
            .flat_map(|l| {
                let row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected_words: Vec<String> = (0..400).map(|i| format!("word{i}")).collect();
        assert_eq!(got_words, expected_words);

        // Colours stay correct across the cursor boundaries.
        for span in out.iter().flat_map(|l| l.spans.iter()) {
            if let Some(idx) = span
                .content
                .as_ref()
                .trim()
                .strip_prefix("word")
                .and_then(|rest| rest.parse::<usize>().ok())
            {
                let expected = if idx % 2 == 0 {
                    Color::Red
                } else {
                    Color::Blue
                };
                assert_eq!(span.style.fg, Some(expected), "word{idx} lost its colour");
            }
        }
    }

    // -- byte_range_to_row_cols tests -----------------------------------------

    #[test]
    fn highlight_single_row_match() {
        // "hello world" on one row (no wrapping).
        let text = "hello world";
        let ranges = vec![0..11]; // one row, full text
        let segments = byte_range_to_row_cols(text, &ranges, 6..11); // "world"
        assert_eq!(
            segments,
            vec![HighlightSegment {
                row: 0,
                col_start: 6,
                col_end: 11
            }]
        );
    }

    #[test]
    fn highlight_match_spanning_two_rows() {
        // "world" (bytes 6..11) crosses the wrap at column 10, so the match spans both rows.
        let text = "hello world end";
        let ranges = vec![0..10, 10..15];
        let segments = byte_range_to_row_cols(text, &ranges, 6..11);
        assert_eq!(
            segments,
            vec![
                HighlightSegment {
                    row: 0,
                    col_start: 6,
                    col_end: 10
                },
                HighlightSegment {
                    row: 1,
                    col_start: 0,
                    col_end: 1
                },
            ]
        );
    }

    #[test]
    fn highlight_match_at_row_boundary() {
        // Match ends exactly at the wrap boundary.
        let text = "0123456789abcdefghij";
        let ranges = vec![0..10, 10..20];
        let segments = byte_range_to_row_cols(text, &ranges, 5..10);
        // Only row 0: the end is exclusive, so byte 10 is not included
        assert_eq!(
            segments,
            vec![HighlightSegment {
                row: 0,
                col_start: 5,
                col_end: 10
            },]
        );
    }

    #[test]
    fn highlight_match_starts_at_row_boundary() {
        // Match starts exactly at the second row.
        let text = "0123456789abcdefghij";
        let ranges = vec![0..10, 10..20];
        let segments = byte_range_to_row_cols(text, &ranges, 10..15);
        assert_eq!(
            segments,
            vec![HighlightSegment {
                row: 1,
                col_start: 0,
                col_end: 5
            },]
        );
    }

    #[test]
    fn highlight_no_overlap() {
        // Match is entirely outside the wrapped rows.
        let text = "0123456789abcdefghij";
        let ranges = vec![0..10, 10..20];
        let segments = byte_range_to_row_cols(text, &ranges, 25..30);
        assert!(segments.is_empty());
    }

    #[test]
    fn highlight_match_spans_three_rows() {
        let text = "0123456789abcde";
        let ranges = vec![0..5, 5..10, 10..15];
        let segments = byte_range_to_row_cols(text, &ranges, 3..12);
        assert_eq!(
            segments,
            vec![
                HighlightSegment {
                    row: 0,
                    col_start: 3,
                    col_end: 5
                },
                HighlightSegment {
                    row: 1,
                    col_start: 0,
                    col_end: 5
                },
                HighlightSegment {
                    row: 2,
                    col_start: 0,
                    col_end: 2
                },
            ]
        );
    }

    #[test]
    fn highlight_with_multibyte_chars() {
        // "ab—cd" where — is U+2014 (3 bytes, 1 display column).
        // Byte layout: a(1) b(1) —(3) c(1) d(1) = 7 bytes total.
        // Display:     a(0) b(1) —(2) c(3) d(4) = 5 display columns.
        let text = "ab\u{2014}cd";
        assert_eq!(text.len(), 7);
        let ranges = vec![0..7]; // one row
        // Match "cd" (bytes 5..7) is display cols 3..5
        let segments = byte_range_to_row_cols(text, &ranges, 5..7);
        assert_eq!(
            segments,
            vec![HighlightSegment {
                row: 0,
                col_start: 3,
                col_end: 5,
            }]
        );
    }

    #[test]
    fn highlight_with_wide_emoji() {
        // "a😀b" where 😀 is U+1F600 (4 bytes, 2 display columns).
        // Byte layout: a(1) 😀(4) b(1) = 6 bytes total.
        // Display:     a(0) 😀(1..3) b(3) = 4 display columns.
        let text = "a\u{1F600}b";
        assert_eq!(text.len(), 6);
        let ranges = vec![0..6];
        // Match "b" (bytes 5..6) is display col 3..4
        let segments = byte_range_to_row_cols(text, &ranges, 5..6);
        assert_eq!(
            segments,
            vec![HighlightSegment {
                row: 0,
                col_start: 3,
                col_end: 4,
            }]
        );
    }

    // -- Table line detection / no-wrap tests ----------------------------------

    #[test]
    fn table_line_box_drawing_not_wrapped() {
        use unicode_width::UnicodeWidthStr;

        // Wider than the content width: clipped (no ellipsis), never wrapped.
        let line = Line::from("│ Column A │ Column B │ Some very long content here │");
        let (wrapped, joiners) = word_wrap_line_with_joiners(&line, 10);
        assert_eq!(wrapped.len(), 1, "Table line should not be wrapped");
        assert_eq!(
            concat_line(nth_line(&wrapped, 0)).width(),
            10,
            "Table line should be clipped to the content width"
        );
        assert!(
            concat_line(&line).starts_with(&concat_line(nth_line(&wrapped, 0))),
            "Clipped row must be a verbatim prefix of the original (no ellipsis)"
        );
        assert_eq!(joiners, vec![None]);
    }

    /// Regression: a table row narrower than the content width must be padded so the app owns every column.
    /// Otherwise a glyph rendered wider than measured strands a ghost cell.
    #[test]
    fn table_row_padded_to_content_width() {
        use unicode_width::UnicodeWidthStr;

        let line = Line::from("│ Status  │ Note      │"); // 23 display columns
        assert_eq!(concat_line(&line).width(), 23);

        let content_width = 40;
        let (wrapped, joiners) = word_wrap_line_with_joiners(&line, content_width);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(joiners, vec![None]);
        assert_eq!(
            concat_line(nth_line(&wrapped, 0)).width(),
            content_width,
            "table row must own every column up to the content width"
        );
        assert!(
            concat_line(nth_line(&wrapped, 0)).starts_with("│ Status  │ Note      │"),
            "original content must be preserved before the padding"
        );
    }

    /// Faithful repro: a body row with an emoji-presentation sequence (`⚠\u{FE0F}`) and an em-dash must be padded to exactly the content width.
    /// Those are the glyphs that desynced the cursor.
    #[test]
    fn table_row_with_emoji_and_em_dash_fills_content_width() {
        use unicode_width::UnicodeWidthStr;

        // Mirrors the markdown table body row "│ ⚠️ warn │ em — dash │".
        let line = Line::from("│ \u{26A0}\u{FE0F} warn │ em \u{2014} dash │");
        let natural = concat_line(&line).width();

        let content_width = natural + 12;
        let (wrapped, _) = word_wrap_line_with_joiners(&line, content_width);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(
            concat_line(nth_line(&wrapped, 0)).width(),
            content_width,
            "emoji/em-dash row must be padded to exactly the content width"
        );
    }

    #[test]
    fn table_border_top_not_wrapped() {
        let line = Line::from("┌──────────┬──────────┬──────────────────────────────┐");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 10);
        assert_eq!(wrapped.len(), 1, "Table top border should not be wrapped");
    }

    #[test]
    fn table_border_bottom_not_wrapped() {
        let line = Line::from("└──────────┴──────────┴──────────────────────────────┘");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 10);
        assert_eq!(
            wrapped.len(),
            1,
            "Table bottom border should not be wrapped"
        );
    }

    #[test]
    fn table_separator_not_wrapped() {
        let line = Line::from("├──────────┼──────────┼──────────────────────────────┤");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 10);
        assert_eq!(wrapped.len(), 1, "Table separator should not be wrapped");
    }

    #[test]
    fn normal_text_still_wraps() {
        let line = Line::from("This is a normal paragraph that should wrap at word boundaries");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 20);
        assert!(wrapped.len() > 1, "Normal text should still wrap");
    }

    #[test]
    fn is_table_line_detection() {
        // Box drawing characters: table borders
        assert!(is_table_line(&Line::from("┌─────┐")));
        assert!(is_table_line(&Line::from("└─────┘")));
        assert!(is_table_line(&Line::from("├─────┤")));
        assert!(is_table_line(&Line::from("─────")));

        // Table content rows (multiple │)
        assert!(is_table_line(&Line::from("│ cell │")));
        assert!(is_table_line(&Line::from("│ a │ b │")));

        // ASCII table borders
        assert!(is_table_line(&Line::from("| cell |")));

        // NOT table lines
        assert!(!is_table_line(&Line::from("Hello world")));
        assert!(!is_table_line(&Line::from("  indented text")));
        assert!(!is_table_line(&Line::from("")));
        assert!(!is_table_line(&Line::from("> blockquote")));

        // Blockquote lines are NOT table lines
        assert!(!is_table_line(&Line::from("│ blockquote text")));
        assert!(!is_table_line(&Line::from("│ │ nested blockquote")));
    }

    #[test]
    fn blockquote_line_wraps_with_prefix() {
        let line =
            Line::from("│ This is a blockquote that should wrap to multiple lines when narrow");
        let (wrapped, joiners) = word_wrap_line_with_joiners(&line, 30);

        assert!(
            wrapped.len() > 1,
            "Blockquote line should wrap. Got: {wrapped:?}"
        );

        // Every output line must start with the │ prefix
        for (i, w) in wrapped.iter().enumerate() {
            let text = concat_line(w);
            assert!(
                text.starts_with("│ "),
                "Line {i} should start with '│ ', got: {text:?}"
            );
        }

        let Some(first_joiner) = joiners.first() else {
            panic!("expected joiners");
        };
        assert_eq!(first_joiner, &None);
        let Some(rest) = joiners.get(1..) else {
            panic!("expected continuation joiners");
        };
        assert!(rest.iter().all(|j| j.is_some()));
    }

    #[test]
    fn nested_blockquote_wraps_with_prefix() {
        let line = Line::from("│ │ Nested blockquote text that should also wrap properly");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 25);

        assert!(
            wrapped.len() > 1,
            "Nested blockquote should wrap. Got: {wrapped:?}"
        );

        for (i, w) in wrapped.iter().enumerate() {
            let text = concat_line(w);
            assert!(
                text.starts_with("│ │ "),
                "Line {i} should start with '│ │ ', got: {text:?}"
            );
        }
    }

    #[test]
    fn short_blockquote_no_wrap() {
        let line = Line::from("│ Short");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 80);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(concat_line(nth_line(&wrapped, 0)), "│ Short");
    }

    #[test]
    fn blockquote_prefix_only_no_crash() {
        // A line that is only the prefix (no content after it) should not set subsequent_indent and should not panic
        let line = Line::from("\u{2502} ");
        let (wrapped, _) = word_wrap_line_with_joiners(&line, 80);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn heavy_vertical_not_treated_as_table() {
        // U+2503 (┃ heavy vertical) should not be treated as a table border.
        assert!(!is_table_line(&Line::from("\u{2503} text")));
    }

    #[test]
    fn wrap_byte_ranges_blockquote_matches_visual() {
        // wrap_byte_ranges_matching must produce the same breakpoints as word_wrap_line_with_joiners for blockquote lines
        // That keeps search highlights aligned with the visual rendering
        let text = "\u{2502} This is a blockquote that should wrap to multiple lines";
        let width = 30;

        let line = Line::from(text);
        let (visual, _) = word_wrap_line_with_joiners(&line, width);
        let ranges = wrap_byte_ranges_matching(text, width);

        assert_eq!(
            visual.len(),
            ranges.len(),
            "Row count mismatch: visual={}, ranges={}",
            visual.len(),
            ranges.len()
        );
    }
}
