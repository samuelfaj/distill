// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for the block viewer and transcript dispatchers.

use super::*;

fn make_test_png(width: u32, height: u32) -> Vec<u8> {
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(width, height, Rgba([128, 64, 32, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

fn make_test_jpeg(width: u32, height: u32) -> Vec<u8> {
    use image::{ImageBuffer, Rgb};
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(width, height, Rgb([128, 64, 32]));
    let mut buf = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    buf
}

#[test]
fn open_block_viewer_on_group_header_toggles_group() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let mut appearance = crate::appearance::AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        agent.scrollback.set_appearance(appearance);
        for i in 0..6 {
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::tool_call(
                    format!("Tool{i}"),
                    "info",
                    true,
                ));
        }
        agent.scrollback.prepare_layout(80, 40);
        agent.scrollback.set_selected(Some(0));
        assert!(agent.scrollback.is_selected_group_header());
    }

    // Enter on the "N more" header expands the group instead of opening the hidden first entry in the block viewer
    dispatch(Action::OpenBlockViewer, &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        assert!(
            agent.block_viewer.is_none(),
            "viewer must not open on a group header"
        );
        assert_eq!(
            agent.scrollback.selected(),
            None,
            "expanding a group clears the selection"
        );
        agent.scrollback.prepare_layout(80, 40);
        agent.scrollback.set_selected(Some(0));
        assert_eq!(
            agent.scrollback.selected_group_header_fold_label(),
            Some("collapse"),
            "entry 0 should now be the expanded group's collapse header"
        );
    }

    // Enter on the collapse header collapses the group back.
    dispatch(Action::OpenBlockViewer, &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.block_viewer.is_none());
        agent.scrollback.prepare_layout(80, 40);
        assert_eq!(
            agent.scrollback.selected_group_header_fold_label(),
            Some("expand"),
            "group should be truncated again ('N more' header)"
        );
    }
}

#[test]
fn open_block_viewer_opens_grep_search_block() {
    use crate::scrollback::blocks::{SearchFileMatch, SearchLineMatch};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let agent = app.agents.get_mut(&id).unwrap();
    agent.scrollback.push_block(RenderBlock::search(
        "fn main",
        1,
        vec![SearchFileMatch {
            path: "src/main.rs".into(),
            matches: vec![SearchLineMatch {
                line_number: 1,
                content: "fn main() {}".into(),
            }],
        }],
    ));
    agent.scrollback.set_selected(Some(0));

    let entry = agent.scrollback.entry(0).unwrap();
    assert!(entry.block.has_normal_fullscreen_viewer());

    let effects = dispatch(Action::OpenBlockViewer, &mut app);
    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.block_viewer.is_some());
    assert_eq!(
        agent.block_viewer.as_ref().unwrap().kind,
        crate::views::block_viewer::ViewerKind::Grep
    );
}

#[test]
fn open_block_viewer_opens_list_dir_block() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::list_dir_with_output("/tmp", "a.txt\nb.txt"));
    agent.scrollback.set_selected(Some(0));

    let entry = agent.scrollback.entry(0).unwrap();
    assert!(entry.block.has_normal_fullscreen_viewer());

    let effects = dispatch(Action::OpenBlockViewer, &mut app);
    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.block_viewer.is_some());
    assert_eq!(
        agent.block_viewer.as_ref().unwrap().kind,
        crate::views::block_viewer::ViewerKind::PlainText
    );
}

#[test]
fn open_block_viewer_prefers_markdown_viewer_over_image_refs() {
    use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let dir = tempfile::tempdir().unwrap();
    let image_path = dir.path().join("referenced.png");
    std::fs::write(&image_path, make_test_png(20, 10)).unwrap();

    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::agent_message(format!(
            "Here is an image: ![ref]({})",
            image_path.display()
        )));
    agent.scrollback.set_selected(Some(0));

    // Without a graphics protocol the top-level media guard returns early before the block viewer is reached
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    let effects = dispatch(Action::OpenBlockViewer, &mut app);

    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.block_viewer.is_some());
    assert!(agent.image_viewer.is_none());
}

#[test]
fn open_block_viewer_uses_markdown_viewer_for_agent_message_with_image_ref() {
    use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let dir = tempfile::tempdir().unwrap();
    let jpg_path = dir.path().join("generated.jpg");
    std::fs::write(&jpg_path, make_test_jpeg(20, 10)).unwrap();

    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::agent_message(format!(
            "![generated]({})",
            jpg_path.display()
        )));
    agent.scrollback.set_selected(Some(0));

    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    let effects = dispatch(Action::OpenBlockViewer, &mut app);

    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    // Agent messages with image refs now open the normal markdown viewer (inline media rendering moved to the tool call block)
    assert!(agent.block_viewer.is_some());
}

