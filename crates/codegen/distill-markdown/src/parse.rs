// Modified for Distill by Samuel Fajreldines, 2026.
//! Markdown parser: transforms markdown text into styled highlight ranges.
//!
//! The parser processes markdown events and populates buffers with:
//! - Highlights: Style ranges for inline formatting
//! - Replaces: Syntax-highlighted code blocks
//! - Transforms: Character substitutions (bullets, etc.)
//! - Table replaces: Formatted table content

use std::ops::Range;

use anstyle::Style;
use pulldown_cmark::{CodeBlockKind, CowStr, Event, Tag, TagEnd, TextMergeWithOffset};
use ratatui::style::Stylize as RatatuiStylize;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::buffers::{
    CellJoin, CodeBlockMeta, Highlight, LinkTarget, MarkdownBuffers, Replace, StyledCell,
    TableCellCopy, TableHyperlink, TableReplace, TableState, Transform, floor_char_boundary,
    unicode_display_width,
};
use crate::checkpoint::CheckpointKind;
use crate::latex;
use crate::open_code_highlighter::OpenCodeHighlighter;
use crate::style::{MarkdownStyle, TableBorders};
use crate::syntax::{Syntect, syntax_highlight_raw};
use crate::url_scan;

/// Trait for converting anstyle to ratatui style.
trait StyleInto<T> {
    fn style_into(self) -> T;
}

impl StyleInto<ratatui::style::Style> for Style {
    fn style_into(self) -> ratatui::style::Style {
        use ratatui::style::{Modifier, Style as RStyle};

        let mut style = RStyle::default();

        if let Some(fg) = self.get_fg_color() {
            style = style.fg(anstyle_to_ratatui_color(fg));
        }
        if let Some(bg) = self.get_bg_color() {
            style = style.bg(anstyle_to_ratatui_color(bg));
        }

        let effects = self.get_effects();
        let mut modifiers = Modifier::empty();
        if effects.contains(anstyle::Effects::BOLD) {
            modifiers |= Modifier::BOLD;
        }
        if effects.contains(anstyle::Effects::DIMMED) {
            modifiers |= Modifier::DIM;
        }
        if effects.contains(anstyle::Effects::ITALIC) {
            modifiers |= Modifier::ITALIC;
        }
        if effects.contains(anstyle::Effects::UNDERLINE) {
            modifiers |= Modifier::UNDERLINED;
        }
        if effects.contains(anstyle::Effects::STRIKETHROUGH) {
            modifiers |= Modifier::CROSSED_OUT;
        }
        if effects.contains(anstyle::Effects::HIDDEN) {
            modifiers |= Modifier::HIDDEN;
        }

        style.add_modifier(modifiers)
    }
}

fn anstyle_to_ratatui_color(color: anstyle::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        anstyle::Color::Ansi(ansi) => match ansi {
            anstyle::AnsiColor::Black => Color::Black,
            anstyle::AnsiColor::Red => Color::Red,
            anstyle::AnsiColor::Green => Color::Green,
            anstyle::AnsiColor::Yellow => Color::Yellow,
            anstyle::AnsiColor::Blue => Color::Blue,
            anstyle::AnsiColor::Magenta => Color::Magenta,
            anstyle::AnsiColor::Cyan => Color::Cyan,
            anstyle::AnsiColor::White => Color::Gray,
            anstyle::AnsiColor::BrightBlack => Color::DarkGray,
            anstyle::AnsiColor::BrightRed => Color::LightRed,
            anstyle::AnsiColor::BrightGreen => Color::LightGreen,
            anstyle::AnsiColor::BrightYellow => Color::LightYellow,
            anstyle::AnsiColor::BrightBlue => Color::LightBlue,
            anstyle::AnsiColor::BrightMagenta => Color::LightMagenta,
            anstyle::AnsiColor::BrightCyan => Color::LightCyan,
            anstyle::AnsiColor::BrightWhite => Color::White,
        },
        anstyle::Color::Ansi256(idx) => Color::Indexed(idx.index()),
        anstyle::Color::Rgb(rgb) => Color::Rgb(rgb.0, rgb.1, rgb.2),
    }
}

/// Find a substring within a haystack, optionally searching outside or using rfind.
fn find_substring(
    haystack: &str,
    needle: &CowStr,
    allow_outside: bool,
    rfind: bool,
) -> Option<Range<usize>> {
    if needle.is_empty() || haystack.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    if !allow_outside {
        if let CowStr::Borrowed(needle) = needle {
            let (hp, np) = (haystack.as_ptr(), needle.as_ptr());
            unsafe {
                let (he, ne) = (hp.add(haystack.len()), np.add(needle.len()));
                if np >= hp && ne <= he {
                    let offset = np.offset_from(hp) as usize;
                    let range = offset..(offset + needle.len());
                    if cfg!(debug_assertions) {
                        assert_eq!(
                            haystack.as_bytes().get(range.clone()),
                            Some(needle.as_bytes())
                        );
                    }
                    return Some(range);
                }
            }
        }
        None
    } else {
        if rfind {
            haystack.rfind(needle.as_ref())
        } else {
            haystack.find(needle.as_ref())
        }
        .map(|pos| pos..(pos + needle.len()))
    }
}

/// Decode a single HTML character entity reference (`entity` includes the leading `&` and trailing `;`) into its replacement string.
/// Substituting raw control bytes would let untrusted markdown inject terminal escape sequences, so the raw source is left literal instead.
fn decode_html_entity(entity: &str) -> Option<String> {
    let decoded = html_escape::decode_html_entities(entity);
    // Unchanged output means `html_escape` did not recognize the reference.
    if decoded.as_ref() == entity {
        return None;
    }
    if decoded.chars().any(char::is_control) {
        return None;
    }
    Some(decoded.into_owned())
}

/// Check if there's a blank line after the given position.
fn has_blank_line_after(text: &str, pos: usize) -> bool {
    text.as_bytes()
        .get(pos..)
        .and_then(|s| s.iter().copied().find(|&c| c != b' ' && c != b'\t'))
        == Some(b'\n')
}

/// Transient state for the fenced code block currently being parsed.
/// Fenced blocks never nest (an inner fence closes the outer), so a single `Option` suffices.
/// Finalized in the `TagEnd::CodeBlock` arm, where the body range and the block range together decide whether the fence was closed.
struct PendingCodeBlock {
    info: String,
    /// Body byte range in the raw source.
    /// Initialized to an empty range just past the opening fence line, then widened to the merged body text range as text events arrive.
    /// `body_seen` distinguishes the empty-body case.
    body_range: Range<usize>,
    /// De-prefixed body content, the clean diagram/code source.
    /// pulldown's merged text gives the logical code with container markers (blockquote `>`, list indent) stripped and CRLF normalized to `\n`.
    body_text: String,
    body_seen: bool,
}

/// Markdown parser that processes events and populates buffers.
///
/// After calling `parse()`, the transient state (tag_stack, table_state, depth) is dropped and a `ParsedMarkdown` is returned for rendering.
pub struct MarkdownParser<'a, 'b, 'syn, 'oc> {
    text: &'a str,
    ms: MarkdownStyle,
    buffers: &'b mut MarkdownBuffers,
    syntect: Option<&'syn Syntect>,
    /// Incremental highlighter for the trailing still-open fenced code block.
    /// Only set by the streaming tail re-render.
    /// For batch renders it is `None` and code blocks go through the from-scratch [`syntax_highlight_raw`].
    open_code: Option<&'oc mut OpenCodeHighlighter>,
    // Transient state (dropped after parse)
    tag_stack: Vec<Tag<'a>>,
    table_state: Option<TableState>,
    depth: usize,
    /// Current blockquote nesting depth (0 = not in any blockquote).
    /// Used to determine which `>` on a line belongs to the current level.
    bq_depth: usize,
    last_checkpoint: Option<(CheckpointKind, usize)>,
    /// Maximum width for rendered tables (in display columns).
    /// When `Some(w)`, column widths are shrunk proportionally so the table fits within `w` columns.
    /// When `None`, columns use natural widths.
    max_table_width: Option<usize>,
    /// Monotonically increasing counter for assigning stable link IDs.
    /// Persisted across `rerender_tail` calls via the streaming renderer.
    link_id_counter: u32,
    /// When `true` (default), CommonMark soft breaks inside a paragraph collapse to a single space.
    /// When `false`, the source newline is preserved so each source line keeps its own visual line.
    /// The line-numbered plan preview needs that, since its rendered lines must map 1:1 to file lines.
    collapse_soft_breaks: bool,
    /// In-progress fenced code block, set between its start and end events.
    pending_code_block: Option<PendingCodeBlock>,
}

/// Custom word separator for table cells.
/// Only `,` and `.` between digits suppress the break; these are number formatting.
fn is_br_tag(html: &str) -> bool {
    let Some(inner) = html
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
    else {
        return false;
    };
    let tag = inner.trim();
    let tag = tag.strip_suffix('/').map_or(tag, str::trim);
    tag.eq_ignore_ascii_case("br")
}

