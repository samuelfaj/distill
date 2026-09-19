//! Mouse-routing tests for the line viewer's plan preview: the scrollbar must own a click-and-drag gesture end-to-end.
//! A press on the track was previously also treated as a comment-gutter anchor (the hit test was row-only).
//! Dragging the thumb then selected plan lines for a comment instead of scrolling.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::actions::ActionRegistry;
use crate::app::agent_view::AgentView;
use crate::app::agent_view::test_fixtures::make_agent;
use crate::views::plan_approval_view::PlanApprovalFocus;

const POPUP: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 10,
};
/// Scrollbar track column as split off by the list pane render (`maybe_split_for_scrollbar`): last column of the popup area.
const TRACK_X: u16 = 79;

fn mouse(kind: MouseEventKind, col: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

/// Agent showing a plan-approval preview whose plan overflows the viewport, with the render-time areas planted so mouse dispatch works.
fn agent_with_scrollable_plan() -> AgentView {
    let mut agent = make_agent();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let plan: String = (1..=60).fold(String::new(), |mut acc, i| {
        acc.push_str(&format!("step {i}\n"));
        acc
    });
    let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
        session_id: "test-session".into(),
        tool_call_id: "call-1".into(),
        plan_content: Some(plan),
    };
    agent.plan_approval_view = Some(
        crate::views::plan_approval_view::PlanApprovalViewState::new(
            request,
            crate::views::prompt_widget::StashedPrompt {
                text: String::new(),
                cursor: 0,
                images: Vec::new(),
                chip_elements: Vec::new(),
                image_counter: 0,
                image_undo_stash: Vec::new(),
            },
            tx,
        ),
    );
    agent.show_plan_preview();

    let viewer = agent
        .line_viewer
        .as_mut()
        .expect("plan preview opens the line viewer");
    viewer.prepare_layout(POPUP.width, POPUP.height);
    viewer.last_popup_area = Some(POPUP);
    viewer.last_modal_area = Some(Rect::new(0, 0, 80, 12));
    viewer
        .list_state
        .set_scrollbar_area(Some(Rect::new(TRACK_X, POPUP.y, 1, POPUP.height)));
    assert!(
        viewer.list_state.total_height() > POPUP.height as usize,
        "plan must overflow the viewport so the scrollbar is live"
    );
    agent
}

/// Presses on the modal border column next to the track used to fall into the click-outside-modal path instead of grabbing the thumb.
/// Users read the thumb and the border as one two-column scrollbar.
#[test]
fn border_column_press_grabs_scrollbar() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X + 1, 5),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().expect("viewer stays open");
    assert!(
        viewer.list_state.is_scrollbar_dragging(),
        "press one column right of the track (modal border) must grab the thumb"
    );
    assert!(
        viewer.list_state.scroll_offset() > 0,
        "the press must scroll toward the clicked track position"
    );
    assert!(
        viewer
            .plan_ref()
            .and_then(|p| p.gutter_drag_start)
            .is_none(),
        "a border-column press must not anchor a comment-gutter drag"
    );
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(pav.focus, PlanApprovalFocus::Preview);

    let offset_after_press = agent
        .line_viewer
        .as_ref()
        .unwrap()
        .list_state
        .scroll_offset();
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), TRACK_X + 1, 9),
        &registry,
    );
    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        viewer.list_state.scroll_offset() > offset_after_press,
        "dragging on the border column must keep scrolling (offset {} -> {})",
        offset_after_press,
        viewer.list_state.scroll_offset()
    );
}

#[test]
fn gap_column_press_grabs_scrollbar() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X - 1, 5),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        viewer.list_state.is_scrollbar_dragging(),
        "press on the gap column must grab the thumb"
    );
    assert!(
        viewer
            .plan_ref()
            .and_then(|p| p.gutter_drag_start)
            .is_none(),
        "a gap-column press must not anchor a comment-gutter drag"
    );
}

