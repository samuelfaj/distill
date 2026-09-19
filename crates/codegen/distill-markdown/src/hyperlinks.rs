//! Project parser-emitted `LinkTarget`s onto rendered display cells.
//!
//! Three coordinate systems are in play:
//!
//! 1. **Source bytes**: offsets into the raw markdown the parser saw. `LinkTarget::source_range` lives here.
//! 2. **Transformed bytes**: what `apply_transforms` produces for a *chunk* of source bytes between two render events.
//!    In pretty mode the transforms strip `[` and rewrite `](` as ` (`, so transformed bytes do not line up with source bytes.
//! 3. **Display cells**: `(line_index, display_column)`, what `HyperlinkTarget` exposes for the OSC 8 layer to consume.
//!
//! A chunk's transformed string is split on `\n` into *segments*; one segment becomes one rendered line.
//! A link spanning multiple segments (a wrapped or autolink-bracketed link) produces one `HyperlinkTarget` per segment, all sharing the same `id`.

use crate::buffers::{
    LinkTarget, Transform, ceil_char_boundary, floor_char_boundary, unicode_display_width,
};
use crate::output::HyperlinkTarget;

/// One link's projection onto the current chunk's transformed string.
/// Returned by `chunk_link_offsets`; bounds are in coordinate system #2 (transformed bytes within the chunk).
/// `emit_segment_hyperlinks` later maps them onto display cells.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkLinkRange {
    /// Start byte (inclusive) within the chunk's transformed string.
    pub(crate) xform_start: usize,
    /// End byte (exclusive) within the chunk's transformed string.
    pub(crate) xform_end: usize,
    /// Index into the `link_targets` slice the caller passed in.
    pub(crate) link_idx: usize,
}

/// Project a source byte position into the chunk's transformed coordinate space (system #1 to system #2). See the module doc.
/// `transforms` is sorted by `range.start` (`apply_transforms` relies on the same invariant; the parser pushes transforms in source order); No transform's source range overlaps the bytes a caller intends to locate. Transforms touch *boundary* characters around link text (the `[` and `](` markers), never the link text itself. All transforms pushed by the parser today (link bracket removal, bullet substitutions) satisfy this. The `debug_assert!` at the call sites in `render_ratatui` enforces it via the cursor invariant.
/// **Straddle policy** (a transform contains `src_pos` despite invariant 2): the source position clamps to the start of the replacement string.
pub(crate) fn source_to_chunk_offset(
    src_pos: usize,
    chunk_start: usize,
    transforms: &[Transform],
) -> usize {
    let mut delta: isize = 0;
    for t in transforms {
        if t.range.end <= chunk_start {
            continue;
        }
        if t.range.start >= src_pos {
            break;
        }
        let t_src_start = t.range.start.max(chunk_start);
        if t.range.end <= src_pos {
            let src_len = (t.range.end - t_src_start) as isize;
            let dst_len = t.to.len() as isize;
            delta += dst_len - src_len;
        } else {
            // Transform straddles src_pos.  See "Straddle policy" above.
            debug_assert!(
                false,
                "source_to_chunk_offset: transform [{}..{}) straddles src_pos {}; \
                 link text should never overlap a transform.  See straddle policy.",
                t.range.start, t.range.end, src_pos,
            );
            let consumed = (src_pos - t_src_start) as isize;
            delta -= consumed;
            break;
        }
    }
    let raw = (src_pos - chunk_start) as isize + delta;
    raw.max(0) as usize
}

