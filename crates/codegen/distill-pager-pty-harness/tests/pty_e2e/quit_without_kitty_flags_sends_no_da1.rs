// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Terminals that never pushed kitty flags must keep today's teardown byte stream: no DA1 query on quit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn quit_without_kitty_flags_sends_no_da1() {
    let content = ContentController::start().await.expect("start content");
    let binary = pager_binary().expect("resolve pager binary");
    // No brand override: the sandbox scrubs the runner's terminal markers, leaving the Unknown brand
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    assert!(
        raw_position_after(harness.raw_output(), 0, KITTY_PUSH_FLAGS).is_none(),
        "an unclassified terminal must not be pushed kitty keyboard flags"
    );

    let pre = quit_with_double_ctrl_c(&mut harness);
    // Drained past exit so the post-`pre` suffix holds the whole teardown before the negative scan
    let exit = harness
        .wait_for_exit_and_drain(Duration::from_secs(10), Duration::from_secs(2))
        .expect("wait for double-Ctrl+C exit");
    assert_eq!(0, exit);

    assert!(
        raw_position_after(harness.raw_output(), pre, DA1_QUERY).is_none(),
        "teardown sent a DA1 fence query although no kitty flags were pushed"
    );
}
