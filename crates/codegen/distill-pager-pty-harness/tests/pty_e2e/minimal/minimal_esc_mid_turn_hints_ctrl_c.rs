// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// The prompt is always focused in minimal mode, so a bare Esc reaches the Esc policy's turn-running branch.
/// That branch never cancels: minimal has no toast slot, so the Ctrl+C hint is committed to native scrollback as a system line, and the turn keeps streaming.
/// Ctrl+C then cancels, and the cancellation marker is committed like any other block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_esc_mid_turn_hints_ctrl_c() {
    let content = ContentController::start().await.expect("start content");
    // Paced, long stream so the turn is provably still running when Esc lands.
    let long = format!(
        "{MOCK_RESPONSE_SENTINEL} {}",
        "streaming filler words for the cancellation window. ".repeat(120)
    );
    content.set_response(long);
    content.set_chunk_delay(Some(Duration::from_millis(50)));

    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn streaming in the live tail");

    harness.inject_keys(keys::ESC).expect("press esc");

    // The hint may be committed above the pinned viewport, so check the full text, not just the screen
    harness
        .wait_for_full_text("Press Ctrl+c to cancel the turn", Duration::from_secs(10))
        .expect("mid-turn Esc must commit the Ctrl+C hint");
    harness.update(Duration::from_millis(600));
    assert!(
        !harness.full_text().contains("Turn cancelled by user"),
        "Esc must not cancel the turn\nfull text:\n{}",
        harness.full_text()
    );

    harness
        .inject_keys(keys::CTRL_C)
        .expect("press ctrl+c to cancel");
    harness
        .wait_for_full_text("Turn cancelled by user", Duration::from_secs(15))
        .expect("cancellation marker committed to scrollback");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
