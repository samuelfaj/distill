// Modified for Distill by Samuel Fajreldines, 2026.
//! BlockContent trait, RenderBlock enum, and shared bullet rendering.

use ratatui::style::Style;
use ratatui::text::{Span, Text};

use crate::appearance::AppearanceConfig;
use crate::inline_media_ffmpeg::inline_media_reserved_rows;
use crate::prompt_images::{InlineMediaInfo, ScrollbackImageRef, ScrollbackVideoRef};
use distill_pager_diff::DiffHunk;

use super::blocks::mermaid_content::DiagramAffordance;
use super::blocks::{
    AgentMessageBlock, BgTaskBlock, BtwBlock, ContextInfoBlock, EditToolCallBlock,
    ExecuteToolCallBlock, LineRange, ListDirToolCallBlock, MemoryCaptureBlock, OtherToolCallBlock,
    ReadToolCallBlock, SearchFileMatch, SearchToolCallBlock, SessionEvent, SessionEventBlock,
    SubagentBlock, SubagentBlockKind, SystemMessageBlock, ThinkingBlock, ToolCallBlock,
    UserPromptBlock, WorkflowBlock,
};
use super::types::{
    AccentStyle, BlockBackground, BlockContext, BlockOutput, DisplayMode, RenderedBlockOutput,
    Selectable, SelectionBoundaries, derive_selection_text,
};

/// The trailing inline image anchored within a block's rendered output. Mermaid diagrams do not use this path; they
/// render as a code block plus a text affordance row instead.
#[derive(Debug, Clone)]
pub struct AnchoredMedia {
    /// Media metadata (path, raster dimensions, type).
    pub info: InlineMediaInfo,
    /// Post-wrap row offset, from the block's first content row, where the image's top edge is anchored.
    /// (Tool-media blocks have no top vpad, so this is measured from the entry top.)
    pub row_offset: u16,
    /// Height of the image area in rows (the crop region).
    pub rows: u16,
}

/// Each block type implements this trait. The RenderBlock enum delegates to the inner type via the `delegate_block!` macro below.
/// This trait describes *what* to render (content, styles, padding).
/// Actual rendering to a Buffer is done via the `Renderable` trait from `ui/render/renderable.rs`.
pub trait BlockContent {
    /// Produce renderable content for the given context.
    fn output(&self, ctx: &BlockContext) -> BlockOutput;

    /// Accent line style (color, animation).
    /// Returns `None` for blocks without an accent line.
    /// Returns `Some(AccentStyle)` with color and animation info.
    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle>;

    /// Bullet/icon color style. Default: delegates to `accent()`, so the bullet matches the accent color. a failed Read
    /// has no accent but a red bullet.
    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        self.accent(ctx)
    }

    /// Whether accent column gets block's background.
    fn accent_background(&self, _ctx: &BlockContext) -> bool {
        false
    }

    /// Block content area background.
    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    /// Vertical padding (blank line with accent top/bottom).
    ///
    /// Borrows the appearance rather than taking a [`BlockContext`] so the O(history) height passes do not build one per entry.
    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        true
    }

    fn has_vpad(&self, ctx: &BlockContext) -> bool {
        self.has_vpad_for(&ctx.appearance)
    }

    /// Whether block supports raw mode toggle.
    fn has_raw_mode(&self) -> bool {
        false
    }

    /// Whether block can be collapsed/expanded.
    fn is_foldable(&self) -> bool {
        true
    }

    /// Get the next display mode when toggling fold. Default behavior: toggle between Collapsed and Expanded. Blocks
    /// can override for 3-way cycling. The `is_running` parameter allows blocks to behave differently while streaming.
    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        let _ = is_running; // Default ignores running state
        match current {
            DisplayMode::Collapsed => DisplayMode::Expanded,
            DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }

    /// Get the display mode to use when explicitly collapsing (left/h key).
    /// Default: Collapsed. Blocks can override to use a different minimum mode when running.
    /// Execute blocks use Truncated while running to keep showing the streaming output preview.
    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        let _ = is_running;
        DisplayMode::Collapsed
    }

    /// Get the default display mode for this block type.
    ///
    /// Default: Expanded. Blocks can override (e.g., thinking defaults to Truncated).
    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Expanded
    }

    /// Display mode to adopt when the entry finishes running. Called by `finish_running()`. Returns `Some(mode)` to
    /// override the current display mode, or `None` to keep it as-is. Default: `None` (no change). Blocks that
    /// auto-collapse on finish (thinking, execute) or on error (edit) should override this.
    fn finished_display_mode(&self) -> Option<DisplayMode> {
        None
    }

    /// Whether block can be selected via j/k navigation.
    /// Non-selectable blocks are rendered normally but skipped during navigation.
    fn is_selectable(&self) -> bool {
        true
    }

    /// Whether this block should display a bullet/icon prefix.
    /// Default: `false`. Override to opt in (e.g., ToolCallBlock when the bullet is configured, ThinkingBlock when collapsed).
    /// The bullet character and color are determined by the appearance config and accent style.
    fn has_bullet(&self, _ctx: &BlockContext) -> bool {
        false
    }

    /// Preamble content for the fullscreen viewer. Returns `None` for blocks without a natural header.
    fn preamble(&self, _ctx: &BlockContext) -> Option<Text<'static>> {
        None
    }

    /// Whether this block participates in dense group rendering. Groupable blocks that are adjacent form a "group":
    /// they render without gap rows between them when collapsed. Non-groupable blocks always have gap rows around them
    /// and break any adjacent group.
    fn is_groupable(&self) -> bool {
        false
    }

    /// Image file references in this block (default: none).
    fn image_references(&self) -> &[ScrollbackImageRef] {
        &[]
    }

    /// Video file references in this block (default: none).
    fn video_references(&self) -> &[ScrollbackVideoRef] {
        &[]
    }

    /// Inline media metadata for blocks that should display media inline in the scrollback.
    /// The renderer uses this to reserve height and the draw loop uses it to emit terminal image escape sequences. Default: none.
    fn inline_media(&self) -> Option<InlineMediaInfo> {
        None
    }

    /// Blocks without `inline_media()` return empty. Every non-media block (all agent messages, all non-media tool
    /// calls) returns here without building `output()`. The `output()` rebuild below runs only for a media block (today
    /// only `OtherToolCallBlock`), whose `output()` is a cheap 2 or 3 line build.
    fn inline_media_placements(&self, ctx: &BlockContext) -> Vec<AnchoredMedia> {
        let Some(info) = self.inline_media() else {
            return Vec::new();
        };
        // Trailing geometry: the image starts one row below the block's text (`content_lines + 1`)
        // It is fitted to the same cell budget `EntryRenderer::inline_media_rows` reserves
        // The line count needs the laid-out output; the generic wrapper can't know it otherwise
        let content_lines = self.output(ctx).lines.len() as u16;
        let (rows, _total_rows) = inline_media_reserved_rows(&info, ctx.width);
        vec![AnchoredMedia {
            info,
            row_offset: content_lines + 1,
            rows,
        }]
    }

    /// Clickable affordance rows for the diagrams in this block's `output()` (the `auto`/`on` Mermaid display).
    /// Default: none (only agent messages with diagrams override this).
    fn diagram_affordances(&self, _ctx: &BlockContext) -> Vec<DiagramAffordance> {
        Vec::new()
    }

    /// Rows this block inserts into `output()` that the source-text height *estimate* cannot see.
    /// The off-screen estimate adds them so it never under-reserves.
    /// The only such rows today are Mermaid treatment rows (one affordance row or fallback caption per detected diagram). Default: `0`.
    fn estimate_extra_rows(&self) -> u16 {
        0
    }

    /// For media blocks on terminals without inline-graphics support, returns `(path, is_video)`.
    /// The block uses it to render a clickable text `[Open]` line.
    /// `None` on graphics terminals (the overlay hosts its own buttons).
    fn inline_open_button(&self) -> Option<(std::path::PathBuf, bool)> {
        None
    }
}

