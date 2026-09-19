// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Minimal mode shows the Shift+Tab session mode in the one-line info bar directly under the
/// prompt. `crate::minimal::live::render_prompt_info` must then draw a lowercase `plan` flag below
/// the prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_shift_tab_shows_mode_in_info_bar() {
    let content = ContentController::start().await.expect("start content");
    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    // Baseline: nothing on the idle screen says "plan". (The shell's transient "Switched to mode:
    // Plan" banner uses a capital P, which we don't match.).
    assert!(
        !harness.contains_text("plan"),
        "precondition: idle minimal screen must not already show 'plan'\nscreen:\n{}",
        harness.screen_contents()
    );

    // Shift+Tab arrives as BackTab (CSI Z); the first press cycles Normal to Plan
    harness.inject_keys(b"\x1b[Z").expect("inject BackTab");
    harness
        .wait_for_text("plan", Duration::from_secs(10))
        .expect("plan flag in the info bar under the prompt after Shift+Tab");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
