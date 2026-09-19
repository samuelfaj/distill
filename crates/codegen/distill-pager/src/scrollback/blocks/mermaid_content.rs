// Modified for Distill by Samuel Fajreldines, 2026.
//! Mermaid diagram detection and the on-screen affordance row.
//!
//! The markdown renderer draws ` ```mermaid ` blocks inline as Unicode box-drawing art.
//! This module detects those blocks in an agent message via the generic [`CodeBlockSpan`](distill_markdown::CodeBlockSpan) API.
//! It exposes each diagram's clean source so a full-fidelity PNG can be rendered on demand.
//! It never renders and tracks no per-diagram render state (rendering is lazy, driven by the affordance row on click).
//! For `auto`/`on` a clickable affordance row (`◇ mermaid [Open Image] [Copy Image Path] [Copy Source]`) is placed beneath the inline art.
//! For `off` only the inline art is shown.
//! The rendered PNG is never drawn inline; it is reached only through the affordance row's actions.

use std::ops::Range;

use distill_markdown::MarkdownRenderView;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::appearance::RenderMermaid;
use crate::scrollback::types::{BlockLine, BlockOutput};
use crate::theme::ThemeKind;

/// Fence info string identifying a Mermaid diagram.
pub const MERMAID_INFO: &str = "mermaid";

/// Subtle `◇ mermaid` marker: the leading (dim, non-clickable) label on the affordance row.
const MERMAID_LABEL: &str = "\u{25c7} mermaid";

/// Status hint shown in the affordance row while an on-click diagram render is in flight.
const MERMAID_RENDERING: &str = "rendering diagram\u{2026}";

/// Affordance-row button label: open the rendered PNG in the OS default app.
const AFFORDANCE_OPEN: &str = "[Open Image]";
/// Affordance-row button label: copy the rendered PNG's filesystem path.
const AFFORDANCE_COPY_PATH: &str = "[Copy Image Path]";
/// Affordance-row button label: copy the diagram's Mermaid source.
const AFFORDANCE_COPY_SOURCE: &str = "[Copy Source]";

/// Display-column gap between adjacent affordance-row buttons (and before the trailing status hint).
const AFFORDANCE_GAP: u16 = 3;

/// Width quantum (in display columns) for the cache key's width bucket.
/// Renders are reused across small resizes by bucketing the target width.
/// Only applies to [`MermaidRenderQuality::Terminal`]; the open tier ignores terminal width.
const MERMAID_WIDTH_BUCKET: u16 = 8;

/// Sentinel width-bucket for [`MermaidRenderQuality::Open`] (OS viewer / copy path).
/// Not derived from terminal columns, so open-tier PNGs never collide with terminal-budget renders of the same source+theme.
const OPEN_QUALITY_WIDTH_BUCKET: u16 = u16::MAX;

/// Quantize a target content-column count to the cache key's width bucket, so a sub-bucket resize maps to the same key (no re-render, no rescan).
fn width_bucket(target_width_cols: u16) -> u16 {
    target_width_cols / MERMAID_WIDTH_BUCKET
}

/// Output quality tier for a rendered Mermaid PNG.
/// `[Open Image]` / `[Copy Image Path]` use [`Open`] so the PNG is sharp in an OS image viewer.
/// A future terminal-budget path can use [`Terminal`] without sharing cache files with the open tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MermaidRenderQuality {
    /// Sized from the terminal content width (HiDPI oversample + modest caps).
    #[default]
    Terminal,
    /// Auto-scaled for OS viewers: prefer at least 2x the intrinsic SVG size and a minimum pixel width, with higher height/area headroom.
    Open,
}

/// Content hash of a diagram source: the theme/width-independent component of a [`MermaidCacheKey`].
/// Matching a pending render against this rather than the full key keeps the `rendering…` hint tied to the diagram.
/// It stays tied even if the live theme/width changes mid-render.
pub(crate) fn hash_source(source: &str) -> [u8; 32] {
    *blake3::hash(source.as_bytes()).as_bytes()
}