/// Prepend a bullet/icon span to the first line of a block's output. Called by `RenderBlock::output()` when
/// `has_bullet()` returns true. The bullet character comes from the appearance config. The color comes from the
/// block's `bullet()` method. `None` defaults to gray when collapsed, primary when expanded.
pub fn prepend_bullet(output: &mut BlockOutput, ctx: &BlockContext, bullet: Option<AccentStyle>) {
    let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
    let Some(bullet_str) = tool_cfg.bullet.char() else {
        return;
    };
    let Some(first_line) = output.lines.first_mut() else {
        return;
    };

    let theme = crate::theme::Theme::current();

    let color = match bullet {
        Some(style) => style.color,
        None => {
            if ctx.mode == DisplayMode::Collapsed {
                theme.gray
            } else {
                theme.gray_bright
            }
        }
    };

    let bullet_span = Span::styled(format!("{bullet_str} "), Style::default().fg(color));
    first_line.content.spans.insert(0, bullet_span);
    crate::scrollback::types::shift_selection_metadata_for_prefix(first_line, 1);
}

/// A stub block that just renders plain text.
/// Used during bootstrap before real blocks are implemented.
#[derive(Debug, Clone)]
pub struct StubBlock {
    pub text: String,
    pub accent_color: ratatui::style::Color,
    /// Whether this stub participates in dense group rendering.
    /// Default: `true`. Set to `false` for stubs that simulate non-groupable blocks (e.g., agent messages) in tests.
    pub groupable: bool,
    /// Per-line background applied to every output line, plus its panel flag ([`BlockLine::background_is_panel`]).
    /// Lets renderer tests exercise line-background handling with a FIXED injected color.
    /// It stays independent of `Theme::current()`, whose process-global kind and color-level state is not stable across a parallel test run.
    pub line_bg: Option<(ratatui::style::Color, bool)>,
}

impl StubBlock {
    pub fn new(text: impl Into<String>, accent_color: ratatui::style::Color) -> Self {
        Self {
            text: text.into(),
            accent_color,
            groupable: true,
            line_bg: None,
        }
    }

    /// Create a non-groupable stub (simulates AgentMessage-like behavior in tests).
    pub fn non_groupable(text: impl Into<String>, accent_color: ratatui::style::Color) -> Self {
        Self {
            text: text.into(),
            accent_color,
            groupable: false,
            line_bg: None,
        }
    }

    /// Give every output line a background, marked panel (decorative, like a tool preview) or not (semantic, like diff shading).
    pub fn with_line_bg(mut self, color: ratatui::style::Color, panel: bool) -> Self {
        self.line_bg = Some((color, panel));
        self
    }
}

impl BlockContent for StubBlock {
    fn output(&self, _ctx: &BlockContext) -> BlockOutput {
        let mut output = BlockOutput::plain(&self.text);
        if let Some((color, panel)) = self.line_bg {
            for line in &mut output.lines {
                line.background = Some(color);
                line.background_is_panel = panel;
            }
        }
        output
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.is_running {
            Some(AccentStyle::animated(self.accent_color))
        } else {
            Some(AccentStyle::static_color(self.accent_color))
        }
    }

    fn is_groupable(&self) -> bool {
        self.groupable
    }
}

/// BlockContent is implemented via match-based delegation so we can intercept `output()` to conditionally prepend bullets based on `has_bullet()`.
#[derive(Debug, Clone)]
pub enum RenderBlock {
    /// Stub block for testing.
    Stub(StubBlock),
    /// User's prompt.
    UserPrompt(UserPromptBlock),
    /// Agent's response message.
    AgentMessage(AgentMessageBlock),
    /// Tool call result (Execute, Read, Edit, ListDir, Search, Other).
    ToolCall(ToolCallBlock),
    /// Thinking/reasoning content.
    Thinking(ThinkingBlock),
    /// System message (arbitrary text).
    System(SystemMessageBlock),
    /// Session-level event (typed: turn completed, cancelled, failed, etc.).
    SessionEvent(SessionEventBlock),
    /// Background task (always collapsed, animated bullet while running).
    BgTask(BgTaskBlock),
    /// Subagent lifecycle (started / completed / failed).
    Subagent(SubagentBlock),
    Workflow(WorkflowBlock),
    /// /btw side-question response (golden accent).
    Btw(BtwBlock),
    /// `/context` snapshot with categorical bar and breakdown.
    ContextInfo(ContextInfoBlock),
    /// Debug-only generated memory details.
    MemoryCapture(MemoryCaptureBlock),
}

/// Delegate a method call to the inner block variant.
macro_rules! delegate_block {
    ($self:expr, $method:ident ( $($arg:expr),* )) => {
        match $self {
            RenderBlock::Stub(b) => b.$method($($arg),*),
            RenderBlock::UserPrompt(b) => b.$method($($arg),*),
            RenderBlock::AgentMessage(b) => b.$method($($arg),*),
            RenderBlock::ToolCall(b) => b.$method($($arg),*),
            RenderBlock::Thinking(b) => b.$method($($arg),*),
            RenderBlock::System(b) => b.$method($($arg),*),
            RenderBlock::SessionEvent(b) => b.$method($($arg),*),
            RenderBlock::BgTask(b) => b.$method($($arg),*),
            RenderBlock::Subagent(b) => b.$method($($arg),*),
            RenderBlock::Workflow(b) => b.$method($($arg),*),
            RenderBlock::Btw(b) => b.$method($($arg),*),
            RenderBlock::ContextInfo(b) => b.$method($($arg),*),
            RenderBlock::MemoryCapture(b) => b.$method($($arg),*),
        }
    };
}

fn plain_text_from_output(
    output: &BlockOutput,
    boundaries: &SelectionBoundaries,
) -> Option<String> {
    let mut result = String::new();
    let mut wrote_any = false;

    for (line_index, line) in output.lines.iter().enumerate() {
        if matches!(line.selectable, Selectable::None) {
            continue;
        }

        let text = derive_selection_text(line);
        if wrote_any {
            result.push_str(line.joiner.as_deref().unwrap_or("\n"));
        }
        if let Some(boundary) = boundaries.get(line_index) {
            result.push_str(&boundary.apply(text, true, true));
        } else {
            result.push_str(&text);
        }
        wrote_any = true;
    }

    wrote_any.then_some(result)
}

