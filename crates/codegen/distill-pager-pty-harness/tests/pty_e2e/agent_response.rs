// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Submitting a prompt produces the mock server's response text on screen.
/// This is the full loop: the pager sends to the shell agent, which hits mock inference, streams chunks back, and the pager renders them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn agent_response() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!(
        "{MOCK_RESPONSE_SENTINEL} hello from the mock inference server."
    ));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Type the prompt and submit with Enter.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("mock response on screen");
    assert!(
        content.has_chat_completion(),
        "mock inference server never received a chat completion request\nrequests: {:?}",
        content.requests()
    );

    harness.quit().expect("clean quit");
}