/// A detected Mermaid block within a rendered agent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidBlock {
    /// The clean diagram source: the fence body with container markers (blockquote `>`, list indentation) stripped and CRLF normalized.
    /// Taken from [`CodeBlockSpan::body`](distill_markdown::CodeBlockSpan::body).
    /// For a blockquoted or list-nested diagram this is the de-prefixed code, not the raw source slice.
    pub source: String,
    /// Range of pre-wrap rendered body lines this diagram occupies, as indices into [`MarkdownRenderView::lines`].
    /// Mirrors [`CodeBlockSpan::output_line_range`](distill_markdown::CodeBlockSpan::output_line_range).
    pub prewrap_line_range: Range<usize>,
}

/// Whether a fence info string identifies a Mermaid diagram: its first whitespace-delimited token equals `mermaid` (case-insensitive).
/// ` ```mermaid `, ` ```Mermaid `, and ` ```mermaid theme=base ` all match while a code block in another language does not.
fn is_mermaid_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case(MERMAID_INFO))
}

/// The view's code-block spans that are Mermaid fences, in document order.
fn mermaid_spans<'a>(
    view: &'a MarkdownRenderView,
) -> impl Iterator<Item = &'a distill_markdown::CodeBlockSpan> {
    view.code_blocks
        .iter()
        .filter(|span| is_mermaid_info(&span.info))
}

/// Filter a rendered view's code-block spans down to Mermaid fences.
/// Returns one [`MermaidBlock`] per closed ` ```mermaid ` fence, in document order, carrying the clean de-prefixed diagram source.
/// Allocates a `source` String per block; for the per-frame render path that only needs line positions use [`mermaid_block_ranges`] instead.
pub fn mermaid_blocks(view: &MarkdownRenderView) -> Vec<MermaidBlock> {
    mermaid_spans(view)
        .map(|span| MermaidBlock {
            source: span.body.clone(),
            prewrap_line_range: span.output_line_range.clone(),
        })
        .collect()
}

/// Pre-wrap line ranges of the view's Mermaid fences, in document order.
///
/// The allocation-free counterpart of [`mermaid_blocks`] for the render hot path (caption placement needs only line positions, never the source).
pub fn mermaid_block_ranges(view: &MarkdownRenderView) -> Vec<Range<usize>> {
    mermaid_spans(view)
        .map(|span| span.output_line_range.clone())
        .collect()
}

/// `GrokDay` is the only light theme. `Terminal` has no polarity of its own — a rendered diagram is a raster with a
/// baked background, so it takes the dark treatment that minimal mode already gets. It lives here (not in the
/// engine crate) so the always-compiled detection module stays independent of the optional `mermaid` feature.
pub fn theme_is_dark(theme: ThemeKind) -> bool {
    !matches!(theme, ThemeKind::GrokDay)
}

/// Cache key for a rendered diagram: content hash, theme, quality tier, and (for the terminal tier) bucketed width.
/// Keys the rendered-PNG cache. Theme, quality, and width are part of the key.
/// A theme switch, resize, or open-vs-terminal tier is then a lookup (usually a hit) or a fresh render, never a stale-color/size diagram.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MermaidCacheKey {
    /// `blake3` hash of the diagram source.
    pub source_hash: [u8; 32],
    /// Active theme (its surface color is baked into the rendered diagram).
    pub theme: ThemeKind,
    /// Target render width quantized to [`MERMAID_WIDTH_BUCKET`] columns for [`MermaidRenderQuality::Terminal`].
    /// [`OPEN_QUALITY_WIDTH_BUCKET`] for [`MermaidRenderQuality::Open`].
    pub width_bucket: u16,
    /// Terminal-budget vs OS-viewer quality tier.
    pub quality: MermaidRenderQuality,
}

impl MermaidCacheKey {
    /// Derive a cache key from a diagram's source, the active theme, target render width, and quality tier.
    /// Width is in display columns and ignored for [`MermaidRenderQuality::Open`].
    pub fn derive(
        source: &str,
        theme: ThemeKind,
        target_width_cols: u16,
        quality: MermaidRenderQuality,
    ) -> Self {
        let width_bucket = match quality {
            MermaidRenderQuality::Terminal => width_bucket(target_width_cols),
            MermaidRenderQuality::Open => OPEN_QUALITY_WIDTH_BUCKET,
        };
        Self {
            source_hash: hash_source(source),
            theme,
            width_bucket,
            quality,
        }
    }