/// Join source-text parts for full-text search, dropping `None` and empty strings so absent fields never inject blank lines or false matches.
///
/// Returns `None` when nothing remains: a block with no source text is left out of the index rather than indexed as an empty string.
pub(crate) fn join_searchable(parts: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let joined = parts
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

impl RenderBlock {
    pub(crate) fn rendered_output(&self, ctx: &BlockContext) -> RenderedBlockOutput {
        let mut rendered = match self {
            RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) => edit.rendered_output(ctx),
            RenderBlock::ToolCall(ToolCallBlock::SentMessage(message)) => {
                message.rendered_output(ctx)
            }
            _ => return RenderedBlockOutput::from(self.output(ctx)),
        };
        if self.has_bullet(ctx) {
            prepend_bullet(&mut rendered.output, ctx, self.bullet(ctx));
        }
        rendered
    }
}

impl BlockContent for RenderBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let mut output = delegate_block!(self, output(ctx));
        if delegate_block!(self, has_bullet(ctx)) {
            let bullet = delegate_block!(self, bullet(ctx));
            prepend_bullet(&mut output, ctx, bullet);
        }
        output
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        delegate_block!(self, accent(ctx))
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        delegate_block!(self, bullet(ctx))
    }

    fn accent_background(&self, ctx: &BlockContext) -> bool {
        delegate_block!(self, accent_background(ctx))
    }

    fn background(&self, ctx: &BlockContext) -> BlockBackground {
        delegate_block!(self, background(ctx))
    }

    fn has_vpad_for(&self, appearance: &AppearanceConfig) -> bool {
        delegate_block!(self, has_vpad_for(appearance))
    }

    fn has_raw_mode(&self) -> bool {
        delegate_block!(self, has_raw_mode())
    }

    fn is_foldable(&self) -> bool {
        delegate_block!(self, is_foldable())
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        delegate_block!(self, next_fold_mode(current, is_running))
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        delegate_block!(self, collapse_mode(is_running))
    }

    fn default_display_mode(&self) -> DisplayMode {
        delegate_block!(self, default_display_mode())
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        delegate_block!(self, finished_display_mode())
    }

    fn is_selectable(&self) -> bool {
        delegate_block!(self, is_selectable())
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        delegate_block!(self, has_bullet(ctx))
    }

    fn is_groupable(&self) -> bool {
        delegate_block!(self, is_groupable())
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        delegate_block!(self, preamble(ctx))
    }

    fn image_references(&self) -> &[crate::prompt_images::ScrollbackImageRef] {
        delegate_block!(self, image_references())
    }

    fn video_references(&self) -> &[crate::prompt_images::ScrollbackVideoRef] {
        delegate_block!(self, video_references())
    }

    fn inline_media(&self) -> Option<InlineMediaInfo> {
        delegate_block!(self, inline_media())
    }

    fn inline_media_placements(&self, ctx: &BlockContext) -> Vec<AnchoredMedia> {
        delegate_block!(self, inline_media_placements(ctx))
    }

    fn diagram_affordances(&self, ctx: &BlockContext) -> Vec<DiagramAffordance> {
        delegate_block!(self, diagram_affordances(ctx))
    }

    fn estimate_extra_rows(&self) -> u16 {
        delegate_block!(self, estimate_extra_rows())
    }

    fn inline_open_button(&self) -> Option<(std::path::PathBuf, bool)> {
        delegate_block!(self, inline_open_button())
    }
}

impl RenderBlock {
    /// Create a stub block (groupable by default).
    pub fn stub(text: impl Into<String>, accent_color: ratatui::style::Color) -> Self {
        RenderBlock::Stub(StubBlock::new(text, accent_color))
    }

    /// Create a non-groupable stub block (simulates AgentMessage-like behavior in tests).
    pub fn stub_non_groupable(
        text: impl Into<String>,
        accent_color: ratatui::style::Color,
    ) -> Self {
        RenderBlock::Stub(StubBlock::non_groupable(text, accent_color))
    }

