use super::*;
use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};

fn area() -> Rect {
    Rect::new(0, 0, 80, 24)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn pane(text: &str) -> BlockViewerPane {
    let mut pane = BlockViewerPane::for_plain_text("title", text);
    pane.prepare_for_test(area());
    pane
}

fn running_markdown_pane(text: &str) -> BlockViewerPane {
    let entry = ScrollbackEntry::running(RenderBlock::agent_message(text));
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.prepare_for_test(area());
    pane
}

#[test]
fn format_blockquote_cases() {
    assert_eq!(format_blockquote(""), "");
    assert_eq!(format_blockquote("\n"), "");
    assert_eq!(format_blockquote("\n\n"), "");
    assert_eq!(format_blockquote("hello"), "> hello");
    assert_eq!(format_blockquote("hello\n"), "> hello");
    assert_eq!(format_blockquote("one\n\ntwo"), "> one\n>\n> two");
    assert_eq!(
        format_blockquote("one\ntwo\nthree\n"),
        "> one\n> two\n> three"
    );
}

#[test]
fn mouse_up_leaves_sticky_highlight() {
    let mut pane = pane("hello world");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 5, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 5, 2);
    let drag = pane.text_drag.expect("sticky highlight after mouse-up");
    assert!(!drag.active);
    assert!(drag.is_non_empty());
    assert!(pane.drag_copy_text.is_some());
}

#[test]
fn double_click_selects_word() {
    let mut pane = pane("hello world");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    let drag = pane.text_drag.expect("word range after double-click");
    assert!(!drag.active);
    let copied = pane.drag_copy_text.as_deref().unwrap_or("");
    assert_eq!(copied, "hello");
    assert_eq!(pane.selected_plain_text(), "hello");
}

#[test]
fn triple_click_selects_paragraph() {
    let mut pane = pane("alpha line\nbeta line\n\ngamma line");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    let copied = pane.drag_copy_text.as_deref().unwrap_or("");
    assert!(
        copied.contains("alpha line") && copied.contains("beta line"),
        "triple-click should cover the paragraph, got {copied:?}"
    );
    assert!(
        !copied.contains("gamma line"),
        "triple-click should stop at the blank line, got {copied:?}"
    );
}

#[test]
fn triple_click_blank_does_not_stick() {
    let mut pane = pane("hello\n\nworld");
    for _ in 0..3 {
        pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 3);
        pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 3);
    }
    assert!(
        pane.text_drag.is_none(),
        "triple-click on a blank line must not leave a sticky highlight"
    );
    assert!(pane.is_close_key(&esc()));
}

#[test]
fn preamble_len_change_clears_sticky_highlight() {
    let mut pane = pane("body line");
    pane.install_prepend_lines(&[Line::from("header")]);
    pane.text_drag = Some(TextDrag {
        anchor: TextEndpoint {
            item_idx: 1,
            col: 0,
        },
        head: TextEndpoint {
            item_idx: 1,
            col: 4,
        },
        active: false,
    });
    pane.install_prepend_lines(&[Line::from("header"), Line::from(""), Line::from("wrapped")]);
    assert!(
        pane.text_drag.is_none(),
        "sticky highlight must drop when preamble length changes"
    );
}