    /// Stable, filesystem-safe filename for this key's on-disk PNG.
    /// The name carries the content hash, theme, width bucket, quality tag, and render revision.
    /// The same diagram at the same theme/width/tier reuses one file and never leaks the source in the name.
    pub fn cache_filename(&self) -> String {
        use std::fmt::Write as _;
        let mut name = String::with_capacity(64 + 24);
        for byte in self.source_hash {
            let _ = write!(name, "{byte:02x}");
        }
        let quality_tag = match self.quality {
            MermaidRenderQuality::Terminal => "t",
            MermaidRenderQuality::Open => "o",
        };
        let _ = write!(
            name,
            "-{}-{}-{}-r{RENDER_REVISION}.png",
            self.theme as u8, self.width_bucket, quality_tag
        );
        name
    }
}

/// Render-pipeline revision baked into [`MermaidCacheKey::cache_filename`].
/// Bump whenever the renderer's output changes for the same source/theme/width/tier.
const RENDER_REVISION: u8 = 3;

/// Detected Mermaid diagrams for one agent message. A detection skeleton: it records detection results and exposes
/// each diagram's source, but never renders and tracks no per-diagram render state. Constructed once at message
/// construction/finish (never per streaming chunk), like the image/video references.
#[derive(Debug, Clone, Default)]
pub struct MermaidContent {
    blocks: Vec<MermaidBlock>,
}

impl MermaidContent {
    /// Detect Mermaid blocks in a finished render view.
    pub fn from_view(view: &MarkdownRenderView) -> Self {
        Self {
            blocks: mermaid_blocks(view),
        }
    }

    /// Whether the message contains any Mermaid diagrams.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Number of detected diagrams.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Diagram source at `idx`, if it exists.
    pub fn source(&self, idx: usize) -> Option<&str> {
        self.blocks.get(idx).map(|b| b.source.as_str())
    }
}

/// How a detected Mermaid block's affordance row is presented.
/// The diagram itself is always drawn inline as Unicode art by the markdown renderer.
/// The rendered PNG is never inline (it is reached only through the affordance row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDisplay {
    /// The inline diagram art alone, with no affordance row (`render_mermaid = off`).
    SourceOnly,
    /// The inline diagram art plus the clickable affordance row (`◇ mermaid [Open Image] [Copy Image Path] [Copy Source]`): `auto`/`on`.
    Affordances,
}

/// The render engine is always compiled in, so engine availability is not a factor. Terminal graphics capability is
/// intentionally not consulted either: the affordance row is text plus mouse hit-rects, so it works everywhere.
/// (The rendered PNG opens in the OS viewer, never inline.).
pub fn mermaid_display(setting: RenderMermaid) -> MermaidDisplay {
    match setting {
        RenderMermaid::Off => MermaidDisplay::SourceOnly,
        RenderMermaid::Auto | RenderMermaid::On => MermaidDisplay::Affordances,
    }
}

/// The clickable affordance row is painted by the interactive draw loop, which minimal never runs. It would commit
/// as a blank reserved line and its buttons would be inert. Suppressing it keeps the inline diagram art (the source
/// stays natively selectable) without the dead row.
pub fn mermaid_display_static(setting: RenderMermaid, static_commit: bool) -> MermaidDisplay {
    if static_commit {
        MermaidDisplay::SourceOnly
    } else {
        mermaid_display(setting)
    }
}

/// Which click action an affordance-row button triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AffordanceKind {
    /// Render the diagram (if not already cached) at the live theme/width, then open the resulting PNG in the OS default app.
    Open,
    /// Render the diagram (if not already cached), then copy the PNG's path.
    CopyPath,
    /// Copy the diagram's Mermaid source (no render needed).
    CopySource,
}

/// One button in a diagram's affordance row, with its start column so the painted label and the click hit-rect can't drift.
/// Every button is always clickable; `[Open]`/`[Copy path]` render lazily on click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AffordanceButton {
    /// Display label, e.g. `[Open]`.
    pub label: &'static str,
    /// The click action this button triggers.
    pub kind: AffordanceKind,
    /// Start column, in display cells from the affordance row's left edge.
    pub col: u16,
}

