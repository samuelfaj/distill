// Modified for Distill by Samuel Fajreldines, 2026.
//! The markdown renderer rewrites each `>` quote marker to a `│` bar styled `blockquote_outer` (distill-markdown parse.rs).
//! The bar is then ordinary span content and would leak into drag-select copies.
//! The helpers here detect that prefix on a rendered row and exclude it from selection via [`Selectable::Spans`].
//! Tool headers and diff gutters exclude their decorations the same way.
//! [`MarkdownContent::output`](super::markdown_content::MarkdownContent::output) renders through these helpers.
//! So does [`ThinkingBlock`](super::ThinkingBlock)'s render path.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::scrollback::types::Selectable;
use crate::theme::Theme;

/// Built once per output pass: whether to strip quote bars, and the bar style the theme paints them with.
/// Raw mode skips the `Theme::current()` lookup entirely.
#[derive(Clone, Copy)]
pub(crate) struct QuoteBarStrip {
    /// `None` means stripping is disabled (raw mode shows source `>` markers).
    bar_style: Option<Style>,
}

impl QuoteBarStrip {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            bar_style: enabled.then(quote_bar_style),
        }
    }

    /// Selection metadata for one rendered row; see [`quote_prefix_selectable`].
    pub(crate) fn selectable(&self, line: &mut Line<'static>) -> Selectable {
        match self.bar_style {
            Some(bar_style) => quote_prefix_selectable(line, bar_style),
            None => Selectable::All,
        }
    }
}

/// The exact ratatui style the renderer paints parser-generated blockquote bars with. This mirrors
/// `blockquote_outer` in pager-render's theme/md_style.rs, and a comment there points back here. The end-to-end
/// tests below trip if either side drifts.
fn quote_bar_style() -> Style {
    let muted = Theme::current().md_muted;
    let style = Style::default().add_modifier(Modifier::DIM);
    if muted == Color::Reset {
        style
    } else {
        style.fg(muted)
    }
}

/// Byte length of the rendered blockquote prefix at the start of `line`. A bar-only row (blank line inside a quote:
/// `│`, `│ │`, optional trailing space) counts as all prefix. Every prefix bar must carry `bar_style`. The shape
/// must agree with the looser `blockquote_prefix_len` in pager-render's wrapping.rs.
fn rendered_quote_prefix_len(line: &Line<'_>, bar_style: Style) -> Option<usize> {
    const BAR: char = '\u{2502}';
    const BAR_LEN: usize = '\u{2502}'.len_utf8();
    let mut chars = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .peekable();
    let mut len = 0usize;
    loop {
        match chars.next() {
            Some((BAR, style)) if style == bar_style => len += BAR_LEN,
            _ => return None,
        }
        match chars.next() {
            None => return Some(len),
            Some((' ', _)) => {
                len += 1;
                match chars.peek() {
                    None => return Some(len),
                    Some((BAR, style)) if *style == bar_style => continue,
                    Some(_) => break,
                }
            }
            Some(_) => return None,
        }
    }
    if chars.any(|(c, _)| c == BAR) {
        return None;
    }
    Some(len)
}

/// Split `line`'s spans at `byte_offset` (splitting a straddling span in two) and return the number of spans covering `0..byte_offset`.
fn split_spans_at(line: &mut Line<'static>, byte_offset: usize) -> usize {
    let mut acc = 0usize;
    for i in 0..line.spans.len() {
        let Some(span) = line.spans.get(i) else { break };
        let end = acc + span.content.len();
        if end == byte_offset {
            return i + 1;
        }
        if end > byte_offset {
            let Some(local) = byte_offset.checked_sub(acc) else {
                return line.spans.len();
            };
            let Some(span) = line.spans.get_mut(i) else {
                break;
            };
            let tail: Cow<'static, str> = match &mut span.content {
                Cow::Borrowed(s) => {
                    let (head, tail) = s.split_at(local);
                    span.content = Cow::Borrowed(head);
                    Cow::Borrowed(tail)
                }
                Cow::Owned(s) => Cow::Owned(s.split_off(local)),
            };
            let style = span.style;
            line.spans.insert(
                i + 1,
                Span {
                    content: tail,
                    style,
                },
            );
            return i + 1;
        }
        acc = end;
    }
    line.spans.len()
}

