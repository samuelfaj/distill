// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

const REPORT: &str = "minimal-pty-draft-send-report-xyz";

/// Minimal: a failed `/feedback <text>` send is kept as a typeless predraft; bare `/feedback` opens it from
/// Drafts, a typeless send is refused, and picking a type sends the draft to the mock and empties the drafts file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn minimal_feedback_draft_send() {
    let content = ContentController::start().await.expect("start content");
    let overrides = enable_feedback_posting(&content, "minimal-feedback-draft-send");
    let mut harness =
        spawn_minimal_env_ops(&content, 40, 100, &[], &overrides, Some(content.home()));
    wait_minimal_ready(&mut harness);
    bind_minimal_session(&mut harness, &content);
    let session_dir = session_dir(&content, &mut harness);

    drive_draft_send(&mut harness, &content, &session_dir, REPORT);

    quit_minimal(&mut harness);
}