fn long_agent_lines() -> String {
    (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn push_selected_running_message(
    app: &mut AppView,
    text: String,
) -> crate::scrollback::entry::EntryId {
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    let entry_id = agent
        .scrollback
        .push_block(RenderBlock::agent_message(text));
    agent
        .scrollback
        .get_by_id_mut(entry_id)
        .expect("just pushed")
        .is_running = true;
    agent.scrollback.set_selected(Some(0));
    entry_id
}

fn follow_viewer_area() -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(0, 0, 80, 8)
}

#[test]
fn open_block_viewer_restores_place_for_same_entry() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let entry_id = {
        let agent = app.agents.get_mut(&id).unwrap();
        let entry_id = agent
            .scrollback
            .push_block(RenderBlock::agent_message("line a\nline b\nline c"));
        agent.scrollback.set_selected(Some(0));
        entry_id
    };
    dispatch(Action::OpenBlockViewer, &mut app);
    let (selected_id, scroll_offset) = {
        let agent = app.agents.get_mut(&id).unwrap();
        let viewer = agent.block_viewer.as_mut().expect("opened");
        viewer.prepare_for_test(ratatui::layout::Rect::new(0, 0, 80, 24));
        viewer.list_state.set_scroll_offset(12);
        let selected_id = viewer.list_state.selected_id();
        let scroll_offset = viewer.list_state.scroll_offset();
        agent.dismiss_block_viewer();
        (selected_id, scroll_offset)
    };
    dispatch(Action::OpenBlockViewer, &mut app);
    let agent = app.agents.get(&id).unwrap();
    let viewer = agent.block_viewer.as_ref().expect("reopened");
    assert_eq!(viewer.entry_id, entry_id);
    assert_eq!(viewer.list_state.selected_id(), selected_id);
    assert_eq!(viewer.list_state.scroll_offset(), scroll_offset);
}

#[test]
fn open_block_viewer_restores_place_after_follow_dismiss() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let entry_id = push_selected_running_message(&mut app, long_agent_lines());
    dispatch(Action::OpenBlockViewer, &mut app);
    let (selected_id, scroll_offset) = {
        let agent = app.agents.get_mut(&id).unwrap();
        let viewer = agent.block_viewer.as_mut().expect("opened");
        viewer.prepare_for_test(follow_viewer_area());
        assert!(viewer.list_state.follow_mode);
        assert_eq!(viewer.list_state.selected_id(), None);
        let scroll_offset = viewer.list_state.scroll_offset();
        assert!(scroll_offset > 0);
        agent.dismiss_block_viewer();
        let resume = agent.block_viewer_resume.expect("resume");
        (resume.selected_id, scroll_offset)
    };
    assert!(selected_id.is_some());
    app.agents
        .get_mut(&id)
        .unwrap()
        .scrollback
        .get_by_id_mut(entry_id)
        .expect("entry")
        .is_running = false;
    dispatch(Action::OpenBlockViewer, &mut app);
    let agent = app.agents.get_mut(&id).unwrap();
    let viewer = agent.block_viewer.as_mut().expect("reopened");
    viewer.prepare_for_test(follow_viewer_area());
    assert!(!viewer.list_state.follow_mode);
    assert_eq!(viewer.list_state.selected_id(), selected_id);
    let vi = viewer.list_state.selected_index().expect("selected");
    assert!(
        viewer.list_state.visible_range().contains(&vi),
        "pinned last nonempty line must stay on screen (was offset {scroll_offset})"
    );
}

