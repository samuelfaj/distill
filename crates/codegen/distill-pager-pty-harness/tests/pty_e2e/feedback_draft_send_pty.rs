// Per-test-case module for the `pty_e2e` integration test crate.
//
// Full-TUI twin of `minimal/minimal_feedback_draft_send.rs`: the same predraft-to-POST flow through
// the centered modal, so a regression in either host's key routing is caught against the mock's
// recorded `/v1/feedback` body.
#[allow(unused_imports)]
use super::common::*;

const REPORT: &str = "fullscreen-pty-draft-send-report-xyz";

/// A failed `/feedback <text>` send is kept as a typeless predraft; bare `/feedback` opens it from Drafts,
/// a typeless send is refused, and picking a type sends the draft to the mock and empties the drafts file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn feedback_draft_send_pty() {
    let content = ContentController::start().await.expect("start content");
    let overrides = enable_feedback_posting(&content, "feedback-draft-send-pty");
    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        &overrides,
        Some(content.home()),
    )
    .expect("spawn pager with content");
    enter_session(&mut harness, &content);
    let session_dir = session_dir(&content, &mut harness);

    drive_draft_send(&mut harness, &content, &session_dir, REPORT);

    harness.quit().expect("clean quit");
}