/// URLs (validated via `url::Url::parse`) are treated as unbreakable words.
/// No break points are placed within a URL, so terminal hyperlink detection (Cmd+Click) keeps working when cells wrap.
pub(crate) fn cell_word_separator<'a>(
    line: &'a str,
) -> Box<dyn Iterator<Item = textwrap::core::Word<'a>> + 'a> {
    // Pass 1: find break-point byte positions.
    // A break point sits between a punctuation/symbol char and the alphabetic char that follows it
    // We record (break_byte_idx, punct_byte_start) break_byte_idx is where the next word would start if the punct char attaches to the left punct_byte_start is where the punct char begins, for attaching it to the right instead
    let mut breaks: Vec<(usize, usize)> = Vec::new();
    {
        let mut in_whitespace = false;
        let mut after_break_char = false;
        let mut prev_is_digit = false; // was the *previous* char a digit?
        let mut digit_before_break = false; // was the char before the break char a digit?
        let mut last_break_ch: char = '\0';
        let mut break_char_start: usize = 0;
        for (idx, ch) in line.char_indices() {
            let is_space = ch == ' ';
            let is_break_char = !is_space && !ch.is_alphanumeric();

            // After a break char, decide if we should split here.
            // Two cases allow a break: a) Followed by a letter: always break (new word boundary) b) digit-punct-digit: break, unless the punct is `,` or `.` (number formatting like `$145,000` or `3.14`)
            // `foo/bar` breaks (letter after punct); `555-0101` breaks (digit-hyphen-digit); `$145,000` stays (digit-comma-digit); `$145` stays (no digit before `$`); `EMP-1001` breaks at hyphen (letter before it).
            let should_break = if in_whitespace && !is_space {
                true
            } else if after_break_char {
                if ch.is_alphabetic() {
                    true
                } else if ch.is_ascii_digit() && digit_before_break {
                    // digit-punct-digit: only break for non-formatting punct
                    last_break_ch != ',' && last_break_ch != '.'
                } else {
                    false
                }
            } else {
                false
            };

            if should_break {
                if in_whitespace {
                    breaks.push((idx, idx));
                } else {
                    breaks.push((idx, break_char_start));
                }
            }

            if is_break_char {
                break_char_start = idx;
                last_break_ch = ch;
                digit_before_break = prev_is_digit;
            }
            prev_is_digit = ch.is_ascii_digit();
            in_whitespace = is_space;
            after_break_char = is_break_char;
        }
    }

    // Filter out break points that fall inside a URL.
    // Each whitespace-delimited token is tested with `url::Url::parse`; tokens that parse as valid URLs are protected from splitting
    let url_ranges: Vec<Range<usize>> = {
        let mut ranges = Vec::new();
        let mut pos = 0;
        for token in line.split_whitespace() {
            let Some(rel) = line.get(pos..).and_then(|rest| rest.find(token)) else {
                break;
            };
            let start = rel + pos;
            let end = start + token.len();
            if url::Url::parse(token).is_ok() {
                ranges.push(start..end);
            }
            pos = end;
        }
        ranges
    };
    breaks.retain(|&(break_pos, _)| {
        !url_ranges
            .iter()
            .any(|r| break_pos > r.start && break_pos < r.end)
    });

    // Pass 2: decide attachment for each break point.
    // For punct breaks, choose the side that minimizes max(left_len, right_len).
    let mut split_positions: Vec<usize> = Vec::with_capacity(breaks.len());
    {
        let len = line.len();
        for (i, &(attach_left, attach_right)) in breaks.iter().enumerate() {
            if attach_left == attach_right {
                // Whitespace break, no choice
                split_positions.push(attach_left);
            } else {
                // Determine segment boundaries for this break.
                let seg_start = if i == 0 {
                    0
                } else {
                    match i
                        .checked_sub(1)
                        .and_then(|j| split_positions.get(j))
                        .copied()
                    {
                        Some(s) => s,
                        None => continue,
                    }
                };
                let seg_end = if i + 1 < breaks.len() {
                    // Use the leftward attachment of the next break as a conservative estimate of the right segment end
                    match breaks.get(i + 1) {
                        Some(&(end, _)) => end,
                        None => len,
                    }
                } else {
                    len
                };

                let Some(left_if_attach_left) =
                    line.get(seg_start..attach_left).map(unicode_display_width)
                else {
                    continue;
                };
                let Some(right_if_attach_left) =
                    line.get(attach_left..seg_end).map(unicode_display_width)
                else {
                    continue;
                };
                let max_attach_left = left_if_attach_left.max(right_if_attach_left);

                let Some(left_if_attach_right) =
                    line.get(seg_start..attach_right).map(unicode_display_width)
                else {
                    continue;
                };
                let Some(right_if_attach_right) =
                    line.get(attach_right..seg_end).map(unicode_display_width)
                else {
                    continue;
                };
                let max_attach_right = left_if_attach_right.max(right_if_attach_right);

                if max_attach_right < max_attach_left {
                    split_positions.push(attach_right);
                } else {
                    split_positions.push(attach_left);
                }
            }
        }
    }

    // Pass 3: emit Words at the chosen split positions.
    let mut pos = 0usize;
    let mut idx = 0usize;
    Box::new(std::iter::from_fn(move || {
        if pos >= line.len() {
            return None;
        }
        let end = if idx < split_positions.len() {
            let e = *split_positions.get(idx)?;
            idx += 1;
            e
        } else {
            line.len()
        };
        let slice = line.get(pos..end)?;
        let word = textwrap::core::Word::from(slice);
        pos = end;
        Some(word)
    }))
}

fn cell_wrap_joins(source: &str, lines: &[String]) -> Vec<CellJoin> {
    let want = lines.len().saturating_sub(1);
    if want == 0 {
        return Vec::new();
    }
    let mut joins = Vec::with_capacity(want);
    let mut cursor = 0usize;
    let mut prev_end: Option<usize> = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if prev_end.is_some() {
                joins.push(CellJoin::Tight);
            }
            continue;
        }
        let Some(rest) = source.get(cursor..) else {
            if prev_end.is_some() {
                joins.push(CellJoin::Tight);
            }
            continue;
        };
        let Some(off) = rest.find(trimmed) else {
            if prev_end.is_some() {
                joins.push(CellJoin::Tight);
            }
            continue;
        };
        let start = cursor + off;
        let end = start + trimmed.len();
        if let Some(prev) = prev_end {
            if prev <= start
                && start <= source.len()
                && source.is_char_boundary(prev)
                && source.is_char_boundary(start)
            {
                if start > prev {
                    match source.get(prev..start) {
                        Some(gap) => joins.push(CellJoin::Gap(gap.to_string())),
                        None => joins.push(CellJoin::Tight),
                    }
                } else {
                    joins.push(CellJoin::Tight);
                }
            } else {
                joins.push(CellJoin::Tight);
            }
        }
        prev_end = Some(end);
        cursor = end;
    }
    joins.resize(want, CellJoin::Tight);
    joins.truncate(want);
    joins
}

/// Output of [`MarkdownParser::format_table`]: the rendered lines of a single table.
#[derive(Default)]
struct FormattedTable {
    /// Plain-text lines (for ANSI rendering).
    lines: Vec<String>,
    /// Styled lines (for ratatui rendering).
    styled_lines: Vec<Line<'static>>,
    /// Per-line source offset within the table (0 = header, 1 = separator, 2+ = body rows).
    line_source_offsets: Vec<usize>,
    /// Hyperlinks (in table-local line coordinates) for links inside cells.
    hyperlinks: Vec<TableHyperlink>,
    cell_copies: Vec<TableCellCopy>,
    n_cols: usize,
}

impl<'a, 'b, 'syn, 'oc> MarkdownParser<'a, 'b, 'syn, 'oc> {
    pub fn new(
        text: &'a str,
        ms: MarkdownStyle,
        buffers: &'b mut MarkdownBuffers,
        syntect: Option<&'syn Syntect>,
    ) -> Self {
        Self {
            text,
            ms,
            buffers,
            syntect,
            open_code: None,
            tag_stack: Vec::new(),
            table_state: None,
            depth: 0,
            bq_depth: 0,
            last_checkpoint: None,
            max_table_width: None,
            link_id_counter: 0,
            collapse_soft_breaks: true,
            pending_code_block: None,
        }
    }

    /// Set whether CommonMark soft breaks collapse to a space.
    /// Defaults to `true`.
    /// Set `false` for source-faithful rendering (plan preview) where each source line must keep its own visual line and `line_source_map` entry.
    pub fn collapse_soft_breaks(mut self, collapse: bool) -> Self {
        self.collapse_soft_breaks = collapse;
        self
    }

    /// Set the maximum width for rendered tables.
    ///
    /// When set, column widths are shrunk proportionally so the table fits within the given number of display columns.
    pub fn max_table_width(mut self, width: Option<usize>) -> Self {
        self.max_table_width = width;
        self
    }

    /// Set the starting link ID counter (for streaming renderer continuity).
    /// Internal: only the in-crate streaming renderer needs to manage the link counter across `rerender_tail` calls.
    /// Consumers should use `StreamingMarkdownRenderer` instead of touching the parser directly.
    pub(crate) fn link_id_start(mut self, id: u32) -> Self {
        self.link_id_counter = id;
        self
    }

    /// Provide an incremental highlighter for the trailing still-open fenced code block (streaming tail re-render only).
    /// That keeps an open code block at O(N) total highlighting instead of O(N²).
    /// Batch/non-streaming callers leave this `None`.
    pub(crate) fn open_code(mut self, cache: Option<&'oc mut OpenCodeHighlighter>) -> Self {
        self.open_code = cache;
        self
    }

    /// Parse markdown and return a ParsedMarkdown ready for rendering.
    ///
    /// Consumes self, dropping transient parsing state.
    pub fn parse(mut self) -> ParsedMarkdown<'a, 'b> {
        self.tag_stack.clear();
        self.buffers.clear();
        self.table_state = None;
        self.depth = 0;
        self.last_checkpoint = None;
        self.pending_code_block = None;

        for (event, range) in
            TextMergeWithOffset::new(distill_markdown_core::offset_events(self.text))
        {
            self.on_event(event, range);
        }

        ParsedMarkdown::new(
            self.text,
            self.ms,
            self.buffers,
            self.last_checkpoint,
            self.link_id_counter,
        )
    }