#[test]
fn border_column_press_does_not_close_casual_preview() {
    let mut agent = agent_with_scrollable_plan();
    agent.plan_approval_view = None;
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X + 1, 5),
        &registry,
    );

    let viewer = agent
        .line_viewer
        .as_ref()
        .expect("a border-column press must not close the casual preview");
    assert!(viewer.list_state.is_scrollbar_dragging());
}

#[test]
fn press_beyond_grab_zone_still_closes_casual_preview() {
    let mut agent = agent_with_scrollable_plan();
    agent.plan_approval_view = None;
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X + 2, 5),
        &registry,
    );

    assert!(
        agent.line_viewer.is_none(),
        "a click two columns right of the track is outside the modal and must close it"
    );
}

#[test]
fn scrollbar_press_does_not_enter_commenting() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X, 5),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        viewer.list_state.is_scrollbar_dragging(),
        "press on the track must latch a scrollbar drag"
    );
    assert!(
        viewer
            .plan_ref()
            .and_then(|p| p.gutter_drag_start)
            .is_none(),
        "press on the track must not anchor a comment-gutter drag"
    );
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(
        pav.focus,
        PlanApprovalFocus::Preview,
        "press on the track must not enter commenting"
    );
}

#[test]
fn scrollbar_drag_scrolls_plan_instead_of_selecting_lines() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X, 2),
        &registry,
    );
    let offset_after_press = agent
        .line_viewer
        .as_ref()
        .unwrap()
        .list_state
        .scroll_offset();

    // Drag the thumb to the bottom of the track.
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), TRACK_X, 9),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        viewer.list_state.scroll_offset() > offset_after_press,
        "dragging the thumb down must scroll the plan (offset {} -> {})",
        offset_after_press,
        viewer.list_state.scroll_offset()
    );
    assert!(
        viewer.plan_ref().and_then(|p| p.gutter_drag_end).is_none(),
        "thumb drag must not extend a comment line selection"
    );

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), TRACK_X, 9),
        &registry,
    );
    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        !viewer.list_state.is_scrollbar_dragging(),
        "release must end the scrollbar drag"
    );
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(
        pav.commenting_range, None,
        "releasing the thumb must not open a comment on the dragged lines"
    );
    assert_eq!(pav.focus, PlanApprovalFocus::Preview);
}

/// The thumb must keep following the pointer when a drag drifts off the popup rect (standard scrollbar behavior in every toolkit).
#[test]
fn scrollbar_drag_outside_popup_keeps_scrolling() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X, 8),
        &registry,
    );
    let offset_after_press = agent
        .line_viewer
        .as_ref()
        .unwrap()
        .list_state
        .scroll_offset();
    assert!(offset_after_press > 0, "press near the bottom scrolls down");

    // Pointer drifts left of the track and above the popup while dragging.
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 40, 0),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        viewer.list_state.scroll_offset() < offset_after_press,
        "drag toward the top of the track must scroll back up (offset {} -> {})",
        offset_after_press,
        viewer.list_state.scroll_offset()
    );
    assert!(
        viewer.plan_ref().and_then(|p| p.gutter_drag_end).is_none(),
        "scrollbar drag must never turn into a comment line selection"
    );
}

/// A gutter line-selection whose Up was lost must not survive a later scrollbar gesture.
/// The track press drops the stale anchor, so a stray release afterwards cannot commit the leftover lines as a comment.
#[test]
fn scrollbar_gesture_drops_stale_gutter_anchor() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    // Anchor and extend a comment line selection, then lose the Up
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), 10, 4),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 10, 6),
        &registry,
    );
    {
        let viewer = agent.line_viewer.as_ref().unwrap();
        let start = viewer.plan_ref().and_then(|p| p.gutter_drag_start);
        let end = viewer.plan_ref().and_then(|p| p.gutter_drag_end);
        assert!(
            start.is_some() && end.is_some() && start != end,
            "precondition: a multi-line gutter drag is live (start {start:?}, end {end:?})"
        );
    }
    // Scrollbar click and release: the track press must drop the stale anchor
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X, 5),
        &registry,
    );
    {
        let viewer = agent.line_viewer.as_ref().unwrap();
        assert!(viewer.list_state.is_scrollbar_dragging());
        assert!(
            viewer
                .plan_ref()
                .and_then(|p| p.gutter_drag_start)
                .is_none()
                && viewer.plan_ref().and_then(|p| p.gutter_drag_end).is_none(),
            "track press must drop a stale comment-gutter anchor"
        );
    }
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), TRACK_X, 5),
        &registry,
    );

    // The track press also discarded the in-progress comment draft (same rule as clicking back into the modal)
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(pav.commenting_range, None);
    assert_eq!(pav.focus, PlanApprovalFocus::Preview);

    // A stray release on content must not commit the leftover lines.
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), 10, 6),
        &registry,
    );
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(
        pav.commenting_range, None,
        "stale gutter lines must not be committed as a comment range"
    );
    assert_eq!(
        pav.focus,
        PlanApprovalFocus::Preview,
        "a stray release must not re-enter commenting"
    );
}

