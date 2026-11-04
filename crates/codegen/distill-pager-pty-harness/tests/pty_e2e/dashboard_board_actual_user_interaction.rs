// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

fn click_label(h: &mut PtyHarness, label: &str) {
    let (row, col) = locate_screen_text(&h.screen_contents(), label)
        .unwrap_or_else(|| panic!("missing {label:?}\n{}", h.screen_contents()));
    h.inject_keys(
        format!(
            "{}{}",
            sgr_mouse(0, row, col, 'M'),
            sgr_mouse(0, row, col, 'm')
        )
        .as_bytes(),
    )
    .expect("click label");
    h.update(Duration::from_millis(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn dashboard_board_actual_user_interaction() {
    let content = ContentController::start().await.expect("start content");
    let project = tempfile::tempdir().expect("project directory");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} BOARDACTIVITY"));
    let binary = pager_binary().expect("resolve pager binary");
    let mut h = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &[("GROK_WORKSPACE_DASHBOARD", "1")],
        Some(project.path()),
    )
    .expect("spawn pager");
    h.wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    h.inject_keys(b"BOARDONE\r").expect("start session");
    h.wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("mock response");
    h.wait_for_turn_idle(Duration::from_secs(15))
        .expect("idle turn");
    h.inject_keys(b"/rename BOARDONE\r").expect("name session");
    h.wait_for_turn_idle(Duration::from_secs(15))
        .expect("rename completed");
    h.inject_keys(CTRL_BACKSLASH).expect("open dashboard");
    h.wait_for_text("+ New Agent", Duration::from_secs(10))
        .expect("dashboard");
    h.wait_for_text("Board view", Duration::from_secs(10))
        .expect("board toggle");
    click_label(&mut h, "Board view");
    h.wait_for_text("Row view", Duration::from_secs(10))
        .expect("board active");
    h.inject_keys(keys::ENTER).expect("keyboard toggle to rows");
    h.wait_for_text("Board view", Duration::from_secs(10))
        .expect("keyboard row presentation");
    h.inject_keys(keys::ENTER)
        .expect("keyboard toggle to board");
    h.wait_for_text("Row view", Duration::from_secs(10))
        .expect("keyboard board presentation");
    write_screen_dump_if_requested(&h, "board_wide");

    click_label(&mut h, "BOARDONE");
    let expanded = h.screen_contents();
    h.inject_keys(b" ").expect("collapse card");
    h.update(Duration::from_millis(200));
    let collapsed = h.screen_contents();
    h.inject_keys(b" ").expect("expand card");
    h.update(Duration::from_millis(200));
    let reexpanded = h.screen_contents();
    let cwd = project.path().file_name().unwrap().to_string_lossy();
    assert!(
        expanded.matches(cwd.as_ref()).count() > collapsed.matches(cwd.as_ref()).count(),
        "expanded card must expose its directory\nexpanded:\n{expanded}\ncollapsed:\n{collapsed}"
    );
    assert!(reexpanded.matches(cwd.as_ref()).count() > collapsed.matches(cwd.as_ref()).count());
    write_screen_dump_if_requested(&h, "board_card_expanded");
    h.inject_keys(keys::ENTER).expect("attach selected session");
    wait_for_labels_absent(&mut h, &["+ New Agent"], Duration::from_secs(10));
    h.wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(10))
        .expect("attached transcript");
    h.inject_keys(CTRL_BACKSLASH).expect("return to board");
    h.wait_for_text("Row view", Duration::from_secs(10))
        .expect("board presentation retained");

    h.resize(24, 60).expect("narrow terminal");
    h.wait_until(
        "row toggle remains reachable",
        Duration::from_secs(10),
        |h| h.contains_text("Row view") || h.contains_text("Rows"),
    )
    .expect("narrow row toggle");
    write_screen_dump_if_requested(&h, "board_narrow");
    let label = if h.contains_text("Row view") {
        "Row view"
    } else {
        "Rows"
    };
    click_label(&mut h, label);
    h.wait_for_text("Board", Duration::from_secs(10))
        .expect("row presentation restored");
    h.resize(DEFAULT_ROWS, DEFAULT_COLS).expect("wide terminal");
    h.wait_for_text("Board view", Duration::from_secs(10))
        .expect("wide board control");
    click_label(&mut h, "Board view");
    h.wait_for_text("Row view", Duration::from_secs(10))
        .expect("board restored");
    h.inject_keys(b"\x1b[47;5u").expect("Ctrl+/ search");
    h.wait_for_text("Search:", Duration::from_secs(10))
        .expect("search mode");
    h.inject_keys(b"ZZNOMATCHBOARD").expect("filter");
    h.wait_for_text("No rows match", Duration::from_secs(10))
        .expect("empty filtered board");
    write_screen_dump_if_requested(&h, "board_filter_empty");
    h.inject_keys(keys::ESC).expect("clear search");
    h.wait_for_text("BOARDONE", Duration::from_secs(10))
        .expect("agent restored");
    assert!(!h.contains_text("panicked"), "{}", h.screen_contents());
    #[cfg(unix)]
    write_cast_if_requested(&h, "dashboard_board_actual_user_interaction.cast");
    h.quit().expect("clean quit");
}
