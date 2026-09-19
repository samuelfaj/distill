use super::*;
use crate::scrollback::ToolCallBlock;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::render::ScratchBuffer;
use crate::scrollback::scrollback_pane::ScrollbackPane;
use crate::scrollback::state::ScrollbackState;
use crate::scrollback::text_selection::{
    ActiveTextDrag, ResolvedSelectionBoundaries, ResolvedSelectionModel,
    reconstruct_full_selection_text_with_boundaries, reconstruct_selection_text_with_boundaries,
};
use crate::scrollback::types::{BlockContext, RenderedBlockOutput, line_plain_text};

const LABEL: &str = "Explore \u{201c}find callers\u{201d}";
const CHILD_SID: &str = "child-sid";
const REJECTED_REASON: &str = "Subagent is not active or is finalizing.";
const UNCONFIRMED_REASON: &str =
    "Message admission could not be confirmed; the message may or may not have been accepted.";

fn unresolved(subagent_id: &str, text: &str) -> Option<SentMessageInput> {
    unresolved_with(subagent_id, Some(SentMessageDelivery::Steer), text)
}

fn unresolved_with(
    subagent_id: &str,
    delivery: Option<SentMessageDelivery>,
    text: &str,
) -> Option<SentMessageInput> {
    input(
        SentMessageTarget::Unresolved {
            subagent_id: subagent_id.to_owned(),
        },
        delivery,
        text,
    )
}

fn named(delivery: SentMessageDelivery, text: &str) -> Option<SentMessageInput> {
    input(
        SentMessageTarget::Named {
            label: Arc::from(LABEL),
            child_session_id: Arc::from(CHILD_SID),
        },
        Some(delivery),
        text,
    )
}

fn input(
    target: SentMessageTarget,
    delivery: Option<SentMessageDelivery>,
    text: &str,
) -> Option<SentMessageInput> {
    Some(SentMessageInput {
        target,
        delivery,
        text: text.to_owned(),
    })
}

fn sending(input: Option<SentMessageInput>) -> SentMessageToolCallBlock {
    SentMessageToolCallBlock::new(SentMessagePresentation::Sending, input)
}

fn sent(input: Option<SentMessageInput>) -> SentMessageToolCallBlock {
    SentMessageToolCallBlock::new(SentMessagePresentation::Sent, input)
}

fn rejected(input: Option<SentMessageInput>) -> SentMessageToolCallBlock {
    SentMessageToolCallBlock::new(
        SentMessagePresentation::Rejected {
            reason: REJECTED_REASON.to_owned(),
        },
        input,
    )
}

fn unconfirmed(input: Option<SentMessageInput>) -> SentMessageToolCallBlock {
    SentMessageToolCallBlock::new(
        SentMessagePresentation::Unconfirmed {
            reason: UNCONFIRMED_REASON.to_owned(),
        },
        input,
    )
}

fn with_elapsed(mut block: SentMessageToolCallBlock, elapsed_ms: i64) -> SentMessageToolCallBlock {
    block.elapsed_ms = Some(elapsed_ms);
    block
}

fn context(width: u16, mode: DisplayMode) -> BlockContext {
    BlockContext {
        width,
        mode,
        is_running: false,
        raw: false,
        max_lines: None,
        appearance: Default::default(),
        is_selected: false,
        cwd: None,
    }
}