/// A second multi-line gutter drag while already Commenting must not replace the frozen freeform stash with the unsaved comment draft.
#[test]
fn gutter_drag_while_commenting_does_not_clobber_freeform_stash() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    agent.prompt.set_text("keep my freeform notes");
    // First multi-line drag: enter commenting and freeze freeform.
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), 10, 4),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 10, 6),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), 10, 6),
        &registry,
    );
    {
        let pav = agent.plan_approval_view.as_ref().unwrap();
        assert_eq!(pav.focus, PlanApprovalFocus::Commenting);
        assert_eq!(
            pav.stashed_feedback_prompt
                .as_ref()
                .map(|s| s.text.as_str()),
            Some("keep my freeform notes")
        );
    }
    agent.prompt.set_text("unsaved comment draft");

    // Second multi-line drag: new range, must keep original freeform stash.
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), 10, 5),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 10, 7),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), 10, 7),
        &registry,
    );
    {
        let pav = agent.plan_approval_view.as_ref().unwrap();
        assert_eq!(pav.focus, PlanApprovalFocus::Commenting);
        assert_eq!(
            pav.stashed_feedback_prompt
                .as_ref()
                .map(|s| s.text.as_str()),
            Some("keep my freeform notes"),
            "second gutter drag must not replace freeform with comment draft"
        );
    }
    // Cancel commenting: freeform must restore, not the abandoned draft.
    agent.prompt.set_text("another draft");
    let esc = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    let _ = agent.handle_plan_feedback_key(&esc);
    assert_eq!(agent.prompt.text(), "keep my freeform notes");
}

/// A lost mouse-up after a track press must not leave `is_scrollbar_dragging` sticky.
/// The next plan-line click must still anchor the gutter and enter click-to-comment.
#[test]
fn lost_scrollbar_up_does_not_block_next_line_click() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X, 5),
        &registry,
    );
    assert!(
        agent
            .line_viewer
            .as_ref()
            .unwrap()
            .list_state
            .is_scrollbar_dragging(),
        "precondition: track press latched a thumb drag"
    );

    // No Up: simulate a dropped release, then click a plan line
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), 10, 4),
        &registry,
    );

    let viewer = agent.line_viewer.as_ref().unwrap();
    assert!(
        !viewer.list_state.is_scrollbar_dragging(),
        "content Down must clear the stale scrollbar latch"
    );
    assert!(
        viewer
            .plan_ref()
            .and_then(|p| p.gutter_drag_start)
            .is_some(),
        "content Down must still anchor a comment-gutter drag"
    );
    let pav = agent.plan_approval_view.as_ref().unwrap();
    assert_eq!(
        pav.focus,
        PlanApprovalFocus::Commenting,
        "content Down must still enter click-to-comment"
    );
}