/// The full affordance-row layout: the leading `◇ mermaid` label, the three buttons (with columns), and the trailing status hint (with column).
/// The painter and the click hit-rects draw from one source of truth and can't drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AffordanceRow {
    /// `(start_col, text)` of the leading dim, non-clickable `◇ mermaid` label.
    pub label: (u16, &'static str),
    /// `[Open Image] [Copy Image Path] [Copy Source]`, left-to-right with their columns (shifted right past the leading label).
    pub buttons: [AffordanceButton; 3],
    /// `(start_col, text)` of the trailing `rendering…` hint, present only while an on-click render for this diagram is in flight.
    pub status: Option<(u16, &'static str)>,
}

/// The affordance row's three buttons laid out left-to-right starting at `start_col` (which leaves room for the leading `◇ mermaid` label).
fn affordance_buttons(start_col: u16) -> [AffordanceButton; 3] {
    let specs = [
        (AFFORDANCE_OPEN, AffordanceKind::Open),
        (AFFORDANCE_COPY_PATH, AffordanceKind::CopyPath),
        (AFFORDANCE_COPY_SOURCE, AffordanceKind::CopySource),
    ];
    let mut col = start_col;
    specs.map(|(label, kind)| {
        let button = AffordanceButton { label, kind, col };
        col += UnicodeWidthStr::width(label) as u16 + AFFORDANCE_GAP;
        button
    })
}

/// The whole affordance-row layout for a diagram.
/// The leading `◇ mermaid` label, the three (always-clickable) buttons shifted past it, and the trailing `rendering…` hint when `rendering` is true.
/// One source of truth shared by the painter and hit-testing, so the painted columns and click hit-rects align.
pub(crate) fn affordance_row(rendering: bool) -> AffordanceRow {
    let buttons_start = UnicodeWidthStr::width(MERMAID_LABEL) as u16 + AFFORDANCE_GAP;
    let buttons = affordance_buttons(buttons_start);
    let status = rendering.then(|| buttons.last()).flatten().map(|last| {
        let after = last.col + UnicodeWidthStr::width(last.label) as u16 + AFFORDANCE_GAP;
        (after, MERMAID_RENDERING)
    });
    AffordanceRow {
        label: (0, MERMAID_LABEL),
        buttons,
        status,
    }
}

/// A diagram's clickable affordance row, anchored within a block's output.
/// Carries no raster, only the row position plus the diagram source the affordance buttons act on.
/// Rendering is lazy, driven from the source on click, so no rendered path is tracked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramAffordance {
    /// Post-wrap, block-relative row offset of the affordance row (its index in the block's `output()` lines).
    pub row_offset: u16,
    /// Diagram source (the fence body); the data every button acts on.
    pub source: String,
}

/// Post-wrap end row of every pre-wrap line, indexed by pre-wrap line number. It can't drift from the media-row /
/// hyperlink mappings.
fn prewrap_end_rows(lines: &[BlockLine]) -> Vec<usize> {
    let mut ends = Vec::new();
    for (row, prewrap) in crate::scrollback::types::prewrap_index_per_row(lines)
        .into_iter()
        .enumerate()
    {
        if let Some(slot) = ends.get_mut(prewrap) {
            *slot = row + 1;
        } else {
            ends.push(row + 1);
        }
    }
    ends
}

/// Row in `lines` at which a continuation row sits right after each non-empty pre-wrap range's last body row.
/// Callers insert back-to-front without invalidating earlier positions.
fn diagram_insert_rows(lines: &[BlockLine], ranges: &[Range<usize>]) -> Vec<(usize, usize)> {
    let ends = prewrap_end_rows(lines);
    ranges
        .iter()
        .enumerate()
        .filter(|(_, range)| !range.is_empty())
        .filter_map(|(idx, range)| ends.get(range.end - 1).map(|&insert_at| (insert_at, idx)))
        .collect()
}

/// A non-selectable continuation row inserted beneath a diagram.
/// `separator` means not selectable (excluded from copy).
/// The empty joiner marks it a continuation of the diagram's last logical line, so the pre-wrap to post-wrap walk for hyperlinks is unaffected.
fn continuation_row(line: Line<'static>) -> BlockLine {
    BlockLine::separator(line).with_joiner(Some(String::new()))
}

