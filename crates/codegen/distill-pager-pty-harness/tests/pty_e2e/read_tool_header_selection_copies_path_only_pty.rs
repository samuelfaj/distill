// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// PTY: drag-select on a `Read {path}` tool header copies only the path (not the `Read ` label). On
/// macOS the clipboard route only emits OSC 52 when it believes the session is remote (see
/// `resolve_clipboard_route`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn read_tool_header_selection_copies_path_only_pty() {
    let content = ContentController::start().await.expect("start content");

    // File lives under the isolated HOME so the agent sandbox can read it.
    let target = content.home().join(READ_HDR_FILE);
    std::fs::write(&target, "hello from read header selection test\n").expect("write target file");
    let abs_path = dunce::canonicalize(&target).unwrap_or(target.clone());
    let path_display = abs_path.to_string_lossy().into_owned();
    // The header may abbreviate directories the way the fish shell does; the filename is always present
    let path_tail = READ_HDR_FILE;

    let _read_turn = seed_read_file_tool_call(&content, &abs_path);

    let binary = pager_binary().expect("resolve pager binary");
    let overrides: Vec<(String, String)> = vec![(
        "SSH_CONNECTION".into(),
        "scripted-test 1 127.0.0.1 2".into(),
    )];
    let env_refs: Vec<(&str, &str)> = overrides
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    // The invariant under test is the raw `Read {path}` header's selectable span
    // With `group_tool_verbs` on (the default), even a lone read folds into the aggregated "Read 1 file" label and the path row never renders
    seed_ui_config(&content, "group_tool_verbs = false");

    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env_refs,
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");

    // Kick off a turn so the agent consumes the scripted read_file tool call.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // Wait for the Read header (label and filename) to appear
    harness
        .wait_for_text(path_tail, Duration::from_secs(45))
        .unwrap_or_else(|_| {
            panic!(
                "expected Read header with {path_tail:?} on screen; got:\n{}",
                harness.screen_contents()
            )
        });

    // Wait for the follow-up completion sentinel before selecting. The tool block isn't committed into
    // the scrollback selection model yet, so the drag's hit-test misses and no OSC 52 is emitted.
    harness
        .wait_for_text(READ_HDR_SENTINEL, Duration::from_secs(45))
        .unwrap_or_else(|_| {
            panic!(
                "expected follow-up turn to settle ({READ_HDR_SENTINEL:?}); got:\n{}",
                harness.screen_contents()
            )
        });

    // Focus scrollback so mouse selection targets the tool header, not the prompt.
    harness.inject_keys(b"\t").expect("focus scrollback");
    // Gate on the scrollback-focused footer instead of a fixed sleep
    // The footer only appears once scrollback actually owns focus
    // The drag below therefore can't hit a frame where the prompt still has focus under host load
    harness
        .wait_for_text("Space:prompt", Duration::from_secs(10))
        .expect("scrollback focused (Space:prompt hint) after Tab");

    let screen = harness.screen_contents();
    let (row, col) = locate_screen_text(&screen, path_tail).unwrap_or_else(|| {
        panic!("could not locate {path_tail:?} after wait; screen:\n{screen}");
    });
    let end_col = col + path_tail.chars().count() as u16 - 1;

    // Drag across the path/filename only (not the "Read " prefix columns).
    harness
        .inject_keys(mouse_drag_line(row, col, end_col).as_bytes())
        .expect("drag select path");

    let payloads = wait_for_osc52_payloads(&mut harness, Duration::from_secs(10));
    assert!(
        !payloads.is_empty(),
        "expected at least one OSC 52 clipboard write; screen:\n{}",
        harness.screen_contents()
    );
    let joined = payloads.join("\n");
    assert!(
        joined.contains(path_tail) || joined.contains(&path_display),
        "clipboard should contain the path/filename; payloads={payloads:?}"
    );
    // Full-line selection would include the tool verb prefix.
    assert!(
        !joined.contains(&format!("Read {path_tail}"))
            && !joined.contains(&format!("Read {path_display}")),
        "clipboard must not include the 'Read ' label prefix; payloads={payloads:?}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