#[test]
fn wheel_on_border_column_scrolls_plan() {
    let mut agent = agent_with_scrollable_plan();
    let registry = ActionRegistry::defaults();

    let _ = agent.handle_input(
        &mouse(MouseEventKind::Down(MouseButton::Left), TRACK_X + 1, 9),
        &registry,
    );
    let _ = agent.handle_input(
        &mouse(MouseEventKind::Up(MouseButton::Left), TRACK_X + 1, 9),
        &registry,
    );
    let off = agent
        .line_viewer
        .as_ref()
        .unwrap()
        .list_state
        .scroll_offset();
    assert!(off > 0, "border click near track bottom scrolls down");

    agent.handle_scroll(-3, TRACK_X + 1, 5);
    let off_after = agent
        .line_viewer
        .as_ref()
        .unwrap()
        .list_state
        .scroll_offset();
    assert!(
        off_after < off,
        "wheel-up on the border column must scroll up ({off} -> {off_after})"
    );
}

fn enter_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn agent_with_markdown_viewer(text: &str) -> AgentView {
    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::agent_message(text));
    let mut viewer = {
        let entry = agent.scrollback.get_by_id(id).expect("just pushed");
        crate::views::block_viewer::BlockViewerPane::for_markdown(id, entry)
            .expect("markdown viewer")
    };
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    agent.block_viewer = Some(viewer);
    agent
}

fn agent_with_running_markdown_viewer(text: &str) -> AgentView {
    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::agent_message(text));
    agent
        .scrollback
        .get_by_id_mut(id)
        .expect("just pushed")
        .is_running = true;
    let mut viewer = {
        let entry = agent.scrollback.get_by_id(id).expect("just pushed");
        crate::views::block_viewer::BlockViewerPane::for_markdown(id, entry)
            .expect("markdown viewer")
    };
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    agent.block_viewer = Some(viewer);
    agent
}

#[test]
fn block_viewer_enter_quotes_current_line_and_closes() {
    let mut agent = agent_with_markdown_viewer("hello world");
    let outcome = agent.handle_block_viewer_key(&enter_key());
    assert!(matches!(
        outcome,
        crate::app::app_view::InputOutcome::Changed
    ));
    assert!(agent.block_viewer.is_none());
    assert_eq!(agent.active_pane, crate::app::agent_view::AgentPane::Prompt);
    let text = agent.prompt.text();
    assert!(
        text.contains("> hello world"),
        "quoted line missing from prompt: {text:?}"
    );
    assert!(
        text.ends_with("\n\n"),
        "quote should end with a blank line, got {text:?}"
    );
    assert!(agent.block_viewer_resume.is_some());
}

#[test]
fn block_viewer_enter_starts_quote_on_its_own_line() {
    let mut agent = agent_with_markdown_viewer("hello world");
    agent.prompt.set_text("draft");
    agent.prompt.set_cursor(agent.prompt.text().len());
    agent.handle_block_viewer_key(&enter_key());
    assert_eq!(agent.prompt.text(), "draft\n> hello world\n\n");
}

#[test]
fn block_viewer_enter_delimit_uses_selection_start() {
    let mut agent = agent_with_markdown_viewer("hello world");
    agent.prompt.set_text("prefix\nmore");
    agent.prompt.textarea.set_selection(3, 7);
    agent.handle_block_viewer_key(&enter_key());
    assert_eq!(agent.prompt.text(), "pre\n> hello world\n\nmore");
}

#[test]
fn block_viewer_enter_quotes_last_line_while_following() {
    let mut agent = agent_with_running_markdown_viewer("hello\n\nworld");
    assert!(agent.block_viewer.as_ref().unwrap().list_state.follow_mode);
    assert_eq!(
        agent
            .block_viewer
            .as_ref()
            .unwrap()
            .list_state
            .selected_index(),
        None
    );
    let outcome = agent.handle_block_viewer_key(&enter_key());
    assert!(matches!(
        outcome,
        crate::app::app_view::InputOutcome::Changed
    ));
    assert!(agent.block_viewer.is_none());
    let text = agent.prompt.text();
    assert!(
        text.contains("> world"),
        "follow-mode Enter should quote the last line, got {text:?}"
    );
}