    pub fn user_prompt(text: impl Into<String>) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::new(text))
    }

    pub fn bash_prompt(text: impl Into<String>) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::bash(text))
    }

    pub fn skill_prompt(text: impl Into<String>) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::skill(text))
    }

    /// Create a user prompt block with recognized mid-text slash tokens styled in the skill accent (byte ranges into `text`).
    pub fn user_prompt_with_skill_tokens(
        text: impl Into<String>,
        ranges: Vec<std::ops::Range<usize>>,
    ) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::with_skill_tokens(text, ranges))
    }

    pub fn cron_prompt(text: impl Into<String>) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::cron(text))
    }

    /// Create a mid-turn interjection prompt block (standard user prompt rendering, excluded from shell prompt-index bookkeeping).
    pub fn interjection_prompt(text: impl Into<String>) -> Self {
        RenderBlock::UserPrompt(UserPromptBlock::interjection(text))
    }

    pub fn agent_message(text: impl Into<String>) -> Self {
        RenderBlock::AgentMessage(AgentMessageBlock::new(text))
    }

    pub fn memory_capture(
        from_turn: u32,
        through_turn: u32,
        entries: Vec<distill_shell::extensions::notification::MemoryCaptureDebugEntry>,
    ) -> Self {
        RenderBlock::MemoryCapture(MemoryCaptureBlock::new(from_turn, through_turn, entries))
    }

    /// Create an empty streaming agent message block.
    ///
    /// Use `as_agent_message_mut()` to get the block and call `push_chunk()` to append streaming content.
    pub fn agent_message_streaming() -> Self {
        RenderBlock::AgentMessage(AgentMessageBlock::streaming())
    }

    /// Create an Other tool call block (generic, parsed from name).
    pub fn tool_call(kind: impl Into<String>, summary: impl Into<String>, success: bool) -> Self {
        let kind_str = kind.into();
        let mut block = ToolCallBlock::from_name(&kind_str, summary);
        if let ToolCallBlock::Other(ref mut b) = block
            && !success
        {
            b.error = Some("Tool call failed".to_string());
        }
        RenderBlock::ToolCall(block)
    }

    /// Create an Other tool call block with output (for expanded view).
    pub fn tool_call_with_details(
        kind: impl Into<String>,
        summary: impl Into<String>,
        success: bool,
        output: impl Into<String>,
    ) -> Self {
        let mut block = OtherToolCallBlock::new(kind, summary).with_output(output);
        if !success {
            block = block.with_error("Tool call failed");
        }
        RenderBlock::ToolCall(ToolCallBlock::Other(block))
    }

    pub fn execute(command: impl Into<String>) -> Self {
        RenderBlock::ToolCall(ToolCallBlock::Execute(ExecuteToolCallBlock::new(command)))
    }

    /// Use `error: None` for success, `error: Some(msg)` for failure.
    pub fn execute_with_output(
        command: impl Into<String>,
        output: impl Into<String>,
        error: Option<impl Into<String>>,
    ) -> Self {
        let mut block = ExecuteToolCallBlock::new(command).with_output(output);
        if let Some(e) = error {
            block = block.with_error(e);
        }
        RenderBlock::ToolCall(ToolCallBlock::Execute(block))
    }

    pub fn read(path: impl Into<String>, line_range: Option<LineRange>) -> Self {
        let mut block = ReadToolCallBlock::new(path);
        if let Some(range) = line_range {
            block = block.with_line_range(range);
        }
        RenderBlock::ToolCall(ToolCallBlock::Read(block))
    }

    pub fn list_dir(path: impl Into<String>) -> Self {
        RenderBlock::ToolCall(ToolCallBlock::ListDir(ListDirToolCallBlock::new(path)))
    }

    pub fn list_dir_with_output(path: impl Into<String>, output: impl Into<String>) -> Self {
        RenderBlock::ToolCall(ToolCallBlock::ListDir(
            ListDirToolCallBlock::new(path).with_output(output),
        ))
    }

    pub fn search(
        pattern: impl Into<String>,
        match_count: usize,
        file_matches: Vec<SearchFileMatch>,
    ) -> Self {
        RenderBlock::ToolCall(ToolCallBlock::Search(
            SearchToolCallBlock::new(pattern).with_matches(match_count, file_matches),
        ))
    }

    pub fn edit_with_hunks(path: impl Into<String>, hunks: Vec<DiffHunk>) -> Self {
        RenderBlock::ToolCall(ToolCallBlock::Edit(EditToolCallBlock::new(path, hunks)))
    }

    /// Create a simple Edit block (no hunks, for legacy compatibility).
    pub fn edit(path: impl Into<String>, edit_info: Option<String>) -> Self {
        let mut block = EditToolCallBlock::new(path, vec![]);
        if let Some(info) = edit_info {
            // Parse edit count from info like "2 edits"
            if let Some(count) = info.split_whitespace().next().and_then(|s| s.parse().ok()) {
                block = block.with_edit_count(count);
            }
        }
        RenderBlock::ToolCall(ToolCallBlock::Edit(block))
    }

    pub fn thinking(text: impl Into<String>) -> Self {
        RenderBlock::Thinking(ThinkingBlock::new(text))
    }

    /// The time is displayed in collapsed mode as "Thought for Xs".
    pub fn thinking_with_time(text: impl Into<String>, time_ms: i64) -> Self {
        let mut block = ThinkingBlock::new(text);
        block.set_elapsed_time_ms(Some(time_ms));
        RenderBlock::Thinking(block)
    }

    /// Create an empty streaming thinking block.
    ///
    /// Use `as_thinking_mut()` to get the block and call `push_chunk()` to append streaming content.
    pub fn thinking_streaming() -> Self {
        RenderBlock::Thinking(ThinkingBlock::streaming())
    }

    /// Create an empty streaming thinking block for historical replay.
    /// Does not start a local elapsed timer; the collapsed "Thought for Xs" duration comes from the server-reported elapsed instead.
    /// See [`ThinkingBlock::streaming_replay`].
    pub fn thinking_streaming_replay() -> Self {
        RenderBlock::Thinking(ThinkingBlock::streaming_replay())
    }

    pub fn system(text: impl Into<String>) -> Self {
        RenderBlock::System(SystemMessageBlock::new(text))
    }

    /// Create a `/context` snapshot block.
    /// The block stores the raw `ContextInfo` snapshot and model name and rebuilds its styled output on every redraw.
    /// Theme switches thus take effect without re-running `/context`.
    pub fn context_info(
        snapshot: distill_shell::session::ContextInfo,
        model: impl Into<String>,
    ) -> Self {
        RenderBlock::ContextInfo(ContextInfoBlock::new(snapshot, model))
    }

    pub fn session_event(event: SessionEvent) -> Self {
        RenderBlock::SessionEvent(SessionEventBlock::new(event))
    }

    /// Create a "Task started" background task block.
    pub fn bg_task(command: impl Into<String>, task_id: impl Into<String>) -> Self {
        RenderBlock::BgTask(BgTaskBlock::started(command, task_id))
    }

    /// Create a "Task completed" background task block.
    pub fn bg_task_completed(
        command: impl Into<String>,
        task_id: impl Into<String>,
        elapsed: std::time::Duration,
    ) -> Self {
        RenderBlock::BgTask(BgTaskBlock::completed(command, task_id, elapsed))
    }

    /// Create a "Task failed" background task block.
    pub fn bg_task_failed(
        command: impl Into<String>,
        task_id: impl Into<String>,
        elapsed: std::time::Duration,
        exit_code: Option<i32>,
        signal: Option<String>,
    ) -> Self {
        RenderBlock::BgTask(BgTaskBlock::failed(
            command, task_id, elapsed, exit_code, signal,
        ))
    }

    /// Set the description on a `BgTask` block (builder pattern, no-op for other variants).
    pub fn with_bg_task_description(mut self, description: Option<String>) -> Self {
        if let RenderBlock::BgTask(ref mut b) = self {
            b.description = description;
        }
        self
    }

    pub fn as_tool_call_mut(&mut self) -> Option<&mut ToolCallBlock> {
        match self {
            RenderBlock::ToolCall(b) => Some(b),
            _ => None,
        }
    }

    /// This is useful for streaming: get the block, then call `push_chunk()` to append streaming content.
    pub fn as_agent_message_mut(&mut self) -> Option<&mut AgentMessageBlock> {
        match self {
            RenderBlock::AgentMessage(b) => Some(b),
            _ => None,
        }
    }

    /// The read-only counterpart of [`as_agent_message_mut`](Self::as_agent_message_mut).
    /// Use it to inspect a message's content (e.g. its detected diagrams) without mutating it.
    pub fn as_agent_message(&self) -> Option<&AgentMessageBlock> {
        match self {
            RenderBlock::AgentMessage(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_thinking_mut(&mut self) -> Option<&mut ThinkingBlock> {
        match self {
            RenderBlock::Thinking(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_user_prompt(&self) -> bool {
        matches!(self, RenderBlock::UserPrompt(_))
    }

    /// Used by the entry cache to decide whether selection state should invalidate the cached output.
    /// Tool call variants undim their collapsed header text when selected.
    pub fn is_tool_call(&self) -> bool {
        matches!(self, RenderBlock::ToolCall(_))
    }

    /// Used by the entry cache for the same reason as `is_tool_call`: the thinking header text undims on selection.
    pub fn is_thinking(&self) -> bool {
        matches!(self, RenderBlock::Thinking(_))
    }

    /// Used by the entry cache for the same reason as `is_tool_call`: the bold "Task" label undims on selection.
    pub fn is_bg_task(&self) -> bool {
        matches!(self, RenderBlock::BgTask(_))
    }

    /// Used by the entry cache for the same reason as `is_tool_call`: the bold "Subagent" label undims on selection.
    pub fn is_subagent(&self) -> bool {
        matches!(self, RenderBlock::Subagent(_))
    }

    pub fn is_agent_message(&self) -> bool {
        matches!(self, RenderBlock::AgentMessage(_))
    }

    /// A session event that closes a turn (`Worked for …`, cancelled, failed); see [`SessionEvent::is_turn_terminal`].
    pub fn is_turn_terminal_marker(&self) -> bool {
        matches!(self, RenderBlock::SessionEvent(b) if b.event.is_turn_terminal())
    }

    /// Check if this block is a plan mode tool call (enter or exit). Exact-matches the canonical tool-name set rather
    /// than substring-matching the human title. Titles incorporate raw model/user input, so a substring match on
    /// `"enter_plan_mode"` false-positives on ordinary tool calls.
    pub fn is_plan_mode_tool(&self) -> bool {
        use super::blocks::ToolCallBlock;
        const PLAN_MODE_TOOL_NAMES: &[&str] = &[
            "EnterPlanMode",
            "ExitPlanMode",
            "enter_plan_mode",
            "exit_plan_mode",
            "Plan: Enter",
            "Plan: Exit",
        ];
        matches!(
            self,
            RenderBlock::ToolCall(ToolCallBlock::Other(b))
                if PLAN_MODE_TOOL_NAMES.iter().any(|n| b.name == *n)
        )
    }

    /// Absolute path of the media (image/video) this block references, if it is a media-generation tool result.
    /// Used to resolve the short relative paths the model prints in prose (`images/1.jpg`) to a clickable link.
    pub(crate) fn media_ref_path(&self) -> Option<std::path::PathBuf> {
        match self {
            RenderBlock::ToolCall(ToolCallBlock::Other(b)) => b.media_ref_path(),
            _ => None,
        }
    }

    /// Drop rebuildable render caches held inside the block. Currently the markdown word-wrap cache on markdown-backed
    /// blocks (agent messages, thinking, /btw). The source text and pre-wrap render stay. the next `output()` call
    /// rebuilds the wrap transparently. Called by `ScrollbackState::evict_offscreen_render_caches`.
    pub fn evict_render_caches(&self) {
        match self {
            RenderBlock::AgentMessage(b) => b.content().evict_wrap_cache(),
            RenderBlock::Thinking(b) => b.content().evict_wrap_cache(),
            RenderBlock::Btw(b) => b.content().evict_wrap_cache(),
            _ => {}
        }
    }

    /// Returns the default static accent color for the block type.
    /// Uses RGB colors from theme for proper fade blending.
    pub fn accent_color(&self) -> Option<ratatui::style::Color> {
        use crate::theme::Theme;
        let theme = Theme::current();

        match self {
            RenderBlock::UserPrompt(_) => Some(theme.text_primary),
            RenderBlock::AgentMessage(_) => None,
            RenderBlock::Workflow(_) => None,
            RenderBlock::ToolCall(block) => {
                // Execute: Green for success, red for failure
                // Read/Edit/ListDir/Search: No accent
                match block {
                    ToolCallBlock::Execute(b) => {
                        if b.is_success() {
                            Some(theme.accent_success)
                        } else {
                            Some(theme.accent_error)
                        }
                    }
                    ToolCallBlock::SentMessage(b) => Some(match &b.presentation {
                        crate::scrollback::blocks::SentMessagePresentation::Sending
                        | crate::scrollback::blocks::SentMessagePresentation::Sent => {
                            theme.accent_tool
                        }
                        crate::scrollback::blocks::SentMessagePresentation::Rejected { .. } => {
                            theme.accent_error
                        }
                        crate::scrollback::blocks::SentMessagePresentation::Unconfirmed {
                            ..
                        } => theme.warning,
                    }),
                    ToolCallBlock::Other(b) => {
                        if b.is_success() {
                            Some(theme.accent_tool)
                        } else {
                            Some(theme.accent_error)
                        }
                    }
                    _ => None, // No accent for Read/Edit/ListDir/Search
                }
            }
            RenderBlock::Thinking(_) => Some(theme.accent_thinking),
            RenderBlock::BgTask(block) => {
                if block.is_running() {
                    Some(theme.accent_running)
                } else {
                    None
                }
            }
            RenderBlock::Subagent(block) => {
                if block.is_running() {
                    Some(theme.accent_running)
                } else {
                    None
                }
            }
            RenderBlock::System(_)
            | RenderBlock::SessionEvent(_)
            | RenderBlock::ContextInfo(_)
            | RenderBlock::MemoryCapture(_) => None,
            RenderBlock::Btw(_) => Some(theme.accent_plan),
            RenderBlock::Stub(block) => Some(block.accent_color),
        }
    }

    pub fn has_normal_fullscreen_viewer(&self) -> bool {
        match self {
            RenderBlock::AgentMessage(_)
            | RenderBlock::Thinking(_)
            | RenderBlock::ToolCall(ToolCallBlock::Execute(_))
            | RenderBlock::ToolCall(ToolCallBlock::Edit(_))
            | RenderBlock::ToolCall(ToolCallBlock::WebFetch(_))
            | RenderBlock::ToolCall(ToolCallBlock::WebSearch(_))
            | RenderBlock::ToolCall(ToolCallBlock::IntegrationSearch(_))
            | RenderBlock::ToolCall(ToolCallBlock::UseTool(_))
            | RenderBlock::BgTask(_) => true,
            RenderBlock::ToolCall(ToolCallBlock::Read(b)) => b.has_content(),
            RenderBlock::ToolCall(ToolCallBlock::Search(b)) => b.error.is_none(),
            RenderBlock::ToolCall(ToolCallBlock::ListDir(b)) => {
                b.error.is_none() && !b.output.is_empty()
            }
            _ => false,
        }
    }

    pub fn supports_fullscreen(&self) -> bool {
        self.has_normal_fullscreen_viewer()
            || !self.image_references().is_empty()
            || !self.video_references().is_empty()
    }

    /// The child session this row can open in the takeover: a Subagent row, or a sent-message row whose target
    /// resolved through a spawn. The caller still has to own that child view.
    pub(crate) fn child_session_id(&self) -> Option<&str> {
        if let RenderBlock::Subagent(block) = self {
            Some(&block.child_session_id)
        } else if let RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)) = self {
            block.child_session_id()
        } else {
            None
        }
    }

    pub fn supports_copy(&self) -> bool {
        matches!(
            self,
            RenderBlock::UserPrompt(_)
                | RenderBlock::AgentMessage(_)
                | RenderBlock::Thinking(_)
                | RenderBlock::ToolCall(ToolCallBlock::Execute(_))
                | RenderBlock::ToolCall(ToolCallBlock::Read(_))
                | RenderBlock::ToolCall(ToolCallBlock::Edit(_))
                | RenderBlock::ToolCall(ToolCallBlock::WebFetch(_))
                | RenderBlock::ToolCall(ToolCallBlock::WebSearch(_))
        )
    }

    pub fn is_drag_block_selectable(&self) -> bool {
        !matches!(self, RenderBlock::Stub(_))
    }

    pub fn copy_visible_text_in_state(&self, ctx: &BlockContext) -> Option<String> {
        let rendered = self.rendered_output(ctx);
        plain_text_from_output(&rendered.output, &rendered.boundaries)
    }

    /// The `raw` parameter controls whether markdown blocks return raw source or rendered plain text.
    /// It is ignored for non-markdown block types.
    pub fn copy_text(&self, raw: bool) -> Option<String> {
        match self {
            RenderBlock::UserPrompt(b) => Some(b.copy_text()),
            RenderBlock::AgentMessage(b) => Some(b.copy_text(raw)),
            RenderBlock::Thinking(b) => Some(b.copy_text(raw)),
            RenderBlock::ToolCall(ToolCallBlock::Execute(b)) => Some(b.copy_text()),
            RenderBlock::ToolCall(ToolCallBlock::Read(b)) => b.content.clone(),
            RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => Some(b.copy_text()),
            RenderBlock::ToolCall(ToolCallBlock::WebFetch(b)) => Some(b.copy_text()),
            RenderBlock::ToolCall(ToolCallBlock::WebSearch(b)) => Some(b.copy_text()),
            RenderBlock::ToolCall(ToolCallBlock::IntegrationSearch(b)) => Some(b.copy_text()),
            RenderBlock::ToolCall(ToolCallBlock::UseTool(b)) => Some(b.copy_text()),
            _ => None,
        }
    }

    /// Get block metadata text for clipboard (e.g., command for execute, path for edit).
    ///
    /// Returns `None` for blocks without copyable metadata.
    pub fn copy_meta(&self) -> Option<String> {
        match self {
            RenderBlock::ToolCall(ToolCallBlock::Execute(b)) => Some(b.command.clone()),
            RenderBlock::ToolCall(ToolCallBlock::Read(b)) => Some(b.path.clone()),
            RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => Some(b.path.clone()),
            RenderBlock::ToolCall(ToolCallBlock::WebFetch(b)) => Some(b.url.clone()),
            RenderBlock::ToolCall(ToolCallBlock::WebSearch(b)) => Some(b.query.clone()),
            RenderBlock::ToolCall(ToolCallBlock::Search(b)) => Some(b.pattern.clone()),
            RenderBlock::BgTask(b) => Some(b.command.clone()),
            _ => None,
        }
    }

    /// Full searchable text of this block for full-text scrollback search. Searching `is important` finds the rendered
    /// `is important` rather than missing the source `is important`. This reads the renderer's last-rendered view
    /// without running layout (`output()` word-wrap) or re-highlighting syntax.
    pub fn searchable_text(&self) -> Option<String> {
        match self {
            RenderBlock::Stub(b) => join_searchable([Some(b.text.clone())]),
            RenderBlock::UserPrompt(b) => join_searchable([Some(b.text.clone())]),
            RenderBlock::AgentMessage(b) => join_searchable([Some(b.copy_text(false))]),
            RenderBlock::Thinking(b) => join_searchable([Some(b.copy_text(false))]),
            RenderBlock::System(b) => join_searchable([Some(b.text.clone())]),
            RenderBlock::SessionEvent(b) => join_searchable([Some(b.event.message())]),
            RenderBlock::BgTask(b) => {
                join_searchable([Some(b.command.clone()), b.description.clone()])
            }
            RenderBlock::Workflow(b) => {
                join_searchable([Some(b.name.clone()), Some(b.objective.clone())])
            }
            RenderBlock::Subagent(b) => {
                // Only the failed variant carries an error string worth indexing.
                let error = match &b.kind {
                    SubagentBlockKind::Failed { error, .. } => error.clone(),
                    SubagentBlockKind::Started
                    | SubagentBlockKind::Completed { .. }
                    | SubagentBlockKind::Cancelled { .. } => None,
                };
                join_searchable([
                    Some(b.description.clone()),
                    Some(b.subagent_type.clone()),
                    b.persona.clone(),
                    b.role.clone(),
                    b.model.clone(),
                    b.activity_label.clone(),
                    error,
                ])
            }
            RenderBlock::Btw(b) => join_searchable([
                Some(b.question.clone()),
                Some(b.content().rendered_plain_text()),
            ]),
            RenderBlock::ContextInfo(b) => join_searchable([Some(b.model.clone())]),
            RenderBlock::MemoryCapture(b) => join_searchable([Some(b.searchable_text())]),
            RenderBlock::ToolCall(tc) => tc.searchable_text(),
        }
    }

    /// Label for the copy-meta shortcut hint (e.g., "copy cmd", "copy path").
    ///
    /// Returns `None` for blocks without copyable metadata.
    pub fn copy_meta_label(&self) -> Option<&'static str> {
        match self {
            RenderBlock::ToolCall(ToolCallBlock::Execute(_)) => Some("copy cmd"),
            RenderBlock::ToolCall(ToolCallBlock::Read(_)) => Some("copy path"),
            RenderBlock::ToolCall(ToolCallBlock::Edit(_)) => Some("copy path"),
            RenderBlock::ToolCall(ToolCallBlock::WebFetch(_)) => Some("copy url"),
            RenderBlock::ToolCall(ToolCallBlock::WebSearch(_)) => Some("copy query"),
            RenderBlock::ToolCall(ToolCallBlock::Search(_)) => Some("copy pattern"),
            _ => None,
        }
    }

    /// Access pre-wrap hyperlink targets via a closure, avoiding allocation.
    /// The hyperlinks are in the markdown renderer's coordinate space (pre-wrap line index, display-cell column range).
    /// The caller is responsible for mapping through word-wrapping and entry layout to reach screen coordinates.
    pub fn with_hyperlinks<R>(
        &self,
        f: impl FnOnce(&[distill_markdown::HyperlinkTarget]) -> R,
    ) -> R {
        match self {
            RenderBlock::AgentMessage(b) => b.content().with_hyperlinks(f),
            RenderBlock::Thinking(b) => b.content().with_hyperlinks(f),
            RenderBlock::Btw(b) => b.content().with_hyperlinks(f),
            _ => f(&[]),
        }
    }

    pub fn with_table_copy_meta<R>(
        &self,
        f: impl FnOnce(&[distill_markdown::TableCopyMeta]) -> R,
    ) -> R {
        match self {
            RenderBlock::AgentMessage(b) => b.content().with_table_copy_meta(f),
            RenderBlock::Thinking(b) => b.content().with_table_copy_meta(f),
            RenderBlock::Btw(b) => b.content().with_table_copy_meta(f),
            _ => f(&[]),
        }
    }

    pub fn markdown_body_line_offset(
        &self,
        mode: DisplayMode,
        appearance: &AppearanceConfig,
    ) -> usize {
        match self {
            RenderBlock::Btw(_) if mode != DisplayMode::Collapsed => 2,
            RenderBlock::Thinking(_)
                if mode != DisplayMode::Collapsed
                    && appearance.scrollback.blocks.thinking.header =>
            {
                2
            }
            _ => 0,
        }
    }

    /// Set the raw mode for blocks that support it.
    /// This should be called before `output()` when the raw mode might have changed.
    /// Only affects AgentMessage and Thinking blocks; other blocks ignore this.
    pub fn set_raw_mode(&mut self, raw: bool) {
        match self {
            RenderBlock::AgentMessage(block) => block.set_raw_mode(raw),
            RenderBlock::Thinking(block) => block.set_raw_mode(raw),
            _ => {} // Other blocks don't support raw mode
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceConfig;
    use crate::scrollback::DisplayMode;
    use crate::scrollback::types::BlockLine;
    use pretty_assertions::assert_eq;
    use ratatui::text::{Line, Span};

    fn ctx(mode: DisplayMode, is_running: bool) -> BlockContext {
        BlockContext {
            mode,
            is_running,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    /// A block with N text lines and one trailing inline image.
    /// Exercises the default `inline_media_placements` wrapper that preserves tool media.
    struct TrailingMediaBlock {
        lines: usize,
    }

    impl BlockContent for TrailingMediaBlock {
        fn output(&self, _ctx: &BlockContext) -> BlockOutput {
            BlockOutput {
                lines: (0..self.lines)
                    .map(|i| BlockLine::text(format!("line {i}")))
                    .collect(),
            }
        }
        fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
            None
        }
        fn inline_media(&self) -> Option<InlineMediaInfo> {
            Some(InlineMediaInfo {
                path: std::path::PathBuf::from("/tmp/img.png"),
                width: 400,
                height: 200,
                is_video: false,
                alt_text: String::new(),
            })
        }
    }

    #[test]
    fn default_inline_media_placements_wrap_trailing_media() {
        // The default wrapper turns a single `inline_media()` into one trailing placement
        // The image anchors one row below the text, with the filepath line exposed, preserving the historical tool-media geometry
        let block = TrailingMediaBlock { lines: 2 };
        let c = ctx(DisplayMode::Expanded, false);
        let placements = block.inline_media_placements(&c);
        assert_eq!(placements.len(), 1);
        let Some(p) = placements.first() else {
            panic!("expected one placement: {placements:?}");
        };
        assert_eq!(
            p.row_offset, 3,
            "image trails the 2 text lines + 1 padding row"
        );
        assert!(p.rows >= 2, "image area reserves the fitted rows");
        assert_eq!(p.info.path, std::path::PathBuf::from("/tmp/img.png"));
    }

    #[test]
    fn default_inline_media_placements_empty_without_media() {
        // A block with no `inline_media()` yields no placements (the common case for every non-media block)
        let block = StubBlock::new("hi", ratatui::style::Color::Blue);
        assert!(
            block
                .inline_media_placements(&ctx(DisplayMode::Expanded, false))
                .is_empty()
        );
    }

    #[test]
    fn test_stub_block() {
        let block = RenderBlock::stub("Hello, world!", ratatui::style::Color::Blue);
        let c = ctx(DisplayMode::Expanded, false);
        let output = block.output(&c);
        assert_eq!(output.len(), 1);
        let accent = block.accent(&c);
        assert!(accent.is_some());
        assert!(!accent.unwrap().animated);
    }

    #[test]
    fn test_stub_block_running() {
        let block = RenderBlock::stub("Running...", ratatui::style::Color::Green);
        let c = ctx(DisplayMode::Expanded, true);
        let accent = block.accent(&c);
        assert!(accent.is_some());
        assert!(accent.unwrap().animated);
    }

    #[test]
    fn test_user_prompt_block() {
        let block = RenderBlock::user_prompt("How do I foo?");
        let c = ctx(DisplayMode::Expanded, false);
        let output = block.output(&c);
        assert!(!output.lines.is_empty());
        assert!(!block.is_foldable());
    }

    #[test]
    fn test_agent_message_block() {
        let block = RenderBlock::agent_message("Here's how to do it...");
        let c = ctx(DisplayMode::Expanded, false);
        let output = block.output(&c);
        assert!(!output.lines.is_empty());
        assert!(block.has_raw_mode());
    }

    #[test]
    fn test_tool_call_block() {
        let block = RenderBlock::tool_call("Read", "src/main.rs (50 lines)", true);
        let c = ctx(DisplayMode::Collapsed, false);
        let output = block.output(&c);
        assert_eq!(output.len(), 1);
        assert!(!block.is_foldable());
    }

    #[test]
    fn test_tool_call_with_output_is_foldable() {
        let block = RenderBlock::execute_with_output("cargo test", "test output", None::<String>);
        assert!(block.is_foldable());
    }

    #[test]
    fn test_search_block_with_matches_has_fullscreen_viewer() {
        use crate::scrollback::blocks::SearchLineMatch;
        let block = RenderBlock::search(
            "fn main",
            1,
            vec![SearchFileMatch {
                path: "src/main.rs".into(),
                matches: vec![SearchLineMatch {
                    line_number: 1,
                    content: "fn main() {}".into(),
                }],
            }],
        );
        assert!(block.has_normal_fullscreen_viewer());
    }

    #[test]
    fn test_errored_search_block_has_no_fullscreen_viewer() {
        let block = RenderBlock::ToolCall(ToolCallBlock::Search(
            SearchToolCallBlock::new("fn main").with_error("boom"),
        ));
        assert!(!block.has_normal_fullscreen_viewer());
    }

    #[test]
    fn test_list_dir_block_with_output_has_fullscreen_viewer() {
        let block = RenderBlock::list_dir_with_output("/tmp", "a.txt\nb.txt");
        assert!(block.has_normal_fullscreen_viewer());
    }

    #[test]
    fn test_empty_list_dir_block_has_no_fullscreen_viewer() {
        let block = RenderBlock::list_dir("/tmp");
        assert!(!block.has_normal_fullscreen_viewer());
    }

    #[test]
    fn test_copy_visible_text_in_state_rejoins_wrapped_lines() {
        let block = RenderBlock::stub_non_groupable("hello world", ratatui::style::Color::Blue);
        let copied = block.copy_visible_text_in_state(&ctx(DisplayMode::Expanded, false));
        assert_eq!(copied.as_deref(), Some("hello world"));
    }

    #[test]
    fn edit_whole_block_copy_preserves_path_boundary_whitespace() {
        let block = RenderBlock::edit("   foo.rs   ", None);
        let mut context = ctx(DisplayMode::Expanded, false);
        context.width = 8;

        assert_eq!(
            block.copy_visible_text_in_state(&context).as_deref(),
            Some("   foo.rs   ")
        );
    }

    #[test]
    fn test_copy_visible_text_in_state_skips_non_selectable_lines() {
        let output = BlockOutput {
            lines: vec![
                BlockLine::separator(Line::raw("---")),
                BlockLine::text("body"),
            ],
        };
        assert_eq!(
            plain_text_from_output(&output, &SelectionBoundaries::default()).as_deref(),
            Some("body")
        );
    }

    #[test]
    fn test_copy_does_not_capture_render_only_table_padding() {
        // Table rows are padded to the content width for rendering
        // Block-drag copy must not include that trailing padding
        let output = BlockOutput {
            lines: vec![
                BlockLine::styled(Line::from(vec![
                    Span::raw("│ a │ b │"),
                    Span::raw("        "),
                ])),
                BlockLine::styled(Line::from(vec![
                    Span::raw("│ c │ d │"),
                    Span::raw("        "),
                ])),
            ],
        };
        assert_eq!(
            plain_text_from_output(&output, &SelectionBoundaries::default()).as_deref(),
            Some("│ a │ b │\n│ c │ d │")
        );
    }

    #[test]
    fn test_bullet_preserves_selection_metadata() {
        let mut output = BlockOutput {
            lines: vec![
                BlockLine::styled(Line::from(vec![Span::raw("body")]))
                    .with_selection_range(Some(3))
                    .with_selection_text(Some("body".to_string())),
            ],
        };
        prepend_bullet(&mut output, &ctx(DisplayMode::Expanded, false), None);
        let Some(line) = output.lines.first() else {
            panic!("expected a prepended line: {output:?}");
        };

        assert_eq!(line.selection_range, Some(3));
        assert_eq!(line.selection_text.as_deref(), Some("body"));
        assert!(matches!(line.selectable, Selectable::Spans(ref r) if *r == (1..2)));
    }
}

#[cfg(test)]
mod searchable_text_tests {
    use super::*;
    use crate::scrollback::blocks::SearchLineMatch;
    use crate::scrollback::blocks::tool::WebSearchToolCallBlock;
    use crate::scrollback::blocks::tool::memory_search::{MemoryResult, MemorySearchToolCallBlock};
    use distill_shell::session::ContextInfo;
    use std::time::Duration;

    #[test]
    fn system_indexes_message_text() {
        let block = RenderBlock::system("disk almost full");
        assert_eq!(block.searchable_text().as_deref(), Some("disk almost full"));
    }

    #[test]
    fn user_prompt_indexes_text() {
        let block = RenderBlock::user_prompt("how do I foo the bar");
        assert_eq!(
            block.searchable_text().as_deref(),
            Some("how do I foo the bar")
        );
    }

    #[test]
    fn empty_only_source_field_returns_none() {
        // join_searchable drops empty strings, so a block whose only source field is empty is left out of the index entirely
        assert_eq!(RenderBlock::system("").searchable_text(), None);
        assert_eq!(RenderBlock::user_prompt("").searchable_text(), None);
    }

    #[test]
    fn session_event_flattens_to_sentence() {
        let block = RenderBlock::session_event(SessionEvent::TurnFailed {
            error: "connection reset".into(),
            elapsed: None,
        });
        let text = block.searchable_text().expect("session event text");
        assert!(text.contains("connection reset"), "got: {text:?}");
    }

    #[test]
    fn bg_task_indexes_command_and_description() {
        let block = RenderBlock::bg_task("cargo build --release", "task-1")
            .with_bg_task_description(Some("compile in release mode".into()));
        let text = block.searchable_text().expect("bg task text");
        assert!(text.contains("cargo build --release"), "got: {text:?}");
        assert!(text.contains("compile in release mode"), "got: {text:?}");
    }

    #[test]
    fn subagent_failed_indexes_metadata_and_error() {
        let mut block = RenderBlock::Subagent(SubagentBlock::failed(
            "investigate flaky test",
            "child-1",
            Duration::from_secs(3),
            Some("panicked at assert".into()),
        ));
        // Populate the metadata fields a failed background block leaves empty.
        if let RenderBlock::Subagent(b) = &mut block {
            b.subagent_type = "explore".into();
            b.persona = Some("scout".into());
            b.role = Some("researcher".into());
            b.model = Some("grok-test".into());
            b.activity_label = Some("Running: cargo build".into());
        }
        let text = block.searchable_text().expect("subagent text");
        assert!(text.contains("investigate flaky test"), "got: {text:?}");
        assert!(text.contains("explore"), "got: {text:?}");
        assert!(text.contains("scout"), "got: {text:?}");
        assert!(text.contains("researcher"), "got: {text:?}");
        assert!(text.contains("grok-test"), "got: {text:?}");
        assert!(text.contains("Running: cargo build"), "got: {text:?}");
        assert!(text.contains("panicked at assert"), "got: {text:?}");
    }

    #[test]
    fn btw_indexes_question_and_rendered_response() {
        let block = RenderBlock::Btw(BtwBlock::new("what is rust", "a **systems** language"));
        let text = block.searchable_text().expect("btw text");
        assert!(text.contains("what is rust"), "got: {text:?}");
        // Rendered plain text is indexed (markers stripped)
        // A query that spans the emphasis, "a systems language", matches the index just as it matches the on-screen highlight
        assert!(text.contains("a systems language"), "got: {text:?}");
        assert!(!text.contains("**"), "markers should be stripped: {text:?}");
    }

    #[test]
    fn context_info_indexes_model_only() {
        let snapshot = ContextInfo {
            used: 100,
            total: 1_000,
            system_prompt_tokens: 10,
            tool_definitions_count: 1,
            tool_definitions_tokens: 20,
            compaction_count: 0,
            turn_count: 1,
            tool_call_count: 1,
            message_count: 1,
            message_tokens: 30,
            free_tokens: 900,
            usage_pct: 10,
            auto_compact_threshold_percent: 85,
            usage_categories: vec![],
        };
        let block = RenderBlock::context_info(snapshot, "grok-4.5");
        // Only the model name is source text; the rest is a numeric breakdown.
        assert_eq!(block.searchable_text().as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn search_tool_indexes_pattern_and_match_line() {
        let block = RenderBlock::search(
            "fn main",
            1,
            vec![SearchFileMatch {
                path: "src/main.rs".into(),
                matches: vec![SearchLineMatch {
                    line_number: 10,
                    content: "fn main() {}".into(),
                }],
            }],
        );
        let text = block.searchable_text().expect("search text");
        assert!(text.contains("fn main"), "got: {text:?}");
        assert!(text.contains("src/main.rs"), "got: {text:?}");
        assert!(text.contains("fn main() {}"), "got: {text:?}");
    }

    #[test]
    fn list_dir_indexes_path_and_output() {
        let block = RenderBlock::list_dir_with_output("/tmp/proj", "a.txt\nb.txt");
        let text = block.searchable_text().expect("list dir text");
        assert!(text.contains("/tmp/proj"), "got: {text:?}");
        assert!(text.contains("a.txt"), "got: {text:?}");
    }

    #[test]
    fn memory_search_indexes_query_and_results() {
        let mut mem = MemorySearchToolCallBlock::new("deployment process");
        mem.results = vec![MemoryResult {
            score: 0.9,
            source: "global".into(),
            path: "MEMORY.md".into(),
            start_line: 1,
            end_line: 5,
            snippet: "use graphite for PRs".into(),
        }];
        let block = RenderBlock::ToolCall(ToolCallBlock::MemorySearch(mem));
        let text = block.searchable_text().expect("memory search text");
        assert!(text.contains("deployment process"), "got: {text:?}");
        assert!(text.contains("global"), "got: {text:?}");
        assert!(text.contains("MEMORY.md"), "got: {text:?}");
        assert!(text.contains("use graphite for PRs"), "got: {text:?}");
    }

    #[test]
    fn other_tool_indexes_name_summary_output_and_error() {
        let block =
            RenderBlock::tool_call_with_details("MyTool", "do something", false, "the output");
        let text = block.searchable_text().expect("other tool text");
        assert!(text.contains("MyTool"), "got: {text:?}");
        assert!(text.contains("do something"), "got: {text:?}");
        assert!(text.contains("the output"), "got: {text:?}");
        assert!(text.contains("Tool call failed"), "got: {text:?}");
    }

    #[test]
    fn execute_indexes_command_output_and_error() {
        let block =
            RenderBlock::execute_with_output("cargo test", "running 5 tests", Some("1 failed"));
        let text = block.searchable_text().expect("execute text");
        assert!(text.contains("cargo test"), "got: {text:?}");
        assert!(text.contains("running 5 tests"), "got: {text:?}");
        assert!(text.contains("1 failed"), "got: {text:?}");
    }

    #[test]
    fn agent_message_indexes_rendered_text() {
        // Rendered plain text is indexed (markers stripped) so the index agrees with the on-screen highlight: a phrase spanning emphasis is found
        let block = RenderBlock::agent_message("this is **really** important");
        let text = block.searchable_text().expect("agent text");
        assert!(text.contains("is really important"), "got: {text:?}");
        assert!(!text.contains("**"), "markers should be stripped: {text:?}");
    }

    #[test]
    fn thinking_indexes_rendered_text() {
        let block = RenderBlock::thinking("plan: call `_foo()` then verify");
        let text = block.searchable_text().expect("thinking text");
        // Inline-code backticks are stripped in the rendered view; the code content itself is preserved
        assert!(text.contains("call _foo() then verify"), "got: {text:?}");
        assert!(
            !text.contains('`'),
            "backticks should be stripped: {text:?}"
        );
    }

    #[test]
    fn web_search_indexes_query_and_citation() {
        let mut ws = WebSearchToolCallBlock::new("rust async runtime");
        ws.citations = vec!["https://tokio.rs/docs".into()];
        let block = RenderBlock::ToolCall(ToolCallBlock::WebSearch(ws));
        let text = block.searchable_text().expect("web search text");
        assert!(text.contains("rust async runtime"), "got: {text:?}");
        assert!(text.contains("https://tokio.rs/docs"), "got: {text:?}");
    }
}
