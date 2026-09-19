//! Dedicated presentation for `send_subagent_message` tool calls.
//!
//! Destination and message arguments are inert literal text: they never enter generic media discovery, command interpretation, or Q&A parsing.
//! `header_text()` (`Message <verb> <target>`) is the whole collapsed row, the export line, and the search anchor;
//! the text and the reason for a rejected or unconfirmed send show only in the expanded body, where the text is
//! copy-exact.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::appearance::AppearanceConfig;
use crate::render::line_utils::is_unsafe_display_char;
use crate::render::wrapping::{RtOptions, word_wrap_lines, word_wrap_lines_with_joiners};
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
    RenderedBlockOutput, Selectable, SelectionBoundaries, SelectionBoundary,
    SelectionBoundaryEntry, derive_selection_text, line_plain_text, selectable_cols,
};
use crate::theme::Theme;
use crate::util::format_duration;

const SENT_MESSAGE_ID_RANGE: u16 = 0;
const SENT_MESSAGE_TEXT_RANGE: u16 = 1;
const HEADER_LABEL: &str = "Message ";
const FALLBACK_NOUN: &str = "subagent";
const PARENT_NOUN: &str = "parent";
/// Trailing chars of a raw id: UUIDv7 prefixes are identical for ids minted within ~65 s, so the head would not tell two children apart.
const SHORT_ID_CHARS: usize = 8;
/// Admission is an in-memory call, so elapsed only shows for stalls; the floor also hides the ~0 ms a replay re-stamps.
const MIN_SHOWN_ELAPSED_MS: i64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentMessagePresentation {
    Sending,
    Sent,
    Rejected { reason: String },
    Unconfirmed { reason: String },
}

/// Who the message was addressed to, resolved once when the row is built (render time has no registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentMessageTarget {
    /// A spawn was seen: the finished display label (already clamped and quoted) and the child session the row opens.
    /// The raw id is never shown.
    Named {
        label: Arc<str>,
        child_session_id: Arc<str>,
    },
    /// The literal `parent` alias a child-depth sender uses; never looked up and never openable.
    Parent,
    /// No spawn was seen for this id (foreign, agent-scoped, or empty), so the raw id is the only name available.
    Unresolved { subagent_id: String },
}

impl SentMessageTarget {
    /// The label, `parent`, or `subagent …<last chars>`; an empty id is just `subagent`. The label and the id are
    /// model-authored, so both are scrubbed of characters that would split or reorder the one-line row.
    fn noun(&self) -> Cow<'_, str> {
        match self {
            Self::Named { label, .. } => scrub_display(label),
            Self::Parent => Cow::Borrowed(PARENT_NOUN),
            Self::Unresolved { subagent_id } => {
                let id = scrub_display(subagent_id);
                match id.char_indices().nth_back(SHORT_ID_CHARS) {
                    // The char just before the last `SHORT_ID_CHARS`; the tail starts after it.
                    Some((index, ch)) => {
                        let (_, tail) = id.split_at(index + ch.len_utf8());
                        Cow::Owned(format!("{FALLBACK_NOUN} \u{2026}{tail}"))
                    }
                    None if id.is_empty() => Cow::Borrowed(FALLBACK_NOUN),
                    None => Cow::Owned(format!("{FALLBACK_NOUN} {id}")),
                }
            }
        }
    }

    /// The unresolved id worth showing and indexing; the recognizer trims, so only an empty id carries nothing.
    fn raw_id(&self) -> Option<&str> {
        match self {
            Self::Unresolved { subagent_id } if !subagent_id.is_empty() => Some(subagent_id),
            Self::Unresolved { .. } | Self::Named { .. } | Self::Parent => None,
        }
    }
}

/// The expanded header names the requested mode as its lowercase suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum SentMessageDelivery {
    Steer,
    Queue,
    Interject,
}

/// The tool arguments as sent, kept verbatim; `None` on the block when the input could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessageInput {
    pub target: SentMessageTarget,
    /// `None` when the wire carried a delivery this pager does not recognize.
    pub delivery: Option<SentMessageDelivery>,
    pub text: String,
}