fn rendered(block: &SentMessageToolCallBlock, width: u16, mode: DisplayMode) -> String {
    block
        .output(&context(width, mode))
        .lines
        .iter()
        .map(|line| line_plain_text(&line.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_selection_state(
    block: SentMessageToolCallBlock,
    width: u16,
) -> (
    ResolvedSelectionModel,
    ResolvedSelectionBoundaries,
    RenderedBlockOutput,
) {
    let mut state = ScrollbackState::new();
    let id = state.push_block(RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)));
    state
        .get_by_id_mut(id)
        .expect("message entry")
        .set_display_mode(DisplayMode::Expanded);
    let area = ratatui::layout::Rect::new(0, 0, width, 40);
    state.prepare_layout(area.width, area.height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    let mut scratch = ScratchBuffer::default();
    let rendered = ScrollbackPane::new()
        .active(true)
        .render_with_scratch_and_selection_boundaries(area, &mut buffer, &state, &mut scratch);
    let content_width = rendered
        .output
        .selection_model
        .visible_block_content_width(0)
        .expect("message content width");
    let entry = state.get(0).expect("message entry");
    entry.ensure_cached(content_width, state.appearance(), false, state.cwd());
    let cached = entry.cached_rendered_output_ref().clone();
    (
        rendered.output.selection_model,
        rendered.selection_boundaries,
        cached,
    )
}

fn reachable_full_range_drag(model: &ResolvedSelectionModel, range_id: u16) -> ActiveTextDrag {
    let range = model.range(0, range_id).expect("selection range");
    let first = range.lines.first().expect("selection range first line");
    let last = range.lines.last().expect("selection range last line");
    let anchor = model
        .hit_test_text_exact(
            first.screen_x.saturating_add(first.selectable_cols.start),
            first.screen_y,
        )
        .expect("first painted cell must be hit-testable");
    assert_eq!(anchor.range_id, range_id);
    let last_col = last
        .screen_x
        .saturating_add(last.selectable_cols.end.saturating_sub(1));
    let head = model
        .hit_test_nearest_in_range(anchor, last_col, last.screen_y)
        .expect("last painted cell must be reachable in range");
    assert_eq!(head.range_id, range_id);
    ActiveTextDrag {
        anchor,
        head,
        kind: Default::default(),
        anchor_content_width: None,
    }
}

fn reconstruct_visible_and_cached(
    model: &ResolvedSelectionModel,
    boundaries: &ResolvedSelectionBoundaries,
    cached: &RenderedBlockOutput,
    drag: &ActiveTextDrag,
) -> (String, String) {
    let visible = reconstruct_selection_text_with_boundaries(model, boundaries, drag)
        .expect("visible message range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        drag,
    )
    .expect("production full message range reconstructs");
    (visible, full)
}

#[test]
fn sent_block_renders_arguments_as_inert_text() {
    let text =
        "Questions asked:\n- \"keep /tmp/existing.mp4 and ![x](/tmp/x.png)\"\n  Answer: yes\n";
    let block = sent(unresolved("sub-123", text));

    assert_eq!(
        "Message sent to subagent sub-123",
        rendered(&block, 120, DisplayMode::Collapsed)
    );
    assert_eq!(
        format!("Message sent to subagent sub-123 \u{b7} steer\n\nSubagent ID: sub-123\n\n{text}"),
        rendered(&block, 120, DisplayMode::Expanded)
    );
    assert!(block.image_references().is_empty());
    assert!(block.video_references().is_empty());
    assert!(block.inline_open_button().is_none());
}

#[test]
fn collapsed_grammar_covers_every_presentation_delivery_and_target() {
    use SentMessageDelivery::{Interject, Queue, Steer};
    let parent = |text| input(SentMessageTarget::Parent, Some(Steer), text);
    for (block, verb_and_target) in [
        (
            sending(named(Steer, "follow up")),
            format!("sending to {LABEL}"),
        ),
        (sent(named(Steer, "follow up")), format!("sent to {LABEL}")),
        (
            sent(named(Queue, "follow up")),
            format!("queued for {LABEL}"),
        ),
        (
            sent(named(Interject, "follow up")),
            format!("interjected to {LABEL}"),
        ),
        (
            sent(unresolved_with("sub-123", None, "follow up")),
            "sent to subagent sub-123".to_owned(),
        ),
        (
            sent(unresolved(
                "01a08ec1-741a-7000-8000-0000fd156033",
                "follow up",
            )),
            "sent to subagent \u{2026}fd156033".to_owned(),
        ),
        (
            sent(unresolved("sub-1234", "follow up")),
            "sent to subagent sub-1234".to_owned(),
        ),
        (
            sent(unresolved("sub\u{7}-1\u{202E}23", "follow up")),
            "sent to subagent sub-123".to_owned(),
        ),
        (
            sent(input(
                SentMessageTarget::Named {
                    label: Arc::from("Explore \u{201c}scan\n src/\u{202E}\u{201d}"),
                    child_session_id: Arc::from(CHILD_SID),
                },
                Some(Steer),
                "follow up",
            )),
            "sent to Explore \u{201c}scan src/\u{201d}".to_owned(),
        ),
        (
            sent(unresolved("", "follow up")),
            "sent to subagent".to_owned(),
        ),
        (sent(parent("follow up")), "sent to parent".to_owned()),
        (sent(None), "sent to subagent".to_owned()),
        (
            rejected(named(Queue, "follow up")),
            format!("rejected \u{b7} {LABEL}"),
        ),
        (
            unconfirmed(unresolved("sub-123", "follow up")),
            "unconfirmed \u{b7} subagent sub-123".to_owned(),
        ),
    ] {
        let header = format!("Message {verb_and_target}");
        assert_eq!(header, block.header_text(), "{verb_and_target}");
        assert_eq!(
            header,
            rendered(&block, 120, DisplayMode::Collapsed),
            "{verb_and_target}: the collapsed row is the header alone"
        );
    }
}

#[test]
fn expanded_shows_header_suffixes_and_only_the_id_line_when_unresolved() {
    use SentMessageDelivery::{Queue, Steer};
    let parent = input(SentMessageTarget::Parent, Some(Steer), "follow up");
    for (block, expected) in [
        (
            sent(named(Steer, "follow up")),
            format!("Message sent to {LABEL} \u{b7} steer\n\nfollow up"),
        ),
        (
            sent(parent),
            "Message sent to parent \u{b7} steer\n\nfollow up".to_owned(),
        ),
        (
            sent(unresolved_with("sub-123", None, "follow up")),
            "Message sent to subagent sub-123\n\nSubagent ID: sub-123\n\nfollow up".to_owned(),
        ),
        (
            sent(unresolved("", "follow up")),
            "Message sent to subagent \u{b7} steer\n\nfollow up".to_owned(),
        ),
        (
            sent(None),
            "Message sent to subagent\n\nunavailable".to_owned(),
        ),
        (
            rejected(named(Queue, "follow up")),
            format!(
                "Message rejected \u{b7} {LABEL} \u{b7} queue\n\n{REJECTED_REASON}\n\nfollow up"
            ),
        ),
        (
            rejected(unresolved("sub-123", "follow up")),
            format!(
                "Message rejected \u{b7} subagent sub-123 \u{b7} steer\n\n{REJECTED_REASON}\n\nSubagent ID: sub-123\n\nfollow up"
            ),
        ),
        // A content-block fallback reason can start with a newline; it is shown verbatim, line for line.
        (
            SentMessageToolCallBlock::new(
                SentMessagePresentation::Rejected {
                    reason: "\nfirst line\nsecond line".to_owned(),
                },
                named(Steer, "follow up"),
            ),
            format!(
                "Message rejected \u{b7} {LABEL} \u{b7} steer\n\n\nfirst line\nsecond line\n\nfollow up"
            ),
        ),
        (
            unconfirmed(named(Steer, "follow up")),
            format!(
                "Message unconfirmed \u{b7} {LABEL} \u{b7} steer\n\n{UNCONFIRMED_REASON}\n\nfollow up"
            ),
        ),
        (
            with_elapsed(sent(named(Steer, "follow up")), 1_200),
            format!("Message sent to {LABEL} \u{b7} steer \u{b7} 1.2s\n\nfollow up"),
        ),
        (
            with_elapsed(sent(named(Steer, "follow up")), 40),
            format!("Message sent to {LABEL} \u{b7} steer\n\nfollow up"),
        ),
        (
            with_elapsed(sending(named(Steer, "follow up")), 1_200),
            format!("Message sending to {LABEL} \u{b7} steer\n\nfollow up"),
        ),
    ] {
        assert_eq!(expected, rendered(&block, 120, DisplayMode::Expanded));
    }
}

#[test]
fn bullet_and_label_styles_follow_running_and_selection() {
    use DisplayMode::{Collapsed, Expanded};
    let theme = Theme::current();
    let running = |mode| BlockContext {
        is_running: true,
        ..context(120, mode)
    };
    let in_flight = sending(named(SentMessageDelivery::Steer, "x"));
    let pulsing = Some(AccentStyle::animated_running(&running(Collapsed), &theme));
    let fixed = |color| Some(AccentStyle::static_color(color));
    for (block, ctx, expected) in [
        (in_flight.clone(), running(Collapsed), pulsing),
        (in_flight.clone(), running(Expanded), pulsing),
        // A cancelled turn leaves the row Sending with the turn no longer running
        (in_flight, context(120, Collapsed), None),
        (sent(None), running(Collapsed), None),
        (sent(None), running(Expanded), fixed(theme.accent_tool)),
        (
            rejected(None),
            running(Collapsed),
            fixed(theme.accent_error),
        ),
        (unconfirmed(None), running(Collapsed), fixed(theme.warning)),
    ] {
        assert_eq!(
            expected,
            block.bullet(&ctx),
            "{:?} {:?} running={}",
            block.presentation,
            ctx.mode,
            ctx.is_running
        );
    }

    let block = sent(named(SentMessageDelivery::Steer, "follow up"));
    let first_line_styles = |ctx: &BlockContext| {
        let line = block.output(ctx).lines.remove(0).content;
        line.spans.iter().map(|span| span.style).collect::<Vec<_>>()
    };
    let muted = theme.muted();
    for (is_selected, label_style) in [(false, muted), (true, theme.primary())] {
        let ctx = BlockContext {
            is_selected,
            ..context(120, Collapsed)
        };
        assert_eq!(
            vec![label_style.add_modifier(Modifier::BOLD), muted],
            first_line_styles(&ctx),
            "selected={is_selected}"
        );
    }
    assert_eq!(
        Some(&theme.primary().add_modifier(Modifier::BOLD)),
        first_line_styles(&context(120, Expanded)).first()
    );
}

#[test]
fn header_text_never_carries_text_reason_or_raw_id_for_named() {
    let block = rejected(named(SentMessageDelivery::Queue, "follow up"));
    let header = block.header_text();
    assert_eq!(format!("Message rejected \u{b7} {LABEL}"), header);
    assert_eq!(
        format!("{header}\nfollow up\n{REJECTED_REASON}"),
        block.searchable_text().expect("searchable message")
    );
    assert_eq!(Some(CHILD_SID), block.child_session_id());
    let (model, _, _) = render_selection_state(block, 60);
    assert!(model.range(0, SENT_MESSAGE_ID_RANGE).is_none());
    assert!(model.range(0, SENT_MESSAGE_TEXT_RANGE).is_some());

    let unresolved_block = rejected(unresolved("sub-123", "follow up"));
    assert_eq!(
        format!("Message rejected \u{b7} subagent sub-123\nsub-123\nfollow up\n{REJECTED_REASON}"),
        unresolved_block
            .searchable_text()
            .expect("searchable message")
    );
    assert_eq!(None, unresolved_block.child_session_id());
    let parent_block = sent(input(SentMessageTarget::Parent, None, "follow up"));
    assert_eq!(None, parent_block.child_session_id());
}

#[test]
fn wrapped_message_joiners_reconstruct_word_midword_and_trailing_newline_exactly() {
    let text = "alpha beta supercalifragilisticexpialidocious\n";
    let block = sent(unresolved("sub-123", text));
    let output = block.output(&context(22, DisplayMode::Expanded));
    let message_lines: Vec<_> = output
        .lines
        .iter()
        .filter(|line| line.selection_range == Some(SENT_MESSAGE_TEXT_RANGE))
        .collect();
    assert!(
        message_lines
            .iter()
            .any(|line| line.joiner.as_deref() == Some(" "))
    );
    assert!(
        message_lines
            .iter()
            .any(|line| line.joiner.as_deref() == Some(""))
    );

    let (model, boundaries, cached) = render_selection_state(block, 22);
    assert!(
        model.range(0, SENT_MESSAGE_TEXT_RANGE).is_some(),
        "message selection range"
    );
    let drag = reachable_full_range_drag(&model, SENT_MESSAGE_TEXT_RANGE);
    let visible = reconstruct_selection_text_with_boundaries(&model, &boundaries, &drag)
        .expect("visible message range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &drag,
    )
    .expect("production full message range reconstructs");
    assert_eq!(visible, text);
    assert_eq!(full, text);

    let last_line = model
        .range(0, SENT_MESSAGE_TEXT_RANGE)
        .and_then(|range| range.lines.last())
        .expect("last reachable message row");
    let partial_hit = model
        .hit_test_text_exact(
            last_line
                .screen_x
                .saturating_add(last_line.selectable_cols.start),
            last_line.screen_y,
        )
        .expect("partial selection endpoint must be hit-testable");
    let mut partial = drag;
    partial.anchor = partial_hit;
    partial.head = partial_hit;
    let visible_partial = reconstruct_selection_text_with_boundaries(&model, &boundaries, &partial)
        .expect("visible partial message selection");
    let full_partial = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &partial,
    )
    .expect("production partial message selection");
    assert!(!visible_partial.ends_with('\n'));
    assert!(!full_partial.ends_with('\n'));
    assert_eq!(
        model
            .range(0, SENT_MESSAGE_ID_RANGE)
            .expect("destination selection range")
            .range_id,
        SENT_MESSAGE_ID_RANGE
    );
}

#[test]
fn newline_only_messages_have_real_selection_anchors_and_copy_exactly() {
    for text in ["\n", "\n\n"] {
        let block = sent(unresolved("sub-123", text));
        let (model, boundaries, cached) = render_selection_state(block, 40);
        let range = model
            .range(0, SENT_MESSAGE_TEXT_RANGE)
            .expect("blank message selection range");
        let blank_rows = text.chars().filter(|&ch| ch == '\n').count();
        assert_eq!(range.lines.len(), blank_rows);
        assert!(range.lines.iter().all(|line| {
            line.text.is_empty()
                && line.painted_region.as_deref() == Some("")
                && line
                    .selectable_cols
                    .end
                    .saturating_sub(line.selectable_cols.start)
                    == 1
                && model
                    .hit_test_text_exact(
                        line.screen_x.saturating_add(line.selectable_cols.start),
                        line.screen_y,
                    )
                    .is_some()
        }));

        let drag = reachable_full_range_drag(&model, SENT_MESSAGE_TEXT_RANGE);
        let (visible, full) = reconstruct_visible_and_cached(&model, &boundaries, &cached, &drag);
        assert_eq!(visible, text);
        assert_eq!(full, text);
        let output_text = cached
            .output
            .lines
            .iter()
            .map(|line| line_plain_text(&line.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!output_text.contains('\u{200B}'));
        assert!(!output_text.contains('\u{00A0}'));

        if range.lines.len() > 1 {
            let first = range.lines.first().expect("first blank row");
            let first_hit = model
                .hit_test_text_exact(
                    first.screen_x.saturating_add(first.selectable_cols.start),
                    first.screen_y,
                )
                .expect("first blank row anchor");
            let partial = ActiveTextDrag {
                anchor: first_hit,
                head: first_hit,
                kind: Default::default(),
                anchor_content_width: None,
            };
            let (visible_partial, full_partial) =
                reconstruct_visible_and_cached(&model, &boundaries, &cached, &partial);
            assert_eq!(visible_partial, "");
            assert_eq!(full_partial, "");
        }
    }
}

#[test]
fn destination_id_wraps_literally_and_reconstructs_through_joiners() {
    let subagent_id = "019d0000-0000-7000-8000-000000000001";
    let block = sent(unresolved(subagent_id, "hello"));
    let output = block.output(&context(22, DisplayMode::Expanded));
    let id_start = output
        .lines
        .iter()
        .position(|line| line_plain_text(&line.content).starts_with("Subagent ID:"))
        .expect("subagent id heading");
    let Some(id_tail) = output.lines.get(id_start..) else {
        panic!(
            "id_start {id_start} out of bounds, len={}",
            output.lines.len()
        );
    };
    let id_end = id_tail
        .iter()
        .position(|line| line_plain_text(&line.content).is_empty())
        .map_or(output.lines.len(), |offset| id_start + offset);
    let Some(id_lines) = output.lines.get(id_start..id_end) else {
        panic!(
            "id range {id_start}..{id_end} out of bounds, len={}",
            output.lines.len()
        );
    };
    assert!(
        id_lines
            .iter()
            .skip(1)
            .all(|line| line.joiner.as_deref() == Some("")),
        "UUID-only continuations are mid-word/hyphen joins"
    );

    let (model, boundaries, cached) = render_selection_state(block, 22);
    assert!(
        model.range(0, SENT_MESSAGE_ID_RANGE).is_some(),
        "destination selection range"
    );
    let drag = reachable_full_range_drag(&model, SENT_MESSAGE_ID_RANGE);
    let visible = reconstruct_selection_text_with_boundaries(&model, &boundaries, &drag)
        .expect("visible destination range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &drag,
    )
    .expect("production destination range reconstructs");
    assert_eq!(visible, subagent_id);
    assert_eq!(full, subagent_id);
    assert!(
        model.range(0, SENT_MESSAGE_TEXT_RANGE).is_some(),
        "destination and message must have independent ranges"
    );
}

#[test]
fn dedicated_block_participates_in_search_selection_and_export() {
    let block = unconfirmed(unresolved("sub-123", "literal follow up"));
    let render_block = RenderBlock::ToolCall(ToolCallBlock::SentMessage(block));

    let searchable = render_block.searchable_text().expect("searchable message");
    assert!(searchable.contains("sub-123"));
    assert!(searchable.contains("literal follow up"));
    let selected = render_block
        .copy_visible_text_in_state(&context(40, DisplayMode::Expanded))
        .expect("copyable visible message");
    assert!(selected.contains("literal follow up"));
    let named_block = RenderBlock::ToolCall(ToolCallBlock::SentMessage(sent(named(
        SentMessageDelivery::Queue,
        "follow up",
    ))));
    assert_eq!(
        format!(
            "## Tools\n\n- Message unconfirmed \u{b7} subagent sub-123\n- Message queued for {LABEL}"
        ),
        crate::scrollback::export::render_blocks_to_markdown([&render_block, &named_block])
    );
}

#[test]
fn rejected_and_unconfirmed_have_distinct_failure_semantics() {
    let rejected_block = rejected(unresolved("null", ""));
    let unconfirmed_block = unconfirmed(unresolved("sub-123", "retry?"));

    assert!(rejected_block.is_foldable());
    assert!(unconfirmed_block.is_foldable());
    assert!(!rejected_block.is_success());
    assert!(rejected_block.is_failure());
    assert!(!rejected_block.is_unconfirmed());
    assert!(!unconfirmed_block.is_success());
    assert!(!unconfirmed_block.is_failure());
    assert!(unconfirmed_block.is_unconfirmed());

    let in_flight = sending(None);
    assert!(!in_flight.is_success());
    assert!(!in_flight.is_failure());
    assert!(!in_flight.is_unconfirmed());
    assert!(
        rendered(&rejected_block, 120, DisplayMode::Expanded)
            .starts_with("Message rejected \u{b7} subagent null")
    );
    assert!(
        rendered(&unconfirmed_block, 120, DisplayMode::Expanded)
            .starts_with("Message unconfirmed \u{b7} subagent sub-123")
    );
}
