// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

const DRAFTS_TAB_SENTINEL: &str = "Drafts";
const THANKS_SENTINEL: &str = "Thanks for the feedback";
const WRITE_REPORT: &str = "minimal-pty-write-tab-report-abc";
const MAIN_DRAFT: &str = "minimal-pty-main-draft-preserved-xyz";

/// Minimal: with no saved drafts the form opens on Write and Enter posts the typed report untyped
/// through the modal (the inline `/feedback <text>` path is covered elsewhere); the palette route
/// opens the form over a composer draft and Esc hands that draft back untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn minimal_feedback_write_send_and_dismiss() {
    let content = ContentController::start().await.expect("start content");
    let overrides = enable_feedback_posting(&content, "minimal-feedback-write-send");
    let mut harness =
        spawn_minimal_env_ops(&content, 40, 100, &[], &overrides, Some(content.home()));
    wait_minimal_ready(&mut harness);
    bind_minimal_session(&mut harness, &content);

    // No drafts, so the bare open lands on Write; Enter sends what was typed.
    open_feedback_modal(&mut harness);
    inject_keys_paced(&mut harness, WRITE_REPORT.as_bytes());
    harness
        .wait_for_text(WRITE_REPORT, Duration::from_secs(15))
        .expect("typed report renders in the modal composer");
    harness.inject_keys(b"\r").expect("send the report");
    harness
        .wait_until("write send committed", Duration::from_secs(30), |h| {
            h.contains_full_text(THANKS_SENTINEL) && !h.contains_text(WRITE_REPORT)
        })
        .expect("a Write send closes the form and thanks");
    let body = wait_for_feedback_post(&mut harness, &content, "write");
    assert!(
        body["feedbackText"]
            .as_str()
            .is_some_and(|text| text.contains(WRITE_REPORT)),
        "POST body: {body}"
    );
    assert_eq!("tui", body["clientType"], "POST body: {body}");
    assert!(
        body["metadata"]["structured_feedback"]
            .get("type")
            .is_none(),
        "a Write send carries no taxonomy: {body}"
    );
    harness
        .wait_for_text(MINIMAL_IDLE_SENTINEL, Duration::from_secs(15))
        .expect("idle prompt after the send");

    // Palette route over a composer draft: Esc restores the draft (minimal draws no composer borders).
    inject_keys_paced(&mut harness, MAIN_DRAFT.as_bytes());
    harness
        .wait_for_text(MAIN_DRAFT, Duration::from_secs(15))
        .expect("main composer holds the draft");
    inject_keys_paced(&mut harness, b"\x10"); // Ctrl+P opens the command palette
    inject_keys_paced(&mut harness, b"send feedback");
    harness
        .wait_for_text("Send Feedback", Duration::from_secs(15))
        .expect("palette filter shows the Send Feedback entry");
    harness.inject_keys(b"\r").expect("pick Send Feedback");
    harness
        .wait_for_text(FEEDBACK_MODAL_PLACEHOLDER_SENTINEL, Duration::from_secs(15))
        .expect("the palette opens the form on the Write tab");
    harness.inject_keys(keys::ESC).expect("Esc closes the form");
    harness
        .wait_until("form dismissed", Duration::from_secs(15), |h| {
            !h.contains_text(FEEDBACK_MODAL_PLACEHOLDER_SENTINEL)
                && !h.contains_text(DRAFTS_TAB_SENTINEL)
                && h.contains_text(MAIN_DRAFT)
        })
        .expect("Esc must close the form and hand the composer draft back");
    assert_eq!(
        1,
        content.feedback_posts().len(),
        "dismissing the form must not POST"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