/// Insert a blank, non-selectable affordance row beneath each detected diagram and return one [`DiagramAffordance`]
/// per inserted row (document order). So it neither shifts pre-wrap line indices nor reaches the clipboard.
pub(crate) fn apply_affordance_rows(
    output: &mut BlockOutput,
    prewrap_ranges: &[Range<usize>],
    mut source_for: impl FnMut(usize) -> String,
) -> Vec<DiagramAffordance> {
    let inserts = diagram_insert_rows(&output.lines, prewrap_ranges);
    let affordances: Vec<DiagramAffordance> = inserts
        .iter()
        .enumerate()
        .map(|(k, &(insert_at, idx))| DiagramAffordance {
            row_offset: (insert_at + k) as u16,
            source: source_for(idx),
        })
        .collect();
    for &(insert_at, _) in inserts.iter().rev() {
        output
            .lines
            .insert(insert_at, continuation_row(Line::from(String::new())));
    }
    affordances
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::Selectable;
    use crate::syntax::get_syntect;
    use crate::theme::md_style;
    use distill_markdown::StreamingMarkdownRenderer;

    /// Render markdown to a view and collect the detected mermaid blocks.
    fn detect(src: &str, pretty: bool) -> Vec<MermaidBlock> {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), pretty);
        renderer.push(src);
        let view = renderer.finish(Some(get_syntect()));
        mermaid_blocks(&view)
    }

    #[test]
    fn detects_mermaid_and_ignores_other_fences() {
        let src =
            "intro\n\n```rust\nfn a() {}\n```\n\n```mermaid\nflowchart TD\n  A --> B\n```\n\nbye\n";
        for pretty in [true, false] {
            let blocks = detect(src, pretty);
            assert_eq!(blocks.len(), 1, "pretty={pretty}");
            let Some(block) = blocks.first() else {
                panic!("expected one mermaid block");
            };
            // `source` is the clean fence body (trailing newline included).
            assert_eq!(block.source, "flowchart TD\n  A --> B\n");
            assert!(!block.prewrap_line_range.is_empty());
        }
    }

    #[test]
    fn detects_multiple_mermaid_blocks_in_order() {
        let src = "```mermaid\nA-->B\n```\n\ntext\n\n```mermaid\nC-->D\n```\n";
        let blocks = detect(src, true);
        assert_eq!(blocks.len(), 2);
        let [b0, b1] = blocks.as_slice() else {
            panic!("expected two mermaid blocks: {blocks:?}");
        };
        assert_eq!(b0.source, "A-->B\n");
        assert_eq!(b1.source, "C-->D\n");
        assert!(b0.prewrap_line_range.end <= b1.prewrap_line_range.start);
    }

    #[test]
    fn detect_blockquote_fence_yields_clean_source() {
        // The blockquote case: the closing fence is "> ```" and the source must come out de-prefixed (no leaked "> "/"│ "), in both modes
        let src = "> ```mermaid\n> flowchart TD\n>   A --> B\n> ```\n";
        for pretty in [true, false] {
            let blocks = detect(src, pretty);
            assert_eq!(blocks.len(), 1, "pretty={pretty}");
            assert_eq!(
                blocks.first().map(|b| b.source.as_str()),
                Some("flowchart TD\n  A --> B\n"),
                "pretty={pretty}"
            );
        }
    }

    #[test]
    fn detect_list_nested_fence_yields_clean_source() {
        let src = "- item\n  ```mermaid\n  flowchart TD\n    A --> B\n  ```\n";
        for pretty in [true, false] {
            let blocks = detect(src, pretty);
            assert_eq!(blocks.len(), 1, "pretty={pretty}");
            assert_eq!(
                blocks.first().map(|b| b.source.as_str()),
                Some("flowchart TD\n  A --> B\n"),
                "pretty={pretty}"
            );
        }
    }

    #[test]
    fn detect_matches_info_first_token_case_insensitively() {
        // `mermaid theme=base`, `Mermaid` and `MERMAID` all detect; a different first token does not
        for info in ["mermaid theme=base", "Mermaid", "MERMAID"] {
            let src = format!("```{info}\nA-->B\n```\n");
            assert_eq!(detect(&src, true).len(), 1, "info={info:?}");
        }
        assert!(detect("```mermaidx\nA-->B\n```\n", true).is_empty());
        assert!(detect("```rust\nlet a = 1;\n```\n", true).is_empty());
    }

    #[test]
    fn block_ranges_match_block_spans() {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.push("```mermaid\nA-->B\n```\n\ntext\n\n```mermaid\nC-->D\n```\n");
        let view = renderer.finish(Some(get_syntect()));
        let from_blocks: Vec<Range<usize>> = mermaid_blocks(&view)
            .into_iter()
            .map(|b| b.prewrap_line_range)
            .collect();
        assert_eq!(mermaid_block_ranges(&view), from_blocks);
    }

    #[test]
    fn no_blocks_for_non_mermaid_or_empty() {
        assert!(detect("just prose, no fences\n", true).is_empty());
        assert!(detect("```python\nprint(1)\n```\n", true).is_empty());
    }

    #[test]
    fn open_fence_during_stream_is_not_detected() {
        // Detection is meaningful only on a closed fence; an unterminated fence in the streamed tail yields no block until it closes
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.push_and_render("```mermaid\nflowchart TD\n", Some(get_syntect()));
        assert!(mermaid_blocks(&renderer.view()).is_empty());
        renderer.push_and_render("A --> B\n```\n", Some(get_syntect()));
        let view = renderer.finish(Some(get_syntect()));
        assert_eq!(mermaid_blocks(&view).len(), 1);
    }

    #[test]
    fn cache_key_sensitivity() {
        let dark = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Terminal,
        );
        // Same inputs give the same key
        assert_eq!(
            dark,
            MermaidCacheKey::derive(
                "flowchart TD\nA-->B",
                ThemeKind::GrokNight,
                80,
                MermaidRenderQuality::Terminal,
            )
        );
        // A source change gives a different key
        assert_ne!(
            dark,
            MermaidCacheKey::derive(
                "flowchart TD\nA-->C",
                ThemeKind::GrokNight,
                80,
                MermaidRenderQuality::Terminal,
            )
        );
        // A theme change gives a different key
        assert_ne!(
            dark,
            MermaidCacheKey::derive(
                "flowchart TD\nA-->B",
                ThemeKind::GrokDay,
                80,
                MermaidRenderQuality::Terminal,
            )
        );
        // A width change beyond the bucket gives a different key
        assert_ne!(
            dark,
            MermaidCacheKey::derive(
                "flowchart TD\nA-->B",
                ThemeKind::GrokNight,
                160,
                MermaidRenderQuality::Terminal,
            )
        );
        // A quality tier change gives a different key (and filename)
        let open = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Open,
        );
        assert_ne!(dark, open);
        assert_ne!(dark.cache_filename(), open.cache_filename());
        // Open tier ignores terminal width.
        assert_eq!(
            open,
            MermaidCacheKey::derive(
                "flowchart TD\nA-->B",
                ThemeKind::GrokNight,
                999,
                MermaidRenderQuality::Open,
            )
        );
    }

    #[test]
    fn cache_key_width_bucketing() {
        // Widths within the same bucket collapse to one key.
        let a = MermaidCacheKey::derive(
            "x",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Terminal,
        );
        let b = MermaidCacheKey::derive(
            "x",
            ThemeKind::GrokNight,
            80 + MERMAID_WIDTH_BUCKET - 1,
            MermaidRenderQuality::Terminal,
        );
        assert_eq!(a, b);
        let c = MermaidCacheKey::derive(
            "x",
            ThemeKind::GrokNight,
            80 + MERMAID_WIDTH_BUCKET,
            MermaidRenderQuality::Terminal,
        );
        assert_ne!(a, c);
    }

    #[test]
    fn cache_key_usable_in_hash_set() {
        // The derived `Hash` must round-trip through a `HashSet` (PNG cache).
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let key = MermaidCacheKey::derive(
            "A-->B\n",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Terminal,
        );
        set.insert(key.clone());
        assert!(set.contains(&key));
        assert!(!set.contains(&MermaidCacheKey::derive(
            "A-->B\n",
            ThemeKind::GrokDay,
            80,
            MermaidRenderQuality::Terminal,
        )));
    }

    #[test]
    fn mermaid_content_skeleton_detects_source_without_render_state() {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.push("```mermaid\nA-->B\n```\n");
        let view = renderer.finish(Some(get_syntect()));
        let content = MermaidContent::from_view(&view);
        assert_eq!(content.len(), 1);
        assert!(!content.is_empty());
        // The skeleton only exposes the clean source (what the lazy click path renders); there is no per-diagram render state to track
        assert_eq!(content.source(0), Some("A-->B\n"));
        assert_eq!(content.source(1), None);
    }

    #[test]
    fn display_selection_matrix() {
        // Off: inline art only; Auto/On: inline art and the clickable affordance row (the engine is always compiled in; no capability input)
        assert_eq!(
            mermaid_display(RenderMermaid::Off),
            MermaidDisplay::SourceOnly
        );
        for setting in [RenderMermaid::Auto, RenderMermaid::On] {
            assert_eq!(
                mermaid_display(setting),
                MermaidDisplay::Affordances,
                "setting={setting:?}",
            );
        }
    }

    #[test]
    fn static_commit_forces_source_only() {
        // Minimal (static commit) suppresses the affordance row for every setting; non-static keeps the normal per-setting behavior
        for setting in [RenderMermaid::Off, RenderMermaid::Auto, RenderMermaid::On] {
            assert_eq!(
                mermaid_display_static(setting, true),
                MermaidDisplay::SourceOnly,
                "static_commit must force SourceOnly (setting={setting:?})",
            );
            assert_eq!(
                mermaid_display_static(setting, false),
                mermaid_display(setting),
                "non-static must match mermaid_display (setting={setting:?})",
            );
        }
    }

    #[test]
    fn affordance_buttons_start_after_the_label_with_a_fixed_gap() {
        // Buttons are laid out from `start_col` (which leaves room for the leading `◇ mermaid` label) with a fixed inter-button gap
        // Every button is clickable (no per-button enable flag)
        let start = UnicodeWidthStr::width(MERMAID_LABEL) as u16 + AFFORDANCE_GAP;
        let buttons = affordance_buttons(start);
        assert_eq!(
            buttons.map(|b| (b.label, b.kind)),
            [
                ("[Open Image]", AffordanceKind::Open),
                ("[Copy Image Path]", AffordanceKind::CopyPath),
                ("[Copy Source]", AffordanceKind::CopySource),
            ],
        );
        assert_eq!(buttons[0].col, start);
        for win in buttons.windows(2) {
            let [prev, next] = win else {
                panic!("windows(2): {win:?}");
            };
            let prev_end = prev.col + UnicodeWidthStr::width(prev.label) as u16;
            assert_eq!(next.col, prev_end + AFFORDANCE_GAP, "fixed gap: {win:?}");
        }
    }

    #[test]
    fn affordance_row_has_label_and_shows_status_only_while_rendering() {
        // Display widths: `◇ mermaid` (9) + gap (3), so buttons start at col 12
        // [Open Image] (12), [Copy Image Path] (17), [Copy Source] (13) with a 3-col gap between
        let start = UnicodeWidthStr::width(MERMAID_LABEL) as u16 + AFFORDANCE_GAP;
        let idle = affordance_row(false);
        assert_eq!(idle.label, (0, MERMAID_LABEL));
        assert_eq!(idle.buttons.map(|b| b.col), [start, start + 15, start + 35]);
        assert_eq!(
            idle.buttons.map(|b| b.label),
            ["[Open Image]", "[Copy Image Path]", "[Copy Source]"],
        );
        assert_eq!(idle.status, None, "no status unless a render is in flight");

        // While rendering, the `rendering…` hint sits after the last button plus a gap; the label and button columns are unchanged
        let busy = affordance_row(true);
        let last = busy.buttons[2];
        let after = last.col + UnicodeWidthStr::width(last.label) as u16 + AFFORDANCE_GAP;
        assert_eq!(busy.status, Some((after, MERMAID_RENDERING)));
        assert_eq!(busy.label, idle.label);
        assert_eq!(busy.buttons, idle.buttons);
    }

    /// Build a `BlockOutput` whose joiners describe the given pre-wrap to row layout.
    /// `wraps[i]` is the number of post-wrap rows pre-wrap line `i` occupies (at least 1).
    fn output_with_wraps(wraps: &[usize]) -> BlockOutput {
        let mut lines = Vec::new();
        for (pre, &rows) in wraps.iter().enumerate() {
            for row in 0..rows {
                let joiner = if row == 0 { None } else { Some(String::new()) };
                lines.push(BlockLine::text(format!("pre{pre}-row{row}")).with_joiner(joiner));
            }
        }
        BlockOutput { lines }
    }

    fn caption_text(line: &BlockLine) -> String {
        line.content
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// One-element range slice (a bound value, so it sidesteps the `single_range_in_vec_init` lint that a `&[a..b]` literal trips).
    fn one(range: Range<usize>) -> Vec<Range<usize>> {
        vec![range]
    }

    #[test]
    fn prewrap_end_rows_handles_wrapping() {
        // pre0: 1 row, pre1: 2 rows, pre2: 1 row, so the rows are [0],[1,2],[3]
        let out = output_with_wraps(&[1, 2, 1]);
        assert_eq!(prewrap_end_rows(&out.lines), vec![1, 3, 4]);
    }

    // -- theme mapping and cache filename -------------------------------------

    #[test]
    fn theme_is_dark_maps_grokday_to_light_only() {
        assert!(
            !theme_is_dark(ThemeKind::GrokDay),
            "GrokDay is the light theme"
        );
        for dark in [
            ThemeKind::GrokNight,
            ThemeKind::TokyoNight,
            ThemeKind::RosePineMoon,
            ThemeKind::OscuraMidnight,
        ] {
            assert!(theme_is_dark(dark), "{dark:?} should be dark");
        }
    }

    #[test]
    fn cache_filename_is_stable_and_keyed() {
        let a = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Terminal,
        );
        // Deterministic, ends in .png, and carries the hex hash, theme, and bucket fields
        assert_eq!(a.cache_filename(), a.cache_filename());
        assert!(a.cache_filename().ends_with(".png"));
        assert!(
            a.cache_filename()
                .ends_with(&format!("-r{RENDER_REVISION}.png")),
            "filename must carry the render revision: {}",
            a.cache_filename()
        );
        // A different theme / source / width gives a different filename
        let b = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::GrokDay,
            80,
            MermaidRenderQuality::Terminal,
        );
        assert_ne!(a.cache_filename(), b.cache_filename());
        let c = MermaidCacheKey::derive(
            "flowchart TD\nA-->C",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Terminal,
        );
        assert_ne!(a.cache_filename(), c.cache_filename());
        let open = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::GrokNight,
            80,
            MermaidRenderQuality::Open,
        );
        assert_ne!(a.cache_filename(), open.cache_filename());
        // No raw source in the name (only the hash).
        assert!(!a.cache_filename().contains("flowchart"));
    }

    #[test]
    fn apply_affordance_rows_inserts_blank_rows_and_reports_source() {
        // Two diagrams at pre-wrap 0..1 and 2..3 in a 4-line output; each affordance row carries its own diagram's source (document order)
        let mut out = output_with_wraps(&[1, 1, 1, 1]);
        let sources = ["A-->B\n", "C-->D\n"];
        let mut iter = sources.into_iter();
        let affs = apply_affordance_rows(&mut out, &[0..1, 2..3], |_| {
            iter.next().unwrap().to_string()
        });

        // One blank, non-selectable continuation row inserted per diagram.
        assert_eq!(out.lines.len(), 6);
        let blanks: Vec<usize> = out
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(l.selectable, Selectable::None))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(blanks, vec![1, 4], "affordance rows after each diagram");

        // The reported offsets point at the inserted rows in the FINAL output.
        assert_eq!(affs.len(), 2);
        let [a0, a1] = affs.as_slice() else {
            panic!("expected two affordances: {affs:?}");
        };
        assert_eq!(a0.row_offset, 1);
        assert_eq!(a1.row_offset, 4);
        assert!(matches!(
            out.lines.get(1).map(|l| &l.selectable),
            Some(&Selectable::None)
        ));
        assert!(matches!(
            out.lines.get(4).map(|l| &l.selectable),
            Some(&Selectable::None)
        ));
        assert_eq!(a0.source, "A-->B\n");
        assert_eq!(a1.source, "C-->D\n");
    }

    #[test]
    fn apply_affordance_rows_offset_follows_wrapped_body() {
        // The diagram's single body pre-wrap line (index 1) wraps to two rows [1,2]; the affordance row must land after the LAST wrapped row (3)
        let mut out = output_with_wraps(&[1, 2, 1]);
        let affs = apply_affordance_rows(&mut out, &one(1..2), |_| "A-->B\n".to_string());
        assert_eq!(affs.len(), 1);
        assert_eq!(affs.first().map(|a| a.row_offset), Some(3));
        assert!(matches!(
            out.lines.get(3).map(|l| &l.selectable),
            Some(&Selectable::None)
        ));
        // The trailing pre2 row is pushed down, not overwritten.
        let Some(line) = out.lines.get(4) else {
            panic!("expected trailing pre2 row: {out:?}");
        };
        assert_eq!(caption_text(line), "pre2-row0");
    }
}
