// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

#[cfg(unix)]
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// A release the terminal encodes before applying the pop lands in the tty after the pager stopped reading and the shell
/// inherits it (fish reads `ESC [ 99 ; 5 : 3 u` as Ctrl+D and exits). The scripted terminal answers DA1 only after the
/// release, the order a real terminal produces, so the fence must have consumed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(unix)]
async fn quit_fence_consumes_kitty_release() {
    let content = ContentController::start().await.expect("start content");
    let mut harness = spawn_kitty_pager_with_leftover_capture(&content);
    let pre = quit_with_double_ctrl_c(&mut harness);

    // The reply proves the pop only if the query was written after it
    let query_at = wait_for_raw_bytes_after(&mut harness, pre, DA1_QUERY, STEP_TIMEOUT)
        .expect("pager did not send the DA1 fence query on quit");
    let pop_at = raw_position_after(harness.raw_output(), pre, KITTY_POP_FLAGS)
        .expect("pager did not pop the kitty keyboard flags on quit");
    assert!(
        pop_at < query_at,
        "kitty pop must precede the DA1 query (pop at {pop_at}, query at {query_at})"
    );

    // Well past the old 10 ms crossterm drain: only the fence can still consume this release
    harness.update(Duration::from_millis(100));
    harness
        .inject_keys(b"\x1b[99;5:3u")
        .expect("inject late Ctrl+C release");
    harness.update(Duration::from_millis(50));
    harness.inject_keys(DA1_REPLY).expect("inject DA1 reply");

    let begin_at = wait_for_raw_bytes_after(&mut harness, query_at, LEFTOVER_BEGIN, STEP_TIMEOUT)
        .expect("wrapping shell never reached the leftover capture");
    // Positive control: `cat` must be reading while the leftovers are sampled
    harness.inject_keys(b"PROBE-OK").expect("inject probe");
    let end_at = wait_for_raw_bytes_after(&mut harness, begin_at, LEFTOVER_END_OK, STEP_TIMEOUT)
        .expect("leftover capture never finished with exit 0");

    let leftovers = harness
        .raw_output()
        .get(begin_at + LEFTOVER_BEGIN.len()..end_at)
        .expect("leftover markers are in order");
    let leftovers_text = String::from_utf8_lossy(leftovers);
    assert!(
        raw_position_after(leftovers, 0, b"PROBE-OK").is_some(),
        "leftover capture missed the probe, so its verdict is void: {leftovers_text:?}"
    );
    assert!(
        raw_position_after(leftovers, 0, b":3u").is_none(),
        "the Ctrl+C release leaked into the shell: {leftovers_text:?}"
    );

    let exit = harness
        .wait_for_exit_and_drain(Duration::from_secs(8), Duration::from_secs(2))
        .expect("wait for the wrapping shell to exit");
    assert_eq!(0, exit);
}
