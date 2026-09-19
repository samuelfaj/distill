// Modified for Distill by Samuel Fajreldines, 2026.
//! Relay-mangled X10 mouse reports must not type into the composer, and refocus must re-assert mouse capture.
//!
//! ```bash
//! cargo test -p distill-pager-pty-harness --test x10_mouse_leak -- --ignored --nocapture
//! ```

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // opt-in: real pager binary in a PTY (CI runs with --ignored)
async fn x10_leak_defenses() {
    distill_pager_pty_harness::scenarios::x10_mouse_leak::assert_x10_leak_defenses()
        .await
        .expect("mangled X10 reports must not type; focus-in must re-assert mouse capture");
}