impl SentMessagePresentation {
    fn detail(&self) -> Option<(&str, MessageDetailStyle)> {
        match self {
            Self::Sending | Self::Sent => None,
            Self::Rejected { reason } => Some((reason, MessageDetailStyle::Error)),
            Self::Unconfirmed { reason } => Some((reason, MessageDetailStyle::Warning)),
        }
    }

    fn accent(&self, theme: &Theme, is_running: bool) -> ratatui::style::Color {
        match self {
            Self::Sending if is_running => theme.accent_running,
            Self::Sending | Self::Sent => theme.accent_tool,
            Self::Rejected { .. } => theme.accent_error,
            Self::Unconfirmed { .. } => theme.warning,
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Sending => false,
            Self::Sent | Self::Rejected { .. } | Self::Unconfirmed { .. } => true,
        }
    }

    pub(crate) fn is_failure(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    pub(crate) fn is_unconfirmed(&self) -> bool {
        matches!(self, Self::Unconfirmed { .. })
    }
}

#[derive(Debug, Clone, Copy)]
enum MessageDetailStyle {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct SentMessageToolCallBlock {
    pub presentation: SentMessagePresentation,
    pub input: Option<SentMessageInput>,
    pub started_at: Option<std::time::Instant>,
    pub elapsed_ms: Option<i64>,
}

impl SentMessageToolCallBlock {
    pub fn new(presentation: SentMessagePresentation, input: Option<SentMessageInput>) -> Self {
        Self {
            presentation,
            input,
            started_at: None,
            elapsed_ms: None,
        }
    }

    /// One line for export and search: label, verb, target. Never the text, the reason, or a raw id behind a label.
    pub(crate) fn header_text(&self) -> String {
        format!("{HEADER_LABEL}{}", self.verb_and_target())
    }

    /// The collapsed verb names the outcome: a steer stays unmarked and an unrecognized delivery reads like one; a
    /// failed or unconfirmed send names no delivery mode.
    fn verb_and_target(&self) -> String {
        let noun = self
            .input
            .as_ref()
            .map_or(Cow::Borrowed(FALLBACK_NOUN), |input| input.target.noun());
        match &self.presentation {
            SentMessagePresentation::Sending => format!("sending to {noun}"),
            SentMessagePresentation::Sent => {
                match self.input.as_ref().and_then(|input| input.delivery) {
                    None | Some(SentMessageDelivery::Steer) => format!("sent to {noun}"),
                    Some(SentMessageDelivery::Queue) => format!("queued for {noun}"),
                    Some(SentMessageDelivery::Interject) => format!("interjected to {noun}"),
                }
            }
            SentMessagePresentation::Rejected { .. } => format!("rejected \u{00b7} {noun}"),
            SentMessagePresentation::Unconfirmed { .. } => format!("unconfirmed \u{00b7} {noun}"),
        }
    }

    /// The child session this row opens; only a target that resolved through a spawn has one.
    pub(crate) fn child_session_id(&self) -> Option<&str> {
        match &self.input.as_ref()?.target {
            SentMessageTarget::Named {
                child_session_id, ..
            } => Some(child_session_id),
            SentMessageTarget::Parent | SentMessageTarget::Unresolved { .. } => None,
        }
    }

    /// Bold label, muted verb and target, nothing else: the text and the reason wait for the expanded body. A very
    /// narrow pane clips the row at its edge, as the Subagent row does.
    fn collapsed_line(&self, ctx: &BlockContext, theme: &Theme) -> Line<'static> {
        let is_muted =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);
        let label_style = if is_muted {
            theme.muted()
        } else {
            theme.primary()
        }
        .add_modifier(Modifier::BOLD);
        Line::from(vec![
            Span::styled(HEADER_LABEL, label_style),
            Span::styled(self.verb_and_target(), theme.muted()),
        ])
    }