    fn push_highlight(&mut self, style: Option<Style>, range: &Range<usize>) {
        self.buffers.highlights.push(Highlight {
            style,
            range: range.clone(),
        });
    }

    fn on_event(&mut self, event: Event<'a>, range: Range<usize>) {
        let mut parent_code_block = None;

        // Apply ALL ancestors' inner styles to non-marker events.
        let skip_inner_style = matches!(
            event,
            Event::Start(_) | Event::End(_) | Event::Code(_) | Event::InlineMath(_)
        );

        // Collect ancestor styles first (to avoid borrow issues)
        let ancestor_styles: Vec<Option<Style>> = if !skip_inner_style {
            // Inside a link, inline-format ancestors (strong/emphasis/strikethrough) must not recolor the link text
            // Their inner styles carry the theme's default text fg and land *after* the link_text highlight pushed at Tag::Link start merge_styles is last-wins on fg, so keeping the fg would clobber the link color**`).
            // Only the fg competes with link_text today, so effects (and any bg) pass through
            let in_link = self
                .tag_stack
                .iter()
                .any(|t| matches!(t, Tag::Link { .. } | Tag::Image { .. }));
            let strip_fg_in_link =
                |style: Style| if in_link { style.fg_color(None) } else { style };
            self.tag_stack
                .iter()
                .filter_map(|ancestor| match ancestor {
                    Tag::Heading { level, .. } => (*level as i32 as usize)
                        .checked_sub(1)
                        .and_then(|i| self.ms.heading_inner.get(i).copied())
                        .map(Some),
                    Tag::Emphasis => Some(Some(strip_fg_in_link(self.ms.emphasis_inner))),
                    Tag::Strong => Some(Some(strip_fg_in_link(self.ms.strong_inner))),
                    Tag::Strikethrough => Some(Some(strip_fg_in_link(self.ms.strikethrough_inner))),
                    // Link/Image already push their own inner-style highlight (link_text) during on_start
                    // We only need ancestor_styles to be non-empty so the Event::Text branch below skips pushing ms.text
                    // ms.text would otherwise override the link_text foreground color via merge_styles' last-wins ordering
                    Tag::Link { .. } | Tag::Image { .. } => Some(None),
                    Tag::CodeBlock(block) => {
                        parent_code_block = Some(match block {
                            CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                                Some(lang.to_owned())
                            }
                            _ => None,
                        });
                        None
                    }
                    _ => None,
                })
                .collect()
        } else {
            if let Some(Tag::CodeBlock(block)) = self.tag_stack.last() {
                parent_code_block = Some(match block {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_owned()),
                    _ => None,
                });
            }
            Vec::new()
        };

        for style in &ancestor_styles {
            self.push_highlight(*style, &range);
        }

        match event {
            Event::Start(tag) => self.on_start(tag, range),
            Event::End(tag_end) => self.on_end(tag_end, range),
            Event::Text(text) => {
                // Capture text into table cell if we're inside a table
                self.push_table_text_linking_urls(&text);

                // Record the enclosing fenced block's raw byte range and its de-prefixed body content
                // pulldown merges the body into one text event, but accumulate defensively in case it is split
                if parent_code_block.is_some()
                    && let Some(pending) = self.pending_code_block.as_mut()
                {
                    if pending.body_seen {
                        pending.body_range.start = pending.body_range.start.min(range.start);
                        pending.body_range.end = pending.body_range.end.max(range.end);
                    } else {
                        pending.body_range = range.clone();
                        pending.body_seen = true;
                    }
                    pending.body_text.push_str(&text);
                }

                if let Some(parent_code_block) = parent_code_block {
                    // Closed mermaid fences render as a diagram; open ones fall through so the source shows while still streaming
                    if let Some(lang) = parent_code_block.as_deref()
                        && lang
                            .split_whitespace()
                            .next()
                            .is_some_and(|t| t.eq_ignore_ascii_case("mermaid"))
                        && range.end < self.text.len()
                        && self.try_push_mermaid(&text, &range)
                    {
                        return;
                    }
                    let highlighted = match parent_code_block {
                        Some(lang) => {
                            if let Some(syn) = self.syntect
                                && let Some(cache) = self.open_code.as_deref_mut()
                            {
                                // Streaming tail: the cache routes between the incremental open-block path and the closed-fence memo
                                cache.highlight_block(
                                    syn,
                                    &lang,
                                    range.start,
                                    range.end >= self.text.len(),
                                    &text,
                                )
                            } else {
                                // Batch render (no streaming caches attached).
                                syntax_highlight_raw(self.syntect, &lang, &text)
                            }
                        }
                        None => None,
                    };
                    if let Some(highlighted) = highlighted {
                        self.buffers.replaces.push(Replace {
                            highlighted,
                            range: range.clone(),
                        });
                    } else {
                        self.push_highlight(Some(self.ms.code_untagged), &range);
                        self.buffers.untagged_code_ranges.push(range.clone());
                    }
                } else {
                    if ancestor_styles.is_empty() {
                        // Apply the default text style only when no ancestor (heading, strong, emphasis, etc.) already provides a color
                        // Otherwise ms.text would override them
                        self.push_highlight(Some(self.ms.text), &range);
                    } else {
                        self.push_highlight(None, &range);
                    }
                    if self.table_state.is_none() {
                        self.scan_inline_html_entities(&range);
                    }
                }
            }
            Event::Code(code) => {
                // Capture code content into table cell if we're inside a table
                if let Some(ref mut state) = self.table_state {
                    let prev_code = state.cell_code;
                    state.cell_code = true;
                    state.push_text(&code);
                    state.cell_code = prev_code;
                }
                self.style_inline_code_span(&code, &range);
            }
            Event::InlineMath(math) => {
                // `$...$` inline math: render the TeX to Unicode and swap it in via a pretty-mode transform
                // Falls back to inline-code presentation when conversion declines (oversized input) or produces nothing visible
                let rendered = latex::latex_to_unicode_inline(&math).filter(|r| !r.is_empty());

                if let Some(ref mut state) = self.table_state {
                    match &rendered {
                        Some(r) => {
                            let prev_italic = state.cell_italic;
                            state.cell_italic = true;
                            state.push_text(r);
                            state.cell_italic = prev_italic;
                        }
                        None => {
                            let prev_code = state.cell_code;
                            state.cell_code = true;
                            state.push_text(&math);
                            state.cell_code = prev_code;
                        }
                    }
                }

                match rendered {
                    Some(r) => {
                        // One highlight and one transform span the entire `$...$` range
                        // Pretty mode shows the rendered math, raw mode shows the TeX source in the math style
                        self.push_highlight(Some(self.ms.math), &range);
                        self.buffers.transforms.push(Transform {
                            range: range.clone(),
                            to: r,
                            force: false,
                        });
                    }
                    None => self.style_inline_code_span(&math, &range),
                }
            }
            Event::SoftBreak => {
                // Collapse soft breaks to spaces unless the byte after pulldown's SoftBreak range is a list-item indent or blockquote `>` marker
                // In that case the line ending belongs to a block continuation and the renderer shows it as its own visual line
                // The transform spans the full range so CRLF (`\r\n`, 2 bytes) preserves byte length
                if let Some(ref mut state) = self.table_state {
                    state.push_text(" ");
                } else {
                    let next = self.text.as_bytes().get(range.end);
                    let is_continuation = matches!(next, Some(b' ' | b'\t' | b'>' | b'|'));
                    if self.collapse_soft_breaks && !is_continuation {
                        let span = range.end - range.start;
                        debug_assert!(span >= 1, "SoftBreak range must cover at least one byte");
                        self.buffers.transforms.push(Transform {
                            range: range.clone(),
                            to: " ".repeat(span),
                            force: true,
                        });
                    }
                }
                self.push_highlight(None, &range);
            }
            Event::HardBreak => {
                if let Some(ref mut state) = self.table_state {
                    state.push_text("\n");
                }
                self.push_highlight(None, &range);
            }
            Event::Html(_) => {
                // Render HTML block content as regular text (not code).
                // pulldown-cmark treats XML-like tags (e.g. <example>) as HTML blocks, which would otherwise get code-block styling via Replace.
                self.push_highlight(Some(self.ms.text), &range);
            }
            Event::InlineHtml(html) => {
                if is_br_tag(&html) {
                    if let Some(ref mut state) = self.table_state {
                        state.push_text("\n");
                        self.push_highlight(Some(self.ms.text), &range);
                    } else {
                        self.buffers.transforms.push(Transform {
                            range: range.clone(),
                            to: "\n".to_string(),
                            force: false,
                        });
                        self.push_highlight(None, &range);
                    }
                } else if let Some(ref mut state) = self.table_state {
                    state.push_text(&html);
                    self.push_highlight(Some(self.ms.text), &range);
                } else if let Some(highlighted) = syntax_highlight_raw(self.syntect, "html", &html)
                {
                    self.buffers.replaces.push(Replace {
                        highlighted,
                        range: range.clone(),
                    });
                }
            }
            Event::DisplayMath(math) => {
                // `$$...$$` display math: render to Unicode block lines.
                if let Some(ref mut state) = self.table_state {
                    // Inside a table cell there is no room for a block: render single-line (rows joined with `; `)
                    match latex::latex_to_unicode_inline(&math).filter(|r| !r.is_empty()) {
                        Some(r) => {
                            let prev_italic = state.cell_italic;
                            state.cell_italic = true;
                            state.push_text(&r);
                            state.cell_italic = prev_italic;
                        }
                        None => {
                            let prev_code = state.cell_code;
                            state.cell_code = true;
                            state.push_text(&math);
                            state.cell_code = prev_code;
                        }
                    }
                    self.push_highlight(Some(self.ms.math), &range);
                } else if self.push_display_math_block(range.clone(), &math) {
                    // Raw mode shows the TeX source in the math style; pretty mode consumes the range via the block replacement
                    self.push_highlight(Some(self.ms.math), &range);
                } else {
                    // Fallback (conversion declined or nothing visible): highlight the TeX source as code
                    self.push_highlight(Some(self.ms.code_outer), &range);
                    if let Some(outer_text) = self.text.get(range.clone())
                        && let Some(r) = find_substring(outer_text, &math, true, false)
                    {
                        let inner_range = (range.start + r.start)..(range.start + r.end);
                        if let Some(highlighted) = syntax_highlight_raw(self.syntect, "tex", &math)
                        {
                            self.buffers.replaces.push(Replace {
                                highlighted,
                                range: inner_range,
                            });
                        } else {
                            self.push_highlight(Some(self.ms.code_untagged), &inner_range);
                        }
                    }
                }
            }
            Event::FootnoteReference(_) => {
                self.push_highlight(Some(self.ms.link_outer), &range);
            }
            Event::Rule => {
                // Style and transform "---" to "───" (horizontal rule)
                self.push_highlight(Some(self.ms.rule), &range);
                if let Some(rule_text) = self.text.get(range.clone()) {
                    if let Some(marker_end) = rule_text.find('\n') {
                        // Transform only up to the newline
                        self.buffers.transforms.push(Transform {
                            range: range.start..range.start + marker_end,
                            to: "───".to_string(),
                            force: false,
                        });
                    } else {
                        // No trailing newline, transform the whole range
                        self.buffers.transforms.push(Transform {
                            range: range.clone(),
                            to: "───".to_string(),
                            force: false,
                        });
                    }
                }
                if self.depth == 0 {
                    self.last_checkpoint = Some((CheckpointKind::ThematicBreak, range.end));
                }
            }
            Event::TaskListMarker(checked) => {
                let style = if checked {
                    self.ms.task_checked
                } else {
                    self.ms.task_unchecked
                };
                self.push_highlight(Some(style), &range);
            }
        }
    }

    fn on_start(&mut self, tag: Tag<'a>, range: Range<usize>) {
        // Track nesting depth for checkpoint detection
        match &tag {
            Tag::BlockQuote(_) | Tag::List(_) | Tag::Item | Tag::Table(_) => {
                self.depth += 1;
            }
            _ => {}
        }
        if matches!(&tag, Tag::BlockQuote(_)) {
            self.bq_depth += 1;
        }

        let mut more = Vec::new();
        let style = match &tag {
            Tag::Paragraph => None,
            Tag::Heading { level, .. } => {
                let level_usize = (*level as usize).saturating_sub(1).min(5);
                let heading_outer = self.ms.heading_outer.get(level_usize).copied();
                let heading_text = self.text.get(range.clone());
                if let Some(marker_end) =
                    heading_text.and_then(|t| t.find(|c: char| c != '#' && c != ' '))
                {
                    let marker_range = range.start..range.start + marker_end;
                    more.push(Highlight {
                        style: heading_outer,
                        range: marker_range,
                    });
                    None
                } else {
                    heading_outer
                }
            }
            Tag::BlockQuote(_) => {
                // Transform the `>` belonging to THIS blockquote level to `│`.
                // On those subsequent lines, the outer `>` is included in the inner range, so we must skip it
                // A line starting at a real source line boundary skips (bq_depth-1) `>`s; one starting mid-line (the first fragment) skips none
                if let Some(bq_text) = self.text.get(range.clone()) {
                    let mut pos = range.start;

                    for line in bq_text.split_inclusive('\n') {
                        // Does this fragment start at a source line boundary?
                        let at_line_start =
                            pos == 0 || self.text.as_bytes().get(pos - 1) == Some(&b'\n');
                        // If at a line start, outer levels already have `>`s that we must skip
                        // If mid-line (first fragment of range), the outer `>`s are before the range so skip 0
                        let skip = if at_line_start { self.bq_depth - 1 } else { 0 };

                        let mut found = 0usize;
                        for (byte_offset, ch) in line.char_indices() {
                            if ch == '>' {
                                if found == skip {
                                    let gt_pos = pos + byte_offset;
                                    self.buffers.transforms.push(Transform {
                                        range: gt_pos..gt_pos + 1,
                                        to: "│".to_string(),
                                        force: false,
                                    });
                                    more.push(Highlight {
                                        style: Some(self.ms.blockquote_outer),
                                        range: gt_pos..gt_pos + 1,
                                    });
                                    break;
                                }
                                found += 1;
                            }
                        }

                        pos += line.len();
                    }
                }

                // Return None: we've handled the styling via per-line highlights
                None
            }
            Tag::CodeBlock(code) => {
                // Track the fenced block so its body span can be reported once the fence closes
                // The body starts just past the opening fence line; an empty-body fence keeps this empty range
                // Indented code blocks are not fences and report no span
                self.pending_code_block = match code {
                    CodeBlockKind::Fenced(lang) => {
                        let body_start = self
                            .text
                            .get(range.start..)
                            .and_then(|s| s.find('\n'))
                            .map_or(range.end, |nl| range.start + nl + 1);
                        Some(PendingCodeBlock {
                            info: lang.to_string(),
                            body_range: body_start..body_start,
                            body_text: String::new(),
                            body_seen: false,
                        })
                    }
                    CodeBlockKind::Indented => None,
                };

                // pulldown-cmark's code-block range starts at the fence marker (```) and excludes leading indentation on the opening-fence line
                // Only extend when the prefix is pure whitespace so structural prefixes are left intact.
                let line_start = self
                    .text
                    .get(..range.start)
                    .and_then(|s| s.rfind('\n'))
                    .map_or(0, |p| p + 1);
                let fence_start = if self
                    .text
                    .get(line_start..range.start)
                    .is_some_and(|s| s.bytes().all(|b| b == b' ' || b == b'\t'))
                {
                    line_start
                } else {
                    range.start
                };
                more.push(Highlight {
                    style: Some(self.ms.code_outer),
                    range: fence_start..range.end,
                });
                match code {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        if let Some(slice) = self.text.get(range.clone())
                            && let Some(r) = find_substring(slice, lang, true, false)
                        {
                            let range = (r.start + range.start)..(r.end + range.start);
                            more.push(Highlight {
                                style: Some(self.ms.code_language),
                                range,
                            });
                        }
                    }
                    _ => (),
                }
                None
            }
            Tag::HtmlBlock => {
                // Don't syntax-highlight HTML blocks as code
                // In LLM output, these are typically XML-like structural tags (e.g. <example>) from system prompts, not actual HTML.
                // pulldown-cmark ends HTML blocks at blank lines, so code styling would make the first part look like code and the rest like text
                None
            }
            Tag::List(_) => None,
            Tag::Item => {
                if let Some(item_text) = self.text.get(range.clone()) {
                    let trimmed = item_text.trim_start();
                    let leading_ws = item_text.len() - trimmed.len();

                    let digits_before = |pos: usize| {
                        pos > 0
                            && trimmed
                                .get(..pos)
                                .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
                    };
                    let marker_len = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                        2
                    } else if let Some(pos) = trimmed.find(". ") {
                        if digits_before(pos) { pos + 2 } else { 0 }
                    } else if let Some(pos) = trimmed.find(") ") {
                        if digits_before(pos) { pos + 2 } else { 0 }
                    } else {
                        0
                    };

                    if marker_len > 0 {
                        let marker_start = range.start + leading_ws;
                        let marker_end = marker_start + marker_len;
                        more.push(Highlight {
                            style: Some(self.ms.list_item),
                            range: marker_start..marker_end,
                        });

                        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                            self.buffers.transforms.push(Transform {
                                range: marker_start..marker_start + 1,
                                to: "•".to_string(),
                                force: false,
                            });
                        }
                    }
                }
                None
            }
            Tag::Table(alignments) => {
                self.table_state = Some(TableState::new(alignments.to_vec(), range.start));
                Some(self.ms.table_outer)
            }
            Tag::TableHead => {
                if let Some(ref mut state) = self.table_state {
                    state.in_header = true;
                }
                Some(self.ms.table_outer)
            }
            Tag::TableRow => {
                if let Some(ref mut state) = self.table_state {
                    state.current_row.clear();
                }
                Some(self.ms.table_outer)
            }
            Tag::TableCell => {
                if let Some(ref mut state) = self.table_state {
                    state.current_cell.clear();
                }
                self.push_highlight(None, &range);
                None
            }
            Tag::Emphasis => {
                if let Some(ref mut state) = self.table_state {
                    state.cell_italic = true;
                }
                Some(self.ms.emphasis_outer)
            }
            Tag::Strong => {
                if let Some(ref mut state) = self.table_state {
                    state.cell_bold = true;
                }
                Some(self.ms.strong_outer)
            }
            Tag::Strikethrough => {
                if let Some(ref mut state) = self.table_state {
                    state.cell_strike = true;
                }
                Some(self.ms.strikethrough_outer)
            }
            Tag::Link {
                dest_url, title, ..
            }
            | Tag::Image {
                dest_url, title, ..
            } => {
                // Links inside a table cell go through the table renderer's own hyperlink path (TableHyperlink in TableReplace)
                // The paragraph link path (LinkTarget + chunk_link_offsets) can't project links onto rendered table cells
                if let Some(ref mut state) = self.table_state {
                    let id = self.link_id_counter;
                    self.link_id_counter += 1;
                    state.cell_link = Some((dest_url.to_string(), id));
                    self.tag_stack.push(tag);
                    return;
                }

                let Some(tag_str) = self.text.get(range.clone()) else {
                    self.buffers.link_targets.push(LinkTarget {
                        source_range: range.clone(),
                        url: dest_url.to_string(),
                        id: self.link_id_counter,
                    });
                    self.link_id_counter += 1;
                    self.push_highlight(Some(self.ms.link_outer), &range);
                    self.tag_stack.push(tag);
                    return;
                };

                if !title.is_empty() {
                    for t in [format!("\"{title}\""), format!("'{title}'")].map(CowStr::from) {
                        if let Some(r) = find_substring(tag_str, &t, true, true) {
                            let title_range = (r.start + range.start)..(r.end + range.start);
                            more.push(Highlight {
                                style: Some(self.ms.link_title),
                                range: title_range,
                            });
                        }
                    }
                }

                // This uses allow_outside=true, then rfinds `](` on the prefix before the (last) dest_url occurrence
                // dest_url may be a CowStr::Owned (after percent-decoding or HTML entity expansion), so it may not be a sub-slice of tag_str
                // The rfind on the strict prefix finds the structural `](` closer even when the link text, title, or dest literal contains `](`
                let url_rel_opt = find_substring(tag_str, dest_url, true, true);
                if let Some(r) = &url_rel_opt {
                    let url_range = (r.start + range.start)..(r.end + range.start);
                    more.push(Highlight {
                        style: Some(self.ms.link_url),
                        range: url_range,
                    });
                }

                let bracket_pos_opt = url_rel_opt.as_ref().and_then(|r| {
                    tag_str
                        .get(..r.start)
                        .and_then(|s| s.rfind("]("))
                        .map(|p| p..p + 2)
                });
                if let Some(bracket_pos) = bracket_pos_opt {
                    let open_bracket = if tag_str.starts_with("![") { 1 } else { 0 };
                    if open_bracket > 0 {
                        more.push(Highlight {
                            style: Some(self.ms.link_outer),
                            range: range.start..range.start + 1,
                        });
                    }
                    more.push(Highlight {
                        style: Some(self.ms.link_outer),
                        range: range.start + open_bracket..range.start + open_bracket + 1,
                    });
                    let text_start = range.start + open_bracket + 1;
                    let text_end = bracket_pos.start + range.start;
                    if text_end > text_start {
                        more.push(Highlight {
                            style: Some(self.ms.link_text),
                            range: text_start..text_end,
                        });
                    }
                    let bracket_abs = bracket_pos.start + range.start;
                    more.push(Highlight {
                        style: Some(self.ms.link_outer),
                        range: bracket_abs..bracket_abs + 2,
                    });
                    more.push(Highlight {
                        style: Some(self.ms.link_outer),
                        range: range.end - 1..range.end,
                    });
                    self.buffers.transforms.push(Transform {
                        range: range.start + open_bracket..range.start + open_bracket + 1,
                        to: "".to_string(),
                        force: false,
                    });
                    self.buffers.transforms.push(Transform {
                        range: bracket_abs..bracket_abs + 2,
                        to: " (".to_string(),
                        force: false,
                    });
                    if text_end > text_start {
                        self.buffers.link_targets.push(LinkTarget {
                            source_range: text_start..text_end,
                            url: dest_url.to_string(),
                            id: self.link_id_counter,
                        });
                        self.link_id_counter += 1;
                    }
                    None
                } else {
                    self.buffers.link_targets.push(LinkTarget {
                        source_range: range.clone(),
                        url: dest_url.to_string(),
                        id: self.link_id_counter,
                    });
                    self.link_id_counter += 1;
                    Some(self.ms.link_outer)
                }
            }
            _ => None,
        };

        if let Some(style) = style {
            self.push_highlight(Some(style), &range);
        }
        for hl in more {
            self.buffers.highlights.push(hl);
        }

        self.tag_stack.push(tag);
    }

    fn on_end(&mut self, tag_end: TagEnd, range: Range<usize>) {
        self.tag_stack.pop();

        // Handle tag-specific end logic and determine if we need to push a style
        let style = match &tag_end {
            TagEnd::Emphasis => {
                // Reset italic for table cells (no highlight pushed)
                if let Some(ref mut state) = self.table_state {
                    state.cell_italic = false;
                }
                None
            }
            TagEnd::Strong => {
                // Reset bold for table cells (no highlight pushed)
                if let Some(ref mut state) = self.table_state {
                    state.cell_bold = false;
                }
                None
            }
            TagEnd::Strikethrough => {
                if let Some(ref mut state) = self.table_state {
                    state.cell_strike = false;
                }
                None
            }
            TagEnd::CodeBlock => {
                // pulldown synthesizes a block end at end-of-input even for an unterminated fence, so the end event alone does not prove closure
                // A closing fence always sits after the body, so the block range extends past the body exactly when the fence closed
                // `take` clears the pending block in either case
                if let Some(pending) = self.pending_code_block.take()
                    && pending.body_range.end < range.end
                {
                    self.buffers.code_blocks.push(CodeBlockMeta {
                        info: pending.info,
                        body: pending.body_text,
                        body_source_range: pending.body_range,
                    });
                }
                None
            }
            TagEnd::Link | TagEnd::Image => {
                // Clear link state for table cells so subsequent text in the same cell isn't tagged as part of this link
                if let Some(ref mut state) = self.table_state {
                    state.cell_link = None;
                }
                None
            }
            TagEnd::TableCell => {
                // Finish current cell
                if let Some(ref mut state) = self.table_state {
                    state
                        .current_row
                        .push(std::mem::take(&mut state.current_cell));
                    // Reset cell styles
                    state.cell_bold = false;
                    state.cell_italic = false;
                    state.cell_code = false;
                    state.cell_strike = false;
                    state.cell_link = None;
                }
                None
            }
            TagEnd::TableRow => {
                // Finish body row (if not in header)
                if let Some(ref mut state) = self.table_state
                    && !state.in_header
                {
                    let row = std::mem::take(&mut state.current_row);
                    state.rows.push(row);
                }
                None
            }
            TagEnd::TableHead => {
                // Finish header row
                if let Some(ref mut state) = self.table_state {
                    state.header = std::mem::take(&mut state.current_row);
                    state.in_header = false;
                }
                None
            }
            TagEnd::Table => {
                // Finish table: format and store the replacement
                if let Some(mut state) = self.table_state.take() {
                    state.range.end = range.end;
                    let FormattedTable {
                        lines,
                        styled_lines,
                        line_source_offsets,
                        hyperlinks,
                        cell_copies,
                        n_cols,
                    } = self.format_table(&state);
                    self.buffers.table_replaces.push(TableReplace {
                        lines,
                        styled_lines,
                        range: state.range,
                        line_source_offsets,
                        hyperlinks,
                        cell_copies,
                        n_cols,
                    });
                }
                None
            }
            _ => None,
        };

        if let Some(style) = style {
            self.push_highlight(Some(style), &range);
        }

        // Track depth and checkpoints
        match &tag_end {
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item | TagEnd::Table => {
                self.depth = self.depth.saturating_sub(1);
            }
            _ => {}
        }
        if matches!(&tag_end, TagEnd::BlockQuote(_)) {
            self.bq_depth = self.bq_depth.saturating_sub(1);
        }

        // Record checkpoint at depth=0 block boundaries
        if self.depth == 0 {
            let kind = match &tag_end {
                TagEnd::Paragraph => Some(CheckpointKind::Paragraph),
                TagEnd::Heading(_) => Some(CheckpointKind::Heading),
                TagEnd::CodeBlock => Some(CheckpointKind::CodeBlock),
                TagEnd::BlockQuote(_) => Some(CheckpointKind::BlockQuote),
                TagEnd::List(_) => Some(CheckpointKind::List),
                TagEnd::Table => Some(CheckpointKind::Table),
                TagEnd::HtmlBlock => Some(CheckpointKind::HtmlBlock),
                _ => None,
            };

            if let Some(kind) = kind {
                let has_blank = has_blank_line_after(self.text, range.end);
                let is_code_block = matches!(kind, CheckpointKind::CodeBlock);
                let at_eof = range.end >= self.text.len();
                let code_block_properly_closed = is_code_block && !at_eof;

                if has_blank || code_block_properly_closed {
                    // For code blocks, include one newline to properly close the block.
                    // For other blocks (paragraphs, headings, blockquotes, lists), DON'T include the trailing newline
                    // That way the blank line separator is re-rendered when the next chunk is added
                    let checkpoint_pos = if is_code_block && has_blank {
                        range.end + 1
                    } else {
                        range.end
                    };
                    self.last_checkpoint = Some((kind, checkpoint_pos));
                }
            }
        }
    }

    /// Render a mermaid code block into a [`MermaidReplace`]; `true` if drawn.
    fn try_push_mermaid(&mut self, text: &str, range: &Range<usize>) -> bool {
        let line_style = self.ms.rule.style_into();
        let styles = crate::mermaid::MermaidStyles {
            border: line_style,
            node_text: self.ms.text.style_into(),
            edge: line_style,
            edge_label: self.ms.emphasis_inner.style_into(),
            title: self.ms.strong_inner.style_into(),
        };
        match crate::mermaid::render(text, &styles, self.max_table_width) {
            Some(art) => {
                self.buffers
                    .mermaid_replaces
                    .push(crate::buffers::MermaidReplace {
                        lines: art.plain_lines,
                        styled_lines: art.styled_lines,
                        range: range.clone(),
                    });
                true
            }
            None => false,
        }
    }

    /// Push text into the current table cell (no-op outside a table), tagging
    /// bare URLs/emails as link spans. The post-render URL scan runs after the
    /// table has wrapped the cell, so it would only catch the first line of a
    /// wrapped URL; tagging here uses the wrap-aware `TableHyperlink` path.
    fn push_table_text_linking_urls(&mut self, text: &str) {
        let Some(state) = self.table_state.as_mut() else {
            return;
        };
        if state.cell_link.is_some() {
            state.push_text(text);
            return;
        }

        let mut cursor = 0;
        url_scan::for_each_plain_link(text, |range, url| {
            if range.start > cursor
                && let Some(prefix) = text.get(cursor..range.start)
            {
                state.push_text(prefix);
            }
            let id = self.link_id_counter;
            self.link_id_counter += 1;
            state.cell_link = Some((url, id));
            if let Some(slice) = text.get(range.clone()) {
                state.push_text(slice);
            }
            state.cell_link = None;
            cursor = range.end;
        });
        if cursor < text.len()
            && let Some(rest) = text.get(cursor..)
        {
            state.push_text(rest);
        }
    }

    /// Apply inline-code styling to a code/math span: dim the delimiters, style the content.
    /// Shared by `Event::Code` and the inline-math fallback path.
    fn style_inline_code_span(&mut self, code: &CowStr<'_>, range: &Range<usize>) {
        // Find the actual content range (excluding the delimiters).
        let Some(outer_text) = self.text.get(range.clone()) else {
            self.push_highlight(Some(self.ms.inline_code_inner), range);
            return;
        };
        if let Some(inner_range) = find_substring(outer_text, code, false, false)
            .or_else(|| find_substring(outer_text, code, true, false))
        {
            let absolute_inner = (range.start + inner_range.start)..(range.start + inner_range.end);

            // Left delimiter
            if inner_range.start > 0 {
                self.push_highlight(
                    Some(self.ms.inline_code_outer),
                    &(range.start..absolute_inner.start),
                );
            }
            // Inner code
            self.push_highlight(Some(self.ms.inline_code_inner), &absolute_inner);
            // Right delimiter
            if range.end > absolute_inner.end {
                self.push_highlight(
                    Some(self.ms.inline_code_outer),
                    &(absolute_inner.end..range.end),
                );
            }
        } else {
            self.push_highlight(Some(self.ms.inline_code_inner), range);
        }
    }

    /// Scan a prose `Event::Text` source range for HTML character entity references (`&lt;`, `&gt;`, `&amp;`, numeric, …).
    /// Code spans and fenced blocks never reach here, so entities inside code stay literal.
    /// Panic-safety: pulldown-cmark guarantees `range` is a valid sub-slice of `self.text`.
    fn scan_inline_html_entities(&mut self, range: &Range<usize>) {
        let Some(slice) = self.text.get(range.clone()) else {
            debug_assert!(false, "pulldown-cmark text range out of bounds");
            return;
        };
        if !slice.contains('&') {
            return;
        }
        // Longest HTML5 named entity reference (`&CounterClockwiseContourIntegral;`) is 33 bytes including the leading `&` and trailing `;`
        // Bounding the scan keeps a run of bare `&` characters from degrading to O(n²)
        const MAX_ENTITY_LEN: usize = 33;
        let bytes = slice.as_bytes();
        let mut i = 0;
        while let Some(&b) = bytes.get(i) {
            if b != b'&' {
                i += 1;
                continue;
            }
            // An entity reference contains only ASCII name/numeric characters and no internal `;`
            // The first `;` reached while consuming valid characters therefore closes it
            // Stopping on any other byte avoids both quadratic scans and slicing through a multi-byte char
            let max = (i + MAX_ENTITY_LEN).min(bytes.len());
            let mut j = i + 1;
            let end = loop {
                if j >= max {
                    break None;
                }
                match bytes.get(j) {
                    Some(b';') => break Some(j),
                    Some(b'#' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9') => j += 1,
                    _ => break None,
                }
            };
            if let Some(end) = end
                && let Some(entity) = slice.get(i..=end)
                && let Some(decoded) = decode_html_entity(entity)
            {
                let abs = (range.start + i)..(range.start + end + 1);
                // An earlier scan (e.g. `\(...\)` math) may have already claimed these bytes with its own transform.
                // Overlapping transforms would each emit their replacement, so leave the entity to the existing one
                let overlaps = self
                    .buffers
                    .transforms
                    .iter()
                    .any(|t| t.range.start < abs.end && abs.start < t.range.end);
                if !overlaps {
                    self.push_highlight(None, &abs);
                    self.buffers.transforms.push(Transform {
                        range: abs,
                        to: decoded,
                        force: false,
                    });
                }
                i = end + 1;
                continue;
            }
            i += 1;
        }
    }

    /// Push a pretty-mode block replacement rendering `latex_src` as display math over `range`.
    /// Reuses the table block-replacement machinery: pre-rendered styled lines that substitute the source range in pretty mode only.
    /// Raw mode keeps showing the TeX source.
    fn push_display_math_block(&mut self, range: Range<usize>, latex_src: &str) -> bool {
        let Some(rendered) = latex::latex_to_unicode_display(latex_src) else {
            return false;
        };
        if rendered.is_empty() {
            return false;
        }
        // Consume the line ending right after the closing delimiter, like table ranges do
        // Without it, a batch render emits an extra blank line after the block (the source newline) that the streaming checkpoint+tail path does not
        // That breaks render convergence
        let mut range = range;
        if self
            .text
            .get(range.end..)
            .is_some_and(|s| s.starts_with("\r\n"))
        {
            range.end += 2;
        } else if self
            .text
            .get(range.end..)
            .is_some_and(|s| s.starts_with('\n'))
        {
            range.end += 1;
        }
        let style: ratatui::style::Style = self.ms.math.style_into();
        let src_newlines = self
            .text
            .get(range.clone())
            .map(|s| s.bytes().filter(|&b| b == b'\n').count())
            .unwrap_or(0);
        let mut lines = Vec::with_capacity(rendered.len());
        let mut styled_lines = Vec::with_capacity(rendered.len());
        let mut line_source_offsets = Vec::with_capacity(rendered.len());
        for (i, line) in rendered.iter().enumerate() {
            let text = format!("  {line}");
            styled_lines.push(Line::from(Span::styled(text.clone(), style)));
            lines.push(text);
            // Best-effort scroll mapping: the i-th rendered line maps to the i-th content line of the block (clamped to its source lines)
            line_source_offsets.push((i + 1).min(src_newlines));
        }
        self.buffers.table_replaces.push(TableReplace {
            lines,
            styled_lines,
            range,
            line_source_offsets,
            hyperlinks: Vec::new(),
            cell_copies: Vec::new(),
            n_cols: 0,
        });
        true
    }

    /// Format a buffered table into lines with box-drawing borders.
    fn format_table(&self, state: &TableState) -> FormattedTable {
        let borders = TableBorders::BOX;
        let padding = 1;

        // Style already adapted, no need to call adapt_style again
        let border_style: ratatui::style::Style = self.ms.rule.style_into().dim();

        let all_rows: Vec<&Vec<StyledCell>> = std::iter::once(&state.header)
            .chain(state.rows.iter())
            .filter(|r| !r.is_empty())
            .collect();

        if all_rows.is_empty() {
            return FormattedTable::default();
        }

        let num_cols = all_rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if num_cols == 0 {
            return FormattedTable::default();
        }

        let mut col_widths: Vec<usize> = vec![0; num_cols];
        for row in &all_rows {
            for (col_idx, cell) in row.iter().enumerate() {
                let text = cell.plain_text();
                let cell_width = text
                    .split('\n')
                    .map(unicode_display_width)
                    .max()
                    .unwrap_or(0);
                if let Some(width) = col_widths.get_mut(col_idx) {
                    *width = (*width).max(cell_width);
                }
            }
        }

        // Constrain column widths to fit within max_table_width if set.
        // = num_cols * (2 * padding + 1) + sum(col_width) + 2 - 1
        if let Some(max_width) = self.max_table_width {
            let overhead = num_cols * (2 * padding + 1) + 1; // borders + padding
            let content_budget = max_width.saturating_sub(overhead);
            let total_content: usize = col_widths.iter().sum();

            if total_content > content_budget && total_content > 0 {
                // Compute per-column minimum widths: the longest unbreakable word across all cells in each column
                // The word separator determines what counts as unbreakable (e.g. "LongalphaToken", "$145,000", "ID-AA1001").
                let mut min_col_widths: Vec<usize> = vec![1; num_cols];
                // Per-column hard floors: the widest single grapheme (0 for empty columns)
                // That is the narrowest width at which cell text can still reflow without losing content
                let mut hard_floors: Vec<usize> = vec![0; num_cols];
                for row in &all_rows {
                    for (col, cell) in row.iter().enumerate() {
                        if col >= num_cols {
                            break;
                        }
                        let text = cell.plain_text();
                        for word in cell_word_separator(&text) {
                            let w = unicode_display_width(word.word);
                            if let Some(min_w) = min_col_widths.get_mut(col) {
                                *min_w = (*min_w).max(w);
                            }
                        }
                        if !text.is_empty()
                            && let Some(floor) = hard_floors.get_mut(col)
                        {
                            let widest_grapheme = text
                                .graphemes(true)
                                .map(unicode_display_width)
                                .max()
                                .unwrap_or(0);
                            *floor = (*floor).max(widest_grapheme.max(1));
                        }
                    }
                }

                let min_total: usize = min_col_widths.iter().sum();
                let hard_total: usize = hard_floors.iter().sum();

                // Grow from the word minimums toward natural widths when the word minimums fit the budget
                // Otherwise restart from the grapheme floors
                // When even the grapheme floors cannot fit, keep the word minimums: downstream clipping remains the safety net
                let (base_widths, target_widths) =
                    if min_total > content_budget && hard_total <= content_budget {
                        (hard_floors, min_col_widths)
                    } else {
                        (min_col_widths, col_widths.clone())
                    };
                let base_total: usize = base_widths.iter().sum();
                let extra_budget = content_budget.saturating_sub(base_total);

                // How much each column *wants* above its base.
                let extra_wants: Vec<usize> = target_widths
                    .iter()
                    .zip(&base_widths)
                    .map(|(&target, &base)| target.saturating_sub(base))
                    .collect();
                let total_extra_want: usize = extra_wants.iter().sum();

                let mut new_widths = base_widths.clone();
                if total_extra_want > 0 && extra_budget > 0 {
                    // Distribute proportionally.
                    for (width, &want) in new_widths.iter_mut().zip(&extra_wants) {
                        let share = (want as f64 * extra_budget as f64 / total_extra_want as f64)
                            .floor() as usize;
                        *width += share;
                    }

                    // Hand out any remaining columns (from floor rounding) to columns with the most unmet want, one at a time
                    let used: usize = new_widths.iter().sum();
                    let mut remaining = content_budget.saturating_sub(used);
                    if remaining > 0 {
                        let mut indices: Vec<usize> = (0..num_cols).collect();
                        // Sort by unmet want descending.
                        let unmet = |i: usize| {
                            target_widths
                                .get(i)
                                .copied()
                                .unwrap_or(0)
                                .saturating_sub(new_widths.get(i).copied().unwrap_or(0))
                        };
                        indices.sort_by(|&a, &b| unmet(b).cmp(&unmet(a)));
                        for &idx in &indices {
                            if remaining == 0 {
                                break;
                            }
                            // Don't grow beyond this pass's target width.
                            let target = target_widths.get(idx).copied().unwrap_or(0);
                            if let Some(width) = new_widths.get_mut(idx)
                                && *width < target
                            {
                                *width += 1;
                                remaining -= 1;
                            }
                        }
                    }
                }

                col_widths = new_widths;
            }
        }

        let alignments: Vec<_> = (0..num_cols)
            .map(|i| {
                state
                    .alignments
                    .get(i)
                    .copied()
                    .unwrap_or(pulldown_cmark::Alignment::None)
            })
            .collect();

        let mut lines = Vec::new();
        let mut styled_lines = Vec::new();
        let mut line_source_offsets: Vec<usize> = Vec::new();
        let mut hyperlinks: Vec<TableHyperlink> = Vec::new();
        let mut cell_copies: Vec<TableCellCopy> = Vec::new();

        // Source line layout within a table: offset 0: header row (| Col A | Col B |) offset 1: separator (|-------|-------|) offset 2+: body rows (| val1 | val2 |)
        let header_offset = 0usize;
        let separator_offset = 1usize;

        // Top border, belongs to the header line
        let top_border = self.format_border_line(
            &col_widths,
            padding,
            borders.c_tl(),
            borders.t_t(),
            borders.c_tr(),
            borders.h(),
        );
        styled_lines.push(Line::styled(top_border.clone(), border_style));
        lines.push(top_border);
        line_source_offsets.push(header_offset);

        // Header row
        if !state.header.is_empty() {
            let (row_plains, row_styleds, row_links, row_copies) = self
                .format_styled_content_lines(
                    &state.header,
                    &col_widths,
                    &alignments,
                    padding,
                    borders.v(),
                    border_style,
                    true,
                );
            cell_copies.extend(row_copies);
            let base_line = styled_lines.len();
            for (p, s) in row_plains.into_iter().zip(row_styleds) {
                lines.push(p);
                styled_lines.push(s);
                line_source_offsets.push(header_offset);
            }
            for mut link in row_links {
                link.line_offset += base_line;
                hyperlinks.push(link);
            }

            // Header separator
            let sep = self.format_border_line(
                &col_widths,
                padding,
                borders.t_l(),
                borders.x(),
                borders.t_r(),
                borders.h(),
            );
            styled_lines.push(Line::styled(sep.clone(), border_style));
            lines.push(sep);
            line_source_offsets.push(separator_offset);
        }

        // Body rows
        for (i, row) in state.rows.iter().enumerate() {
            let row_offset = separator_offset + 1 + i; // offset 2, 3, ...

            let (row_plains, row_styleds, row_links, row_copies) = self
                .format_styled_content_lines(
                    row,
                    &col_widths,
                    &alignments,
                    padding,
                    borders.v(),
                    border_style,
                    false,
                );
            cell_copies.extend(row_copies);
            let base_line = styled_lines.len();
            for (p, s) in row_plains.into_iter().zip(row_styleds) {
                lines.push(p);
                styled_lines.push(s);
                line_source_offsets.push(row_offset);
            }
            for mut link in row_links {
                link.line_offset += base_line;
                hyperlinks.push(link);
            }

            // Row divider between body rows (not after last row)
            if i < state.rows.len().saturating_sub(1) {
                let row_sep = self.format_border_line(
                    &col_widths,
                    padding,
                    borders.t_l(),
                    borders.x(),
                    borders.t_r(),
                    borders.h(),
                );
                styled_lines.push(Line::styled(row_sep.clone(), border_style));
                lines.push(row_sep);
                line_source_offsets.push(row_offset);
            }
        }

        // Bottom border, belongs to the last body row
        let last_row_offset = separator_offset + state.rows.len();
        let bottom_border = self.format_border_line(
            &col_widths,
            padding,
            borders.c_bl(),
            borders.t_b(),
            borders.c_br(),
            borders.h(),
        );
        styled_lines.push(Line::styled(bottom_border.clone(), border_style));
        lines.push(bottom_border);
        line_source_offsets.push(last_row_offset);

        FormattedTable {
            lines,
            styled_lines,
            line_source_offsets,
            hyperlinks,
            cell_copies,
            n_cols: num_cols,
        }
    }

    /// Word-wrap a cell's plain text into lines of at most `width` display columns.
    /// Returns visual lines plus the exact omitted gap at each wrap (never empty).
    /// It never breaks mid-word.
    pub(crate) fn wrap_cell_text_joins(text: &str, width: usize) -> (Vec<String>, Vec<CellJoin>) {
        let lines = if width == 0 {
            vec![String::new()]
        } else {
            let opts = textwrap::Options::new(width)
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
                .word_separator(textwrap::WordSeparator::Custom(cell_word_separator))
                .break_words(false);
            let wrapped = textwrap::wrap(text, opts);
            let mut lines: Vec<String> = Vec::with_capacity(wrapped.len());
            for cow in wrapped {
                let line = cow.into_owned();
                if unicode_display_width(&line) <= width {
                    lines.push(line);
                    continue;
                }
                // An unbreakable word survived textwrap wider than the column
                // break_words is off, and textwrap's char-based emergency split can tear grapheme clusters
                // Hard-split on grapheme boundaries using the same display-width model as the table formatter
                let mut piece = String::new();
                let mut piece_width = 0usize;
                for grapheme in line.graphemes(true) {
                    let grapheme_width = unicode_display_width(grapheme);
                    if piece_width > 0 && piece_width.saturating_add(grapheme_width) > width {
                        lines.push(std::mem::take(&mut piece));
                        piece_width = 0;
                    }
                    piece.push_str(grapheme);
                    piece_width = piece_width.saturating_add(grapheme_width);
                }
                if !piece.is_empty() {
                    lines.push(piece);
                }
            }
            if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            }
        };
        let joins = cell_wrap_joins(text, &lines);
        (lines, joins)
    }

    /// Format a table row that may span multiple visual lines (when cells wrap).
    /// There is one plain + styled entry per visual line, plus any hyperlinks discovered in cell spans.
    /// Hyperlink `line_offset`s are relative to the first visual line of this row (caller adds the absolute base to embed in the table).
    #[allow(clippy::too_many_arguments)]
    fn format_styled_content_lines(
        &self,
        cells: &[StyledCell],
        col_widths: &[usize],
        alignments: &[pulldown_cmark::Alignment],
        padding: usize,
        v: char,
        border_style: ratatui::style::Style,
        is_header: bool,
    ) -> (
        Vec<String>,
        Vec<Line<'static>>,
        Vec<TableHyperlink>,
        Vec<TableCellCopy>,
    ) {
        // 1. Wrap each cell's text into lines constrained to col_widths[i].
        let mut wrapped_cells = Vec::with_capacity(col_widths.len());
        let mut cell_copies = Vec::with_capacity(col_widths.len());
        for (i, &width) in col_widths.iter().enumerate() {
            let text = cells.get(i).map(|c| c.plain_text()).unwrap_or_default();
            let (lines, joins) = Self::wrap_cell_text_joins(&text, width);
            cell_copies.push(TableCellCopy { text, joins });
            wrapped_cells.push(lines);
        }

        // 2. Determine the number of visual lines (max wrapped lines across cells).
        let num_visual_lines = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);

        // 3. Build each visual line.
        let mut all_plains = Vec::with_capacity(num_visual_lines);
        let mut all_styled = Vec::with_capacity(num_visual_lines);
        let mut all_links: Vec<TableHyperlink> = Vec::new();

        // Monotonic per-column source cursors: each fragment is searched for strictly after the previous fragment's match end
        // Repeated substrings (a linked "aa" followed by a plain "aa") can never re-match earlier bytes once textwrap has eaten boundary whitespace
        let mut source_cursors: Vec<usize> = vec![0; col_widths.len()];

        for vis_line in 0..num_visual_lines {
            let mut plain = String::new();
            let mut spans: Vec<Span<'static>> = Vec::new();
            // Running display column on this visual line; used to record hyperlink column ranges in the table-local coordinate system
            let mut display_col: usize = 0;

            plain.push(v);
            spans.push(Span::styled(v.to_string(), border_style));
            display_col += unicode_display_width(&v.to_string());

            for (i, width) in col_widths.iter().enumerate() {
                let cell_line_text = wrapped_cells
                    .get(i)
                    .and_then(|c| c.get(vis_line))
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let cell_line_width = unicode_display_width(cell_line_text);
                let total_padding = width.saturating_sub(cell_line_width);

                let alignment = alignments
                    .get(i)
                    .copied()
                    .unwrap_or(pulldown_cmark::Alignment::None);
                let (left_pad, right_pad) = match alignment {
                    pulldown_cmark::Alignment::Left | pulldown_cmark::Alignment::None => {
                        (0, total_padding)
                    }
                    pulldown_cmark::Alignment::Right => (total_padding, 0),
                    pulldown_cmark::Alignment::Center => {
                        let left = total_padding / 2;
                        (left, total_padding - left)
                    }
                };

                // Left padding
                let left_space = " ".repeat(padding + left_pad);
                let left_space_width = unicode_display_width(&left_space);
                plain.push_str(&left_space);
                spans.push(Span::raw(left_space));
                display_col += left_space_width;

                // Cell text: slice the original styled spans to match this visual line's character range
                // This preserves per-span formatting (bold, italic, strike, code, link) across wrap boundaries
                if !cell_line_text.is_empty() {
                    if let Some(cell) = cells.get(i) {
                        // Find the byte offset of this visual line within the full cell plain text, then emit styled spans covering that range
                        let full_text = cell.plain_text();
                        // Search from the previous fragment's match end
                        // Whitespace textwrap ate could otherwise let `.find` re-match an earlier overlapping occurrence of this fragment
                        let cursor = floor_char_boundary(
                            &full_text,
                            source_cursors.get(i).copied().unwrap_or(0),
                        );
                        let line_start = full_text
                            .get(cursor..)
                            .and_then(|rest| rest.find(cell_line_text))
                            .map(|off| cursor + off)
                            .unwrap_or(cursor);
                        let line_end = (line_start + cell_line_text.len()).min(full_text.len());
                        if let Some(next_cursor) = source_cursors.get_mut(i) {
                            *next_cursor = line_end;
                        }

                        // Walk the cell's spans, emitting the slice that overlaps [line_start..line_end]
                        let mut offset = 0usize;
                        for cell_span in &cell.spans {
                            let span_start = offset;
                            let span_end = offset + cell_span.text.len();
                            offset = span_end;

                            // Intersect [span_start..span_end] with [line_start..line_end]
                            let start = span_start.max(line_start);
                            let end = span_end.min(line_end);
                            if start >= end {
                                continue;
                            }

                            let Some(slice) = full_text.get(start..end) else {
                                continue;
                            };
                            if slice.is_empty() {
                                continue;
                            }

                            let mut style: ratatui::style::Style = self.ms.text.style_into();
                            if is_header || cell_span.bold {
                                style = style.bold();
                            }
                            if cell_span.italic {
                                style = style.italic();
                            }
                            if cell_span.code {
                                // Full replace; apply strike after this so ~~`code`~~ stays crossed out.
                                style = self.ms.inline_code_inner.style_into();
                            }
                            if cell_span.strike {
                                style = style.crossed_out();
                            }
                            if let Some((url, id)) = &cell_span.link {
                                // Apply link styling additively (preserves bold/italic if combined)
                                // link_text style typically adds underline + accent color so the cell visually matches paragraph link rendering
                                let link_style: ratatui::style::Style =
                                    self.ms.link_text.style_into();
                                style = style.patch(link_style);

                                let slice_width = unicode_display_width(slice);
                                all_links.push(TableHyperlink {
                                    line_offset: vis_line,
                                    column_range: display_col..(display_col + slice_width),
                                    url: url.clone(),
                                    id: *id,
                                });
                            }
                            let slice_width = unicode_display_width(slice);
                            plain.push_str(slice);
                            spans.push(Span::styled(slice.to_string(), style));
                            display_col += slice_width;
                        }
                    } else {
                        plain.push_str(cell_line_text);
                        spans.push(Span::raw(cell_line_text.to_string()));
                        display_col += cell_line_width;
                    }
                }

                // Right padding
                let right_space = " ".repeat(right_pad + padding);
                let right_space_width = unicode_display_width(&right_space);
                plain.push_str(&right_space);
                spans.push(Span::raw(right_space));
                display_col += right_space_width;

                // Column separator
                plain.push(v);
                spans.push(Span::styled(v.to_string(), border_style));
                display_col += unicode_display_width(&v.to_string());
            }

            all_plains.push(plain);
            all_styled.push(Line::from(spans));
        }

        (all_plains, all_styled, all_links, cell_copies)
    }

    fn format_border_line(
        &self,
        col_widths: &[usize],
        padding: usize,
        left: char,
        mid: char,
        right: char,
        h: char,
    ) -> String {
        let mut line = String::new();
        line.push(left);
        for (i, &width) in col_widths.iter().enumerate() {
            let total_width = width + padding * 2;
            for _ in 0..total_width {
                line.push(h);
            }
            if i < col_widths.len() - 1 {
                line.push(mid);
            }
        }
        line.push(right);
        line
    }
}

