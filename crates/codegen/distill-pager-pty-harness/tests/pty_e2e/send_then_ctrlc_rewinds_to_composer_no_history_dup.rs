// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The UI reads as if the user never hit Send: no stale copy in history, no "Turn cancelled by
/// user" marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn send_then_ctrlc_rewinds_to_composer_no_history_dup() {
    const REWIND_PROMPT: &str = "rewind me home";

    let content = ContentController::start().await.expect("start content");
    content.set_response("REWIND_NEVER_STREAMS.");
    // Delaying every SSE event far beyond the test window makes "before any server activity" deterministic
    // No chunk can arrive and clear the stashed prompt before Ctrl+C lands, so the send stays rewindable
    content.set_chunk_delay(Some(Duration::from_secs(30)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{REWIND_PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // The optimistic "❯ " block committed and the composer cleared
    // The turn is running but no token has arrived yet, so the send can still be rewound
    harness
        .wait_until(
            "prompt block committed and composer cleared",
            Duration::from_secs(30),
            |h| block_lines_containing(h, REWIND_PROMPT) == 1 && !composer_holds(h, REWIND_PROMPT),
        )
        .expect("prompt block committed");
    harness
        .wait_for_text("Waiting for response", Duration::from_secs(25))
        .expect("turn running pre-first-token");

    harness.inject_keys(keys::CTRL_C).expect("Ctrl+C rewind");

    harness
        .wait_until_stable(
            "rewound prompt restored with its scrollback block removed",
            Duration::from_secs(25),
            Duration::from_millis(500),
            |h| composer_holds(h, REWIND_PROMPT) && block_lines_containing(h, REWIND_PROMPT) == 0,
        )
        .expect("rewound prompt restored");
    // The rewind is silent; it looks like the prompt was never sent
    assert!(
        !harness.contains_text("Turn cancelled by user"),
        "rewind must not render a cancelled marker\nscreen:\n{}",
        harness.screen_contents()
    );

    // On a slow runner the transport can retry the aborted turn when its stream stalls, putting the prompt on the wire in two separate requests
    // Each of those requests carries one copy; that is the transport recovering, not the duplicate this test guards against
    // The real guard is that no single request body carries the prompt twice, which is what a stale rewound copy paired with another send would do
    for body in content.request_bodies() {
        let items = body["messages"]
            .as_array()
            .or_else(|| body["input"].as_array());
        let copies = items
            .into_iter()
            .flatten()
            .filter(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains(REWIND_PROMPT))
            })
            .count();
        assert!(
            copies <= 1,
            "rewound prompt must not appear twice in one request (got {copies}): {body}"
        );
    }

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
