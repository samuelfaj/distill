// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Idle-input regression: after a turn completes and the session goes idle, a keystroke must echo
/// promptly. The pager used to rely on an always-on `tracing_rx` animation tick to wake the parked
/// event loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn input_echoes_at_idle_prompt() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} short idle reply."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("response on screen");

    // Let the turn finish and post-turn animation settle so the loop goes fully idle (needs_animation() == false) and parks
    // 3s reliably reaches true idle here; the echo check below flipped from failing to passing at this settle time
    harness.update(Duration::from_secs(3));

    // Type a distinctive marker at the idle prompt (no Enter): it must echo.
    const TYPED: &str = "ZZIDLEKEYSTROKEZZ";
    harness.inject_keys(TYPED.as_bytes()).expect("type at idle");

    if harness
        .wait_for_text(TYPED, Duration::from_secs(2))
        .is_err()
    {
        panic!(
            "typed text did not echo at an idle prompt within 2s: the parked event loop \
             was not woken by input (idle wake regression).\nscreen:\n{}",
            harness.screen_contents()
        );
    }

    harness.quit().expect("clean quit");
}
