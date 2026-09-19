// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A terminal that pushed kitty flags but never answers DA1 must not hold the quit. The marker deadline is what makes this
/// load-bearing: an unbounded fence would still reach exit 0 through the 20 s watchdog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(unix)]
async fn quit_fence_bounded_when_terminal_silent() {
    let content = ContentController::start().await.expect("start content");
    let mut harness = spawn_kitty_pager_with_leftover_capture(&content);
    let pre = quit_with_double_ctrl_c(&mut harness);

    let query_at = wait_for_raw_bytes_after(&mut harness, pre, DA1_QUERY, Duration::from_secs(10))
        .expect("pager did not send the DA1 fence query on quit");

    // Nothing is injected: 1 s fence timeout, then 2 s of quiet for `cat`
    let ended = wait_for_raw_bytes_after(
        &mut harness,
        query_at,
        LEFTOVER_END_OK,
        Duration::from_secs(8),
    );
    assert!(
        ended.is_some(),
        "silent terminal held the quit past the fence timeout: {:?}",
        String::from_utf8_lossy(harness.raw_output().get(query_at..).unwrap_or_default())
    );

    let exit = harness
        .wait_for_exit_and_drain(Duration::from_secs(8), Duration::from_secs(2))
        .expect("wait for the wrapping shell to exit");
    assert_eq!(0, exit);
    assert!(
        raw_position_after(harness.raw_output(), pre, b"\x1b[?25h").is_some(),
        "terminal not restored (no show-cursor after quit)"
    );
}
