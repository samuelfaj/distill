//! PTY: Enter and Ctrl+F on a `Message sent to General “sleeper”` row open the child's takeover, exactly as on its
//! `Subagent` row, and `q` comes back; Enter on the rejected row (an id no spawn named) stays in the parent.
// Unix only: shares the `/bin/sleep`-held child of `send_subagent_message_row`.
#![cfg(unix)]
#[allow(unused_imports)]
use super::common::*;
use super::send_subagent_message_row::{
    CHILD_LABEL, PARENT_DONE, ROW_TIMEOUT, SendMessageScenario,
};

/// The footer hint pinned only while a child takeover is up; the dock shows the child's title and `[✗]` in the
/// parent too, so neither of those can stand in for it.
const TAKEOVER_HINT: &str = "q/Esc:back";
const CTRL_F: &[u8] = b"\x06";

fn assert_takeover_open(harness: &mut PtyHarness, chord: &str) {
    harness
        .wait_for_text(TAKEOVER_HINT, ROW_TIMEOUT)
        .unwrap_or_else(|error| panic!("{chord} did not open the takeover: {error}"));
    assert!(
        !harness.contains_text(PARENT_DONE),
        "{chord}: the takeover must replace the parent transcript\nscreen:\n{}",
        harness.screen_contents()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn send_subagent_message_row_opens_child() {
    // The child must still be running: closing the takeover of a finished child evicts its view.
    let mut scenario = SendMessageScenario::start().await;
    let steer_row = format!("Message sent to {CHILD_LABEL}");
    scenario.focus_scrollback();
    scenario.select_row(&steer_row);

    scenario
        .harness
        .inject_keys(keys::ENTER)
        .expect("press Enter");
    assert_takeover_open(&mut scenario.harness, "Enter");
    write_screen_dump_if_requested(&scenario.harness, "send_row_enter_opens_child");
    scenario.harness.inject_keys(keys::Q).expect("press q");
    scenario
        .harness
        .wait_for_text_absent(TAKEOVER_HINT, ROW_TIMEOUT)
        .expect("q returns to the parent");
    scenario.wait_for_row(&format!("\u{203a} {steer_row}"));

    scenario.harness.inject_keys(CTRL_F).expect("press Ctrl+F");
    assert_takeover_open(&mut scenario.harness, "Ctrl+F");
    scenario.harness.inject_keys(keys::Q).expect("press q");
    scenario
        .harness
        .wait_for_text_absent(TAKEOVER_HINT, ROW_TIMEOUT)
        .expect("q returns to the parent again");

    // The rejected row's target never resolved through a spawn, so Enter has no child to open.
    scenario.select_row("Message rejected \u{b7} subagent");
    scenario
        .harness
        .inject_keys(keys::ENTER)
        .expect("press Enter on the rejected row");
    scenario
        .harness
        .wait_until_stable(
            "parent transcript stays after Enter on the rejected row",
            Duration::from_secs(6),
            Duration::from_secs(3),
            |h| !h.contains_text(TAKEOVER_HINT) && h.contains_text("\u{203a} Message rejected"),
        )
        .expect("no takeover for an unresolved target");
    write_screen_dump_if_requested(&scenario.harness, "send_row_rejected_enter_stays");

    scenario.finish("send_subagent_message_row_opens_child.cast");
}
