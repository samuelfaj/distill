// Modified for Distill by Samuel Fajreldines, 2026.
//! Integration test: the shell re-parks `exit_plan_mode` on resume, so approval chrome reappears after quit/`--continue`.
//! Approving then leaves plan mode and starts the implement turn.
//!
//! CI stages the pager binary via `PAGER_BINARY`.
//! The test also runs under plain cargo, which builds the pager on demand:
//!
//! ```bash
//! cargo test -p distill-pager-pty-harness --test plan_approval_resume -- --nocapture
//! ```

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_approval_restored_after_resume() {
    distill_pager_pty_harness::scenarios::plan_approval_resume::assert_plan_approval_restored_after_resume()
        .await
        .expect("shell must re-park exit_plan_mode on resume so approval chrome returns");
}
