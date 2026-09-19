// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Shift+Tab (BackTab, `ESC [ Z`) on the welcome screen must leave home for the optimistic
/// session and cycle its mode. The "Switched to mode: Plan" banner proves both halves.
/// With the auto gate on (the client default) the cycle runs Normal, Plan, Auto, and on around.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn shift_tab_on_welcome_starts_session_in_plan_mode() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Shift+Tab arrives as BackTab (CSI Z)
    harness.inject_keys(b"\x1b[Z").expect("inject BackTab");

    harness
        .wait_for_text("Switched to mode: Plan", Duration::from_secs(10))
        .expect("plan mode banner after Shift+Tab on welcome screen");

    // Second press cycles Plan to Auto (gate defaults ON)
    harness.inject_keys(b"\x1b[Z").expect("inject BackTab");
    harness
        .wait_for_text("Switched to mode: Auto", Duration::from_secs(10))
        .expect("auto banner on second Shift+Tab");

    harness.quit().expect("clean quit");
}
