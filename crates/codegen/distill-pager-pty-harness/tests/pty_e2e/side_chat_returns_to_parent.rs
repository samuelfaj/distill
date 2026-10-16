// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// `/side` opens a visible side chat and Ctrl+C returns to its parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_smoke Cargo test with --ignored"]
async fn side_chat_returns_to_parent() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} parent reply."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager with content");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness.inject_keys(b"go\r").expect("submit parent prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("parent response");
    harness
        .wait_for_turn_idle(Duration::from_secs(20))
        .expect("parent turn idle");

    harness.inject_keys(b"/side\r").expect("start side chat");
    harness
        .wait_for_text("side - parent idle", Duration::from_secs(20))
        .unwrap_or_else(|e| {
            panic!(
                "side chat did not open: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    harness
        .wait_for_text("Ctrl+C to return", Duration::from_secs(10))
        .expect("idle side return guide");

    let border_colors = |harness: &PtyHarness| {
        harness
            .screen_styled()
            .into_iter()
            .flat_map(|line| line.runs)
            .filter(|run| run.text.chars().any(|ch| "─│┌┐└┘╭╮╰╯".contains(ch)))
            .map(|run| run.fg.clone())
            .collect::<Vec<_>>()
    };
    let first_border_colors = border_colors(&harness);
    let mut border_changed = false;
    for _ in 0..20 {
        harness.update(Duration::from_millis(100));
        assert!(harness.contains_text("side - parent idle"));
        assert!(harness.contains_text("Ctrl+C to return"));
        if border_colors(&harness) != first_border_colors {
            border_changed = true;
            break;
        }
    }
    assert!(
        border_changed,
        "side prompt border color did not animate while idle\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.resize(24, 72).expect("narrow terminal");
    harness
        .wait_for_text("Ctrl+C to return", Duration::from_secs(5))
        .expect("side return guide survives resize");

    let mut side_turn = content.expect_agent_turn_blocked("side-turn", "MOCKSIDE busy reply");
    harness
        .inject_keys(b"side prompt\r")
        .expect("submit side prompt");
    tokio::time::timeout(Duration::from_secs(20), side_turn.wait_received())
        .await
        .expect("side inference request received before timeout");
    harness
        .wait_for_text("MOCKSIDE", Duration::from_secs(10))
        .expect("busy side response");
    harness.inject_keys(b"\x03").expect("cancel busy side turn");
    harness
        .wait_for_text("side - parent idle", Duration::from_secs(10))
        .expect("cancellation leaves side active");
    side_turn.release();
    harness
        .wait_for_text("Ctrl+C to return", Duration::from_secs(10))
        .expect("idle side return guide after cancellation");

    harness.inject_keys(b"\x03").expect("return to parent");
    wait_for_labels_absent(
        &mut harness,
        &[
            "side - parent idle",
            "side - parent running",
            "Ctrl+C to return",
        ],
        Duration::from_secs(10),
    );
    assert!(
        harness.contains_text(MOCK_RESPONSE_SENTINEL),
        "parent transcript must render after return\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
