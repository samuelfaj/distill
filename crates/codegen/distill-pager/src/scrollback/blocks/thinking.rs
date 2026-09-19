use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthStr;

use crate::render::color::blend_line_with_default;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::theme::Theme;

use super::markdown_content::MarkdownContent;
use super::quote_bar::QuoteBarStrip;
use crate::appearance::AppearanceConfig;

/// TODO: hard-coded because `AppView::minimal_key_intercept` matches this chord literally instead of going through the keybinding registry.
/// Resolve the label from the registry once it does, so a remap is advertised correctly.
const EXPAND_HINT: &str = "ctrl+e to expand";

const EXPAND_HINT_GAP: &str = "  ";

/// Append the dim `(ctrl+e to expand)` hint to a collapsed header line. The `Collapsed` guard matters because
/// `render_empty_placeholder` reuses the collapsed renderer for an empty body in other modes. There the hint would
/// be a lie.
fn append_expand_hint(line: Line<'static>, ctx: &BlockContext) -> Line<'static> {
    if !ctx
        .appearance
        .scrollback
        .blocks
        .thinking
        .collapsed_expand_hint
        || ctx.mode != DisplayMode::Collapsed
    {
        return line;
    }
    let hint = format!("{EXPAND_HINT_GAP}({EXPAND_HINT})");
    let used: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if used + hint.width() > ctx.content_width() {
        return line;
    }
    let mut line = line;
    line.spans.push(Span::styled(hint, Theme::current().dim()));
    line
}

/// Attributes only, no foreground: minimal's terminal-native palette makes color-based de-emphasis a no-op.
/// Terminals that merely *ignore* SGR 3 (tmux without `sitm`) are not gated: there is no reliable probe, and they
/// keep the other two cues.
fn body_emphasis_patch(ctx: &BlockContext) -> Option<Style> {
    if !ctx.appearance.scrollback.blocks.thinking.body_dim_italic {
        return None;
    }
    let mut modifiers = Modifier::DIM;
    if !crate::glyphs::is_legacy_windows_console() {
        modifiers |= Modifier::ITALIC;
    }
    Some(Style::new().add_modifier(modifiers))
}

/// Columns spent by the `┃ ` body prefix when the rail renders under the
/// header's bullet ([`crate::appearance::ThinkingConfig::rail_under_bullet`]).
const BODY_RAIL_WIDTH: usize = 2;

/// Whether the reasoning rail renders inside the body rows (directly below the header's bullet) instead of as the reserved accent column.
/// Minimal-only: there every other block starts flush at column 0 with its own `◆`.
/// An accent column would indent the header's diamond out of line and read as a second, different gutter treatment.
///
/// Gated on `accent_enabled` like the accent-column rail it replaces (`rail_style`):
/// a pager.toml `accent_enabled = false` turns off the reasoning rail wherever it is drawn.
fn rail_under_bullet(ctx: &BlockContext) -> bool {
    let cfg = &ctx.appearance.scrollback.blocks.thinking;
    cfg.rail_under_bullet && cfg.accent_enabled
}

/// One body row's rail prefix: dim like the accent-column rail it replaces, so it stays a quiet structural cue under `NO_COLOR`.
fn body_rail_span(ctx: &BlockContext) -> Span<'static> {
    let cfg = &ctx.appearance.scrollback.blocks.thinking;
    Span::styled(
        format!("{} ", crate::glyphs::accent_bar()),
        Style::default().fg(cfg.accent).add_modifier(Modifier::DIM),
    )
}

/// Prefix every body row with [`body_rail_span`] so the rail runs from under the header's bullet down the whole body.
/// The prefix span is excluded from selection (same mechanism as `prepend_bullet`).
///
/// Markdown leaves code-block fill on the line *style*, which `Buffer::set_line`
/// patches under every span — the rail prefix included. Hoist it onto the
/// per-line [`BlockLine::background`] starting after the rail (the same
/// line-style → background split as `MarkdownContent::output`), so the fill
/// spans the row's content but the rail cell stays unshaded.
fn apply_body_rail(output: &mut BlockOutput, ctx: &BlockContext) {
    if !rail_under_bullet(ctx) {
        return;
    }
    for line in &mut output.lines {
        line.content.spans.insert(0, body_rail_span(ctx));
        crate::scrollback::types::shift_selection_metadata_for_prefix(line, 1);
        if let Some(bg) = line.content.style.bg.take() {
            line.background = Some(bg);
            line.bg_start_col = BODY_RAIL_WIDTH as u16;
        }
    }
}