/// One `ChunkLinkRange` per link whose source range overlaps `[chunk_start, chunk_end)`.
/// `from_idx` is the caller's monotonic cursor; links before it were handled in earlier chunks (see the module doc's source-order invariant).
/// Returned bounds live in the chunk's transformed coordinate space.
pub(crate) fn chunk_link_offsets(
    link_targets: &[LinkTarget],
    from_idx: usize,
    chunk_start: usize,
    chunk_end: usize,
    pretty: bool,
    transforms: &[Transform],
) -> Vec<ChunkLinkRange> {
    let mut out = Vec::new();
    for (idx, lt) in link_targets.iter().enumerate().skip(from_idx) {
        if lt.source_range.start >= chunk_end {
            break;
        }
        if lt.source_range.end <= chunk_start {
            continue;
        }
        let src_start = lt.source_range.start.max(chunk_start);
        let src_end = lt.source_range.end.min(chunk_end);
        let (xform_start, xform_end) = if !pretty || transforms.is_empty() {
            (src_start - chunk_start, src_end - chunk_start)
        } else {
            (
                source_to_chunk_offset(src_start, chunk_start, transforms),
                source_to_chunk_offset(src_end, chunk_start, transforms),
            )
        };
        if xform_end > xform_start {
            out.push(ChunkLinkRange {
                xform_start,
                xform_end,
                link_idx: idx,
            });
        }
    }
    out
}

/// Push one `HyperlinkTarget` per `ChunkLinkRange` that overlaps this segment (system #2 to system #3).
/// `col` is the running display column on the in-progress line.
pub(crate) fn emit_segment_hyperlinks(
    chunk_links: &[ChunkLinkRange],
    link_targets: &[LinkTarget],
    segment: &str,
    seg_x_offset: usize,
    col: usize,
    line_index: usize,
    out: &mut Vec<HyperlinkTarget>,
) {
    let seg_x_end = seg_x_offset + segment.len();
    for clr in chunk_links {
        if clr.xform_end <= seg_x_offset || clr.xform_start >= seg_x_end {
            continue;
        }
        let s_in = clr
            .xform_start
            .saturating_sub(seg_x_offset)
            .min(segment.len());
        let e_in = (clr.xform_end - seg_x_offset).min(segment.len());
        let s_in = floor_char_boundary(segment, s_in);
        let e_in = ceil_char_boundary(segment, e_in);
        if s_in >= e_in {
            continue;
        }
        let Some(prefix) = segment.get(..s_in) else {
            continue;
        };
        let Some(mid) = segment.get(s_in..e_in) else {
            continue;
        };
        let col_start = col + unicode_display_width(prefix);
        let col_end = col_start + unicode_display_width(mid);
        let Some(lt) = link_targets.get(clr.link_idx) else {
            continue;
        };
        out.push(HyperlinkTarget {
            line_index,
            column_range: col_start..col_end,
            url: lt.url.clone(),
            id: lt.id,
        });
    }
}