#[test]
fn selected_plain_text_preamble_drag_quotes_header() {
    let mut pane = pane("body line");
    pane.install_prepend_lines(&[Line::from("header")]);
    pane.prepare_for_test(area());
    pane.select_body_line_for_test(2);
    pane.text_drag = Some(TextDrag {
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
    assert_eq!(pane.selected_plain_text(), "header");
}

#[test]
fn selected_plain_text_follow_mode_skips_offscreen_lines() {
    let mut pane = running_markdown_pane("tail");
    let start_id = pane.items.len() as u64;
    for i in 0..40 {
        pane.items.push(ContentLine {
            content: Line::default(),
            plain_text: String::new(),
            id: start_id + i,
            bg: None,
        });
    }
    pane.prepare_for_test(Rect::new(0, 0, 80, 8));
    assert!(pane.list_state.follow_mode);
    assert_eq!(pane.list_state.selected_index(), None);
    assert!(
        pane.selected_plain_text().is_empty(),
        "trailing blanks filling the viewport must not quote off-screen text"
    );
}

#[test]
fn selected_plain_text_filter_uses_physical_index() {
    let mut pane = pane("alpha\nbeta\ngamma");
    pane.list_state
        .set_filter(Some(crate::views::list_pane::FilterMatcher::substring(
            "gamma",
        )));
    pane.prepare_for_test(area());
    assert_eq!(pane.list_state.selected_index(), Some(0));
    assert_eq!(pane.selected_plain_text(), "gamma");
}

#[test]
fn selected_plain_text_visual_filter_uses_physical_index() {
    let mut pane = pane("alpha\nzzz\nalso");
    pane.list_state
        .set_filter(Some(crate::views::list_pane::FilterMatcher::substring("a")));
    pane.prepare_for_test(area());
    assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    pane.prepare_for_test(area());
    assert_eq!(pane.selected_plain_text(), "alpha\nalso");
}

fn double_click_word(pane: &mut BlockViewerPane) {
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
}

#[test]
fn clearing_highlight_resets_multi_click() {
    {
        let mut pane = pane("hello world\n\nsecond");
        double_click_word(&mut pane);
        assert_eq!(pane.selected_plain_text(), "hello");
        assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
        pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
        assert!(
            pane.text_drag.is_none(),
            "j must reset click_count so the next click is not a paragraph select"
        );
    }

    let mut pane = pane("hello world\n\nsecond");
    double_click_word(&mut pane);
    assert!(pane.handle_key(&esc()));
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    assert!(
        pane.text_drag.is_none(),
        "Esc must reset click_count so the next click is not a paragraph select"
    );
    assert_eq!(pane.selected_plain_text(), "hello world");
}

#[test]
fn sticky_highlight_clears_on_raw_toggle() {
    let mut pane = pane("hello world\nsecond line");
    pane.kind = ViewerKind::Markdown;
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 5, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 5, 2);
    assert!(pane.text_drag.is_some());
    assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)));
    assert!(pane.text_drag.is_none());
}

#[test]
fn content_rebuild_clears_sticky_and_visual() {
    let mut entry = ScrollbackEntry::running(RenderBlock::agent_message_streaming());
    entry
        .block
        .as_agent_message_mut()
        .expect("agent")
        .push_chunk("hello world");
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.prepare_for_test(area());
    pane.text_drag = Some(TextDrag {
        anchor: TextEndpoint {
            item_idx: 0,
            col: 0,
        },
        head: TextEndpoint {
            item_idx: 0,
            col: 4,
        },
        active: false,
    });
    pane.drag_copy_text = Some("hello".into());
    assert_eq!(pane.selected_plain_text(), "hello");

    entry
        .block
        .as_agent_message_mut()
        .expect("agent")
        .push_chunk("\n\nmore");
    assert!(pane.tick(&entry));
    pane.prepare_for_test(area());
    assert!(pane.text_drag.is_none());
    assert!(pane.drag_copy_text.is_none());
    assert_eq!(pane.selected_plain_text(), "more");

    pane.list_state.enter_visual_mode(&pane.cached_unified);
    assert!(pane.list_state.visual_mode);
    entry
        .block
        .as_agent_message_mut()
        .expect("agent")
        .push_chunk("\n\nextra");
    assert!(pane.tick(&entry));
    assert!(!pane.list_state.visual_mode);
}

#[test]
fn selected_plain_text_fresh_open_skips_preamble() {
    let entry = ScrollbackEntry::new(RenderBlock::agent_message("hello"));
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.install_prepend_lines(&[Line::from("header")]);
    pane.prepare_for_test(area());
    assert_eq!(pane.selected_plain_text(), "hello");
}

#[test]
fn mouse_click_moves_cursor_for_quote() {
    let mut pane = pane("first\nsecond");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 3);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 3);
    assert!(pane.text_drag.is_none());
    assert_eq!(pane.selected_plain_text(), "second");
}

#[test]
fn mouse_drag_filter_skips_hidden_lines() {
    let mut pane = pane("alpha\nzzz\nalso");
    pane.list_state
        .set_filter(Some(crate::views::list_pane::FilterMatcher::substring("a")));
    pane.prepare_for_test(area());
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
    pane.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 10, 1);
    assert_eq!(pane.selected_plain_text(), "alpha\nalso");
}