/// Body wrap width: the in-body rail prefix spends [`BODY_RAIL_WIDTH`] of the content columns.
/// The markdown must wrap that much narrower to keep `desired_height` and the painted rows in agreement.
fn body_wrap_width(ctx: &BlockContext) -> usize {
    let width = ctx.width as usize;
    if rail_under_bullet(ctx) {
        width.saturating_sub(BODY_RAIL_WIDTH).max(1)
    } else {
        width
    }
}

/// Block displaying agent thinking content with markdown rendering.
///
/// Uses [`MarkdownContent`] for incremental markdown rendering with cached word-wrapping, plus special display modes:
/// - **Collapsed**: Shows "Thought" or "Thought for Xs" if time is set
/// - **Truncated** (default): Shows "…" then the last N lines
/// - **Expanded**: Full content
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    content: MarkdownContent,

    /// Optional elapsed time in milliseconds (from server).
    /// When set, collapsed view shows "Thought for Xs".
    elapsed_time_ms: Option<i64>,
    /// When the thinking block started (local timestamp for live elapsed).
    started_at: Option<std::time::Instant>,
}
impl ThinkingBlock {
    /// Create a new thinking block with complete text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            content: MarkdownContent::new(text),
            elapsed_time_ms: None,
            started_at: None,
        }
    }

    /// Create an empty block for streaming.
    pub fn streaming() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: Some(std::time::Instant::now()),
        }
    }

    /// Create an empty streaming block for historical replay. A local wall-clock timer would then freeze to ~0ms in
    /// [`finish`] and render a bogus "Thought for 0.0s".
    pub fn streaming_replay() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: None,
        }
    }

    /// Push a streaming chunk of markdown text.
    pub fn push_chunk(&mut self, chunk: &str) {
        self.content.push_chunk(chunk);
    }

    /// Push a chunk without rendering immediately.
    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        self.content.push_chunk_deferred(chunk);
    }

    /// Finish streaming and do a full re-render for safety.
    /// Freezes the local elapsed time from `started_at`.
    /// The collapsed view then shows the actual wall-clock duration the user experienced, not the server-reported delta.
    pub fn finish(&mut self) {
        self.content.finish();
        // Freeze local elapsed if no server time has been set.
        // The local timer (started_at to now) captures the full duration from block creation to finish, which is what the user perceives
        if self.elapsed_time_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_time_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    /// Get the source text.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Get the elapsed thinking time in milliseconds.
    ///
    /// Returns server-reported time if available, otherwise live elapsed from `started_at` (for running thinking blocks).
    pub fn elapsed_time_ms(&self) -> Option<i64> {
        match self.elapsed_time_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    /// Set the elapsed time (in milliseconds).
    ///
    /// When set, the collapsed view will show "Thought for Xs".
    pub fn set_elapsed_time_ms(&mut self, time_ms: Option<i64>) {
        self.elapsed_time_ms = time_ms;
    }

    /// Set the raw mode, re-rendering if it changed.
    pub fn set_raw_mode(&mut self, raw: bool) {
        self.content.set_raw_mode(raw);
    }

    /// Access the underlying markdown content (for viewer item building).
    pub fn content(&self) -> &MarkdownContent {
        &self.content
    }

    /// Mutable access to the underlying markdown content.
    pub fn content_mut(&mut self) -> &mut MarkdownContent {
        &mut self.content
    }

    /// Get copyable text for this block.
    /// When `raw` is true, returns the raw markdown source.
    /// When `raw` is false, returns the rendered text (styles stripped).
    pub fn copy_text(&self, raw: bool) -> String {
        if raw {
            self.content.text()
        } else {
            self.content.rendered_plain_text()
        }
    }

    /// Format elapsed time for display.
    fn format_time(&self) -> Option<String> {
        self.elapsed_time_ms.map(|ms| {
            let secs = ms as f64 / 1000.0;
            if secs < 60.0 {
                format!("{:.1}s", secs)
            } else {
                let mins = (secs / 60.0).floor() as u32;
                let remaining = secs - (mins as f64 * 60.0);
                format!("{}m{:.0}s", mins, remaining)
            }
        })
    }

    /// Build the header line: "Thinking." (running) or "Thought for Xs" (done). Respects muted_collapsed: when
    /// collapsed and muting is on, uses muted style. The selected header then reads as undimmed, the same rule as the
    /// tool-call variants.
    fn header_line(&self, ctx: &BlockContext) -> Line<'static> {
        let theme = Theme::current();
        let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
        let thinking_cfg = &ctx.appearance.scrollback.blocks.thinking;
        let is_collapsed = ctx.mode == DisplayMode::Collapsed;
        let is_muted = is_collapsed && ctx.mute_when_collapsed(tool_cfg.muted_collapsed);

        // Bright on selection or config opt-in, but never while muted; keeps legacy-ConHost collapse uniformly muted
        let use_bright = !is_muted && (ctx.is_selected || thinking_cfg.header_bright);

        let label_style = if use_bright {
            theme.primary().bold()
        } else {
            theme.muted().bold()
        };

        let detail_style = theme.muted();

        if ctx.is_running {
            Line::from(Span::styled("Thinking…", label_style))
        } else if let Some(time_str) = self.format_time() {
            Line::from(vec![
                Span::styled("Thought", label_style),
                Span::styled(format!(" for {time_str}"), detail_style),
            ])
        } else {
            Line::from(Span::styled("Thought", label_style))
        }
    }

    /// Render the collapsed view: header line only, truncated to fit.
    fn render_collapsed(&self, ctx: &BlockContext) -> BlockOutput {
        let line = self.header_line(ctx);
        let line = append_expand_hint(line, ctx);
        let line = crate::render::line_utils::truncate_line(line, ctx.content_width());
        BlockOutput {
            lines: vec![BlockLine::separator(line)],
        }
    }

    /// Prepend header to output, if header config is enabled.
    /// Full mode keeps a blank row under the title. Minimal (`rail_under_bullet`) does not — the body starts on the next row.
    fn maybe_prepend_header(&self, mut output: BlockOutput, ctx: &BlockContext) -> BlockOutput {
        if ctx.appearance.scrollback.blocks.thinking.header {
            if !rail_under_bullet(ctx) {
                output.lines.insert(0, BlockLine::separator(Line::from("")));
            }
            output
                .lines
                .insert(0, BlockLine::separator(self.header_line(ctx)));
        }
        output
    }

    /// One wrapped markdown line rendered as a selectable, blended [`BlockLine`]. Quote-bar exclusion must run before
    /// blending: blending rewrites span fg colors, which would defeat the bar-style detection. Blending preserves span
    /// structure, so the computed span indices stay valid after it.
    fn thinking_body_line(
        line: &Line<'static>,
        joiner: &Option<String>,
        strip: &QuoteBarStrip,
        bg_base: Color,
        fg_default: Color,
        blend_factor: f32,
        emphasis: Option<Style>,
    ) -> BlockLine {
        let mut content = line.clone();
        let selectable = strip.selectable(&mut content);
        let indent_width = if joiner.is_some() {
            super::markdown_content::compute_subsequent_indent_width(line)
        } else {
            0
        };
        let mut blended = blend_line_with_default(content, bg_base, fg_default, blend_factor);
        if let Some(emphasis) = emphasis {
            for span in &mut blended.spans {
                span.style = span.style.patch(emphasis);
            }
        }
        let mut block_line = BlockLine::styled(blended)
            .with_selection_range(Some(0))
            .with_joiner(joiner.clone());
        block_line.selectable = selectable;
        block_line.indent_width = indent_width;
        block_line
    }

    /// Render truncated view: optional header, then "…", then the last N lines.
    fn render_truncated(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let n = config.truncated_lines as usize;
        let width = body_wrap_width(ctx);
        let blend_factor = config.bg_blend;
        let emphasis = body_emphasis_patch(ctx);
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let total = wrapped.lines.len();
            if total <= n {
                // Content fits within N lines, show all (with blending)
                let mut output = BlockOutput {
                    lines: wrapped
                        .lines
                        .iter()
                        .zip(wrapped.joiners.iter())
                        .map(|(line, joiner)| {
                            Self::thinking_body_line(
                                line,
                                joiner,
                                &strip,
                                bg_base,
                                fg_default,
                                blend_factor,
                                emphasis,
                            )
                        })
                        .collect(),
                };
                apply_body_rail(&mut output, ctx);
                return self.maybe_prepend_header(output, ctx);
            }

            // Build truncated output: "…" then the last N lines
            let theme = Theme::current();
            let mut output_lines = Vec::with_capacity(n + 1);

            // Ellipsis line
            let ellipsis = Line::from(Span::styled("…", theme.muted()));
            output_lines.push(ellipsis.into());

            // Last N lines (with blending)
            for (line, joiner) in wrapped
                .lines
                .iter()
                .zip(wrapped.joiners.iter())
                .skip(total - n)
            {
                output_lines.push(Self::thinking_body_line(
                    line,
                    joiner,
                    &strip,
                    bg_base,
                    fg_default,
                    blend_factor,
                    emphasis,
                ));
            }

            let mut output = BlockOutput {
                lines: output_lines,
            };
            apply_body_rail(&mut output, ctx);
            self.maybe_prepend_header(output, ctx)
        })
    }

    /// Render expanded view: full content.
    fn render_expanded(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let width = body_wrap_width(ctx);
        let blend_factor = config.bg_blend;
        let emphasis = body_emphasis_patch(ctx);
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let mut output = BlockOutput {
                lines: wrapped
                    .lines
                    .iter()
                    .zip(wrapped.joiners.iter())
                    .map(|(line, joiner)| {
                        Self::thinking_body_line(
                            line,
                            joiner,
                            &strip,
                            bg_base,
                            fg_default,
                            blend_factor,
                            emphasis,
                        )
                    })
                    .collect(),
            };
            apply_body_rail(&mut output, ctx);
            self.maybe_prepend_header(output, ctx)
        })
    }

    /// Placeholder for empty thinking block; shows the same header as collapsed mode ("Thinking…" or "Thought for Xs").
    fn render_empty_placeholder(&self, ctx: &BlockContext) -> BlockOutput {
        self.render_collapsed(ctx)
    }

    /// The rail's accent style (also the running bullet's), independent of
    /// where the rail is drawn (accent column vs in-body prefix).
    fn rail_style(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        if !cfg.accent_enabled {
            return None;
        }
        // No accent when collapsed; accent is only for expanded/truncated content
        // TODO: revisit if we want accent in collapsed state with header enabled.
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        if cfg.animate && ctx.is_running {
            Some(AccentStyle::animated(cfg.accent))
        } else {
            Some(AccentStyle::static_color(cfg.accent))
        }
    }
}

