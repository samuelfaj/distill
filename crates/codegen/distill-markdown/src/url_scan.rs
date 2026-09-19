//! Plain-URL detection over rendered display ratatui Lines.

use std::ops::Range;

use linkify::{LinkFinder, LinkKind};
use ratatui::text::Line;

use crate::buffers::unicode_display_width;
use crate::output::HyperlinkTarget;

/// Scan `lines` for plain URLs and return new `HyperlinkTarget` entries that don't overlap any existing target in `existing`.
///
/// `next_id` is the first id to assign; the returned `u32` is the post-scan counter, suitable for stuffing back into `FrozenState::next_link_id`.
pub(crate) fn detect_plain_urls(
    lines: &[Line<'_>],
    existing: &[HyperlinkTarget],
    next_id: u32,
) -> (Vec<HyperlinkTarget>, u32) {
    detect_plain_urls_with_offset(lines, 0, existing, next_id)
}

/// Like [`detect_plain_urls`] but scans `lines` whose first element represents document line `line_index_offset`.
/// Lines fully inside `0..line_index_offset` are assumed to be in `existing` already and are not re-scanned.
/// The dedup overlap check still works because emitted targets use document-absolute `line_index = line_index_offset + i`.
pub(crate) fn detect_plain_urls_with_offset(
    lines: &[Line<'_>],
    line_index_offset: usize,
    existing: &[HyperlinkTarget],
    next_id: u32,
) -> (Vec<HyperlinkTarget>, u32) {
    let mut result = Vec::new();
    let mut current_id = next_id;

    for (i, line) in lines.iter().enumerate() {
        let line_index = line_index_offset + i;
        // Scan the joined line so a URL split across style spans
        // (pretty-mode link coloring) is one target, not a truncated prefix.
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        for_each_plain_link(&line_text, |range, url| {
            let col_start = unicode_display_width(line_text.get(..range.start).unwrap_or(""));
            let col_end =
                col_start + unicode_display_width(line_text.get(range.clone()).unwrap_or(""));

            // Dedup: skip if any existing or already-added target overlaps on the same line
            let overlaps = existing.iter().chain(result.iter()).any(|h| {
                h.line_index == line_index
                    && col_start < h.column_range.end
                    && h.column_range.start < col_end
            });

            if !overlaps {
                result.push(HyperlinkTarget {
                    line_index,
                    column_range: col_start..col_end,
                    url,
                    id: current_id,
                });
                current_id += 1;
            }
        });
    }

    (result, current_id)
}

/// Call `f` with the byte range and destination of every plain URL or email
/// in `text`. Emails become `mailto:`; scp remotes (`git@host:path`) are skipped.
pub(crate) fn for_each_plain_link(text: &str, mut f: impl FnMut(Range<usize>, String)) {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url, LinkKind::Email]);

    for link in finder.links(text) {
        let start = link.start();
        let end = link.end();
        if start > end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        let url = match link.kind() {
            LinkKind::Email => {
                if matches!(text.as_bytes().get(end), Some(b':' | b'/')) {
                    continue;
                }
                format!("mailto:{}", link.as_str())
            }
            _ => link.as_str().to_string(),
        };
        f(start..end, url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamingMarkdownRenderer;
    use crate::style::test_style;

    /// Render markdown via StreamingMarkdownRenderer::finish() and return the hyperlinks from the finalized output.
    fn finish_and_get_hyperlinks(text: &str) -> Vec<HyperlinkTarget> {
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(text, None);
        let view = renderer.finish(None);
        view.hyperlinks.to_vec()
    }

    fn line_to_string(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plain_url_in_prose_produces_target() {
        let text = "See https://example.com for details.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert_eq!(hyperlinks.len(), 1, "exactly one hyperlink expected");
        let Some(h) = hyperlinks.first() else {
            panic!("expected a hyperlink: {hyperlinks:?}");
        };
        assert_eq!(h.url, "https://example.com");

        // Verify column range covers only the URL
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(text, None);
        let view = renderer.finish(None);
        let Some(line) = view.lines.get(h.line_index) else {
            panic!("missing line {}", h.line_index);
        };
        let rendered = line_to_string(line);
        let slice: String = rendered
            .chars()
            .skip(h.column_range.start)
            .take(h.column_range.len())
            .collect();
        assert_eq!(slice, "https://example.com");
    }

    #[test]
    fn multiple_urls_one_line_distinct_ids() {
        let text = "See https://a.example and https://b.example.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert_eq!(hyperlinks.len(), 2, "two hyperlinks expected");
        let [a, b] = hyperlinks.as_slice() else {
            panic!("expected two hyperlinks: {hyperlinks:?}");
        };
        assert_ne!(a.id, b.id, "ids must differ");
        assert_eq!(a.url, "https://a.example");
        assert_eq!(b.url, "https://b.example");
        assert!(
            a.column_range.end <= b.column_range.start,
            "column ranges must be disjoint, got {:?} vs {:?}",
            a.column_range,
            b.column_range,
        );
    }

    #[test]
    fn markdown_link_with_url_text_does_not_double_link() {
        let text = "Visit [https://example.com](https://example.com).\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        // Pretty mode renders `[url](url)` as `url (url)`, so the URL shows at two disjoint column ranges
        // The parser's HyperlinkTarget covers the link text; the plain-URL scan adds a second target for the `(url)` suffix
        // Dedup only prevents a third entry at the same column range as the parser's target
        assert_eq!(
            hyperlinks.len(),
            2,
            "expected 2 hyperlinks (parser link text + URL in pretty-mode suffix), got {}",
            hyperlinks.len()
        );
        assert!(hyperlinks.iter().all(|h| h.url == "https://example.com"));
        let [a, b] = hyperlinks.as_slice() else {
            panic!("expected two hyperlinks: {hyperlinks:?}");
        };
        assert!(
            a.column_range.end <= b.column_range.start
                || b.column_range.end <= a.column_range.start,
            "column ranges must be disjoint, got {:?} and {:?}",
            a.column_range,
            b.column_range,
        );
    }

    #[test]
    fn autolink_does_not_double_link() {
        let text = "Visit <https://example.com>.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        // All entries should share the same URL; autolinks may produce multiple HyperlinkTarget fragments sharing the same id
        let autolink_count = hyperlinks
            .iter()
            .filter(|h| h.url == "https://example.com")
            .count();
        assert!(autolink_count >= 1, "expected at least one autolink target");
        assert_eq!(
            hyperlinks.len(),
            autolink_count,
            "plain-URL scan should not add duplicates on top of autolink targets"
        );
    }

    #[test]
    fn plain_email_in_prose_produces_mailto_target() {
        let text = "Email foo@bar.com please.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert_eq!(hyperlinks.len(), 1, "exactly one hyperlink expected");
        let Some(h) = hyperlinks.first() else {
            panic!("expected a hyperlink: {hyperlinks:?}");
        };
        assert_eq!(h.url, "mailto:foo@bar.com");

        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(text, None);
        let view = renderer.finish(None);
        let Some(line) = view.lines.get(h.line_index) else {
            panic!("missing line {}", h.line_index);
        };
        let rendered = line_to_string(line);
        let slice: String = rendered
            .chars()
            .skip(h.column_range.start)
            .take(h.column_range.len())
            .collect();
        assert_eq!(slice, "foo@bar.com");
    }

    #[test]
    fn email_after_multibyte_prefix_is_mailto() {
        let hyperlinks = finish_and_get_hyperlinks("連絡先: foo@bar.com です\n");
        assert_eq!(hyperlinks.len(), 1);
        assert_eq!(
            hyperlinks.first().map(|h| h.url.as_str()),
            Some("mailto:foo@bar.com")
        );
    }

    #[test]
    fn scp_git_remote_is_not_mailto() {
        let hyperlinks = finish_and_get_hyperlinks("clone git@github.com:org/repo.git\n");
        assert!(
            hyperlinks.iter().all(|h| !h.url.starts_with("mailto:")),
            "scp-style git remotes must not become mailto links: {hyperlinks:?}"
        );
    }

    #[test]
    fn trailing_period_excluded_from_url() {
        let text = "See https://example.com.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert_eq!(hyperlinks.len(), 1);
        assert_eq!(
            hyperlinks.first().map(|h| h.url.as_str()),
            Some("https://example.com"),
            "trailing dot should be excluded by linkify"
        );
    }

    #[test]
    fn cjk_neighbors_preserve_correct_columns() {
        use crate::buffers::unicode_display_width;

        let text = "日本語 https://example.com 日本語\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert_eq!(hyperlinks.len(), 1);
        let Some(h) = hyperlinks.first() else {
            panic!("expected a hyperlink: {hyperlinks:?}");
        };
        assert_eq!(h.url, "https://example.com");

        // "日本語 " has 3 CJK chars (2 cells each) plus 1 space, 7 display cells
        let prefix = "日本語 ";
        let expected_start = unicode_display_width(prefix);
        assert_eq!(expected_start, 7, "prefix should be 7 display cells");
        assert_eq!(h.column_range.start, expected_start);

        let url_width = unicode_display_width("https://example.com");
        assert_eq!(h.column_range.end, expected_start + url_width);
    }

    #[test]
    fn url_split_across_style_spans_is_one_target() {
        use crate::buffers::unicode_display_width;
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;

        let line = Line::from(vec![
            Span::styled(
                "https://tracker.example.com/",
                Style::default().fg(Color::Blue),
            ),
            Span::styled("projects/issues/#12345", Style::default().fg(Color::Blue)),
        ]);
        let (found, _) = detect_plain_urls(&[line], &[], 0);
        assert_eq!(found.len(), 1, "style boundary must not split the URL");
        assert_eq!(
            found.first().map(|h| h.url.as_str()),
            Some("https://tracker.example.com/projects/issues/#12345")
        );
        assert_eq!(found.first().map(|h| h.column_range.start), Some(0));
        assert_eq!(
            found.first().map(|h| h.column_range.end),
            Some(unicode_display_width(
                "https://tracker.example.com/projects/issues/#12345"
            ))
        );
    }

    #[test]
    fn empty_document_returns_empty() {
        let hyperlinks = finish_and_get_hyperlinks("");
        assert!(
            hyperlinks.is_empty(),
            "empty document should produce no hyperlinks"
        );
    }

    /// Pins the current behavior for a URL inside inline code: linkify matches inside the code-styled span and produces a HyperlinkTarget.
    /// If we later skip code-styled spans, this test fails and forces the change to be intentional.
    #[test]
    fn url_inside_inline_code_documented_behavior() {
        let text = "Use `https://example.com` carefully.\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        assert!(
            !hyperlinks.is_empty(),
            "behavior pin: URL inside inline code currently produces a HyperlinkTarget"
        );
        assert_eq!(
            hyperlinks.first().map(|h| h.url.as_str()),
            Some("https://example.com")
        );
    }

    /// Pins the current behavior for a URL inside a fenced code block; same rationale as `url_inside_inline_code_documented_behavior`.
    #[test]
    fn url_inside_code_fence_documented_behavior() {
        let text = "```\nsee https://example.com\n```\n";
        let hyperlinks = finish_and_get_hyperlinks(text);

        let has_url = hyperlinks.iter().any(|h| h.url == "https://example.com");
        assert!(
            has_url,
            "behavior pin: URL inside fenced code block currently produces a HyperlinkTarget"
        );
    }

    /// URL detection must run from `render()` too, not only `finish()`.
    /// Otherwise a state reset like `set_max_table_width` drops the URL hyperlinks pretty mode adds for the `(url)` suffix of markdown links.
    /// Also pins the OSC 8 grouping invariant: the link-text and URL hyperlinks must have distinct ids and disjoint column ranges.
    #[test]
    fn render_detects_pretty_mode_url_suffix() {
        let text = "[link](https://example.com/some/long/path)\n";
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(text, None);
        // No finish() call: render() alone must produce both the parser (link text) and url_scan (`(url)` suffix) hyperlinks
        let view = renderer.view();
        assert_eq!(
            view.hyperlinks.len(),
            2,
            "render() must produce both the link-text and URL-suffix hyperlinks; \
             got {:?}",
            view.hyperlinks,
        );
        assert!(
            view.hyperlinks
                .iter()
                .all(|h| h.url == "https://example.com/some/long/path")
        );
        let [a, b] = view.hyperlinks else {
            panic!("expected two hyperlinks: {:?}", view.hyperlinks);
        };
        assert_ne!(
            a.id, b.id,
            "link-text and URL-suffix hyperlinks must have distinct OSC 8 ids",
        );
        assert!(
            a.column_range.end <= b.column_range.start
                || b.column_range.end <= a.column_range.start,
            "column ranges must be disjoint, got {:?} and {:?}",
            a.column_range,
            b.column_range,
        );
    }

    /// Snapshot helper used by survival tests below.
    fn snapshot(view: &crate::output::MarkdownRenderView<'_>) -> Vec<HyperlinkTarget> {
        let mut snap: Vec<HyperlinkTarget> = view.hyperlinks.to_vec();
        snap.sort_by_key(|h| (h.line_index, h.column_range.start));
        snap
    }

    fn assert_url_suffix_preserved(
        before: &[HyperlinkTarget],
        after: &[HyperlinkTarget],
        url: &str,
    ) {
        let before_suffix = before
            .iter()
            .find(|h| h.url == url && h.column_range.start > 5)
            .expect("URL-suffix hyperlink must be present BEFORE reset");
        let after_suffix = after
            .iter()
            .find(|h| h.url == url && h.column_range.start > 5)
            .expect("URL-suffix hyperlink must be present AFTER reset");
        assert_eq!(
            before_suffix.column_range, after_suffix.column_range,
            "URL-suffix column range must be stable across the reset",
        );
        assert_eq!(
            before_suffix.line_index, after_suffix.line_index,
            "URL-suffix line index must be stable across the reset",
        );
    }

    /// Re-rendering after `finish()` must not drop the URL hyperlinks pretty mode adds for the `(url)` suffix.
    /// Snapshots the hyperlink list before and after the reset and asserts the URL-suffix entry keeps its column range.
    /// The post-reset re-render may re-assign the OSC 8 id; the location must not move.
    #[test]
    fn url_hyperlinks_survive_re_render_after_finish() {
        let url = "https://example.com/some/long/path";
        let text = format!("[link]({url})\n");
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(&text, None);
        renderer.finish(None);
        let before = snapshot(&renderer.view());

        // Simulate a width change which resets renderer state.
        renderer.set_max_table_width(Some(40));
        renderer.render(None);
        let after = snapshot(&renderer.view());

        assert_eq!(
            before.len(),
            after.len(),
            "hyperlink count must be stable across the reset; before={before:?} after={after:?}",
        );
        assert_url_suffix_preserved(&before, &after, url);
    }

    /// Same contract as `url_hyperlinks_survive_re_render_after_finish`, but through the `set_pretty` reset path.
    /// In production that path is the `MarkdownContent::set_raw_mode` toggle.
    #[test]
    fn url_hyperlinks_survive_re_render_after_set_pretty_toggle() {
        let url = "https://example.com/some/long/path";
        let text = format!("[link]({url})\n");
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(&text, None);
        renderer.finish(None);
        let before = snapshot(&renderer.view());

        // Toggle pretty off then back on; both transitions reset state
        renderer.set_pretty(false);
        renderer.set_pretty(true);
        renderer.render(None);
        let after = snapshot(&renderer.view());

        assert_eq!(
            before.len(),
            after.len(),
            "hyperlink count must be stable across the set_pretty toggle",
        );
        assert_url_suffix_preserved(&before, &after, url);
    }

    /// Same contract as `url_hyperlinks_survive_re_render_after_finish`, but through the `set_style` reset path.
    /// In production that path is a theme change via `MarkdownContent::ensure_wrapped` when the theme cache kind shifts.
    #[test]
    fn url_hyperlinks_survive_re_render_after_set_style() {
        let url = "https://example.com/some/long/path";
        let text = format!("[link]({url})\n");
        let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
        renderer.push_and_render(&text, None);
        renderer.finish(None);
        let before = snapshot(&renderer.view());

        // `set_style` unconditionally resets state, even with same style.
        renderer.set_style(test_style::STYLE);
        renderer.render(None);
        let after = snapshot(&renderer.view());

        assert_eq!(
            before.len(),
            after.len(),
            "hyperlink count must be stable across the set_style reset",
        );
        assert_url_suffix_preserved(&before, &after, url);
    }
}