#[test]
fn open_block_viewer_restores_scrolled_place_while_still_running() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    push_selected_running_message(&mut app, long_agent_lines());
    dispatch(Action::OpenBlockViewer, &mut app);
    let (selected_id, scroll_offset) = {
        let agent = app.agents.get_mut(&id).unwrap();
        let viewer = agent.block_viewer.as_mut().expect("opened");
        viewer.prepare_for_test(follow_viewer_area());
        assert!(viewer.list_state.follow_mode);
        viewer.list_state.follow_mode = false;
        viewer.prepare_for_test(follow_viewer_area());
        viewer.list_state.set_scroll_offset(3);
        viewer.prepare_for_test(follow_viewer_area());
        let selected_id = viewer.list_state.selected_id();
        let scroll_offset = viewer.list_state.scroll_offset();
        assert!(!viewer.list_state.follow_mode);
        agent.dismiss_block_viewer();
        (selected_id, scroll_offset)
    };
    dispatch(Action::OpenBlockViewer, &mut app);
    let agent = app.agents.get_mut(&id).unwrap();
    let viewer = agent.block_viewer.as_mut().expect("reopened");
    viewer.prepare_for_test(follow_viewer_area());
    assert!(!viewer.list_state.follow_mode);
    assert_eq!(viewer.list_state.selected_id(), selected_id);
    let vi = viewer.list_state.selected_index().expect("selected");
    assert!(
        viewer.list_state.visible_range().contains(&vi),
        "restored cursor must be on screen (saved offset {scroll_offset})"
    );
}

#[test]
fn open_block_viewer_keeps_follow_when_dismissed_while_following() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    push_selected_running_message(&mut app, long_agent_lines());
    dispatch(Action::OpenBlockViewer, &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let viewer = agent.block_viewer.as_mut().expect("opened");
        viewer.prepare_for_test(follow_viewer_area());
        assert!(viewer.list_state.follow_mode);
        agent.dismiss_block_viewer();
        assert!(agent.block_viewer_resume.expect("resume").follow_mode);
    }
    dispatch(Action::OpenBlockViewer, &mut app);
    let agent = app.agents.get_mut(&id).unwrap();
    let viewer = agent.block_viewer.as_mut().expect("reopened");
    viewer.prepare_for_test(follow_viewer_area());
    assert!(viewer.list_state.follow_mode);
    assert_eq!(viewer.list_state.selected_id(), None);
}

#[test]
fn open_block_viewer_pins_tail_when_follow_ids_remap() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let finished = (0..8)
        .map(|i| format!("done {i}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n";
    let entry_id = push_selected_running_message(&mut app, long_agent_lines());
    dispatch(Action::OpenBlockViewer, &mut app);
    let stale_id = {
        let agent = app.agents.get_mut(&id).unwrap();
        let viewer = agent.block_viewer.as_mut().expect("opened");
        viewer.prepare_for_test(follow_viewer_area());
        assert!(viewer.list_state.follow_mode);
        agent.dismiss_block_viewer();
        let resume = agent.block_viewer_resume.expect("resume");
        assert!(resume.follow_mode);
        resume
            .selected_id
            .expect("follow dismiss snapshots last line")
    };
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let entry = agent.scrollback.get_by_id_mut(entry_id).expect("entry");
        entry.block = RenderBlock::agent_message(finished);
        entry.is_running = false;
    }
    let expected_last = {
        let agent = app.agents.get(&id).unwrap();
        let entry = agent.scrollback.get_by_id(entry_id).expect("entry");
        crate::views::block_viewer::BlockViewerPane::for_markdown(entry_id, entry)
            .expect("markdown")
            .resume_selected_id()
            .expect("finished body")
    };
    assert_ne!(stale_id, expected_last);
    dispatch(Action::OpenBlockViewer, &mut app);
    let agent = app.agents.get_mut(&id).unwrap();
    let viewer = agent.block_viewer.as_mut().expect("reopened");
    viewer.prepare_for_test(follow_viewer_area());
    assert!(!viewer.list_state.follow_mode);
    assert_eq!(viewer.list_state.selected_id(), Some(expected_last));
}

#[test]
fn open_block_viewer_opens_image_only_blocks_natively() {
    use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let dir = tempfile::tempdir().unwrap();
    let image_path = dir.path().join("referenced.png");
    std::fs::write(&image_path, make_test_png(20, 10)).unwrap();

    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::ToolCall(ToolCallBlock::Other(
            crate::scrollback::blocks::OtherToolCallBlock::new("image_tool", "saved image")
                .with_output(format!("Saved image: {}", image_path.display())),
        )));
    agent.scrollback.set_selected(Some(0));

    let entry = agent.scrollback.entry(0).unwrap();
    assert!(entry.block.supports_fullscreen());
    assert!(!entry.block.has_normal_fullscreen_viewer());

    // Pretend the host terminal speaks Kitty graphics so `guard_image_support` doesn't return early
    // The dispatch then reaches the image branch, which opens the file natively rather than in an in-app viewer
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    let effects = dispatch(Action::OpenBlockViewer, &mut app);

    // Generated media now opens in the OS-native viewer without being tracked, so neither the in-app block viewer nor the image viewer is shown
    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.block_viewer.is_none());
    assert!(agent.image_viewer.is_none());
}