    /// Never muted. Suffixes name the requested delivery and, for a finished send that stalled, its admission time.
    fn expanded_header(&self, theme: &Theme) -> Line<'static> {
        let mut spans = vec![
            Span::styled(HEADER_LABEL, theme.primary().add_modifier(Modifier::BOLD)),
            Span::styled(self.verb_and_target(), theme.primary()),
        ];
        if let Some(delivery) = self.input.as_ref().and_then(|input| input.delivery) {
            let delivery = <&str>::from(delivery);
            spans.push(Span::styled(format!(" \u{00b7} {delivery}"), theme.muted()));
        }
        if self.presentation.is_terminal()
            && let Some(elapsed) = self
                .elapsed_ms()
                .filter(|elapsed| *elapsed >= MIN_SHOWN_ELAPSED_MS)
        {
            let elapsed = format_duration(Duration::from_millis(elapsed.unsigned_abs()));
            spans.push(Span::styled(format!(" \u{00b7} {elapsed}"), theme.muted()));
        }
        Line::from(spans)
    }

    pub fn is_success(&self) -> bool {
        matches!(self.presentation, SentMessagePresentation::Sent)
    }

    pub fn is_failure(&self) -> bool {
        self.presentation.is_failure()
    }

    pub fn is_unconfirmed(&self) -> bool {
        self.presentation.is_unconfirmed()
    }

    pub fn finish(&mut self) {
        if self.elapsed_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        self.elapsed_ms.or_else(|| {
            self.started_at
                .map(|start| start.elapsed().as_millis() as i64)
        })
    }

    pub(crate) fn searchable_text(&self) -> Option<String> {
        crate::scrollback::block::join_searchable([
            Some(self.header_text()),
            self.input
                .as_ref()
                .and_then(|input| input.target.raw_id())
                .map(str::to_owned),
            self.input.as_ref().map(|input| input.text.clone()),
            self.presentation
                .detail()
                .map(|(detail, _)| detail.to_owned()),
        ])
    }

    pub(crate) fn rendered_output(&self, ctx: &BlockContext) -> RenderedBlockOutput {
        let output = self.output(ctx);
        let text = self.input.as_ref().map(|input| input.text.as_str());
        let boundaries = text.map_or_else(Vec::new, |text| {
            let last_message_line = output
                .lines
                .iter()
                .rposition(|line| line.selection_range == Some(SENT_MESSAGE_TEXT_RANGE));
            let is_newline_only = !text.is_empty() && text.chars().all(|ch| ch == '\n');
            output
                .lines
                .iter()
                .enumerate()
                .filter_map(|(line_index, line)| {
                    if line.selection_range != Some(SENT_MESSAGE_TEXT_RANGE) {
                        return None;
                    }
                    let is_last = Some(line_index) == last_message_line;
                    let suffix = (is_last && text.ends_with('\n')).then(|| "\n".to_owned());
                    if is_newline_only
                        && selectable_cols(&line.content, &line.selectable)
                            .is_some_and(|cols| cols.is_empty())
                    {
                        Some(SelectionBoundaryEntry {
                            line_index,
                            boundary: Arc::new(SelectionBoundary::empty_row_anchor(
                                String::new(),
                                suffix.unwrap_or_default(),
                            )),
                        })
                    } else {
                        suffix.map(|suffix| SelectionBoundaryEntry {
                            line_index,
                            boundary: Arc::new(SelectionBoundary::new(String::new(), suffix)),
                        })
                    }
                })
                .collect()
        });
        RenderedBlockOutput {
            output,
            boundaries: SelectionBoundaries::from_entries(boundaries),
        }
    }
}

