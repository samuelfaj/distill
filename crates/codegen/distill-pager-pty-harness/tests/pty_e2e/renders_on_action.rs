// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A resize after the splash forces the pager to emit at least one synchronized-update frame (`CSI ? 2026 h/l`).
/// The pager draws only in response to events and never emits idle frames, so the test drives a resize rather than asserting `frame_count > 0` from boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn renders_on_action() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness.reset_timing();

    harness.resize(40, 100).expect("resize");
    harness.update(Duration::from_millis(500));

    let frames = harness.frame_count();
    assert!(
        frames > 0,
        "pager should emit >=1 synchronized-update frame after a resize, got {frames}"
    );

    harness.quit().expect("clean quit");
}
