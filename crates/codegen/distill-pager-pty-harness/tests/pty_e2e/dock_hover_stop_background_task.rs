// Modified for Distill by Samuel Fajreldines, 2026.
#[allow(unused_imports)]
use super::common::*;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_smoke Cargo test with --ignored"]
async fn dock_hover_reveals_and_dispatches_stop() {
    let content = ContentController::start().await.expect("start content");
    content
        .server()
        .set_settings(json!({ "allow_access": true, "dock_enabled": true }));
    std::fs::write(
        content.sandbox().distill_home().join("requirements.toml"),
        "[features]\ndock = true\n",
    )
    .expect("pin dock in test requirements");

    let flag = content.home().join("dock_hover_stop_flag");
    let args = json!({
        "command": format!(
            "while [ ! -e {} ]; do /bin/sleep 0.2; done",
            flag.display()
        ),
        "description": "dock hover stop task",
        "is_background": true
    })
    .to_string();
    let _background = expect_tool_turn(
        &content,
        "call_dock_hover_stop",
        "run_terminal_command",
        args,
    );
    let _settled = content.expect_agent_turn("turn settled", "DOCK_HOVER_SETTLED");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        &[("GROK_DOCK", "1")],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text("DOCK_HOVER_SETTLED", Duration::from_secs(45))
        .expect("turn settled");
    harness
        .wait_for_text("dock hover stop task", Duration::from_secs(20))
        .expect("background task row");

    let (row, col) = locate_screen_text(&harness.screen_contents(), "dock hover stop task")
        .expect("locate background task row");
    harness
        .inject_keys(sgr_mouse(35, row, col, 'M').as_bytes())
        .expect("move pointer over task row");
    harness
        .wait_for_text("[stop]", Duration::from_secs(10))
        .expect("hover stop action");

    let (stop_row, stop_col) =
        locate_screen_text(&harness.screen_contents(), "[stop]").expect("locate hover stop action");
    let click = format!(
        "{}{}",
        sgr_mouse(0, stop_row, stop_col + 1, 'M'),
        sgr_mouse(0, stop_row, stop_col + 1, 'm')
    );
    harness.inject_keys(click.as_bytes()).expect("click stop");
    wait_for_labels_absent(
        &mut harness,
        &["dock hover stop task"],
        Duration::from_secs(20),
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