/// Parsed markdown ready for rendering.
/// Created by `MarkdownParser::parse()`.
/// Transient parsing state has been dropped at this point.
pub struct ParsedMarkdown<'a, 'b> {
    pub(crate) text: &'a str,
    pub(crate) ms: MarkdownStyle,
    pub(crate) buffers: &'b mut MarkdownBuffers,
    pub(crate) last_checkpoint: Option<(CheckpointKind, usize)>,
    pub(crate) next_link_id: u32,
}

impl<'a, 'b> ParsedMarkdown<'a, 'b> {
    pub fn new(
        text: &'a str,
        ms: MarkdownStyle,
        buffers: &'b mut MarkdownBuffers,
        last_checkpoint: Option<(CheckpointKind, usize)>,
        next_link_id: u32,
    ) -> Self {
        Self {
            text,
            ms,
            buffers,
            last_checkpoint,
            next_link_id,
        }
    }
}

#[cfg(test)]
mod wrap_cell_joins_tests {
    use super::cell_wrap_joins;
    use crate::buffers::CellJoin;
    use crate::parse::MarkdownParser;

    fn reconstruct(lines: &[String], joins: &[CellJoin]) -> String {
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0
                && let Some(CellJoin::Gap(s)) = joins.get(i - 1)
            {
                out.push_str(s);
            }
            out.push_str(line.trim());
        }
        out
    }

    #[test]
    fn zebraone_space_before_slash_is_gap() {
        let src = "ZebraOne / BravoCat / Quick Fox";
        let mut saw_slash_wrap = false;
        for width in 8..src.len() {
            let (lines, joins) = MarkdownParser::wrap_cell_text_joins(src, width);
            if lines.len() <= 1 {
                continue;
            }
            assert_eq!(joins.len(), lines.len() - 1, "width={width}");
            assert_eq!(
                reconstruct(&lines, &joins),
                src,
                "width={width} lines={lines:?} joins={joins:?}"
            );
            for (i, join) in joins.iter().enumerate() {
                if lines
                    .get(i + 1)
                    .is_some_and(|l| l.trim_start().starts_with('/'))
                {
                    assert!(
                        matches!(join, CellJoin::Gap(g) if g.contains(' ')),
                        "width={width} join before slash line must keep the space, \
                         join={join:?} lines={lines:?}"
                    );
                    saw_slash_wrap = true;
                }
            }
        }
        assert!(
            saw_slash_wrap,
            "expected some width to wrap onto a slash-leading line"
        );
    }

    #[test]
    fn foo_bar_punct_wrap_is_tight() {
        let src = "foo/bar";
        let (lines, joins) = MarkdownParser::wrap_cell_text_joins(src, 4);
        assert!(lines.len() > 1, "expected punct wrap, got {lines:?}");
        assert_eq!(joins.len(), lines.len().saturating_sub(1));
        assert_eq!(reconstruct(&lines, &joins), src);
        assert!(
            joins.iter().all(|j| matches!(j, CellJoin::Tight)),
            "punct wrap must be Tight, joins={joins:?} lines={lines:?}"
        );
    }

    #[test]
    fn hard_split_url_is_all_tight() {
        let src = "https://github.com/long/org/repo/pull/9";
        let (lines, joins) = MarkdownParser::wrap_cell_text_joins(src, 20);
        assert!(lines.len() > 1, "expected hard split, got {lines:?}");
        assert_eq!(joins.len(), lines.len().saturating_sub(1));
        assert_eq!(reconstruct(&lines, &joins), src);
        assert!(
            joins.iter().all(|j| matches!(j, CellJoin::Tight)),
            "hard-split must be Tight, joins={joins:?} lines={lines:?}"
        );
        assert_eq!(cell_wrap_joins(src, &lines), joins);
    }

    #[test]
    fn likes_long_walks_gap_is_space() {
        let src = "likes long walks";
        let (lines, joins) = MarkdownParser::wrap_cell_text_joins(src, 8);
        assert!(lines.len() > 1, "expected prose wrap, got {lines:?}");
        assert_eq!(joins.len(), lines.len().saturating_sub(1));
        assert_eq!(reconstruct(&lines, &joins), src);
        assert!(
            joins
                .iter()
                .all(|j| matches!(j, CellJoin::Gap(g) if g == " ")),
            "prose joins must be Gap(\" \"), joins={joins:?} lines={lines:?}"
        );
    }
}
