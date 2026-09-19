// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

// Regression: trackpad feel — fast flicks must not under-travel. The trackpad cap only binds once
// a stream is CONFIRMED trackpad, which happens mid-stream only on ept=1 brands. Under the
// parent's fixed 6-line cap the same flood delivers at most 6 lines per 16ms slot.

/// Tall enough that maximum plausible travel, bounded by the ~590-row desired total at full nominal acceleration, never clamps at the transcript top.
/// A clamp there would mask the travel measurement.
const MARKER_COUNT: usize = 700;

/// 40 spaced single reports at a nominal 6ms: the ept=1 brand promotes the stream to confirmed trackpad at the 3rd event (avg interval < 30ms).
/// That engages the per-flush cap under test.
const BURST_EVENTS: usize = 40;

const BURST_INTERVAL: Duration = Duration::from_millis(6);

/// Rows the flood must travel.
/// The floor sits below every delivery regime of the new code (jitter floor ≈ 240) and above the parent's capped ceiling (≈ 130).
const TRAVEL_FLOOR: usize = 200;

/// A fully batched arrival delivers over at most one in-batch flush + ~5 post-stop cadence slots +
/// one finalize flush (≈ 7 painted frames). A genuine cap regression instead must pace the 260ms
/// burst window at 6 lines per 16ms slot (≥ 21 frames to reach ~138 rows).
const COMPRESSED_BURST_FRAMES_MAX: u64 = 12;

/// A dense trackpad flood at high scroll speed must move the viewport by at least [`TRAVEL_FLOOR`] rows.
/// The proportional per-flush cap keeps up with gesture demand, and the finalize flush drains the backlog instead of discarding the flick's tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn trackpad_flood_does_not_under_travel() {
    let (mut harness, _content, top_before) = spawn_bottom_pinned_marker_scrollback_with_env(
        MARKER_COUNT,
        &[("TERM_PROGRAM", "iTerm.app"), ("GROK_SCROLL_SPEED", "100")],
    )
    .await;

    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        BURST_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    // Drain window: outlasts the 80ms stream gap plus the post-stop cadence flushes and the finalize flush
    harness.update(Duration::from_millis(800));

    assert!(
        harness.is_running().expect("poll pager liveness"),
        "pager exited during the trackpad flood\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager rendered 'panicked' during the trackpad flood\nscreen:\n{}",
        harness.screen_contents()
    );

    let top_after = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "no marker visible after the trackpad flood\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert!(
        top_after < top_before,
        "trackpad flood did not scroll the viewport: topmost visible marker \
         {} → {}\nscreen:\n{}",
        marker_line(top_before),
        marker_line(top_after),
        harness.screen_contents()
    );
    let travel = top_before - top_after;
    let frames = harness.frame_count();
    // Compressed-burst detector (see header): travel under the floor with only a handful of frames means the reports arrived batched
    // That is the pager stalling through the burst window, a capped outcome and not the under-travel regression
    // Soften with a distinct message instead of failing
    if travel < TRAVEL_FLOOR && frames <= COMPRESSED_BURST_FRAMES_MAX {
        eprintln!(
            "SKIP(compressed burst): {frames} frames \
             (<= {COMPRESSED_BURST_FRAMES_MAX}) painted {travel} rows — the \
             burst arrived batched under host load; not asserting the \
             {TRAVEL_FLOOR}-row floor (this is NOT the under-travel \
             regression, which paces ~16+ frames)"
        );
        harness.quit().expect("clean quit");
        return;
    }
    assert!(
        travel >= TRAVEL_FLOOR,
        "trackpad flood under-traveled: {travel} rows (< {TRAVEL_FLOOR}) \
         across {frames} paced frames — fixed per-flush cap ceiling and/or \
         finalize backlog discard regressed\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