/// Selection metadata for a pretty-mode markdown row. Bar-only rows (blank quote lines) return an empty end range
/// so multi-line copies keep the blank line. The bar must be the FIRST span: a quote indented under a list item (`-
/// > quoted`, bullet span first) keeps its prefix in copies.
fn quote_prefix_selectable(line: &mut Line<'static>, bar_style: Style) -> Selectable {
    // Only parser-generated bars are a lone 1-char span carrying the blockquote_outer style
    // A literal "│ " in prose or code stays glued to its content span or carries a different style, so it is left intact
    let genuine = line
        .spans
        .first()
        .is_some_and(|s| s.content.as_ref() == "\u{2502}" && s.style == bar_style);
    if !genuine {
        return Selectable::All;
    }
    let Some(prefix_len) = rendered_quote_prefix_len(line, bar_style) else {
        return Selectable::All;
    };
    let prefix_spans = split_spans_at(line, prefix_len);
    Selectable::Spans(prefix_spans..line.spans.len())
}

#[cfg(test)]
mod tests {
    use super::super::markdown_content::{MARKDOWN_BODY_RANGE, MarkdownContent};
    use super::*;
    use crate::scrollback::text_selection::{ActiveTextDrag, RangeHit};
    use crate::scrollback::types::{
        BlockLine, BlockOutput, derive_selection_text, line_plain_text, selectable_cols,
    };