#[test]
fn block_viewer_enter_pastes_chip_for_four_lines() {
    use crate::views::block_viewer::{TextDrag, TextEndpoint};
    use crate::views::prompt_widget::KIND_PASTE;

    let mut agent = make_agent();
    let mut viewer =
        crate::views::block_viewer::BlockViewerPane::for_plain_text("t", "one\ntwo\nthree\nfour");
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    viewer.text_drag = Some(TextDrag {
        anchor: TextEndpoint {
            item_idx: 2,
            col: 0,
        },
        head: TextEndpoint {
            item_idx: 5,
            col: 3,
        },
        active: false,
    });
    agent.block_viewer = Some(viewer);

    agent.handle_block_viewer_key(&enter_key());
    assert!(agent.block_viewer.is_none());
    assert!(
        agent
            .prompt
            .textarea
            .elements()
            .iter()
            .any(|e| e.kind == KIND_PASTE),
        "4-line quote should become a paste chip"
    );
    let text = agent.prompt.text();
    assert!(text.contains("> one"));
    assert!(text.contains("> four"));
    assert!(
        text.ends_with("\n\n"),
        "quote should end with a blank line, got {text:?}"
    );
}

#[test]
fn block_viewer_search_enter_does_not_quote() {
    let mut agent = agent_with_markdown_viewer("hello world");
    agent.handle_block_viewer_key(&KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(
        agent
            .block_viewer
            .as_ref()
            .unwrap()
            .list_state
            .input_mode()
            .is_some()
    );
    agent.handle_block_viewer_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    agent.handle_block_viewer_key(&enter_key());
    assert!(
        agent.block_viewer.is_some(),
        "search-bar Enter must keep the viewer open"
    );
    assert!(
        agent.prompt.text().is_empty(),
        "search-bar Enter must not quote into the prompt"
    );
}

#[test]
fn block_viewer_enter_on_empty_selection_keeps_viewer_open() {
    let mut agent = make_agent();
    let mut viewer =
        crate::views::block_viewer::BlockViewerPane::for_plain_text("t", "hello\n\nworld");
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    viewer.select_body_line_for_test(3);
    agent.block_viewer = Some(viewer);

    agent.handle_block_viewer_key(&enter_key());
    assert!(
        agent.block_viewer.is_some(),
        "Enter with nothing to quote must keep the viewer open"
    );
    assert!(
        agent.prompt.text().is_empty(),
        "Enter with nothing to quote must not insert into the prompt"
    );
}

#[test]
fn block_viewer_esc_clears_sticky_then_closes() {
    use crate::views::block_viewer::{TextDrag, TextEndpoint};

    let mut agent = agent_with_markdown_viewer("hello world");
    {
        let viewer = agent.block_viewer.as_mut().unwrap();
        viewer.text_drag = Some(TextDrag {
            anchor: TextEndpoint {
                item_idx: 0,
                col: 0,
            },
            head: TextEndpoint {
                item_idx: 0,
                col: 5,
            },
            active: false,
        });
    }
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    agent.handle_block_viewer_key(&esc);
    assert!(
        agent.block_viewer.is_some(),
        "first Esc should clear the highlight, not close"
    );
    assert!(agent.block_viewer.as_ref().unwrap().text_drag.is_none());
    agent.handle_block_viewer_key(&esc);
    assert!(agent.block_viewer.is_none());
    assert!(agent.block_viewer_resume.is_some());
}

#[test]
fn block_viewer_enter_from_fullscreen_child_quotes_into_parent() {
    let mut child = agent_with_markdown_viewer("hello world");
    child.prompt.set_text("child-draft");
    let mut parent = make_agent();
    parent.prompt.set_text("parent-draft");
    parent.prompt.set_cursor(parent.prompt.text().len());
    parent.insert_test_child("child-sid".into(), Box::new(child));
    parent.open_subagent_fullscreen("child-sid".into());
    let registry = ActionRegistry::defaults();
    let outcome = parent.handle_input(&Event::Key(enter_key()), &registry);
    assert!(matches!(
        outcome,
        crate::app::app_view::InputOutcome::Changed
    ));
    assert!(parent.active_subagent.is_none());
    assert_eq!(parent.prompt.text(), "parent-draft\n> hello world\n\n");
    assert_eq!(
        parent.active_pane,
        crate::app::agent_view::AgentPane::Prompt
    );
    if let Some(child) = parent.subagent_views.get("child-sid") {
        assert!(child.block_viewer.is_none());
        assert_eq!(child.prompt.text(), "child-draft");
    }
}

#[test]
fn install_block_viewer_ignores_missing_resume_id() {
    use crate::app::agent_view::BlockViewerResume;
    use crate::scrollback::block::RenderBlock;
    use crate::views::block_viewer::BlockViewerPane;

    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(RenderBlock::agent_message("hello\n\nworld"));
    agent.block_viewer_resume = Some(BlockViewerResume {
        entry_id: id,
        kind: crate::views::block_viewer::ViewerKind::Markdown,
        selected_id: Some(99_999),
        scroll_offset: 0,
        follow_mode: false,
    });
    let pane = {
        let entry = agent.scrollback.get_by_id(id).expect("entry");
        BlockViewerPane::for_markdown(id, entry).expect("markdown")
    };
    agent.install_block_viewer(pane);
    let viewer = agent.block_viewer.as_mut().expect("installed");
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    assert_eq!(viewer.selected_plain_text(), "hello");
}

#[test]
fn block_viewer_enter_quotes_preamble_line() {
    use crate::scrollback::block::RenderBlock;
    use crate::views::block_viewer::BlockViewerPane;
    use ratatui::text::Line;

    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(RenderBlock::agent_message("hello"));
    let mut viewer = {
        let entry = agent.scrollback.get_by_id(id).expect("entry");
        BlockViewerPane::for_markdown(id, entry).expect("markdown")
    };
    viewer.install_prepend_lines(&[Line::from("Read src/main.rs")]);
    viewer.list_state.select_by_id(u64::MAX);
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    agent.block_viewer = Some(viewer);
    agent.handle_block_viewer_key(&enter_key());
    assert!(agent.block_viewer.is_none());
    assert!(
        agent.prompt.text().contains("> Read src/main.rs"),
        "header line should quote, got {:?}",
        agent.prompt.text()
    );
}

#[test]
fn insert_quoted_reply_clears_bash_mode() {
    let mut agent = agent_with_markdown_viewer("hello world");
    agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
    agent.prompt.set_text("draft");
    agent.handle_block_viewer_key(&enter_key());
    assert_eq!(
        agent.prompt_input_mode,
        crate::app::agent_view::PromptInputMode::Normal
    );
    assert!(agent.prompt.text().contains("> hello world"));
}

#[test]
fn insert_quoted_reply_is_one_undo() {
    let mut agent = agent_with_markdown_viewer("hello world");
    agent.prompt.set_text("draft");
    agent.prompt.set_cursor(5);
    agent.handle_block_viewer_key(&enter_key());
    assert!(agent.prompt.textarea.undo());
    assert_eq!(agent.prompt.text(), "draft");

    let mut agent = agent_with_markdown_viewer("hello world");
    agent.prompt.set_text("pre\nmore");
    agent.prompt.textarea.set_selection(0, 4);
    agent.handle_block_viewer_key(&enter_key());
    assert!(agent.prompt.textarea.undo());
    assert_eq!(agent.prompt.text(), "pre\nmore");
}

#[test]
fn install_block_viewer_ignores_mismatched_kind() {
    use crate::app::agent_view::BlockViewerResume;
    use crate::scrollback::block::RenderBlock;
    use crate::views::block_viewer::{BlockViewerPane, ViewerKind};

    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(RenderBlock::agent_message("hello\n\nworld"));
    agent.block_viewer_resume = Some(BlockViewerResume {
        entry_id: id,
        kind: ViewerKind::PlainText,
        selected_id: Some(0),
        scroll_offset: 99,
        follow_mode: false,
    });
    let pane = {
        let entry = agent.scrollback.get_by_id(id).expect("entry");
        BlockViewerPane::for_markdown(id, entry).expect("markdown")
    };
    agent.install_block_viewer(pane);
    let viewer = agent.block_viewer.as_mut().expect("installed");
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    assert_eq!(viewer.selected_plain_text(), "hello");
    assert_ne!(viewer.list_state.scroll_offset(), 99);
}

#[test]
fn install_block_viewer_restores_live_preamble_id() {
    use crate::app::agent_view::BlockViewerResume;
    use crate::scrollback::block::RenderBlock;
    use crate::views::block_viewer::BlockViewerPane;
    use ratatui::text::Line;

    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(RenderBlock::agent_message("hello\n\nworld"));
    agent.block_viewer_resume = Some(BlockViewerResume {
        entry_id: id,
        kind: crate::views::block_viewer::ViewerKind::Markdown,
        selected_id: Some(u64::MAX),
        scroll_offset: 0,
        follow_mode: false,
    });
    let pane = {
        let entry = agent.scrollback.get_by_id(id).expect("entry");
        BlockViewerPane::for_markdown(id, entry).expect("markdown")
    };
    agent.install_block_viewer(pane);
    let viewer = agent.block_viewer.as_mut().expect("installed");
    viewer.install_prepend_lines(&[Line::from("header")]);
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    assert_eq!(viewer.list_state.selected_id(), Some(u64::MAX));
    assert_eq!(viewer.selected_plain_text(), "header");
}

fn test_bg_task(
    task_id: &str,
    stdout: &str,
    scrollback_entry_id: Option<crate::scrollback::entry::EntryId>,
) -> crate::app::agent::BgTaskState {
    let mut task = crate::app::agent::BgTaskState {
        task_id: task_id.into(),
        tool_call_id: format!("call-{task_id}"),
        command: "echo".into(),
        description: None,
        cwd: "/tmp".into(),
        output_file: "/tmp/out".into(),
        status: crate::app::agent::BgTaskStatus::Done,
        start_time: std::time::SystemTime::now(),
        end_time: None,
        exit_code: Some(0),
        signal: None,
        stdout: String::new(),
        stdout_line_count: 0,
        truncated: false,
        pending_kill: false,
        kill_requested_at: None,
        scrollback_entry_id,
        is_monitor: false,
        restored_from_replay: false,
    };
    task.set_stdout(stdout.to_string());
    task
}

/// A task with no scrollback anchor (completed-early race, scrollback swap)
/// still opens its viewer from the task's own stdout, on the sentinel anchor.
#[test]
fn show_bg_task_viewer_opens_unattached() {
    let mut agent = make_agent();
    agent
        .session
        .bg_tasks
        .insert("orphan".into(), test_bg_task("orphan", "out", None));
    assert!(agent.show_bg_task_viewer("orphan"));
    let viewer = agent.block_viewer.as_ref().expect("opened");
    assert_eq!(viewer.entry_id, crate::scrollback::entry::EntryId::new(0));
    assert_eq!(viewer.bg_task_id.as_deref(), Some("orphan"));
}

#[test]
fn show_bg_task_viewer_restores_resume() {
    use crate::scrollback::block::RenderBlock;

    let mut agent = make_agent();
    let id = agent
        .scrollback
        .push_block(RenderBlock::agent_message("task body"));
    agent
        .session
        .bg_tasks
        .insert("t1".into(), test_bg_task("t1", "one\ntwo\nthree", Some(id)));
    assert!(agent.show_bg_task_viewer("t1"));
    let selected_id = {
        let viewer = agent.block_viewer.as_mut().expect("opened");
        assert_eq!(viewer.entry_id, id);
        assert_ne!(viewer.entry_id, crate::scrollback::entry::EntryId::new(0));
        viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
        viewer.select_body_line_for_test(1);
        viewer.list_state.selected_id()
    };
    agent.dismiss_block_viewer();
    assert!(agent.show_bg_task_viewer("t1"));
    let viewer = agent.block_viewer.as_mut().expect("reopened");
    viewer.prepare_for_test(Rect::new(0, 0, 80, 24));
    assert_eq!(viewer.list_state.selected_id(), selected_id);
    assert_eq!(viewer.selected_plain_text(), "two");
}
