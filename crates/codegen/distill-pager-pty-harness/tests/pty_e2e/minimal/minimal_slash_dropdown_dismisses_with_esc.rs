// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// In the minimal overlay host, typing `/` opens a slash dropdown above the prompt, growing the pinned live viewport to make room.
/// A single Esc dismisses it: the pane's slash handler must consume the Esc before the idle clear or the rewind policy runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_slash_dropdown_dismisses_with_esc() {
    let content = ContentController::start().await.expect("start content");
    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    // "/mod" narrows to `/model`, whose description renders only inside the dropdown, not in the typed text or the `minimal · /help` status line
    // Seeing it therefore proves the dropdown is open
    inject_keys_paced(&mut harness, b"/mod");
    harness
        .wait_for_text("Switch the active model", Duration::from_secs(10))
        .expect("slash dropdown open above the prompt");

    harness.inject_keys(keys::ESC).expect("press esc");
    harness
        .wait_for_text_absent("Switch the active model", Duration::from_secs(10))
        .expect("slash dropdown dismissed after Esc");

    harness.update(Duration::from_millis(200));
    let screen = harness.screen_contents();
    assert!(
        !screen.contains("Switch the active model"),
        "Esc must keep the slash dropdown closed\nscreen:\n{screen}"
    );
    // Dismiss only: Esc must not fall through to the idle clear or open rewind
    assert!(
        !screen.contains("press again to clear"),
        "slash-dropdown Esc must not fall through to the idle clear\nscreen:\n{screen}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );

    quit_minimal(&mut harness);
}
