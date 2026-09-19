// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

const TOO_SMALL_HINT_SENTINEL: &str = "needs a bigger terminal";

/// Minimal: a live band too short for the feedback form paints the one-row Esc hint instead of an
/// invisible key owner, and Esc closes it back to the idle prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn minimal_feedback_small_terminal_shows_hint() {
    let content = ContentController::start().await.expect("start content");

    // Cold-start at a comfortable height; the band is squeezed by a resize once a session is bound.
    let mut harness = spawn_minimal_env_ops(
        &content,
        12,
        100,
        &[],
        &[EnvOp::set("GROK_FEEDBACK_ENABLED", "true")],
        Some(content.home()),
    );
    wait_minimal_ready(&mut harness);
    bind_minimal_session(&mut harness, &content);

    // Five rows leaves a four-row band, under the form's six-row floor.
    harness.resize(5, 100).expect("shrink the terminal");
    harness
        .wait_for_text(MINIMAL_IDLE_SENTINEL, Duration::from_secs(15))
        .expect("the idle prompt survives the shrink");

    inject_keys_paced(&mut harness, b"/feedback");
    harness.inject_keys(b"\r").expect("submit bare /feedback");
    harness
        .wait_for_text(TOO_SMALL_HINT_SENTINEL, Duration::from_secs(15))
        .expect("a too-small band paints the Esc hint");
    assert!(
        !harness.contains_text(FEEDBACK_MODAL_PLACEHOLDER_SENTINEL),
        "the form itself must not render on a four-row band\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.inject_keys(keys::ESC).expect("Esc closes the form");
    harness
        .wait_until("hint dismissed", Duration::from_secs(15), |h| {
            !h.contains_text(TOO_SMALL_HINT_SENTINEL) && h.contains_text(MINIMAL_IDLE_SENTINEL)
        })
        .expect("Esc must close the form and restore the idle prompt");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
