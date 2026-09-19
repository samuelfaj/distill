use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::agent_view::test_fixtures::{
    add_running_execute, ctrl, key, make_agent, parent_with_child,
};
use crate::app::agent_view::{AgentPane, AgentView, InputMode, ViewSurface};
use crate::app::app_view::InputOutcome;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SubagentBlock;
use crate::scrollback::blocks::tool::{
    SentMessageDelivery, SentMessageInput, SentMessagePresentation, SentMessageTarget,
    SentMessageToolCallBlock, ToolCallBlock,
};
use crate::scrollback::render::ScratchBuffer;
use crate::scrollback::types::DisplayMode;
use crate::views::shortcuts_bar::PendingHint;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Arc;
use std::time::{Duration, Instant};
const CHILD_SID: &str = "child-sid";
fn subagent_row() -> RenderBlock {
    RenderBlock::Subagent(SubagentBlock::started(
        "child task",
        CHILD_SID,
        "general-purpose",
        None,
        None,
        None,
        false,
    ))
}
fn message_row(target: SentMessageTarget) -> RenderBlock {
    RenderBlock::ToolCall(ToolCallBlock::SentMessage(SentMessageToolCallBlock::new(
        SentMessagePresentation::Sent,
        Some(SentMessageInput {
            target,
            delivery: Some(SentMessageDelivery::Steer),
            text: "follow up".to_owned(),
        }),
    )))
}
fn named_child() -> SentMessageTarget {
    SentMessageTarget::Named {
        label: Arc::from("General \u{201c}sleeper\u{201d}"),
        child_session_id: Arc::from(CHILD_SID),
    }
}
/// A parent with `block` as its only, selected scrollback entry; the child view exists only when `owns_child`.
fn parent_selecting(block: RenderBlock, owns_child: bool) -> AgentView {
    let mut parent = if owns_child {
        parent_with_child(CHILD_SID)
    } else {
        make_agent()
    };
    parent.scrollback.push_block(block);
    parent.scrollback.prepare_layout(80, 40);
    parent.scrollback.set_selected(Some(0));
    if parent.scrollback.toggle_group_expansion() {
        parent.scrollback.prepare_layout(80, 40);
    }
    parent
}
#[test]
fn viewer_keys_open_the_linked_child_from_subagent_and_message_rows() {
    let registry = ActionRegistry::defaults();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
    let unresolved = SentMessageTarget::Unresolved {
        subagent_id: CHILD_SID.to_owned(),
    };
    for (case, block, owns_child, opens) in [
        ("subagent row", subagent_row(), true, true),
        ("named message row", message_row(named_child()), true, true),
        (
            "named row, child view gone",
            message_row(named_child()),
            false,
            false,
        ),
        (
            "parent alias",
            message_row(SentMessageTarget::Parent),
            true,
            false,
        ),
        ("unresolved id", message_row(unresolved), true, false),
    ] {
        for chord in [&enter, &ctrl_f] {
            let mut parent = parent_selecting(block.clone(), owns_child);
            let outcome = parent.handle_scrollback_key(chord, &registry);
            assert_eq!(
                opens.then_some(CHILD_SID),
                parent.active_subagent.as_deref(),
                "{case}: {chord:?}"
            );
            assert!(
                matches!(
                    (opens, &outcome),
                    (true, InputOutcome::Changed)
                        | (false, InputOutcome::Action(Action::OpenBlockViewer))
                ),
                "{case}: {chord:?} gave {outcome:?}"
            );
        }
    }
}
#[test]
fn double_click_opens_the_linked_child_or_folds_the_message_row() {
    let now = Instant::now();
    let again = now + Duration::from_millis(10);
    for (case, block, owns_child, expected_child, expected_mode) in [
        (
            "message row",
            message_row(named_child()),
            true,
            Some(CHILD_SID),
            DisplayMode::Collapsed,
        ),
        (
            "message row, child view gone",
            message_row(named_child()),
            false,
            None,
            DisplayMode::Expanded,
        ),
        (
            "subagent row",
            subagent_row(),
            true,
            Some(CHILD_SID),
            DisplayMode::Collapsed,
        ),
        (
            "subagent row, child view gone",
            subagent_row(),
            false,
            None,
            DisplayMode::Collapsed,
        ),
    ] {
        let mut parent = parent_selecting(block, owns_child);
        (parent.last_click, _) = parent.handle_scrollback_click(now, 0, false);
        let _ = parent.handle_scrollback_click(again, 0, false);
        let mode = parent.scrollback.entry(0).map(|entry| entry.display_mode);
        assert_eq!(
            (expected_child, Some(expected_mode)),
            (parent.active_subagent.as_deref(), mode),
            "{case}"
        );
    }
}
#[test]
fn fullscreen_child_ctrl_b_never_demotes_child_or_parent() {
    let registry = ActionRegistry::defaults();
    let child_sid = "child-sid".to_string();
    let mut parent = make_agent();
    add_running_execute(&mut parent);
    assert!(
        parent
            .session
            .tracker
            .running_execute_tool_call_id()
            .is_some()
    );
    let mut child = make_agent();
    add_running_execute(&mut child);
    assert!(
        child
            .session
            .tracker
            .running_execute_tool_call_id()
            .is_some()
    );
    child.set_active_pane(AgentPane::Scrollback, true);
    parent.insert_test_child(child_sid.clone(), Box::new(child));
    assert_eq!(
        ViewSurface::ChildTakeover,
        parent
            .subagent_views
            .get(&child_sid)
            .unwrap_or_else(|| panic!("missing map entry"))
            .surface()
    );
    parent.open_subagent_fullscreen(child_sid.clone());
    let outcome = parent.handle_input(&ctrl('b'), &registry);
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(!matches!(
        outcome,
        InputOutcome::Action(Action::DemoteToBackground)
    ));
    assert_eq!(parent.active_subagent.as_deref(), Some(child_sid.as_str()));
    assert!(
        parent
            .session
            .tracker
            .running_execute_tool_call_id()
            .is_some()
    );
    assert!(
        parent
            .subagent_views
            .get(&child_sid)
            .unwrap_or_else(|| panic!("missing map entry"))
            .session
            .tracker
            .running_execute_tool_call_id()
            .is_some()
    );
    let child = &parent
        .subagent_views
        .get(&child_sid)
        .unwrap_or_else(|| panic!("missing map entry"));
    assert_eq!(ViewSurface::ChildTakeover, child.surface());
    assert!(
        !child
            .current_shortcut_hints(&registry)
            .iter()
            .any(|hint| hint.label == "send to bg")
    );
    assert!(child.hit_bg_button.rect.is_none());
}
/// Every glyph in `area`, row by row.
fn buffer_text(buf: &Buffer, area: Rect) -> String {
    (area.y..area.y + area.height)
        .map(|y| {
            (area.x..area.x + area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
/// The parent paints none of its own chrome under a takeover, so a pending hint can only reach the buffer by being
/// forwarded into the child's shortcuts bar; the child's hidden composer yields no cursor to forward.
#[test]
fn takeover_draw_forwards_pending_hint_and_child_cursor() {
    let registry = ActionRegistry::defaults();
    let mut parent = parent_with_child("child");
    parent.open_subagent_fullscreen("child".to_owned());
    let area = Rect::new(0, 0, 80, 30);
    let mut buf = Buffer::empty(area);
    let mut scratch = ScratchBuffer::new();
    let (cursor, _) = parent.draw(
        area,
        &mut buf,
        &registry,
        &mut scratch,
        Some(PendingHint {
            shortcut: crate::key!(Esc),
            label: "clear pending hint",
        }),
        false,
        crate::app::agent_view::BannerSlotParams::none(),
        &crate::app::bundle::BundleState::default(),
        false,
        &mut Vec::new(),
        crate::app::agent_view::AppRenderParams::default(),
    );
    assert_eq!(None, cursor);
    let text = buffer_text(&buf, area);
    assert!(text.contains("press again to clear pending hint"), "{text}");
    assert_eq!(
        0,
        parent
            .subagent_view("child")
            .expect("child view")
            .pane_areas
            .prompt
            .height
    );
}
/// A parent whose child, with a transcript, sits on bare scrollback under an open takeover. `vim_mode` is pinned on
/// both views: `AgentView::new` reads it from the user's config, which CI does not have.
fn open_takeover(child_sid: &str, vim_mode: bool) -> AgentView {
    let mut parent = parent_with_child(child_sid);
    let child = parent.subagent_view_mut(child_sid).expect("child view");
    add_running_execute(child);
    child.set_input_mode(InputMode::Vim);
    parent.set_vim_mode_recursive(vim_mode);
    parent.open_subagent_fullscreen(child_sid.to_owned());
    parent
}
/// Root-only chords never open a modal on the child under either binding set; the takeover stays up and `q` still
/// closes it. `?` is the palette's alt key and dies in the funnel on both; with vim bindings it is query text once the
/// child's search is open. `Shift+/` matches no chord, so without vim bindings it falls to type-to-focus, whose
/// `FocusPrompt` the composer guard refuses.
#[test]
fn child_root_only_chords_are_swallowed() {
    let registry = ActionRegistry::defaults();
    for vim_mode in [true, false] {
        let mut parent = open_takeover("child", vim_mode);
        for chord in [
            ctrl('p'),
            ctrl('m'),
            ctrl('r'),
            ctrl('o'),
            ctrl('l'),
            key(KeyCode::F(2)),
        ] {
            let outcome = parent.handle_input(&chord, &registry);
            assert!(
                matches!(outcome, InputOutcome::Changed | InputOutcome::Unchanged),
                "vim={vim_mode} {chord:?}: {outcome:?}"
            );
            let child = parent.subagent_view("child").expect("child view");
            assert!(
                child.active_modal.is_none(),
                "vim={vim_mode} {chord:?} opened a modal"
            );
            assert!(
                child.is_bare_scrollback(),
                "vim={vim_mode} {chord:?} left bare scrollback"
            );
        }
        let question = key(KeyCode::Char('?'));
        let shift_slash = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::SHIFT));
        for printable in [&question, &shift_slash] {
            let outcome = parent.handle_input(printable, &registry);
            let expected = if vim_mode || printable == &question {
                matches!(outcome, InputOutcome::Changed | InputOutcome::Unchanged)
            } else {
                matches!(
                    outcome,
                    InputOutcome::ActionThenForward(Action::FocusPrompt)
                )
            };
            assert!(expected, "vim={vim_mode} {printable:?}: {outcome:?}");
            let child = parent.subagent_view("child").expect("child view");
            assert!(
                child.active_modal.is_none(),
                "vim={vim_mode} {printable:?} opened a modal"
            );
            assert!(
                child.is_bare_scrollback(),
                "vim={vim_mode} {printable:?} left bare scrollback"
            );
            assert_eq!(AgentPane::Scrollback, child.active_pane);
        }
        assert_eq!(Some("child"), parent.active_subagent.as_deref());
        if vim_mode {
            parent.handle_input(&key(KeyCode::Char('/')), &registry);
            parent.handle_input(&key(KeyCode::Char('?')), &registry);
            let child = parent.subagent_view("child").expect("child view");
            assert!(child.active_modal.is_none());
            assert_eq!(
                Some("?"),
                child.scrollback_search.as_ref().map(|s| s.query())
            );
            parent.handle_input(&key(KeyCode::Esc), &registry);
            assert!(
                parent
                    .subagent_view("child")
                    .expect("child view")
                    .scrollback_search
                    .is_none()
            );
            assert_eq!(Some("child"), parent.active_subagent.as_deref());
        }
        parent.handle_input(&key(KeyCode::Char('q')), &registry);
        assert_eq!(None, parent.active_subagent, "vim={vim_mode}");
    }
}
/// The child resolves a key pane-first exactly as a root does: with the mouse-capture toggle enabled, Ctrl+R on
/// scrollback is `ToggleMouseCapture` (allowed) rather than the `OpenSessions` chord it maps to under `AgentScreen`.
#[test]
fn child_ctrl_r_keeps_scrollback_precedence() {
    let mut parent = open_takeover("child", true);
    let outcome = parent.handle_input(&ctrl('r'), &ActionRegistry::defaults_with_config(true));
    assert!(
        matches!(outcome, InputOutcome::Action(Action::ToggleMouseCapture)),
        "{outcome:?}"
    );
    let outcome = parent.handle_input(&ctrl('r'), &ActionRegistry::defaults());
    assert!(matches!(outcome, InputOutcome::Changed), "{outcome:?}");
    let child = parent.subagent_view("child").expect("child view");
    assert!(child.active_modal.is_none());
    assert!(child.is_bare_scrollback());
}