fn plugins_list_response() -> distill_hooks_plugins_types::PluginsListResponse {
    use crate::views::extensions_modal::test_plugin_info;
    distill_hooks_plugins_types::PluginsListResponse {
        plugins: vec![
            test_plugin_info(
                "user-tool",
                Some(distill_hooks_plugins_types::PluginOrigin::UserGrok),
            ),
            test_plugin_info(
                "claude-tool",
                Some(distill_hooks_plugins_types::PluginOrigin::UserClaude),
            ),
        ],
    }
}

fn open_plugins_modal(app: &mut AppView, id: AgentId) {
    app.agents.get_mut(&id).unwrap().extensions_modal =
        Some(crate::views::extensions_modal::ExtensionsModalState::new(
            crate::views::extensions_modal::ExtensionsTab::Plugins,
        ));
}

fn deliver_plugins_list(app: &mut AppView, id: AgentId) {
    dispatch(
        Action::TaskComplete(TaskResult::PluginsListLoaded {
            agent_id: id,
            result: Ok(plugins_list_response()),
        }),
        app,
    );
}

fn plugins_collapsed_keys(app: &AppView, id: AgentId) -> Vec<String> {
    let Some(modal) = app
        .agents
        .get(&id)
        .and_then(|a| a.extensions_modal.as_ref())
    else {
        panic!("expected extensions modal on {id:?}");
    };
    let mut keys: Vec<String> = modal.plugins_collapsed_groups.iter().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn plugins_list_loaded_seeds_all_groups_collapsed_on_first_load() {
    use crate::views::extensions_modal::TabDataState;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    open_plugins_modal(&mut app, id);

    deliver_plugins_list(&mut app, id);

    assert_eq!(
        plugins_collapsed_keys(&app, id),
        vec!["origin:user".to_string(), "origin:user-claude".to_string()]
    );
    let Some(modal) = app
        .agents
        .get(&id)
        .and_then(|a| a.extensions_modal.as_ref())
    else {
        panic!("expected extensions modal on {id:?}");
    };
    match &modal.plugins_data {
        TabDataState::Loaded(response) => assert_eq!(response.plugins.len(), 2),
        other => panic!("expected Loaded plugins data, got {other:?}"),
    }
}

#[test]
fn plugins_list_delivery_seeds_once_then_always_preserves() {
    use crate::views::extensions_modal::TabDataState;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    open_plugins_modal(&mut app, id);
    deliver_plugins_list(&mut app, id);

    // The user expands a group, then the refetch that follows an action arrives
    app.agents
        .get_mut(&id)
        .unwrap()
        .extensions_modal
        .as_mut()
        .unwrap()
        .plugins_collapsed_groups
        .remove("origin:user");
    deliver_plugins_list(&mut app, id);

    assert_eq!(
        plugins_collapsed_keys(&app, id),
        vec!["origin:user-claude".to_string()],
        "post-action refetch must not re-collapse an expanded group"
    );

    // Reload sets Loading, but seeding happens once per modal, so the expanded group is still preserved
    app.agents
        .get_mut(&id)
        .unwrap()
        .extensions_modal
        .as_mut()
        .unwrap()
        .plugins_data = TabDataState::Loading;
    deliver_plugins_list(&mut app, id);

    assert_eq!(
        plugins_collapsed_keys(&app, id),
        vec!["origin:user-claude".to_string()],
        "reload must not re-collapse groups the user expanded"
    );
}

#[test]
fn open_block_viewer_skips_image_viewer_when_no_graphics() {
    use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let dir = tempfile::tempdir().unwrap();
    let image_path = dir.path().join("referenced.png");
    std::fs::write(&image_path, make_test_png(20, 10)).unwrap();

    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::ToolCall(ToolCallBlock::Other(
            crate::scrollback::blocks::OtherToolCallBlock::new("image_tool", "saved image")
                .with_output(format!("Saved image: {}", image_path.display())),
        )));
    agent.scrollback.set_selected(Some(0));

    // The terminal has no inline-image protocol (e.g. Windows ConPTY).
    // The dispatch refuses to open the image-viewer modal and shows a toast instead
    let _guard = set_protocol_for_test(GraphicsProtocol::None);
    let effects = dispatch(Action::OpenBlockViewer, &mut app);

    assert!(effects.is_empty());
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.block_viewer.is_none());
    assert!(
        agent.image_viewer.is_none(),
        "image_viewer modal should not open on terminals without a graphics protocol"
    );
}
