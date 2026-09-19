// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

/// First and last line tokens of the anchor block (turn 1's message).
const ANCHOR_FIRST: &str = "SCROLLOUT_ALPHA";

const ANCHOR_LAST: &str = "SCROLLOUT_OMEGA";

/// Turn-2 filler rows: comfortably taller than the 50-row PTY so drag autoscroll can push the anchor block fully out of the viewport.
const FILLER_ROWS: usize = 120;

/// The copy must still contain the anchor line and the block's late lines. Exercises the drag-start
/// `content_width` snapshot: at mouse-up time `visible_blocks` no longer has the anchor block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn drag_select_autoscroll_full_scrollout_copy_pty() {
    let content = ContentController::start().await.expect("start content");
    // Turn 1: a three-row anchor message (markdown hard breaks keep one row per source line)
    // Turn 2: filler tall enough to scroll it fully out
    let mut anchor_turn = content.expect_agent_turn(
        "selection anchor turn",
        format!("{ANCHOR_FIRST} anchor first line  \nmiddle filler line  \n{ANCHOR_LAST} anchor last line"),
    );
    let mut filler_turn = content.expect_agent_turn(
        "selection autoscroll filler turn",
        marker_response(MOCK_RESPONSE_SENTINEL, FILLER_ROWS),
    );

    let binary = pager_binary().expect("resolve pager binary");
    let overrides: Vec<(String, String)> = vec![(
        "SSH_CONNECTION".into(),
        "scripted-test 1 127.0.0.1 2".into(),
    )];
    let env_refs: Vec<(&str, &str)> = overrides
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env_refs,
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn 1");
    harness
        .wait_for_text(ANCHOR_LAST, Duration::from_secs(45))
        .expect("anchor message rendered");
    // Rendered text alone doesn't prove turn 1 is over
    // Submitting turn 2 while the pager still considers turn 1 live queues the prompt instead of sending it, and the filler wait below times out
    tokio::time::timeout(Duration::from_secs(10), anchor_turn.wait_satisfied())
        .await
        .expect("turn 1 completes before turn 2 send");
    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("turn 1 finalized");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn 2");
    // page_flip_on_send can park at the new prompt (head visible, tail off
    // screen) or follow can pin the tail (head off screen). Either proves
    // the filler rendered; the later wheel-up finds the anchor from both.
    harness
        .wait_until(
            "filler head or tail on screen",
            Duration::from_secs(30),
            |h| {
                h.contains_text(MOCK_RESPONSE_SENTINEL)
                    || h.contains_text(&marker_line(FILLER_ROWS - 1))
                    || topmost_visible_marker(h).is_some()
            },
        )
        .expect("filler rendered");
    tokio::time::timeout(Duration::from_secs(15), filler_turn.wait_satisfied())
        .await
        .expect("filler turn completes");
    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("turn 2 finalized");

    harness.inject_keys(b"\t").expect("focus scrollback");
    harness
        .wait_for_text("Space:prompt", Duration::from_secs(10))
        .expect("scrollback focused (Space:prompt hint) after Tab");
    harness.update(Duration::from_millis(500));

    // Wheel up until the anchor block's first line is back on screen, then a little further so the press row sits clear of the top autoscroll zone
    let scroll_deadline = Instant::now() + Duration::from_secs(30);
    while !harness.contains_text(ANCHOR_FIRST) {
        assert!(
            Instant::now() < scroll_deadline,
            "never scrolled the anchor line back into view\nscreen:\n{}",
            harness.screen_contents()
        );
        send_wheel_burst(
            &mut harness,
            SGR_SCROLL_UP,
            10,
            WHEEL_ROW,
            WHEEL_COL,
            Duration::ZERO,
        );
        harness.update(Duration::from_millis(200));
    }
    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        3,
        WHEEL_ROW,
        WHEEL_COL,
        Duration::ZERO,
    );
    harness.update(Duration::from_millis(400));

    let screen = harness.screen_contents();
    let (row, col) = locate_screen_text(&screen, ANCHOR_FIRST)
        .unwrap_or_else(|| panic!("could not locate {ANCHOR_FIRST:?}; screen:\n{screen}"));

    // Press on the anchor line, drag to the bottom edge, and HOLD: the edge position starts autoscroll and the ticks do the rest
    let bottom_row = DEFAULT_ROWS - 3;
    let mut drag = String::new();
    drag.push_str(&sgr_mouse(0, row, col, 'M'));
    drag.push_str(&sgr_mouse(32, row + 1, col + 8, 'M'));
    drag.push_str(&sgr_mouse(32, bottom_row, col + 20, 'M'));
    harness
        .inject_keys(drag.as_bytes())
        .expect("press and drag to bottom edge");

    // Hold until the anchor block is PROVABLY out of `visible_blocks`. The mouse-up-time width lookup
    // then cannot rescue the copy.
    let out_deadline = Instant::now() + Duration::from_secs(30);
    while topmost_visible_marker(&harness).is_none_or(|m| m < 4) {
        assert!(
            Instant::now() < out_deadline,
            "autoscroll never pushed the anchor block off-screen\nscreen:\n{}",
            harness.screen_contents()
        );
        harness.update(Duration::from_millis(200));
    }
    assert!(
        !harness.contains_text(ANCHOR_FIRST) && !harness.contains_text(ANCHOR_LAST),
        "anchor lines still on screen past marker 4\nscreen:\n{}",
        harness.screen_contents()
    );

    harness
        .inject_keys(sgr_mouse(0, bottom_row, col + 20, 'm').as_bytes())
        .expect("release");

    let payloads = wait_for_osc52_payloads(&mut harness, Duration::from_secs(10));
    assert!(
        !payloads.is_empty(),
        "expected an OSC 52 clipboard write after release; screen:\n{}",
        harness.screen_contents()
    );
    let joined = payloads.join("\n");
    assert!(
        joined.contains(ANCHOR_FIRST),
        "clipboard must contain the (scrolled-out) anchor line; payloads={payloads:?}"
    );
    assert!(
        joined.contains(ANCHOR_LAST),
        "clipboard must contain the anchor block's late lines; payloads={payloads:?}"
    );
    // The selection stays pinned to the anchor block's range: no filler lines.
    assert!(
        !joined.contains("MARKER-"),
        "clipboard must not contain lines from the filler block; payloads={payloads:?}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