/// Drops the characters that would split or reorder a one-line row; borrows when there are none.
fn scrub_display(text: &str) -> Cow<'_, str> {
    if text.chars().any(is_unsafe_display_char) {
        Cow::Owned(
            text.chars()
                .filter(|ch| !is_unsafe_display_char(*ch))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

impl BlockContent for SentMessageToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        if ctx.mode == DisplayMode::Collapsed {
            return BlockOutput {
                lines: vec![self.collapsed_line(ctx, &theme).into()],
            };
        }

        let width = (ctx.width as usize).saturating_sub(2).max(20);
        let mut lines: Vec<BlockLine> = vec![self.expanded_header(&theme).into()];
        if let Some((detail, style)) = self.presentation.detail() {
            let color = match style {
                MessageDetailStyle::Error => theme.accent_error,
                MessageDetailStyle::Warning => theme.warning,
            };
            lines.push(BlockLine::separator(Line::from("")));
            for line in word_wrap_lines(
                detail
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.fg(color)))),
                width,
            ) {
                let mut line = BlockLine::styled(line);
                line.selectable = Selectable::None;
                lines.push(line);
            }
        }

        if let Some(raw_id) = self.input.as_ref().and_then(|input| input.target.raw_id()) {
            lines.push(Line::from("").into());
            let id_wrap = RtOptions::new(width)
                .initial_indent(Line::from(Span::styled("Subagent ID: ", theme.muted())));
            let id_value = Line::from(Span::styled(raw_id.to_owned(), theme.primary()));
            let (wrapped_id, id_joiners) =
                word_wrap_lines_with_joiners(std::iter::once(id_value), id_wrap);
            for (index, (line, joiner)) in wrapped_id.into_iter().zip(id_joiners).enumerate() {
                let mut line = BlockLine::styled(line)
                    .with_selection_range(Some(SENT_MESSAGE_ID_RANGE))
                    .with_joiner(joiner);
                if index == 0 {
                    line.selectable = Selectable::Spans(1..line.content.spans.len());
                }
                line.selection_text = Some(derive_selection_text(&line));
                lines.push(line);
            }
        }
        lines.push(Line::from("").into());
        match self.input.as_ref().map(|input| input.text.as_str()) {
            Some(text) => {
                let source_lines = text
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.muted())));
                let (wrapped, joiners) = word_wrap_lines_with_joiners(source_lines, width);
                let trailing_empty_row = text.ends_with('\n')
                    && wrapped
                        .last()
                        .is_some_and(|line| line_plain_text(line).is_empty());
                let visible_len = wrapped
                    .len()
                    .saturating_sub(usize::from(trailing_empty_row));
                for (index, (line, joiner)) in wrapped.into_iter().zip(joiners).enumerate() {
                    if trailing_empty_row && index == visible_len {
                        break;
                    }
                    let selection_text = line_plain_text(&line);
                    lines.push(
                        BlockLine::styled(line)
                            .with_selection_range(Some(SENT_MESSAGE_TEXT_RANGE))
                            .with_selection_text(Some(selection_text))
                            .with_joiner(joiner),
                    );
                }
                if trailing_empty_row {
                    lines.push(BlockLine::separator(Line::from("")));
                }
            }
            None => {
                let mut line =
                    BlockLine::styled(Line::from(Span::styled("unavailable", theme.muted())));
                line.selectable = Selectable::None;
                lines.push(line);
            }
        }

        BlockOutput { lines }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        Some(AccentStyle::static_color(
            self.presentation.accent(&Theme::current(), ctx.is_running),
        ))
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.presentation {
            // A cancelled turn leaves the row Sending with the turn no longer running: gray, no animation.
            SentMessagePresentation::Sending if ctx.is_running => {
                Some(AccentStyle::animated_running(ctx, &theme))
            }
            SentMessagePresentation::Sending | SentMessagePresentation::Sent
                if ctx.mode == DisplayMode::Collapsed =>
            {
                None
            }
            SentMessagePresentation::Sending
            | SentMessagePresentation::Sent
            | SentMessagePresentation::Rejected { .. }
            | SentMessagePresentation::Unconfirmed { .. } => Some(AccentStyle::static_color(
                self.presentation.accent(&theme, ctx.is_running),
            )),
        }
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        self.input.is_some() || self.presentation.detail().is_some()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        if is_running {
            match current {
                DisplayMode::Truncated => DisplayMode::Expanded,
                DisplayMode::Collapsed | DisplayMode::Expanded => DisplayMode::Truncated,
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
}

#[cfg(test)]
#[path = "sent_message_tests.rs"]
mod tests;