#[cfg(test)]
mod hyperlink_tests {
    use crate::output::HyperlinkTarget;
    use crate::style::test_style;
    use crate::{StreamingMarkdownRenderer, render_markdown_ratatui_full};
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    fn line_to_string(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_at<'a>(lines: &'a [Line<'static>], idx: usize) -> &'a Line<'static> {
        let Some(line) = lines.get(idx) else {
            panic!("expected line {idx}, have {} lines", lines.len());
        };
        line
    }

    /// Slice the rendered line by display-cell `column_range`.
    /// At `range.start` they do not advance `col`; at `range.end` they still satisfy `end <= range.end`.
    fn slice_by_cells(rendered: &str, range: std::ops::Range<usize>) -> String {
        use unicode_width::UnicodeWidthChar;
        let mut col = 0usize;
        let mut out = String::new();
        for ch in rendered.chars() {
            let w = ch.width().unwrap_or(0);
            let end = col + w;
            if col >= range.start && end <= range.end {
                out.push(ch);
            } else if end > range.end {
                break;
            }
            col = end;
        }
        out
    }

    /// Find the parser-produced (link-text) hyperlink: the one whose `column_range` slices to `expected_slice` in the rendered output.
    /// `render_markdown_ratatui_full` also emits a url_scan target for the pretty-mode `(url)` suffix, so tests cannot assume a single hyperlink.
    fn parser_link_text<'a>(
        out: &'a crate::output::MarkdownRenderOutput,
        expected_slice: &str,
    ) -> &'a HyperlinkTarget {
        out.hyperlinks
            .iter()
            .find(|h| {
                let rendered = line_to_string(line_at(&out.lines, h.line_index));
                slice_by_cells(&rendered, h.column_range.clone()) == expected_slice
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected a hyperlink whose column range covers {expected_slice:?}; got {:?}",
                    out.hyperlinks,
                )
            })
    }

    /// `[link](url)` in pretty mode renders as `link (url)`.
    /// The `HyperlinkTarget`'s column range must cover the rendered "link" glyphs (4 cells), not include the stripped `[` or the rewritten ` (`.
    #[test]
    fn pretty_inline_link_column_range_excludes_brackets() {
        let text = "Here is a [link](https://example.com) in text.\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        // The parser produces one HyperlinkTarget over the link text; the url_scan pass produces a second over the `(url)` suffix
        let h = parser_link_text(&out, "link");
        assert_eq!(h.url, "https://example.com");
        let rendered = line_to_string(line_at(&out.lines, h.line_index));
        let slice: String = rendered
            .chars()
            .skip(h.column_range.start)
            .take(h.column_range.len())
            .collect();
        assert_eq!(
            slice, "link",
            "column_range should cover only the link text glyphs"
        );
    }

    /// In non-pretty mode the rendered text keeps `[link](url)` verbatim, but the parser's `LinkTarget` source range still points at just `link`.
    /// The column range therefore covers `link` (4 cells), shifted by the leading `[` that's now visible in the output.
    #[test]
    fn non_pretty_inline_link_column_range_covers_text_not_brackets() {
        let text = "[link](https://example.com)\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, false, None);

        // Non-pretty: `[link](url)` is rendered literally; url_scan also finds the URL inside `(url)` so two hyperlinks are emitted
        let h = parser_link_text(&out, "link");
        let rendered = line_to_string(line_at(&out.lines, h.line_index));
        let slice: String = rendered
            .chars()
            .skip(h.column_range.start)
            .take(h.column_range.len())
            .collect();
        assert_eq!(slice, "link");
    }

    /// Two links with identical text on the same line MUST produce two distinct `HyperlinkTarget`s with distinct URLs and disjoint column ranges.
    /// This is the case the substring approach got wrong (both would resolve to the first occurrence).
    #[test]
    fn duplicated_link_text_on_one_line_produces_distinct_targets() {
        let text = "See [click](https://a.example) and [click](https://b.example) here.\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        // Two parser-produced link-text hyperlinks (cover "click") plus two url_scan-produced hyperlinks for the `(url)` suffixes
        let click_targets: Vec<&HyperlinkTarget> = out
            .hyperlinks
            .iter()
            .filter(|h| {
                let rendered = line_to_string(line_at(&out.lines, h.line_index));
                let slice: String = rendered
                    .chars()
                    .skip(h.column_range.start)
                    .take(h.column_range.len())
                    .collect();
                slice == "click"
            })
            .collect();
        assert_eq!(
            click_targets.len(),
            2,
            "two parser-produced link-text hyperlinks expected",
        );
        let urls: Vec<&str> = click_targets.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.example", "https://b.example"]);

        let [h0, h1] = click_targets.as_slice() else {
            panic!("expected two items: {click_targets:?}");
        };
        assert_eq!(
            h0.line_index, h1.line_index,
            "both links should be on the same rendered line"
        );
        assert_ne!(h0.id, h1.id, "ids must differ");
        assert!(
            h0.column_range.end <= h1.column_range.start,
            "column ranges must be disjoint and in order, got {:?} vs {:?}",
            h0.column_range,
            h1.column_range,
        );
    }

    /// CJK characters in link text consume 2 cells each.
    /// The column range must reflect display width, not byte length (`日本語` is 9 bytes / 6 cells).
    #[test]
    fn cjk_link_uses_display_width_for_column_range() {
        let text = "[日本語](https://example.com)\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        // The parser produces one hyperlink over the CJK link text; the url_scan pass produces a second over the `(url)` suffix
        let h = parser_link_text(&out, "日本語");
        assert_eq!(
            h.column_range.len(),
            6,
            "三 wide CJK chars -> 6 display cells"
        );
    }

    /// `<https://example.com>` autolink: the parser records the source range over the entire `<...>`-bounded text.
    /// They MUST all share the same `id` and `url`, and their column ranges must collectively cover the rendered URL on a single line.
    #[test]
    fn autolink_emits_grouped_targets_for_same_logical_link() {
        let text = "Visit <https://example.com> for info.\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        assert!(
            !out.hyperlinks.is_empty(),
            "expected at least one hyperlink"
        );
        let Some(first) = out.hyperlinks.first() else {
            panic!("expected at least one hyperlink");
        };
        let url = &first.url;
        let id = first.id;
        let line_index = first.line_index;
        for h in &out.hyperlinks {
            assert_eq!(&h.url, url, "all autolink fragments share the same URL");
            assert_eq!(h.id, id, "all autolink fragments share the same id");
            assert_eq!(
                h.line_index, line_index,
                "autolink stays on one rendered line"
            );
        }
        assert_eq!(url, "https://example.com");

        let rendered = line_to_string(line_at(&out.lines, line_index));
        let mut covered: Vec<bool> = vec![false; rendered.chars().count()];
        for h in &out.hyperlinks {
            for col in h.column_range.clone() {
                if let Some(slot) = covered.get_mut(col) {
                    *slot = true;
                }
            }
        }
        let covered_text: String = rendered
            .chars()
            .zip(covered.iter())
            .filter_map(|(c, &k)| if k { Some(c) } else { None })
            .collect();
        assert!(
            covered_text.contains("https://example.com"),
            "combined column ranges must cover the rendered URL; got covered={:?} on rendered={:?}",
            covered_text,
            rendered,
        );
    }

    /// Two links with prose between them on the same line: the second link's column range must be measured from the start of the line.
    /// The running `cur_col_in_line` must survive across emit chunks.
    #[test]
    fn two_links_with_prose_between_have_correct_columns() {
        let text = "Pre [a](https://a.example) mid [b](https://b.example) post.\n";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        // Two parser-produced link-text targets (covering "a" and "b") plus two url_scan-produced targets for the `(url)` suffixes
        let h0 = parser_link_text(&out, "a");
        let h1 = parser_link_text(&out, "b");
        assert_eq!(h0.line_index, h1.line_index);
    }

    /// Streaming byte-by-byte must produce the same hyperlinks (parser + url_scan) as a single full render.
    /// Both code paths run the `url_scan` pass after parsing and sort by the same key, so the outputs must be equal.
    #[test]
    fn streaming_byte_by_byte_matches_full_render() {
        let text = "# Header\n\nSee [docs](https://example.com/docs) and [api](https://example.com/api).\n\n";
        let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        for byte in text.as_bytes() {
            // Push one byte at a time
            // The input is pure ASCII so each single-byte slice is a valid UTF-8 string
            let buf = [*byte];
            let s = std::str::from_utf8(&buf).expect("ascii test input");
            renderer.push_and_render(s, None);
        }
        renderer.finish(None);
        let view = renderer.view();

        // Compare on `(url, line_index, column_range)`; ids are intentionally independent between the two code paths
        // A full re-render restarts id counters; streaming preserves continuity
        let extract = |hs: &[HyperlinkTarget]| -> Vec<(String, usize, std::ops::Range<usize>)> {
            let mut v: Vec<_> = hs
                .iter()
                .map(|h| (h.url.clone(), h.line_index, h.column_range.clone()))
                .collect();
            v.sort_by_key(|a| (a.1, a.2.start));
            v
        };
        assert_eq!(
            extract(&full.hyperlinks),
            extract(view.hyperlinks),
            "full-render and streaming+finish must produce the same hyperlinks",
        );
    }

    /// A link whose source bytes straddle the frozen/tail boundary in the streaming renderer must still produce a `HyperlinkTarget`.
    /// The target must point at the right rendered line and columns.
    #[test]
    fn streaming_link_across_chunk_boundaries_resolves_correctly() {
        let part1 = "Para one.\n\nSee [my ";
        let part2 = "link](https://example.com) here.\n\n";
        let full_text = format!("{part1}{part2}");

        let (full, _) = render_markdown_ratatui_full(&full_text, test_style::STYLE, true, None);
        // Both code paths run url_scan, so the full render has the parser-produced link-text hyperlink and the url_scan URL-suffix hyperlink
        assert_eq!(full.hyperlinks.len(), 2);
        let expected = parser_link_text(&full, "my link");

        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(part1, None);
        renderer.push_and_render(part2, None);
        renderer.finish(None);
        let view = renderer.view();

        assert_eq!(
            view.hyperlinks.len(),
            2,
            "expected 2 hyperlinks (parser link text + URL in pretty-mode suffix)"
        );
        let got = view
            .hyperlinks
            .iter()
            .find(|h| {
                h.column_range == expected.column_range && h.line_index == expected.line_index
            })
            .expect("parser-produced hyperlink should be present after finish()");
        assert_eq!(got.url, expected.url);
        assert_eq!(got.line_index, expected.line_index);
        assert_eq!(got.column_range, expected.column_range);
    }

    /// Covers the case where the URL literal itself contains `](` (plus a streaming split inside the tag source).
    /// The dest-anchored rfind logic also protects realistic nested-image-in-link cases (see nested_image_in_link_finds_outer_closer).
    #[test]
    fn dest_url_containing_bracket_paren_with_streaming_split() {
        let text = "[t](<u](v>) end\n";
        let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
        let line0 = line_to_string(line_at(&full.lines, 0));
        assert!(line0.contains("t (<u](v>)") || line0.contains("t ( <u](v> )"));
        let link = full
            .hyperlinks
            .iter()
            .find(|h| h.url.contains("u](v"))
            .expect("link");
        let slice: String = line0
            .chars()
            .skip(link.column_range.start)
            .take(link.column_range.len())
            .collect();
        assert_eq!(slice, "t");
        let mut r = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        r.push_and_render("[t](<u]", None);
        r.push_and_render("(v>) end\n", None);
        r.finish(None);
        let view = r.view();
        let view_link = view
            .hyperlinks
            .iter()
            .find(|h| h.url.contains("u](v"))
            .expect("view");
        assert_eq!(view_link.url, link.url);
        assert_eq!(view_link.column_range, link.column_range);
    }

    /// Realistic trigger for the bug: nested image inside a link (common in LLM output: badges, thumbnails, etc.).
    /// The outer closer must be found correctly.
    #[test]
    fn nested_image_in_link_finds_outer_closer() {
        let text = "[![badge](https://img.shields.io/v1.svg)](https://github.com/repo) end\n";
        let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        let link = full
            .hyperlinks
            .iter()
            .find(|h| h.url.contains("github.com/repo"))
            .expect("outer link should target repo URL");

        assert!(
            !link.url.contains("shields.io"),
            "should not pick the inner image URL"
        );
    }

    /// Markdown links inside table cells must produce `HyperlinkTarget`s the same way links inside paragraphs do.
    /// Otherwise the pager's OSC 8 overlay never learns about them and the link is not clickable and not styled.
    #[test]
    fn link_inside_table_cell_emits_hyperlink_and_styling() {
        let text = "\
| Name | Link |
|------|------|
| Foo  | [click](https://example.com) |
";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        // (1) Hyperlink present with the right URL.
        let link = out
            .hyperlinks
            .iter()
            .find(|h| h.url == "https://example.com")
            .expect("table cell link should produce a HyperlinkTarget");

        // (2) Column range covers only the rendered "click" glyphs.
        let rendered = line_to_string(line_at(&out.lines, link.line_index));
        let slice: String = rendered
            .chars()
            .skip(link.column_range.start)
            .take(link.column_range.len())
            .collect();
        assert_eq!(
            slice, "click",
            "column_range should cover only the link text glyphs in the cell, \
             got slice={slice:?} from rendered={rendered:?}"
        );

        // (3) The link text span carries `link_text` styling (bold in the test style)
        // The cell wrapper splits the cell into multiple spans; find the span whose content is "click"
        let cell_line = line_at(&out.lines, link.line_index);
        let click_span = cell_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "click")
            .expect("expected a span containing exactly the link text");
        assert!(
            click_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "link text span inside the cell should carry link_text styling \
             (bold in test_style), got style={:?}",
            click_span.style,
        );
    }

    /// `~~strike~~` inside a table cell must survive the table replace.
    /// Tables rebuild cells from `CellSpan` flags, so strike has to be one of those flags.
    #[test]
    fn strikethrough_inside_table_cell_is_crossed_out() {
        use ratatui::style::Modifier;

        let text = "\
| Item | Status | Link | Code |
|------|--------|------|------|
| ~~old plan~~ leftover | **~~gone~~** keep | ~~[click](https://example.com)~~ | ~~`gone-code`~~ |
";
        let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

        let span_named = |name: &str| {
            out.lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|s| s.content.as_ref() == name)
                .unwrap_or_else(|| panic!("expected a span {name:?}"))
        };

        let old_plan = span_named("old plan");
        assert!(
            old_plan.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "struck cell text should be crossed out, got style={:?}",
            old_plan.style,
        );

        let leftover = span_named(" leftover");
        assert!(
            !leftover.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "unstruck sibling in the same cell must not inherit strike, got style={:?}",
            leftover.style,
        );

        let gone = span_named("gone");
        assert!(
            gone.style.add_modifier.contains(Modifier::CROSSED_OUT)
                && gone.style.add_modifier.contains(Modifier::BOLD),
            "struck bold cell text should keep both effects, got style={:?}",
            gone.style,
        );
        let keep = span_named(" keep");
        assert!(
            !keep.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "text after a struck bold run must not stay struck, got style={:?}",
            keep.style,
        );

        let click = span_named("click");
        assert!(
            click.style.add_modifier.contains(Modifier::CROSSED_OUT)
                && click.style.add_modifier.contains(Modifier::BOLD),
            "struck link text in a cell should keep strike and link_text styling, got style={:?}",
            click.style,
        );

        let gone_code = span_named("gone-code");
        assert!(
            gone_code.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "struck inline code in a cell should stay crossed out after the code style replace, got style={:?}",
            gone_code.style,
        );
    }

    /// Paragraph links must keep the `link_text` foreground color even when the `text` style sets its own foreground.
    /// Previously the parser pushed `ms.text` as a highlight after the link_text highlight.
    /// It did so whenever no `Heading`/`Emphasis`/`Strong`/`Strikethrough` ancestor was present.
    #[test]
    fn paragraph_link_keeps_link_text_fg_over_default_text_fg() {
        use crate::MarkdownStyle;
        use anstyle::{AnsiColor, Color, Style as AStyle};

        let style = MarkdownStyle {
            text: AStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
            link_text: AStyle::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Blue)))
                .underline(),
            ..test_style::STYLE
        };

        let text = "Hello [click](https://x.com) world.\n";
        let (out, _) = render_markdown_ratatui_full(text, style, true, None);

        let line = line_at(&out.lines, 0);
        let click_span = line
            .spans
            .iter()
            .find(|s| s.content == "click")
            .expect("expected a span containing the link text");
        assert_eq!(
            click_span.style.fg,
            Some(ratatui::style::Color::Blue),
            "link text fg must be link_text's blue, not ms.text's red; got {:?}",
            click_span.style,
        );
        assert!(
            click_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "link text must remain underlined; got {:?}",
            click_span.style,
        );
    }

    /// Links wrapped in inline formatting must keep the `link_text` foreground while still gaining the formatting effect.
    /// Regression: inline-format ancestors contribute effects only inside a link.
    #[test]
    fn formatted_link_keeps_link_fg_and_gains_effects() {
        use crate::MarkdownStyle;
        use anstyle::{AnsiColor, Color, Style as AStyle};
        use ratatui::style::Modifier;

        let style = MarkdownStyle {
            text: AStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
            strong_inner: AStyle::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Red)))
                .bold(),
            emphasis_inner: AStyle::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Red)))
                .italic(),
            strikethrough_inner: AStyle::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Red)))
                .strikethrough(),
            link_text: AStyle::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Blue)))
                .underline(),
            ..test_style::STYLE
        };

        for (md, effect) in [
            ("**[click](https://x.com)** end.\n", Modifier::BOLD),
            ("[**click**](https://x.com) end.\n", Modifier::BOLD),
            ("*[click](https://x.com)* end.\n", Modifier::ITALIC),
            ("~~[click](https://x.com)~~ end.\n", Modifier::CROSSED_OUT),
        ] {
            let (out, _) = render_markdown_ratatui_full(md, style, true, None);
            let line = line_at(&out.lines, 0);
            let click_span = line
                .spans
                .iter()
                .find(|s| s.content == "click")
                .unwrap_or_else(|| panic!("expected a span for the link text in {md:?}"));
            assert_eq!(
                click_span.style.fg,
                Some(ratatui::style::Color::Blue),
                "in {md:?} the link text fg must stay link_text's blue, \
                 not strong/emphasis_inner's red; got {:?}",
                click_span.style,
            );
            assert!(
                click_span.style.add_modifier.contains(effect),
                "in {md:?} the link text must gain {effect:?}; got {:?}",
                click_span.style,
            );
            assert!(
                click_span.style.add_modifier.contains(Modifier::UNDERLINED),
                "in {md:?} the link text must remain underlined; got {:?}",
                click_span.style,
            );
        }
    }

    // Soft break inside link text: the link stays on one rendered line and the fragments sharing this link's id cover exactly "link text"
    // SoftBreak splits a link into multiple HyperlinkTargets with the same id (OSC 8 wrapped-link grouping), so we check the union
    #[test]
    fn soft_break_inside_link_text_preserves_column_range() {
        let md = "foo [link\ntext](https://example.com) bar";
        let (out, _) = render_markdown_ratatui_full(md, test_style::STYLE, true, None);

        assert_eq!(out.lines.len(), 1, "{:?}", out.lines);

        // url_scan also emits a target over the `(url)` suffix; filter to the parser id.
        let parser_id = out
            .hyperlinks
            .iter()
            .find(|h| h.url == "https://example.com")
            .expect("at least one hyperlink target")
            .id;
        let mut fragments: Vec<&HyperlinkTarget> = out
            .hyperlinks
            .iter()
            .filter(|h| h.id == parser_id && h.url == "https://example.com")
            .collect();
        fragments.sort_by_key(|h| h.column_range.start);

        for f in &fragments {
            assert_eq!(f.line_index, 0, "{f:?}");
        }
        for w in fragments.windows(2) {
            let [a, b] = w else {
                panic!("expected two items: {w:?}");
            };
            assert_eq!(
                a.column_range.end, b.column_range.start,
                "gap: {:?} -> {:?}",
                a.column_range, b.column_range,
            );
        }

        let union_start = fragments.first().unwrap().column_range.start;
        let union_end = fragments.last().unwrap().column_range.end;
        let rendered = line_to_string(line_at(&out.lines, 0));
        assert_eq!(
            slice_by_cells(&rendered, union_start..union_end),
            "link text",
            "rendered={rendered:?} union={union_start}..{union_end}",
        );
        assert_eq!(
            union_end - union_start,
            crate::buffers::unicode_display_width("link text"),
        );
    }
}