    fn find_line<'a>(out: &'a BlockOutput, needle: &str) -> &'a BlockLine {
        out.lines
            .iter()
            .find(|l| line_plain_text(&l.content).contains(needle))
            .unwrap_or_else(|| panic!("no output line contains {needle:?}"))
    }

    #[test]
    fn rendered_quote_prefix_len_shapes() {
        let bq = quote_bar_style();
        let quote = Line::from(vec![Span::styled("│", bq), Span::raw(" text")]);
        assert_eq!(rendered_quote_prefix_len(&quote, bq), Some(4));

        let nested = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
            Span::raw(" deep"),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested, bq), Some(8));

        // Bar-only rows (blank quote line), optional trailing space.
        assert_eq!(
            rendered_quote_prefix_len(&Line::from(Span::styled("│", bq)), bq),
            Some(3)
        );
        let nested_blank = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested_blank, bq), Some(7));
        assert_eq!(
            rendered_quote_prefix_len(&Line::from(vec![Span::styled("│", bq), Span::raw(" ")]), bq),
            Some(4)
        );

        // Unstyled bars are content, never a prefix.
        assert_eq!(rendered_quote_prefix_len(&Line::raw("│ text"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("│ "), bq), None);

        // Interior bars mark table rows or box art, never a quote prefix, even when the leading border span carries the identical style
        let table_row = Line::from(vec![Span::styled("│", bq), Span::raw(" a │ b │")]);
        assert_eq!(rendered_quote_prefix_len(&table_row, bq), None);
        let blank_table_row = Line::from(vec![Span::styled("│", bq), Span::raw("   │   │")]);
        assert_eq!(rendered_quote_prefix_len(&blank_table_row, bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("││"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("── rule"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("plain"), bq), None);
    }

    #[test]
    fn rendered_quote_prefix_len_rejects_content_bars_after_genuine_prefix() {
        let bq = quote_bar_style();
        // Source `> │ box art`: a genuine bar, then a literal (unstyled) bar as the first content char, which must not be consumed as a nesting level
        let literal_second = Line::from(vec![Span::styled("│", bq), Span::raw(" │ box art")]);
        assert_eq!(rendered_quote_prefix_len(&literal_second, bq), None);

        // Degenerate `> │` (content is just a bar).
        let bar_only_content = Line::from(vec![Span::styled("│", bq), Span::raw(" │")]);
        assert_eq!(rendered_quote_prefix_len(&bar_only_content, bq), None);

        // Nested variant `> > │ deep`: two genuine bars, then a literal one.
        let nested_literal = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
            Span::raw(" │ deep"),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested_literal, bq), None);
    }

    #[test]
    fn quote_prefix_selectable_requires_bar_style() {
        // Same content, wrong style (no DIM): a literal bar span is not a parser-generated quote bar and must stay fully selectable
        let mut line = Line::from(vec![Span::raw("│"), Span::raw(" text")]);
        assert_eq!(
            quote_prefix_selectable(&mut line, quote_bar_style()),
            Selectable::All
        );

        let mut line = Line::from(vec![
            Span::styled("│", quote_bar_style()),
            Span::raw(" text"),
        ]);
        assert_eq!(
            quote_prefix_selectable(&mut line, quote_bar_style()),
            Selectable::Spans(2..3)
        );
        // The glued " text" span was split so the prefix ends on a boundary.
        assert_eq!(line.spans.get(1).map(|s| s.content.as_ref()), Some(" "));
        assert_eq!(line.spans.get(2).map(|s| s.content.as_ref()), Some("text"));
    }

    #[test]
    fn quote_line_selection_excludes_bar_prefix() {
        let md = MarkdownContent::new("intro\n\n> QUOTE alpha\n\noutro");
        let out = md.output(80);

        let line = find_line(&out, "QUOTE");
        // Pretty mode renders the bar on screen
        assert!(
            line_plain_text(&line.content).starts_with("│ "),
            "expected rendered bar prefix, got {:?}",
            line_plain_text(&line.content)
        );
        // The bar is excluded from selection and copy
        assert!(
            matches!(line.selectable, Selectable::Spans(_)),
            "quote line should exclude its prefix, got {:?}",
            line.selectable
        );
        assert_eq!(derive_selection_text(line), "QUOTE alpha");
        // Non-quote lines stay fully selectable.
        assert!(matches!(
            find_line(&out, "intro").selectable,
            Selectable::All
        ));
        assert!(matches!(
            find_line(&out, "outro").selectable,
            Selectable::All
        ));
    }

    #[test]
    fn nested_quote_selection_excludes_all_bars() {
        let md = MarkdownContent::new("> outer line\n>\n> > NESTED deep");
        let out = md.output(80);

        let line = find_line(&out, "NESTED");
        assert!(line_plain_text(&line.content).starts_with("│ │ "));
        let text = derive_selection_text(line);
        assert_eq!(text, "NESTED deep");
        assert!(!text.contains('│'));
    }

    #[test]
    fn wrapped_quote_continuations_exclude_reinjected_prefix() {
        let md = MarkdownContent::new("> alpha bravo charlie delta echo foxtrot golf hotel india");
        let out = md.output(16);
        assert!(out.lines.len() > 1, "quote must wrap at width 16");

        for line in &out.lines {
            assert!(
                line_plain_text(&line.content).starts_with('│'),
                "every wrapped row repeats the bar: {:?}",
                line_plain_text(&line.content)
            );
            let text = derive_selection_text(line);
            assert!(!text.contains('│'), "copy text has a bar: {text:?}");
            assert!(
                !text.starts_with(' '),
                "copy text keeps prefix space: {text:?}"
            );
        }

        // Reconstructing via each line's joiner (the drag-copy join rule) yields the original text
        let mut joined = String::new();
        for (i, line) in out.lines.iter().enumerate() {
            if i > 0 {
                joined.push_str(line.joiner.as_deref().unwrap_or("\n"));
            }
            joined.push_str(&derive_selection_text(line));
        }
        assert_eq!(
            joined,
            "alpha bravo charlie delta echo foxtrot golf hotel india"
        );
    }

    #[test]
    fn blank_quote_line_survives_drag_copy_as_blank() {
        let md = MarkdownContent::new("> QUOTE_A first\n>\n> QUOTE_B second");
        let out = md.output(80);
        assert_eq!(out.lines.len(), 3, "quote renders as three rows");

        // The bar-only middle row keeps an (empty) selectable range so it stays in the selection model and contributes its newline
        let Some(mid) = out.lines.get(1) else {
            panic!("expected a middle quote row: {out:?}");
        };
        assert_eq!(line_plain_text(&mid.content), "│");
        assert!(
            matches!(&mid.selectable, Selectable::Spans(r) if r.is_empty()),
            "bar-only row should have an empty Spans range, got {:?}",
            mid.selectable
        );
        assert!(selectable_cols(&mid.content, &mid.selectable).is_some());
        assert_eq!(derive_selection_text(mid), "");

        // Full drag from the first to the last row copies a\n\nb.
        let drag = ActiveTextDrag {
            anchor: RangeHit {
                entry_idx: 0,
                range_id: MARKDOWN_BODY_RANGE,
                block_line_idx: 0,
                col_within_range: 0,
            },
            head: RangeHit {
                entry_idx: 0,
                range_id: MARKDOWN_BODY_RANGE,
                block_line_idx: 2,
                col_within_range: u16::MAX,
            },
            kind: Default::default(),
            anchor_content_width: None,
        };
        let text =
            crate::scrollback::text_selection::reconstruct_full_selection_text(&out.lines, &drag)
                .expect("drag reconstruction");
        assert_eq!(text, "QUOTE_A first\n\nQUOTE_B second");
    }

    #[test]
    fn literal_bar_at_quote_content_start_is_not_stripped() {
        // Quoted box-drawing output: the content's own bar must never be consumed as a nesting level (that would DELETE user bytes from the copy)
        // The row degrades to the conservative interior-bar class
        let md = MarkdownContent::new("> │ box art");
        let out = md.output(80);
        let line = find_line(&out, "box art");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ │ box art");
    }

    #[test]
    fn literal_bar_as_entire_quote_content_is_not_dropped() {
        // Degenerate `> │`: without the style-aware scan this classified as a bar-only blank row and the content bar vanished from copies
        let md = MarkdownContent::new("> │");
        let out = md.output(80);
        let line = find_line(&out, "│");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ │");
    }

    #[test]
    fn list_nested_quote_keeps_prefix() {
        // Bullet span precedes the bar, so the first-span guard skips the row (documented conservative false negative on quote_prefix_selectable)
        let md = MarkdownContent::new("- > quoted text");
        let out = md.output(80);
        let line = find_line(&out, "quoted text");
        assert!(matches!(line.selectable, Selectable::All));
        assert!(derive_selection_text(line).contains("│ quoted text"));
    }

    #[test]
    fn literal_bar_in_paragraph_is_not_stripped() {
        // A paragraph starting with a literal bar (file-tree art) is a single glued span without the quote-bar style, so it stays fully selectable
        let md = MarkdownContent::new("│ literal tree line");
        let out = md.output(80);
        let line = find_line(&out, "literal");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ literal tree line");
    }

    #[test]
    fn literal_bar_in_code_block_is_not_stripped() {
        let md = MarkdownContent::new("```\n│ box art\n└── tree\n```");
        let out = md.output(80);
        let line = find_line(&out, "box art");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ box art");
    }

    #[test]
    fn table_rows_keep_borders_in_copy() {
        let md = MarkdownContent::new("| a | b |\n|---|---|\n| CELL1 | CELL2 |");
        let out = md.output(40);
        let row = find_line(&out, "CELL1");
        assert!(matches!(row.selectable, Selectable::All));
        let text = derive_selection_text(row);
        assert!(
            text.starts_with('│') && text.ends_with('│'),
            "table row copy keeps its borders: {text:?}"
        );
    }

    #[test]
    fn raw_mode_quote_lines_stay_fully_selectable() {
        let mut md = MarkdownContent::new("> QUOTE alpha");
        md.set_raw_mode(true);
        let out = md.output(80);
        let line = find_line(&out, "QUOTE");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "> QUOTE alpha");
    }
}
