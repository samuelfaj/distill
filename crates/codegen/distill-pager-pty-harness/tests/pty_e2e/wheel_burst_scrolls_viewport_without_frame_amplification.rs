// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

// Infra check: timed wheel bursts, viewport markers, frame capture. "The view scrolled up" is then
// a strict index decrease rather than a fragile absolute-row check. Only the byte-deterministic
// frame count is asserted; no wall-clock.

/// Marker count: 120 one-row lines far exceed the 50-row PTY, so early markers sit off-screen-top once the finished stream pins the view to the bottom.
const MARKER_COUNT: usize = 120;

/// 30 spaced single reports at a nominal 6ms: a trackpad-classified flood under the harness terminal.
const BURST_EVENTS: usize = 30;

const BURST_INTERVAL: Duration = Duration::from_millis(6);

/// **Wheel-burst scroll infra check.**
/// A closely spaced wheel-up burst over a marker transcript must scroll previously off-screen-top markers into view without a panic.
/// It must produce at least one repaint frame and at most one frame per wheel event (no frame amplification).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn wheel_burst_scrolls_viewport_without_frame_amplification() {
    let (mut harness, _content, top_before) =
        spawn_bottom_pinned_marker_scrollback(MARKER_COUNT).await;

    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        BURST_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    harness.update(Duration::from_millis(600));

    assert!(
        harness.is_running().expect("poll pager liveness"),
        "pager exited during the wheel burst\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager rendered 'panicked' during the wheel burst\nscreen:\n{}",
        harness.screen_contents()
    );

    // The viewport scrolled: the topmost visible marker index strictly decreased, i.e. a marker that was off-screen-top is now on screen.
    let top_after = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "no marker visible after the wheel burst\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert!(
        top_after < top_before,
        "wheel-up burst did not scroll the viewport: topmost visible marker \
         {} → {} (expected a decrease)\nscreen:\n{}",
        marker_line(top_before),
        marker_line(top_after),
        harness.screen_contents()
    );

    // Amplification bound on the live frame capture: the burst repainted at least once, and never more than once per wheel event
    // (Durations are not asserted: the no-drain driver means chunks were parsed at drain time, and wall-clock is load-sensitive anyway.)
    let frames = harness.frame_count();
    assert!(
        frames >= 1,
        "wheel burst produced no repaint frames (no ?2026h/l pairs after reset_timing)"
    );
    assert!(
        frames <= BURST_EVENTS as u64,
        "burst of {BURST_EVENTS} wheel events produced {frames} frames — more than one \
         repaint per event (frame amplification)"
    );

    // Driver shape check: a mixed-direction sequence (momentum reversal) must keep the pager alive
    // No position assertion; direction handling is for the behavioral tests
    let reversal = [
        SGR_SCROLL_UP,
        SGR_SCROLL_UP,
        SGR_SCROLL_DOWN,
        SGR_SCROLL_UP,
        SGR_SCROLL_DOWN,
        SGR_SCROLL_DOWN,
    ];
    send_wheel_sequence(
        &mut harness,
        &reversal,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    harness.update(Duration::from_millis(300));
    let running = harness.is_running().expect("poll pager liveness");
    assert!(
        running && !harness.contains_text("panicked"),
        "pager broke on a mixed-direction wheel sequence\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