#[test]
fn double_click_one_character_word_quotes_the_word() {
    let mut pane = pane("I am");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 2);
    assert_eq!(pane.selected_plain_text(), "I");
    assert!(pane.has_sticky_selection());
}

#[test]
fn click_empty_space_clears_sticky_highlight() {
    let mut pane = pane("hello world");
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2);
    pane.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 5, 2);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 5, 2);
    assert!(pane.has_sticky_selection());
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0, 20);
    assert!(
        pane.text_drag.is_none(),
        "click in empty space must drop the sticky highlight"
    );
    assert_eq!(pane.selected_plain_text(), "hello world");
}

#[test]
fn mouse_click_filter_selects_full_message_line() {
    let mut pane = pane("alpha\nbeta\ngamma");
    pane.list_state
        .set_filter(Some(crate::views::list_pane::FilterMatcher::substring(
            "gamma",
        )));
    pane.prepare_for_test(area());
    pane.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
    pane.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 5, 0);
    pane.handle_mouse(MouseEventKind::Up(MouseButton::Left), 5, 0);
    assert_eq!(pane.selected_plain_text(), "gamma");
}

#[test]
fn finish_selects_last_nonempty_line() {
    let mut entry = ScrollbackEntry::running(RenderBlock::agent_message_streaming());
    entry
        .block
        .as_agent_message_mut()
        .expect("agent")
        .push_chunk("hello\n\n");
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.prepare_for_test(area());
    assert!(pane.list_state.follow_mode);
    entry.is_running = false;
    assert!(pane.tick(&entry));
    pane.prepare_for_test(area());
    assert!(!pane.list_state.follow_mode);
    assert_eq!(pane.selected_plain_text(), "hello");
}

#[test]
fn pin_to_tail_skips_trailing_blanks() {
    let mut pane = running_markdown_pane("hello");
    let Some(hello_id) = pane.items.first().map(|item| item.id) else {
        panic!("expected a body line");
    };
    pane.items.push(ContentLine {
        content: Line::default(),
        plain_text: String::new(),
        id: hello_id + 1,
        bg: None,
    });
    pane.pin_to_tail();
    pane.prepare_for_test(area());
    assert_eq!(pane.list_state.selected_id(), Some(hello_id));
    assert_eq!(pane.selected_plain_text(), "hello");
    let vi = pane.list_state.selected_index().expect("selected");
    assert!(
        pane.list_state.visible_range().contains(&vi),
        "last nonempty line must stay on screen after pin"
    );
}

#[test]
fn ensure_body_cursor_drops_stale_preamble_id() {
    let entry = ScrollbackEntry::new(RenderBlock::agent_message("hello"));
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.list_state.select_by_id(u64::MAX - 5);
    pane.install_prepend_lines(&[Line::from("header")]);
    pane.prepare_for_test(area());
    let first_id = pane
        .items
        .first()
        .map(|item| item.id)
        .expect("expected a body line");
    assert_eq!(pane.list_state.selected_id(), Some(first_id));
    assert_eq!(pane.selected_plain_text(), "hello");
}

#[test]
fn selected_plain_text_visual_includes_preamble() {
    let entry = ScrollbackEntry::new(RenderBlock::agent_message("hello"));
    let mut pane = BlockViewerPane::for_markdown(entry.id, &entry).expect("markdown");
    pane.install_prepend_lines(&[Line::from("header")]);
    pane.list_state.select_by_id(u64::MAX);
    pane.prepare_for_test(area());
    assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert!(pane.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    pane.prepare_for_test(area());
    assert_eq!(pane.selected_plain_text(), "header\nhello");
}

#[test]
fn a_failed_tool_search_viewer_shows_the_error() {
    let block = crate::scrollback::blocks::IntegrationSearchToolCallBlock::new("slack")
        .with_error("no such server");
    let entry = ScrollbackEntry::new(RenderBlock::ToolCall(ToolCallBlock::IntegrationSearch(
        block,
    )));
    let pane = BlockViewerPane::for_integration_search(entry.id, &entry).expect("search viewer");
    let text: Vec<&str> = pane
        .items
        .iter()
        .map(|item| item.plain_text.as_str())
        .collect();
    assert_eq!(text, ["0 results", "", "no such server"]);
}
