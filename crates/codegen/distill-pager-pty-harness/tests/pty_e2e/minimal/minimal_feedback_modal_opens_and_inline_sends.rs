// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;
use distill_pager_pty_harness::inference_request_count;

const DRAFTS_TAB_SENTINEL: &str = "Drafts";
const THANKS_SENTINEL: &str = "Thanks for the feedback";
const INLINE_FEEDBACK: &str = "minimal-pty-feedback-report-xyz";

/// Minimal: bare `/feedback` opens the feedback form in the live band with a live caret and Esc closes
/// it back to the idle prompt; inline `/feedback <text>` POSTs once on Enter with a transcript thank-you
/// and no model turn. The thank-you is pushed before the verdict, so the POST itself is what proves the send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn minimal_feedback_modal_opens_and_inline_sends() {
    let content = ContentController::start().await.expect("start content");
    let overrides = enable_feedback_posting(&content, "feedback-minimal-pty");
    let mut harness =
        spawn_minimal_env_ops(&content, DEFAULT_ROWS, DEFAULT_COLS, &[], &overrides, None);
    wait_minimal_ready(&mut harness);
    bind_minimal_session(&mut harness, &content);

    // An empty drafts list lands the bare open on the Write tab.
    open_feedback_modal(&mut harness);
    let screen = harness.screen_contents();
    let tab_row = screen
        .lines()
        .position(|line| line.contains(DRAFTS_TAB_SENTINEL))
        .unwrap_or_else(|| panic!("the form must show its Drafts tab\nscreen:\n{screen}"));

    // The Write composer's caret is the hardware cursor, so it sits inside the form under the tab bar.
    let (cursor_row, _) = harness.cursor_position();
    assert!(
        usize::from(cursor_row) > tab_row,
        "caret row {cursor_row} must be below the tab bar row {tab_row}\nscreen:\n{screen}"
    );
    assert!(
        harness.contains_full_text(MOCK_RESPONSE_SENTINEL),
        "opening the form must not lose the committed turn\nfull:\n{}",
        harness.full_text()
    );

    harness.inject_keys(b"\x1b").expect("Esc closes the form");
    harness
        .wait_until("feedback form closed", Duration::from_secs(15), |h| {
            !h.contains_text(FEEDBACK_MODAL_PLACEHOLDER_SENTINEL)
                && !h.contains_text(DRAFTS_TAB_SENTINEL)
                && h.contains_text(MINIMAL_IDLE_SENTINEL)
        })
        .expect("Esc must close the form and restore the idle prompt");

    let turns_before_inline = inference_request_count(&content);
    inject_keys_paced(
        &mut harness,
        format!("/feedback {INLINE_FEEDBACK}").as_bytes(),
    );
    harness
        .inject_keys(b"\r")
        .expect("submit inline /feedback with session");
    harness
        .wait_for_full_text(THANKS_SENTINEL, Duration::from_secs(15))
        .expect("minimal inline feedback thanks in the transcript at send time");
    let body = wait_for_feedback_post(&mut harness, &content, "write");
    assert!(
        body["feedbackText"]
            .as_str()
            .is_some_and(|text| text.contains(INLINE_FEEDBACK)),
        "POST body: {body}"
    );
    assert_eq!(
        1,
        content.feedback_posts().len(),
        "one POST per inline report"
    );
    assert_eq!(
        turns_before_inline,
        inference_request_count(&content),
        "inline /feedback must not start a model turn"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