impl BlockContent for ThinkingBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        match ctx.mode {
            DisplayMode::Collapsed => self.render_collapsed(ctx),
            DisplayMode::Truncated => self.render_truncated(ctx),
            DisplayMode::Expanded => self.render_expanded(ctx),
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        // With the in-body rail the accent column must stay unreserved and unpainted; the rail lives in the body rows instead
        if rail_under_bullet(ctx) {
            return None;
        }
        self.rail_style(ctx)
    }

    /// Thinking bullet: default (None) when not running, animated when running.
    /// This means collapsed thinking shows gray bullet, running thinking syncs with accent.
    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.is_running {
            // Sync bullet with the rail animation when running — via
            // `rail_style`, not `accent`, so the in-body rail mode keeps the
            // animated bullet.
            self.rail_style(ctx)
        } else {
            None // default gray/primary
        }
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn accent_background(&self, _ctx: &BlockContext) -> bool {
        false
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        true
    }

    fn is_foldable(&self) -> bool {
        true
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        if is_running {
            match current {
                DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
                DisplayMode::Expanded => DisplayMode::Truncated,
            }
        } else {
            match current {
                DisplayMode::Collapsed => DisplayMode::Expanded,
                DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
            }
        }
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        if is_running {
            DisplayMode::Truncated
        } else {
            DisplayMode::Collapsed
        }
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        Some(DisplayMode::Collapsed)
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        let has_header_visible = ctx.mode == DisplayMode::Collapsed || cfg.header;
        has_header_visible
            && ctx
                .appearance
                .scrollback
                .blocks
                .tool
                .bullet
                .char()
                .is_some()
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        // Use expanded (bright) styling, not muted collapsed
        let bright_ctx = BlockContext {
            mode: DisplayMode::Expanded,
            ..ctx.clone()
        };
        Some(Text::from(self.header_line(&bright_ctx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceConfig;
    use crate::scrollback::types::Selectable;

    fn ctx(mode: DisplayMode, width: u16) -> BlockContext {
        BlockContext {
            mode,
            is_running: false,
            width,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    #[test]
    fn collapsed_thinking_header_is_non_selectable() {
        let block = ThinkingBlock::new("hello world");
        let out = block.output(&ctx(DisplayMode::Collapsed, 40));
        assert_eq!(out.lines.len(), 1);
        let Some(line) = out.lines.first() else {
            panic!("expected a header line: {out:?}");
        };
        assert!(matches!(line.selectable, Selectable::None));
        assert_eq!(line.selection_range, None);
    }

    #[test]
    fn prepended_thinking_header_is_non_selectable() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = true;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };
        let block = ThinkingBlock::new("hello world");
        let out = block.output(&ctx);

        assert!(out.lines.len() >= 3);
        assert!(matches!(
            out.lines.first().map(|l| &l.selectable),
            Some(&Selectable::None)
        ));
        assert!(matches!(
            out.lines.get(1).map(|l| &l.selectable),
            Some(&Selectable::None)
        ));
        assert!(
            out.lines
                .iter()
                .skip(2)
                .all(|line| line.selection_range == Some(0))
        );
    }

    #[test]
    fn thinking_body_lines_keep_markdown_range_ids() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = false;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 10)
        };
        let block = ThinkingBlock::new("hello world this should wrap across lines");
        let out = block.output(&ctx);
        assert!(out.lines.len() > 1);
        assert!(out.lines.iter().all(|line| line.selection_range == Some(0)));
    }

    #[test]
    fn thinking_quote_line_selection_excludes_bar_prefix() {
        use crate::scrollback::types::{derive_selection_text, line_plain_text};

        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = false;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };
        let block = ThinkingBlock::new("> QUOTE alpha");
        let out = block.output(&ctx);

        let line = out
            .lines
            .iter()
            .find(|l| line_plain_text(&l.content).contains("QUOTE"))
            .expect("quote line rendered");
        assert!(line_plain_text(&line.content).starts_with("│ "));
        assert!(matches!(line.selectable, Selectable::Spans(_)));
        assert_eq!(derive_selection_text(line), "QUOTE alpha");
    }

    #[test]
    fn thinking_body_is_not_dimmed_or_italic_by_default() {
        let block = ThinkingBlock::new("plain reasoning text");
        let out = block.output(&ctx(DisplayMode::Expanded, 40));
        let body = out.lines.last().expect("body line");
        for span in &body.content.spans {
            assert!(
                !span.style.add_modifier.contains(Modifier::ITALIC),
                "default appearance must not italicize reasoning: {span:?}"
            );
            assert!(
                !span.style.add_modifier.contains(Modifier::DIM),
                "default appearance must not dim reasoning: {span:?}"
            );
        }
    }

    /// Under the terminal-native (`NO_COLOR`) palette the `bg_blend` fade is a no-op, so the distinction has to live in SGR attributes.
    #[test]
    fn thinking_body_dim_italic_survives_the_terminal_native_palette() {
        let _guard = crate::theme::cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct LockReset;
        impl Drop for LockReset {
            fn drop(&mut self) {
                crate::theme::cache::set_terminal_native_lock(false);
            }
        }
        let _reset = LockReset;
        crate::theme::cache::set_terminal_native_lock(true);

        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = false;
        appearance.scrollback.blocks.thinking.body_dim_italic = true;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };
        let block = ThinkingBlock::new("weighing the `options` with **care**");
        let out = block.output(&ctx);

        assert!(!out.lines.is_empty());
        for line in &out.lines {
            for span in &line.content.spans {
                assert!(
                    span.style.add_modifier.contains(Modifier::DIM),
                    "every reasoning span must be dim under the native palette: {span:?}"
                );
                assert!(
                    span.style.add_modifier.contains(Modifier::ITALIC),
                    "every reasoning span must be italic: {span:?}"
                );
            }
        }
    }

    /// `accent_enabled = false` in pager.toml turned off the accent-column rail
    /// (`rail_style`); the in-body rail must honor the same switch.
    #[test]
    fn body_rail_honors_accent_enabled() {
        let appearance_with = |accent_enabled: bool| {
            let mut appearance = AppearanceConfig::default();
            appearance.scrollback.blocks.thinking.header = true;
            appearance.scrollback.blocks.thinking.rail_under_bullet = true;
            appearance.scrollback.blocks.thinking.accent_enabled = accent_enabled;
            appearance
        };
        let block = ThinkingBlock::new("hello world");
        let rail = crate::glyphs::accent_bar();

        let with_rail = block.output(&BlockContext {
            appearance: appearance_with(true),
            ..ctx(DisplayMode::Expanded, 40)
        });
        assert!(
            with_rail
                .lines
                .iter()
                .skip(1)
                .all(|l| crate::scrollback::types::line_plain_text(&l.content).starts_with(rail)),
            "accent enabled: every row below the header carries the rail"
        );

        let without = block.output(&BlockContext {
            appearance: appearance_with(false),
            ..ctx(DisplayMode::Expanded, 40)
        });
        assert!(
            without
                .lines
                .iter()
                .all(|l| !crate::scrollback::types::line_plain_text(&l.content).contains(rail)),
            "accent_enabled = false must not draw the in-body rail"
        );
    }

    /// Markdown puts code-block fill on the line style, which `set_line` patches
    /// under every span — the rail prefix included. `apply_body_rail` must hoist
    /// it to the per-line background, starting after the rail.
    ///
    /// Exercises `apply_body_rail` with a synthetic filled line: the test theme
    /// renders fenced code without a `code_background`, so a real markdown
    /// round-trip cannot produce the fill here.
    #[test]
    fn body_rail_keeps_code_block_fill_off_the_rail_cell() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.rail_under_bullet = true;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };

        let fill = Color::Rgb(30, 30, 46);
        let code_line = Line::from(Span::raw("let x = 1;")).style(Style::default().bg(fill));
        let mut output = BlockOutput {
            lines: vec![BlockLine::styled(code_line)],
        };
        apply_body_rail(&mut output, &ctx);

        let Some(line) = output.lines.first() else {
            panic!("expected a railed line: {output:?}");
        };
        assert_eq!(
            line.content.style.bg, None,
            "a line-style bg would paint under the rail prefix"
        );
        assert_eq!(
            line.background,
            Some(fill),
            "the fill must move onto the per-line background"
        );
        assert_eq!(
            line.bg_start_col, BODY_RAIL_WIDTH as u16,
            "the hoisted fill must start after the rail prefix"
        );
    }

    #[test]
    fn collapsed_header_advertises_the_expand_key_without_adding_a_row() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.collapsed_expand_hint = true;
        let hinted = |mode, width| BlockContext {
            appearance: appearance.clone(),
            ..ctx(mode, width)
        };
        let text_of = |out: &BlockOutput| {
            out.lines
                .iter()
                .map(|l| crate::scrollback::types::line_plain_text(&l.content))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let block = ThinkingBlock::new("hello world");

        let plain = block.output(&ctx(DisplayMode::Collapsed, 60));
        assert!(!text_of(&plain).contains(EXPAND_HINT));

        let out = block.output(&hinted(DisplayMode::Collapsed, 60));
        assert_eq!(out.lines.len(), 1, "the hint must not add a row");
        assert!(text_of(&out).contains(EXPAND_HINT), "{:?}", text_of(&out));

        // Too narrow for header and hint: the header must not be pushed into truncation by a hint that then gets dropped anyway
        let narrow = block.output(&hinted(DisplayMode::Collapsed, 12));
        assert_eq!(narrow.lines.len(), 1);
        let narrow_text = text_of(&narrow);
        assert!(!narrow_text.contains(EXPAND_HINT), "{narrow_text:?}");
        assert_eq!(
            narrow_text,
            text_of(&block.output(&ctx(DisplayMode::Collapsed, 12))),
            "a hint that does not fit must leave the header untouched"
        );

        // Not folded in these modes, so there is nothing to expand.
        for mode in [DisplayMode::Expanded, DisplayMode::Truncated] {
            let out = block.output(&hinted(mode, 60));
            assert!(!text_of(&out).contains(EXPAND_HINT), "{mode:?}");
        }

        // An empty body reuses the collapsed renderer as its placeholder in every mode; nothing to open there either
        let empty = ThinkingBlock::new("");
        for mode in [DisplayMode::Expanded, DisplayMode::Truncated] {
            let out = empty.output(&hinted(mode, 60));
            assert!(!text_of(&out).contains(EXPAND_HINT), "empty/{mode:?}");
        }
    }
}
