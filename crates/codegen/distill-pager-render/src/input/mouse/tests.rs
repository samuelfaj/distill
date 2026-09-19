//! Unit tests for scroll normalization in [`super`] (`mouse`), split out via `#[path]` to keep the module itself small.

use super::*;

fn make_config(events_per_tick: u16, mode: ScrollInputMode) -> ScrollConfig {
    ScrollConfig::from_terminal(
        TerminalName::Unknown,
        ScrollConfigOverrides {
            events_per_tick: Some(events_per_tick),
            mode: Some(mode),
            ..ScrollConfigOverrides::default()
        },
    )
}

fn make_vscode_config() -> ScrollConfig {
    ScrollConfig::from_terminal(TerminalName::VsCode, ScrollConfigOverrides::default())
}

/// Tick at each `scroll_clock_deadline` until finalize. A zero delay advances 1ms; the gap check is strict so an exact 80ms tick cannot finalize.
fn drive_suggested_ticks(state: &mut MouseScrollState, mut now: Instant) -> Vec<(Instant, i32)> {
    let mut ticks = Vec::new();
    for _ in 0..64 {
        let Some(delay) = state.scroll_clock_deadline(now) else {
            return ticks;
        };
        now += delay.max(Duration::from_millis(1));
        let update = state.on_tick_at(now);
        ticks.push((now, update.lines));
    }
    panic!("stream did not finalize within 64 suggested ticks");
}

#[test]
fn clamped_pending_tail_does_not_busy_spin_scroll_clock() {
    // Decaying accel used to pull desired below applied; a deadline that disagrees with the flush then busy-spins.
    // Forced-Trackpad keeps desired monotone. Deadlines through a decelerating tail must never be zero.
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // Fast flick: 15 events at 7ms build a capped backlog across the in-burst cadence flushes
    // 7ms is the fast band (accel 2.5x), above the 6ms duplicate-guard boundary
    let mut at = base;
    for i in 0..15u64 {
        at = base + Duration::from_millis(1 + i * 7);
        let _ = state.on_scroll_event_at(at, ScrollDirection::Down, config);
    }

    // Slow drag tail: 40ms intervals sit above the medium accel band once the rolling window turns over, so the multiplier decays to 1.0
    // Desired used to collapse here; now every update must suggest a real deadline (16ms cadence while backlog drains, 80ms gap when drained)
    // Some(ZERO) means the busy-spin bug is back
    for i in 1..=6u64 {
        at = base + Duration::from_millis(99 + i * 40);
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        let deadline = update.next_tick_in.expect("active stream has a deadline");
        assert!(
            deadline > Duration::ZERO && deadline <= STREAM_GAP,
            "tail event must suggest a bounded nonzero deadline, got {deadline:?}"
        );
    }
    let stream = state.stream.as_ref().expect("stream active through tail");
    assert!(
        stream.desired_lines_f32(state.carry_lines) as i32 >= stream.applied_lines,
        "per-event accel weighting must keep desired ({}) >= applied ({}) — \
         the collapsed clamp state must be unreachable",
        stream.desired_lines_f32(state.carry_lines),
        stream.applied_lines
    );

    // Following the suggested deadlines from the last event must reach finalize in a couple of wakeups (the gap check and the strict-boundary step)
    // Under the spin bug this panics at the 64-tick cap long before the 80ms gap elapses in 1ms steps
    let ticks = drive_suggested_ticks(&mut state, at);
    assert!(
        ticks.len() <= 3,
        "decelerating tail must finalize in bounded wakeups, got {} ticks",
        ticks.len()
    );
    assert!(
        !state.has_active_stream(),
        "stream must finalize after the gap"
    );
}

#[test]
fn residual_backlog_flushes_on_16ms_cadence_slots() {
    // The event loop's scroll clock follows scroll_clock_deadline
    // Residual (cap-suppressed) trackpad lines must be due in exact REDRAW_CADENCE (16ms) slots
    // Pacing these flushes on the ~33ms animation tick was the "laggy yet too sensitive" defect: fewer, bigger jumps
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // 8 events at 2ms land inside one cadence slot (sub-6ms spacing is accel-excluded, so desired is the raw 8).
    // That is more than the 6-line floor cap can flush at once, leaving a residual backlog.
    let mut last_event_at = base;
    for i in 0..8u64 {
        last_event_at = base + Duration::from_millis(1 + i * 2);
        let _ = state.on_scroll_event_at(last_event_at, ScrollDirection::Down, config);
    }
    assert!(
        state.has_active_stream(),
        "fixture: burst must arm a stream"
    );

    let ticks = drive_suggested_ticks(&mut state, last_event_at);
    let flushes: Vec<&(Instant, i32)> = ticks.iter().filter(|(_, lines)| *lines != 0).collect();
    assert!(
        flushes.len() >= 2,
        "capped backlog must drain over multiple cadence flushes, got {}",
        flushes.len()
    );
    // The synthetic clock makes the deadline chain exact: consecutive residual flushes land exactly one REDRAW_CADENCE apart, never a 33ms slot
    for pair in flushes.windows(2) {
        let [a, b] = pair else { continue };
        let spacing = b.0.duration_since(a.0);
        assert_eq!(
            spacing, REDRAW_CADENCE,
            "residual flushes must be 16ms apart, got {spacing:?}"
        );
    }
    assert!(
        !state.has_active_stream(),
        "stream must finalize once the deadline chain crosses the 80ms gap"
    );
}

#[test]
fn no_flush_starvation_when_events_stop_mid_cadence() {
    // Events that stop inside a cadence window (every event suppressed, nothing flushed yet) must still flush at the next 16ms slot
    // The suggested deadline drives that flush; without it they starve until the 80ms gap finalize
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let _ = state.on_scroll_event_at(
        base + Duration::from_millis(1),
        ScrollDirection::Down,
        config,
    );
    let last_event_at = base + Duration::from_millis(5);
    let update = state.on_scroll_event_at(last_event_at, ScrollDirection::Down, config);
    assert_eq!(
        update.lines, 0,
        "fixture: both events must land inside the first cadence window"
    );

    let ticks = drive_suggested_ticks(&mut state, last_event_at);
    let first_flush = ticks
        .iter()
        .find(|(_, lines)| *lines != 0)
        .expect("pending lines must flush after events stop");
    let waited = first_flush.0.duration_since(last_event_at);
    assert!(
        waited <= REDRAW_CADENCE,
        "first flush after events stop must land within one 16ms cadence \
         slot, waited {waited:?} (starved until the 80ms finalize?)"
    );
}

#[test]
fn suggested_deadlines_finalize_at_80ms_gap_without_idle_spin() {
    // With nothing pending, the only deadline is the 80ms gap check: the scroll clock must sleep the full remainder (no 16ms idle spinning)
    // The stream must finalize just past the gap, exactly the STREAM_GAP behavior the animation tick used to provide
    let config = make_config(3, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // One full wheel tick: promotes on the 3rd event and flushes immediately, leaving desired == applied
    let mut last_event_at = base;
    for i in 0..3u64 {
        last_event_at = base + Duration::from_millis(1 + i);
        let _ = state.on_scroll_event_at(last_event_at, ScrollDirection::Down, config);
    }
    assert!(state.has_active_stream());

    let delay = state
        .scroll_clock_deadline(last_event_at)
        .expect("active stream must expose a deadline");
    assert_eq!(
        delay, STREAM_GAP,
        "an idle stream's only deadline is the gap check, got {delay:?}"
    );

    let ticks = drive_suggested_ticks(&mut state, last_event_at);
    assert!(!state.has_active_stream(), "stream must finalize");
    let finalize_at = ticks.last().expect("at least the finalize tick").0;
    let gap = finalize_at.duration_since(last_event_at);
    assert!(
        gap > STREAM_GAP && gap <= STREAM_GAP + Duration::from_millis(2),
        "finalize must land just past the 80ms gap, got {gap:?}"
    );
    assert!(
        ticks.len() <= 2,
        "an idle stream must not spin 16ms wakeups, got {} ticks",
        ticks.len()
    );
}

#[test]
fn wheel_tick_scrolls_three_lines_when_terminal_emits_three_events() {
    let config = make_config(3, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let _ = state.on_scroll_event_at(
        base + Duration::from_millis(1),
        ScrollDirection::Down,
        config,
    );
    let _ = state.on_scroll_event_at(
        base + Duration::from_millis(2),
        ScrollDirection::Down,
        config,
    );
    let update = state.on_scroll_event_at(
        base + Duration::from_millis(3),
        ScrollDirection::Down,
        config,
    );

    assert_eq!(update.lines, 3);
}

#[test]
fn wheel_tick_scrolls_three_lines_when_terminal_emits_nine_events() {
    let config = make_config(9, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut update = ScrollUpdate::default();
    for idx in 0..9u64 {
        update = state.on_scroll_event_at(
            base + Duration::from_millis(idx + 1),
            ScrollDirection::Down,
            config,
        );
    }
    assert_eq!(update.lines, 3);
}

#[test]
fn direction_flip_closes_previous_stream() {
    let config = make_config(3, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let _ = state.on_scroll_event_at(base + Duration::from_millis(1), ScrollDirection::Up, config);
    let _ = state.on_scroll_event_at(base + Duration::from_millis(2), ScrollDirection::Up, config);
    let _ = state.on_scroll_event_at(base + Duration::from_millis(3), ScrollDirection::Up, config);

    let update = state.on_scroll_event_at(
        base + Duration::from_millis(4),
        ScrollDirection::Down,
        config,
    );

    assert!(update.lines >= 0);
}

#[test]
fn continuous_trackpad_scroll_does_not_stall() {
    // Regression test: continuous scrolling must not stop producing lines after many events
    // Before the fix, accumulated_events was capped at ±256 and desired_lines was clamped at ±256
    // Scroll froze during long trackpad gestures (1-2 seconds of continuous two-finger scroll)
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut total_lines = 0i64;
    // Simulate 1000 events at ~5ms intervals (typical fast trackpad scroll).
    for i in 0..1000u64 {
        let update = state.on_scroll_event_at(
            base + Duration::from_millis(i * 5),
            ScrollDirection::Down,
            config,
        );
        total_lines += update.lines as i64;
    }

    // With 1000 events, events_per_tick=3, lines_per_tick=3, and up to 3x trackpad acceleration, we should have scrolled well over 500 lines
    // Before the fix this would stall at ~256 lines.
    assert!(
        total_lines > 500,
        "expected > 500 total lines from 1000 scroll events, got {total_lines} (scroll stalled?)"
    );
}

#[test]
fn continuous_trackpad_scroll_single_event_terminal() {
    // Regression test for terminals that emit 1 event per tick (iTerm2, WezTerm, VS Code)
    // With normalized trackpad base rate (always ept=3 divisor), these now behave identically to ept=3 terminals
    // Verify scrolling doesn't stall
    let config = make_config(1, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut total_lines = 0i64;
    for i in 0..500u64 {
        let update = state.on_scroll_event_at(
            base + Duration::from_millis(i * 5),
            ScrollDirection::Down,
            config,
        );
        total_lines += update.lines as i64;
    }

    // With normalized base rate (2/3 lines/event, same as ept=3) and per-flush cap of 4, 500 events over 2.5s produce substantial scroll distance
    // Base = 500 × 0.67 ≈ 333 lines
    assert!(
        total_lines > 200,
        "expected > 200 total lines (ept=1, normalized), got {total_lines}"
    );
}

#[test]
fn stream_gap_closes_stream() {
    let config = make_config(3, ScrollInputMode::Wheel);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let _ = state.on_scroll_event_at(
        base + Duration::from_millis(1),
        ScrollDirection::Down,
        config,
    );

    // 100ms is past the 80ms stream gap, so on_tick has no next deadline
    let update = state.on_tick_at(base + Duration::from_millis(100));
    assert!(update.next_tick_in.is_none());
}

#[test]
fn high_rate_wheel_coalesces_redraws() {
    // Free-spinning wheel at 3ms. Cadence coalescing must flush once per 16ms window, not once per event, without losing lines.
    let config = make_config(3, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let event_count = 300u64;
    let interval_ms = 3u64;
    let mut flush_count = 0u64;
    let mut total_lines = 0i64;

    for i in 0..event_count {
        let update = state.on_scroll_event_at(
            base + Duration::from_millis(i * interval_ms + 1),
            ScrollDirection::Down,
            config,
        );
        if update.lines != 0 {
            flush_count += 1;
        }
        total_lines += update.lines as i64;
    }

    // Total lines scrolled should still be correct (no data loss).
    assert!(
        total_lines > 200,
        "expected > 200 total lines, got {total_lines}"
    );

    // Flush count should be bounded by cadence (~60fps), not proportional to the raw event count
    // 300 events × 3ms = 900ms → 900/16 ≈ 56 cadence windows.
    let max_expected_flushes = (event_count * interval_ms / REDRAW_CADENCE_MS) + 10;
    assert!(
        flush_count <= max_expected_flushes,
        "expected <= {max_expected_flushes} flushes (cadence-bound), \
         got {flush_count} (nearly every event flushed — no coalescing)"
    );
}

#[test]
fn discrete_wheel_tick_still_flushes_promptly() {
    // A single wheel tick (3 events in <12ms) should flush on promotion (just_promoted), not be delayed to the next cadence window
    // Regular mouse wheels must remain responsive
    let config = make_config(3, ScrollInputMode::Auto);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let u1 = state.on_scroll_event_at(
        base + Duration::from_millis(1),
        ScrollDirection::Down,
        config,
    );
    let u2 = state.on_scroll_event_at(
        base + Duration::from_millis(2),
        ScrollDirection::Down,
        config,
    );
    let u3 = state.on_scroll_event_at(
        base + Duration::from_millis(3),
        ScrollDirection::Down,
        config,
    );

    // The 3rd event completes the tick and triggers promotion, flushing all 3 lines immediately
    let total = u1.lines + u2.lines + u3.lines;
    assert_eq!(
        total, 3,
        "single wheel tick should produce 3 lines immediately"
    );
}

#[test]
fn finalized_stream_carry_is_only_fractional() {
    // After a fast capped stream ends, carry_lines should only hold the sub-line fractional remainder, not cap-induced integer backlog
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // Fast burst: 100 events at 4ms intervals (250/sec, fast band).
    for i in 0..100u64 {
        state.on_scroll_event_at(
            base + Duration::from_millis(i * 4),
            ScrollDirection::Down,
            config,
        );
    }

    // Finalize via stream gap, driving the drain to completion (a single overdue tick may now be a tapered drain flush, not the finalize)
    let _ = state.on_tick_at(base + Duration::from_millis(500));
    let _ = drive_suggested_ticks(&mut state, base + Duration::from_millis(500));
    assert!(!state.has_active_stream(), "stream must finalize");

    assert!(
        state.carry_lines.abs() < 1.0,
        "carry after finalization should be fractional, got {}",
        state.carry_lines
    );
}

#[test]
fn two_same_direction_gestures_no_carry_pollution() {
    // Two consecutive same-direction gestures separated by a gap.
    // The second gesture should start clean, with no burst from the first gesture's capped backlog
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // Gesture 1: fast burst.
    for i in 0..80u64 {
        state.on_scroll_event_at(
            base + Duration::from_millis(i * 4),
            ScrollDirection::Down,
            config,
        );
    }
    // Finalize gesture 1.
    let _ = state.on_tick_at(base + Duration::from_millis(500));

    // Gesture 2: slow scroll starting 200ms after finalization.
    let g2_start = 700u64;
    let mut g2_first_flush_lines = 0i32;
    for i in 0..10u64 {
        let update = state.on_scroll_event_at(
            base + Duration::from_millis(g2_start + i * 12),
            ScrollDirection::Down,
            config,
        );
        if g2_first_flush_lines == 0 && update.lines != 0 {
            g2_first_flush_lines = update.lines;
        }
    }

    assert!(
        g2_first_flush_lines <= 4,
        "second gesture first flush should be small, got {} lines (carry pollution?)",
        g2_first_flush_lines
    );
}

/// Feed a dense trackpad flick (`events` at `interval_ms`), then drive the suggested deadlines to finalize; returns total delivered lines.
fn run_flick_to_finalize(config: ScrollConfig, events: u64, interval_ms: u64) -> i64 {
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);
    let mut total = 0i64;
    let mut at = base;
    for i in 0..events {
        at = base + Duration::from_millis(1 + i * interval_ms);
        total += state
            .on_scroll_event_at(at, ScrollDirection::Down, config)
            .lines as i64;
    }
    for (_, lines) in drive_suggested_ticks(&mut state, at) {
        total += lines as i64;
    }
    assert!(!state.has_active_stream(), "flick must finalize");
    total
}

#[test]
fn fast_flick_delivery_scales_with_viewport() {
    // Cap is max(6, viewport/2): the same flick must deliver more on a taller viewport, never less than the legacy floor.
    let base_config = ScrollConfig::from_terminal(
        TerminalName::Unknown,
        ScrollConfigOverrides {
            events_per_tick: Some(3),
            mode: Some(ScrollInputMode::Trackpad),
            speed_multiplier: Some(speed_to_multiplier(100)),
            ..ScrollConfigOverrides::default()
        },
    );
    // 60 events at 2ms: desired = 360 lines accrues far faster than the floor cap can drain (6 per 16ms slot), so delivery is cap-bound
    let floor = run_flick_to_finalize(base_config, 60, 2);
    let small = run_flick_to_finalize(base_config.with_viewport_height(20), 60, 2);
    let tall = run_flick_to_finalize(base_config.with_viewport_height(60), 60, 2);

    assert!(
        tall > small && small > floor,
        "flick delivery must scale with viewport: floor {floor}, viewport-20 \
         {small}, viewport-60 {tall}"
    );
    assert!(
        tall >= floor,
        "a stamped viewport must never under-deliver the legacy fixed cap \
         (floor {floor}, viewport-60 {tall})"
    );
}

#[test]
fn finalize_flushes_whole_line_backlog_not_just_carry() {
    // Stream-end backlog: finalize used to deliver at most the 6-line cap and silently discard the remaining whole lines
    // Only the fractional remainder survived as carry, so a fast flick's tail evaporated
    // With the proportional cap the finalize flush must drain the whole-line backlog when it fits in one cap
    let config = make_config(3, ScrollInputMode::Trackpad).with_viewport_height(40);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // 40 events at 2ms (sub-6ms spacing is accel-excluded, so desired is the raw 40): the four in-burst cadence flushes apply 33
    // That leaves a whole-line backlog bigger than the legacy 6-line cap but within one proportional cap (20)
    let mut at = base;
    for i in 0..40u64 {
        at = base + Duration::from_millis(1 + i * 2);
        let _ = state.on_scroll_event_at(at, ScrollDirection::Down, config);
    }
    let pending = {
        let stream = state.stream.as_ref().expect("stream active after burst");
        stream.effective_pending(state.carry_lines)
    };
    assert!(
        pending > MIN_DELTA_PER_FLUSH && pending <= config.flush_cap(),
        "fixture: backlog must exceed the legacy cap yet fit one proportional \
         cap, got {pending}"
    );

    // First tick past the gap still delivers the unflushed backlog whole, but does not finalize while lines remain.
    let update = state.on_tick_at(at + STREAM_GAP + Duration::from_millis(1));
    assert_eq!(
        update.lines, pending,
        "post-gap catch-up must flush the whole-line backlog, not just carry"
    );
    assert!(
        state.has_active_stream(),
        "finalize is deferred to the tick after the drain ran dry"
    );
    let ticks = drive_suggested_ticks(&mut state, at + STREAM_GAP + Duration::from_millis(1));
    assert!(!state.has_active_stream(), "drained stream must finalize");
    assert_eq!(
        ticks.iter().map(|(_, lines)| lines).sum::<i32>(),
        0,
        "nothing may remain after the drain: the finalize flushes zero"
    );
    assert!(
        state.carry_lines.abs() < 1.0,
        "carry after finalize stays sub-line, got {}",
        state.carry_lines
    );
}

#[test]
fn fractional_carry_not_reamplified_by_speed_multiplier() {
    // Carry is final line units. Consuming it before the speed multiplier minted phantom lines per gesture.
    let config = ScrollConfig::from_terminal(
        TerminalName::Unknown,
        ScrollConfigOverrides {
            events_per_tick: Some(3),
            mode: Some(ScrollInputMode::Trackpad),
            speed_multiplier: Some(speed_to_multiplier(100)),
            ..ScrollConfigOverrides::default()
        },
    )
    // Cap 30: big enough that the flush cap cannot mask the phantom lines.
    .with_viewport_height(60);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);
    // Seed the boundary state a prior same-direction gesture leaves.
    state.carry_lines = 0.9;
    state.carry_direction = Some(ScrollDirection::Down);

    // First event of the next gesture flushes immediately (>16ms since the last redraw)
    // 1.0-weighted (no interval history) × 6.0 speed + 0.9 carry = 6.9 → 6 lines
    // Re-amplified carry would price (1.0 + 0.9) × 6.0 = 11.4 → 11
    let update = state.on_scroll_event_at(
        base + Duration::from_millis(100),
        ScrollDirection::Down,
        config,
    );
    assert_eq!(
        update.lines, 6,
        "carry must cross the stream boundary in final line units, not \
         be re-scaled by the speed multiplier"
    );

    // Round-trip: the finalize remainder stays sub-line in the same units.
    let _ = state.on_tick_at(base + Duration::from_millis(300));
    assert!(!state.has_active_stream(), "gap tick must finalize");
    assert!(
        state.carry_lines.abs() < 1.0,
        "carry after finalize stays sub-line, got {}",
        state.carry_lines
    );
}

#[test]
fn desired_monotone_no_zero_flush_window_under_decaying_accel() {
    // Retroactive whole-stream accel shrank desired below applied and paused mid-stream. Per-event weights keep desired monotone.
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut last_desired = 0.0f32;
    // 7ms spacing: fast band, above the 6ms duplicate-guard boundary
    for i in 0..15u64 {
        let at = base + Duration::from_millis(1 + i * 7);
        let _ = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        let desired = {
            let stream = state.stream.as_ref().expect("stream active");
            stream.desired_lines_f32(state.carry_lines)
        };
        assert!(desired >= last_desired, "desired shrank during fast phase");
        last_desired = desired;
    }

    // Decelerating tail: 24ms intervals decay the multiplier toward 1.0 as the rolling window turns over
    for i in 1..=10u64 {
        let at = base + Duration::from_millis(99 + i * 24);
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        let desired = {
            let stream = state.stream.as_ref().expect("stream active");
            stream.desired_lines_f32(state.carry_lines)
        };
        assert!(
            desired >= last_desired,
            "desired shrank under decaying accel: {last_desired} -> {desired}"
        );
        last_desired = desired;
        assert!(
            update.lines >= 1,
            "tail event {i} delivered no lines mid-stream (movement pause)"
        );
    }
}

#[test]
fn tiny_viewport_floor_cap_still_scrolls() {
    // Cap floor: viewport/2 on a 4-row pane would be 2 lines per flush
    // The floor keeps tiny (and unknown, height 0) viewports at the legacy 6-line cap so they still travel
    let base_config = make_config(3, ScrollInputMode::Trackpad);
    assert_eq!(base_config.with_viewport_height(4).flush_cap(), 6);
    assert_eq!(base_config.with_viewport_height(0).flush_cap(), 6);
    assert_eq!(base_config.with_viewport_height(60).flush_cap(), 30);

    let config = base_config.with_viewport_height(4);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);
    let mut total = 0i64;
    let mut at = base;
    for i in 0..12u64 {
        at = base + Duration::from_millis(1 + i * 4);
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        assert!(update.lines <= 6, "flush exceeded the floor cap");
        total += update.lines as i64;
    }
    for (_, lines) in drive_suggested_ticks(&mut state, at) {
        assert!(lines <= 6, "tick flush exceeded the floor cap");
        total += lines as i64;
    }
    assert!(total > 0, "tiny viewport must still scroll, got {total}");
}

#[test]
fn vscode_fast_scroll_matches_native_terminal_throughput() {
    // VS Code emits events at ~30ms intervals vs ~10ms for native terminals.
    // With wider accel bands and higher trackpad_lines_per_tick, VS Code should achieve comparable scroll throughput
    let vscode = make_vscode_config();
    let native = make_config(1, ScrollInputMode::Trackpad);
    let base = Instant::now();

    let mut vscode_state = MouseScrollState::new_at(base);
    let mut native_state = MouseScrollState::new_at(base);

    let mut vscode_lines = 0i64;
    let mut native_lines = 0i64;

    // VS Code: 200 events at 30ms intervals (typical fast trackpad).
    for i in 0..200u64 {
        let update = vscode_state.on_scroll_event_at(
            base + Duration::from_millis(i * 30),
            ScrollDirection::Down,
            vscode,
        );
        vscode_lines += update.lines as i64;
    }

    // Native (iTerm2-like): 600 events at 10ms intervals (same wall time).
    for i in 0..600u64 {
        let update = native_state.on_scroll_event_at(
            base + Duration::from_millis(i * 10),
            ScrollDirection::Down,
            native,
        );
        native_lines += update.lines as i64;
    }

    // VS Code should reach at least 60% of native throughput.
    let ratio = vscode_lines as f64 / native_lines as f64;
    assert!(
        ratio >= 0.6,
        "VS Code scroll throughput too low: {vscode_lines} vs native {native_lines} \
         (ratio {ratio:.2}, expected >= 0.6)"
    );
}

#[test]
fn vscode_accel_bands_are_wider_than_default() {
    let vscode = make_vscode_config();
    let default = ScrollConfig::default();
    assert!(vscode.accel_interval_fast_ms > default.accel_interval_fast_ms);
    assert!(vscode.accel_interval_medium_ms > default.accel_interval_medium_ms);
}

#[test]
fn vscode_single_wheel_notch_scrolls_at_least_one_line() {
    let config = ScrollConfig::from_terminal(
        TerminalName::VsCode,
        ScrollConfigOverrides {
            speed_multiplier: Some(1.0),
            ..ScrollConfigOverrides::default()
        },
    );
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut total = state
        .on_scroll_event_at(
            base + Duration::from_millis(1),
            ScrollDirection::Down,
            config,
        )
        .lines;
    total += state
        .on_tick_at(base + Duration::from_millis(1 + STREAM_GAP_MS + 20))
        .lines;

    assert!(
        total >= 1,
        "single VS Code wheel notch must scroll >= 1 line, got {total}"
    );
}

#[test]
fn vscode_slow_isolated_events_still_produce_motion() {
    let config = make_vscode_config();
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut notches_with_motion = 0u32;
    for notch in 0..6u64 {
        let t0 = base + Duration::from_millis(notch * (STREAM_GAP_MS + 50));
        let mut lines = state
            .on_scroll_event_at(t0, ScrollDirection::Down, config)
            .lines;
        lines += state
            .on_tick_at(t0 + Duration::from_millis(STREAM_GAP_MS + 20))
            .lines;
        if lines >= 1 {
            notches_with_motion += 1;
        }
    }

    assert_eq!(
        notches_with_motion, 6,
        "each isolated VS Code wheel event should produce motion"
    );
}

#[test]
fn vscode_xterm_js_spacing_single_stream_scrolls() {
    let config = make_vscode_config();
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut total = state
        .on_scroll_event_at(
            base + Duration::from_millis(1),
            ScrollDirection::Down,
            config,
        )
        .lines;
    total += state
        .on_scroll_event_at(
            base + Duration::from_millis(40),
            ScrollDirection::Down,
            config,
        )
        .lines;
    total += state
        .on_tick_at(base + Duration::from_millis(40 + STREAM_GAP_MS + 20))
        .lines;

    assert!(
        total >= 1,
        "xterm.js-spaced VS Code events must scroll >= 1 line, got {total}"
    );
}

#[test]
fn vscode_embed_family_shares_vscode_scroll_profile() {
    let vscode = make_vscode_config();
    for brand in [TerminalName::Cursor, TerminalName::Windsurf] {
        let cfg = ScrollConfig::from_terminal(brand, ScrollConfigOverrides::default());
        assert_eq!(cfg.trackpad_lines_per_tick, vscode.trackpad_lines_per_tick);
        assert_eq!(cfg.accel_interval_fast_ms, vscode.accel_interval_fast_ms);
        assert_eq!(
            cfg.accel_interval_medium_ms,
            vscode.accel_interval_medium_ms
        );
        assert_eq!(
            cfg.trackpad_detect_max_interval_ms,
            vscode.trackpad_detect_max_interval_ms
        );
        assert_eq!(cfg.events_per_tick, vscode.events_per_tick);
    }
    let zed = ScrollConfig::from_terminal(TerminalName::Zed, ScrollConfigOverrides::default());
    assert_eq!(zed.events_per_tick, 1);
    assert_eq!(zed.trackpad_lines_per_tick, DEFAULT_TRACKPAD_LINES_PER_TICK);
    assert_eq!(zed.accel_interval_fast_ms, ACCEL_INTERVAL_FAST_MS);
}

#[test]
fn cancel_stream_drops_pending_momentum_and_fractional_carry() {
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);
    let config = make_config(3, ScrollInputMode::Trackpad);

    state.on_scroll_event_at(base, ScrollDirection::Up, config);
    state.carry_lines = 0.75;
    state.carry_direction = Some(ScrollDirection::Up);
    assert!(state.has_active_stream());

    state.cancel_stream();

    assert!(!state.has_active_stream());
    assert_eq!(state.carry_lines, 0.0);
    assert_eq!(state.carry_direction, None);
    assert_eq!(state.on_tick_at(base + Duration::from_secs(1)).lines, 0);
}

#[test]
fn wheel_flood_flushes_capped_with_backlog_carry() {
    // Wheel used to be uncapped, so a flood flushed its whole backlog in one slot. Every flush must respect the proportional cap.
    let config = make_config(3, ScrollInputMode::Auto).with_viewport_height(20);
    let cap = config.flush_cap();
    assert_eq!(cap, 10, "fixture: viewport 20 must yield cap 10");
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut at = base;
    let mut flushes: Vec<i32> = Vec::new();
    for i in 0..30u64 {
        at = base + Duration::from_millis(1 + i);
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        if update.lines != 0 {
            flushes.push(update.lines);
        }
    }
    assert_eq!(
        state.stream.as_ref().map(|s| s.kind),
        Some(ScrollStreamKind::Wheel),
        "fixture: 30 events at 1ms must promote to Wheel"
    );

    let mut post_burst_flushes = 0usize;
    for (_, lines) in drive_suggested_ticks(&mut state, at) {
        if lines != 0 {
            post_burst_flushes += 1;
            flushes.push(lines);
        }
    }
    assert!(
        flushes.iter().all(|&lines| lines <= cap),
        "a wheel flood must never flush past the cap ({cap}), got {flushes:?}"
    );
    assert!(
        flushes.contains(&cap),
        "fixture: the flood must actually engage the cap, got {flushes:?}"
    );
    assert!(
        post_burst_flushes >= 2,
        "cap overflow must carry into later cadence slots, got \
         {post_burst_flushes} post-burst flushes ({flushes:?})"
    );
    assert_eq!(
        flushes.iter().sum::<i32>(),
        30,
        "the capped backlog must still deliver every line ({flushes:?})"
    );
}

#[test]
fn unclassified_flood_on_ept3_capped_not_teleported() {
    // Unknown-path cap: an ept=3 stream that misses the 12ms wheel promotion window never classifies mid-stream
    // The old code flushed it uncapped: the exact misclassified-trackpad flood (a brand the table calls ept=3 whose trackpad never promotes)
    // Three spaced events pin kind at Unknown, then a 1ms flood must stay under the proportional cap per flush
    let config = make_config(3, ScrollInputMode::Auto).with_viewport_height(20);
    let cap = config.flush_cap();
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    let mut at = base;
    let mut flushes: Vec<i32> = Vec::new();
    // 3 events at 25ms: the third lands 50ms after start, past the 12ms promotion window, so the stream can never become Wheel
    // The interval window sits at base weight, so the finalize reprice does not change the total
    // Then a 37-event flood at 1ms follows
    for i in 0..40u64 {
        at = base + Duration::from_millis(if i < 3 { 1 + i * 25 } else { 49 + i });
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        if update.lines != 0 {
            flushes.push(update.lines);
        }
    }
    {
        let stream = state.stream.as_ref().expect("stream active after flood");
        assert_eq!(
            stream.kind,
            ScrollStreamKind::Unknown,
            "fixture: the flood must stay unclassified mid-stream"
        );
        assert!(!stream.is_confirmed_trackpad());
    }
    for (_, lines) in drive_suggested_ticks(&mut state, at) {
        if lines != 0 {
            flushes.push(lines);
        }
    }
    assert!(
        flushes.iter().all(|&lines| lines <= cap),
        "an unclassified flood must never flush past the cap ({cap}), got {flushes:?}"
    );
    assert!(
        flushes.contains(&cap),
        "fixture: the flood must actually engage the cap, got {flushes:?}"
    );
    assert_eq!(
        flushes.iter().sum::<i32>(),
        40,
        "the capped backlog must still deliver every line ({flushes:?})"
    );
}

#[test]
fn legit_ept1_wheel_notches_never_hit_the_cap() {
    // ept=1 wheel has no accel, so a flush covers only notches since the last slot — far under the cap. Every notch must arrive intact.
    let config = ScrollConfig::from_terminal(TerminalName::Iterm2, Default::default())
        .with_viewport_height(20);
    assert_eq!(config.events_per_tick, 1);
    assert_eq!(config.wheel_lines_per_tick, 1);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // 10 notches at 40ms: above the 30ms ept=1 trackpad-detect window, so the stream stays wheel-like throughout
    let mut at = base;
    let mut total = 0i32;
    for i in 0..10u64 {
        at = base + Duration::from_millis(1 + i * 40);
        let update = state.on_scroll_event_at(at, ScrollDirection::Down, config);
        assert!(
            update.lines < MIN_DELTA_PER_FLUSH,
            "a legit ept=1 notch flush ({}) must stay under the floor cap",
            update.lines
        );
        total += update.lines;
    }
    for (_, lines) in drive_suggested_ticks(&mut state, at) {
        assert!(lines < MIN_DELTA_PER_FLUSH);
        total += lines;
    }
    assert_eq!(total, 10, "every notch delivers exactly one line");
}

#[test]
fn ghostty_duplicate_reports_do_not_feed_accel_banding() {
    // Ghostty emits at least two SGR reports per physical notch, ~4ms apart
    // The old code fed those 4ms gaps into the interval window, reading a human 25ms notch cadence as max-velocity scrolling
    // Duplicates must be excluded from banding (accel equals the same stream with duplicates removed) while still counting for line accumulation
    let config = ScrollConfig::from_terminal(TerminalName::Ghostty, Default::default());
    let base = Instant::now();

    // Stream A: 5 notches at 25ms, each doubled 4ms later (10 events).
    let mut dup_state = MouseScrollState::new_at(base);
    for notch in 0..5u64 {
        for offset in [0u64, 4] {
            let at = base + Duration::from_millis(1 + notch * 25 + offset);
            let _ = dup_state.on_scroll_event_at(at, ScrollDirection::Down, config);
        }
    }
    // Stream B: the same 5 notches with the duplicates removed.
    let mut dedup_state = MouseScrollState::new_at(base);
    for notch in 0..5u64 {
        let at = base + Duration::from_millis(1 + notch * 25);
        let _ = dedup_state.on_scroll_event_at(at, ScrollDirection::Down, config);
    }

    let dup = dup_state.stream.as_ref().expect("dup stream active");
    let dedup = dedup_state.stream.as_ref().expect("dedup stream active");
    assert!(
        dup.interval_history
            .iter()
            .all(|&ms| ms >= ACCEL_MIN_INTERVAL_MS),
        "sub-6ms duplicate gaps must not enter the interval window, got {:?}",
        dup.interval_history
    );
    assert_eq!(
        dup.interval_accel(),
        dedup.interval_accel(),
        "banding must equal the duplicate-free stream (parent read the \
         4ms gaps as the fast band)"
    );
    assert_eq!(dup.interval_accel(), ACCEL_MULTIPLIER_BASE);
    // Line accumulation still counts both reports of every pair.
    assert_eq!(dup.accumulated_events, 10);
    assert_eq!(dedup.accumulated_events, 5);
}

#[test]
fn multiplexed_sessions_use_conservative_profile_regardless_of_brand() {
    // tmux/screen/zellij/herdr re-encode mouse into their own SGR stream, so the outer brand's ept/pacing calibration is wrong under them
    // The conservative ept=1 shape applies no matter the brand
    // Cmux is a passthrough and Undetected means no multiplexer; both keep the brand profile byte-identical to `from_terminal`
    let brands = [
        TerminalName::Ghostty,
        TerminalName::Iterm2,
        TerminalName::AppleTerminal,
        TerminalName::WezTerm,
        TerminalName::Kitty,
        TerminalName::VsCode,
        TerminalName::Cursor,
        TerminalName::Zed,
        TerminalName::Unknown,
    ];
    let reference = ScrollConfig::from_terminal_context(
        TerminalName::Unknown,
        MultiplexerKind::Tmux,
        Default::default(),
    );
    for mux in [
        MultiplexerKind::Tmux,
        MultiplexerKind::Screen,
        MultiplexerKind::Zellij,
        MultiplexerKind::Herdr,
    ] {
        for brand in brands {
            let cfg = ScrollConfig::from_terminal_context(brand, mux, Default::default());
            assert_eq!(cfg.events_per_tick, 1, "{brand:?} under {mux:?}");
            assert_eq!(cfg.wheel_lines_per_tick, 1, "{brand:?} under {mux:?}");
            assert_eq!(
                format!("{cfg:?}"),
                format!("{reference:?}"),
                "conservative profile must be brand-independent \
                 ({brand:?} under {mux:?})"
            );
        }
    }
    for mux in [MultiplexerKind::Undetected, MultiplexerKind::Cmux] {
        for brand in brands {
            let cfg = ScrollConfig::from_terminal_context(brand, mux, Default::default());
            let plain = ScrollConfig::from_terminal(brand, Default::default());
            assert_eq!(
                format!("{cfg:?}"),
                format!("{plain:?}"),
                "{brand:?} under {mux:?} must keep the brand profile"
            );
        }
    }
    // User overrides still beat the conservative profile.
    let overridden = ScrollConfig::from_terminal_context(
        TerminalName::Ghostty,
        MultiplexerKind::Tmux,
        ScrollConfigOverrides {
            events_per_tick: Some(5),
            ..Default::default()
        },
    );
    assert_eq!(overridden.events_per_tick, 5);
}

// ── User-facing scroll settings (scroll_mode / invert_scroll /
//    scroll_lines) — override plumbing + forced-mode pricing ──────────────

/// The settings caches assemble into overrides and reach the config through `from_terminal_context`.
/// That covers forced mode, inverted direction, and the single `scroll_lines` knob overriding BOTH per-tick values.
#[test]
fn settings_cache_overrides_reach_scroll_config() {
    // Thread-local caches: sets are visible only to this test's thread.
    crate::appearance::cache::set_scroll_speed(50);
    crate::appearance::cache::set_scroll_mode(crate::appearance::ScrollMode::Wheel);
    crate::appearance::cache::set_invert_scroll(true);
    crate::appearance::cache::set_scroll_lines(2);

    let overrides = ScrollConfigOverrides::from_settings_caches();
    assert_eq!(overrides.mode, Some(ScrollInputMode::Wheel));
    assert!(overrides.invert_direction);
    assert_eq!(overrides.wheel_lines_per_tick, Some(2));
    assert_eq!(overrides.trackpad_lines_per_tick, Some(2));
    assert_eq!(overrides.speed_multiplier, Some(1.0));

    let config = ScrollConfig::from_terminal_context(
        TerminalName::VsCode,
        MultiplexerKind::Undetected,
        overrides,
    );
    assert_eq!(config.mode, ScrollInputMode::Wheel);
    assert!(config.invert_direction);
    // One knob, both paths: beats VS Code's wheel=3 / trackpad=15 profile.
    assert_eq!(config.wheel_lines_per_tick, 2);
    assert_eq!(config.trackpad_lines_per_tick, 2);

    // Auto mode with unset lines expresses no opinion: the profile stays in charge
    crate::appearance::cache::set_scroll_mode(crate::appearance::ScrollMode::Auto);
    crate::appearance::cache::set_invert_scroll(false);
    let neutral = ScrollConfigOverrides {
        wheel_lines_per_tick: None,
        trackpad_lines_per_tick: None,
        ..ScrollConfigOverrides::from_settings_caches()
    };
    let config = ScrollConfig::from_terminal_context(
        TerminalName::VsCode,
        MultiplexerKind::Undetected,
        neutral,
    );
    assert_eq!(config.mode, ScrollInputMode::Auto);
    assert!(!config.invert_direction);
    assert_eq!(config.wheel_lines_per_tick, DEFAULT_WHEEL_LINES_PER_TICK);
    assert_eq!(config.trackpad_lines_per_tick, 15);
}

/// `scroll_lines` unset keeps every profile's own values; set, it overrides both paths on any brand (from_terminal_context is the only prod path).
#[test]
fn scroll_lines_override_beats_profile_and_unset_keeps_it() {
    let unset = ScrollConfig::from_terminal(TerminalName::VsCode, Default::default());
    assert_eq!(unset.wheel_lines_per_tick, DEFAULT_WHEEL_LINES_PER_TICK);
    assert_eq!(unset.trackpad_lines_per_tick, 15);

    let set = ScrollConfig::from_terminal(
        TerminalName::VsCode,
        ScrollConfigOverrides {
            wheel_lines_per_tick: Some(4),
            trackpad_lines_per_tick: Some(4),
            ..Default::default()
        },
    );
    assert_eq!(set.wheel_lines_per_tick, 4);
    assert_eq!(set.trackpad_lines_per_tick, 4);
}

/// Forced-wheel prices at exact wheel rates. Finalize reclassification must not add demand, so Auto equals that total on this shape.
/// Live accel still applies to streams confirmed trackpad mid-stream.
#[test]
fn forced_wheel_mode_prices_flood_as_wheel_regardless_of_timing() {
    let run = |mode: Option<ScrollInputMode>| -> i32 {
        let config = ScrollConfig::from_terminal(
            TerminalName::Unknown,
            ScrollConfigOverrides {
                mode,
                ..Default::default()
            },
        );
        let base = Instant::now();
        let mut state = MouseScrollState::new_at(base);
        let mut total = 0;
        let mut at = base;
        for i in 0..30u64 {
            at = base + Duration::from_millis(1 + i * 8);
            total += state
                .on_scroll_event_at(at, ScrollDirection::Down, config)
                .lines;
        }
        for (_, lines) in drive_suggested_ticks(&mut state, at) {
            total += lines;
        }
        assert!(!state.has_active_stream(), "stream must finalize");
        total
    };

    let forced = run(Some(ScrollInputMode::Wheel));
    assert_eq!(
        forced, 30,
        "forced wheel: 30 events / ept 3 x 3 lines x speed 1.0 = exactly 30"
    );

    let auto = run(None);
    assert_eq!(
        auto, forced,
        "an Unknown-priced Auto gesture must deliver its mid-stream total \
         exactly — a higher Auto total means the finalize re-price burst \
         (the end-of-gesture jerk) is back"
    );
}

/// `invert_direction` flips the sign end-to-end through `on_scroll_event`.
/// A physical scroll-down burst lands as upward line deltas of the same magnitude as the non-inverted run.
#[test]
fn invert_direction_flips_sign_end_to_end() {
    let run = |invert: bool| -> i32 {
        let config = ScrollConfig::from_terminal(
            TerminalName::Unknown,
            ScrollConfigOverrides {
                invert_direction: invert,
                ..Default::default()
            },
        );
        let base = Instant::now();
        let mut state = MouseScrollState::new_at(base);
        let mut total = 0;
        let mut at = base;
        for i in 0..6u64 {
            at = base + Duration::from_millis(1 + i * 8);
            total += state
                .on_scroll_event_at(at, ScrollDirection::Down, config)
                .lines;
        }
        for (_, lines) in drive_suggested_ticks(&mut state, at) {
            total += lines;
        }
        total
    };

    let normal = run(false);
    let inverted = run(true);
    assert!(normal > 0, "fixture: scroll-down must move down normally");
    assert_eq!(
        inverted, -normal,
        "inverted run must mirror the normal run's magnitude with flipped sign"
    );
}

/// `debug_snapshot` follows the stream from idle through live flood to the finalized breadcrumb and echoes the config in effect.
/// It does not mutate state: consecutive snapshots at the same `now` are identical.
#[test]
fn debug_snapshot_tracks_stream_lifecycle_without_mutating() {
    // ept=1 Auto so the live trackpad promotion fires (>2 events with avg interval < 30ms); viewport 40 pins the cap echo at 40/2 = 20
    let config = make_config(1, ScrollInputMode::Auto).with_viewport_height(40);
    let base = Instant::now();
    let mut state = MouseScrollState::new_at(base);

    // Idle: no stream, no breadcrumb; echo comes from the passed config.
    let idle = state.debug_snapshot(&config, base);
    assert!(idle.stream.is_none() && idle.last_stream.is_none());
    assert_eq!(idle.viewport_height, 40);
    assert_eq!(idle.flush_cap, 20);
    assert_eq!(idle.mode.label(), "auto");
    assert!(idle.next_deadline_ms.is_none(), "no stream, clock disarmed");

    // Trackpad flood: 12 events at 8ms.
    let mut at = base;
    for i in 0..12u64 {
        at = base + Duration::from_millis(1 + i * 8);
        let _ = state.on_scroll_event_at(at, ScrollDirection::Down, config);
    }

    let live = state.debug_snapshot(&config, at);
    assert_eq!(
        live,
        state.debug_snapshot(&config, at),
        "consecutive snapshots must be identical (read-only contract)"
    );
    let stream = live.stream.expect("stream live during the flood");
    assert_eq!(stream.kind, "trackpad");
    assert!(stream.promoted, "ept=1 fast flood promotes mid-stream");
    assert_eq!(stream.events, 12);
    assert!(
        stream.applied_lines > 0,
        "cadence flushes already delivered"
    );
    assert!(stream.backlog >= 0);
    assert!(stream.desired_lines >= stream.applied_lines as f32);
    assert_eq!(live.flush_cap, 20, "stream captured the stamped viewport");
    assert!(
        live.next_deadline_ms.is_some(),
        "active stream arms the clock"
    );

    // Past the 80ms gap the stream finalizes into the breadcrumb.
    let _ = drive_suggested_ticks(&mut state, at);
    let done = state.debug_snapshot(&config, at + Duration::from_millis(500));
    assert!(done.stream.is_none());
    assert!(done.next_deadline_ms.is_none());
    let last = done.last_stream.expect("finalize records the breadcrumb");
    assert_eq!(last.kind, "trackpad");
    assert_eq!(last.events, 12);
    assert!(last.applied_lines > 0);
}

#[test]
fn scroll_log_records_flood_flushes_and_capped_finalize_drop() {
    // GROK_SCROLL_LOG: a trackpad flood on the synthetic clock must produce parseable JSONL ordered stream_start, then flushes, then finalize
    // ts_ms must sit on the state machine's own timeline, and the finalize's flushed/dropped/backlog_after must be mutually consistent
    // Recording must not change delivered lines (pure-observation invariant)
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scroll-log.jsonl");
    let mut state = MouseScrollState::new_at(base);
    state.recorder = Some(ScrollLogRecorder::new(path.clone(), base));
    // A second state without a recorder, driven identically, proves recording does not change delivered lines
    let mut mirror = MouseScrollState::new_at(base);

    // 50 events at 2ms: sub-6ms spacing is accel-excluded (multiplier stays 1.0), so desired is exactly 50.0 lines
    // The 16ms cadence flushes deliver at most the 6-line floor cap, so the backlog grows
    let mut at = base;
    let mut delivered = 0;
    let mut mirrored = 0;
    for i in 0..50u64 {
        at = base + Duration::from_millis(i * 2);
        delivered += state
            .on_scroll_event_at(at, ScrollDirection::Down, config)
            .lines;
        mirrored += mirror
            .on_scroll_event_at(at, ScrollDirection::Down, config)
            .lines;
    }
    // Starved clock: drain tapered first; finalize `dropped` is written-off flood excess, not a burst.
    let mut final_at = at + Duration::from_millis(81);
    delivered += state.on_tick_at(final_at).lines;
    mirrored += mirror.on_tick_at(final_at).lines;
    for _ in 0..10 {
        if !state.has_active_stream() {
            break;
        }
        final_at += REDRAW_CADENCE;
        delivered += state.on_tick_at(final_at).lines;
        mirrored += mirror.on_tick_at(final_at).lines;
    }
    assert!(!state.has_active_stream());
    assert_eq!(delivered, mirrored, "recording must not change scrolling");

    // The finalize record flushed the BufWriter: the file is readable while the recorder is still alive (the tail -f contract)
    let raw = std::fs::read_to_string(&path).expect("log readable mid-session");
    let records: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line parses as JSON"))
        .collect();
    assert!(
        records.len() >= 3,
        "start + >=1 flush + finalize, got {records:?}"
    );

    // Ordering: stream_start first, finalize last, only flushes between.
    let Some(start) = records.first() else {
        panic!("expected records: {records:?}");
    };
    assert_eq!(
        start.get("evt").and_then(|v| v.as_str()),
        Some("stream_start")
    );
    assert_eq!(start.get("trigger").and_then(|v| v.as_str()), Some("event"));
    assert_eq!(start.get("events_total"), Some(&serde_json::json!(0)));
    // Config echo rides stream_start only, matching the synthetic config.
    assert_eq!(start.get("mode").and_then(|v| v.as_str()), Some("trackpad"));
    assert_eq!(start.get("ept"), Some(&serde_json::json!(3)));
    assert_eq!(start.get("wheel_lpt"), Some(&serde_json::json!(3)));
    assert_eq!(start.get("trackpad_lpt"), Some(&serde_json::json!(3)));
    assert_eq!(start.get("invert"), Some(&serde_json::json!(false)));
    assert_eq!(start.get("speed"), Some(&serde_json::json!(1.0)));
    assert_eq!(start.get("viewport_height"), Some(&serde_json::json!(0)));
    let last = records.last().expect("nonempty");
    assert!(
        records.get(1).is_some_and(|r| r.get("ept").is_none()),
        "flushes skip the echo"
    );
    assert!(last.get("mode").is_none(), "finalize skips the echo");
    assert_eq!(last.get("evt").and_then(|v| v.as_str()), Some("finalize"));
    assert_eq!(
        last.get("trigger").and_then(|v| v.as_str()),
        Some("finalize")
    );
    let mid = records
        .len()
        .checked_sub(1)
        .and_then(|end| records.get(1..end))
        .unwrap_or(&[]);
    for flush in mid {
        assert_eq!(flush.get("evt").and_then(|v| v.as_str()), Some("flush"));
        // In-burst flushes ride the event path; the post-gap drain flushes ride the tick path (they replace the old finalize burst)
        let trigger = flush.get("trigger").and_then(|v| v.as_str());
        assert!(
            trigger == Some("event") || trigger == Some("tick"),
            "unexpected flush trigger: {flush}"
        );
        assert!(
            flush
                .get("flushed")
                .and_then(|v| v.as_i64())
                .is_some_and(|n| n != 0),
            "zero-delta flushes are not logged"
        );
    }
    let drain_flushes: Vec<i64> = mid
        .iter()
        .filter(|r| r.get("trigger").and_then(|v| v.as_str()) == Some("tick"))
        .map(|r| r.get("flushed").and_then(|v| v.as_i64()).expect("flushed"))
        .collect();
    assert!(
        !drain_flushes.is_empty(),
        "the starved flood must drain over tick flushes before finalizing"
    );
    assert!(
        drain_flushes
            .windows(2)
            .all(|w| matches!(w, [a, b] if a >= b)),
        "drain flushes must decelerate (non-increasing), got {drain_flushes:?}"
    );

    // ts_ms is the synthetic timeline: monotone, finalize at the tick's exact offset
    let ts: Vec<f64> = records
        .iter()
        .map(|r| {
            r.get("ts_ms")
                .and_then(|v| v.as_f64())
                .expect("ts_ms is a number")
        })
        .collect();
    assert!(
        ts.windows(2).all(|w| matches!(w, [a, b] if a <= b)),
        "ts_ms monotone: {ts:?}"
    );
    let expected_ms = final_at.duration_since(base).as_secs_f64() * 1000.0;
    assert!((ts.last().expect("nonempty") - expected_ms).abs() < 1e-6);

    // Finalize consistency: the finalize flushes nothing because the drain already ran dry or the coast budget wrote the rest off
    // `dropped` is exactly the whole-line backlog the budget declined
    let cap = last.get("cap").and_then(|v| v.as_i64()).expect("cap");
    let flushed = last
        .get("flushed")
        .and_then(|v| v.as_i64())
        .expect("flushed");
    let dropped = last
        .get("dropped")
        .and_then(|v| v.as_i64())
        .expect("dropped");
    let backlog_after = last
        .get("backlog_after")
        .and_then(|v| v.as_i64())
        .expect("backlog_after");
    let desired = last
        .get("desired")
        .and_then(|v| v.as_f64())
        .expect("desired");
    let applied_total = last
        .get("applied_total")
        .and_then(|v| v.as_i64())
        .expect("applied_total");
    assert_eq!(cap, 6, "unstamped viewport floors the cap");
    assert_eq!(
        flushed, 0,
        "the finalize burst is gone: nothing flushable may remain"
    );
    assert!(
        dropped > 0,
        "flood excess beyond the coast budget is dropped"
    );
    assert_eq!(dropped, backlog_after);
    assert_eq!(dropped, desired.trunc() as i64 - applied_total);
    assert_eq!(last.get("kind").and_then(|v| v.as_str()), Some("trackpad"));
    assert_eq!(last.get("events_total"), Some(&serde_json::json!(50)));

    // Per-stream event accounting: since-flush counts partition the total.
    let since_sum: u64 = records
        .iter()
        .map(|r| {
            r.get("events_since_flush")
                .and_then(|v| v.as_u64())
                .expect("count")
        })
        .sum();
    assert_eq!(since_sum, 50);
    assert!(
        records
            .first()
            .is_some_and(|r| r.get("ms_since_prev_flush").is_none()),
        "no flush precedes the first record"
    );
    assert!(
        last.get("ms_since_prev_flush")
            .and_then(|v| v.as_f64())
            .expect("spacing")
            > 0.0
    );
}

/// Hardcoded JSON keys, not shared with the serializer, so a rename cannot update both sides silently. No harness dependency.
#[test]
fn scroll_log_wire_format_matches_harness_required_field_set() {
    // Always-emitted fields the harness parser requires on every record.
    const REQUIRED_KEYS: &[&str] = &[
        "ts_ms",
        "evt",
        "trigger",
        "kind",
        "events_total",
        "events_since_flush",
        "accel",
        "desired",
        "applied_total",
        "flushed",
        "backlog_after",
        "carry",
        "cap",
    ];
    // Skip-if-None bookkeeping fields the harness parser knows as Options.
    const OPTIONAL_KEYS: &[&str] = &["avg_interval_ms", "ms_since_prev_flush", "dropped"];
    // Config echo flattened onto stream_start records only.
    const CONFIG_ECHO_KEYS: &[&str] = &[
        "mode",
        "ept",
        "wheel_lpt",
        "trackpad_lpt",
        "invert",
        "speed",
        "viewport_height",
    ];

    // Same trackpad-flood drive as the recorder test above: 50 events at 2ms build a capped backlog (at least one nonzero mid-stream flush)
    // Ticks then drain to the finalize with dropped > 0, so there is one record of each evt
    let config = make_config(3, ScrollInputMode::Trackpad);
    let base = Instant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scroll-log.jsonl");
    let mut state = MouseScrollState::new_at(base);
    state.recorder = Some(ScrollLogRecorder::new(path.clone(), base));
    let mut at = base;
    for i in 0..50u64 {
        at = base + Duration::from_millis(i * 2);
        let _ = state.on_scroll_event_at(at, ScrollDirection::Down, config);
    }
    // The post-gap drain runs before the finalize, so tick until the stream actually ends
    let mut tick_at = at + Duration::from_millis(81);
    let _ = state.on_tick_at(tick_at);
    for _ in 0..10 {
        if !state.has_active_stream() {
            break;
        }
        tick_at += REDRAW_CADENCE;
        let _ = state.on_tick_at(tick_at);
    }
    assert!(
        !state.has_active_stream(),
        "fixture must reach the finalize"
    );

    let raw = std::fs::read_to_string(&path).expect("log readable after finalize flush");
    let records: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line parses as JSON"))
        .collect();
    let evts: Vec<&str> = records
        .iter()
        .map(|r| {
            r.get("evt")
                .and_then(|v| v.as_str())
                .expect("evt is a string")
        })
        .collect();
    for expected in ["stream_start", "flush", "finalize"] {
        assert!(
            evts.contains(&expected),
            "fixture must produce one record of each evt, got {evts:?}"
        );
    }

    for record in &records {
        let obj = record.as_object().expect("records are flat JSON objects");
        let evt = record.get("evt").and_then(|v| v.as_str()).expect("evt");

        // Every record carries the full harness-required key set
        for key in REQUIRED_KEYS {
            assert!(
                obj.contains_key(*key),
                "{evt} record missing harness-required key {key}: {record}"
            );
        }
        // The values carry the JSON types the harness parser declares
        for key in [
            "ts_ms",
            "events_total",
            "events_since_flush",
            "accel",
            "desired",
            "applied_total",
            "flushed",
            "backlog_after",
            "carry",
            "cap",
        ] {
            assert!(
                record.get(key).is_some_and(|v| v.is_number()),
                "{evt} key {key} must be a JSON number: {record}"
            );
        }
        for key in ["evt", "trigger", "kind"] {
            assert!(
                record.get(key).is_some_and(|v| v.is_string()),
                "{evt} key {key} must be a JSON string: {record}"
            );
        }
        // No keys unknown to the harness schema
        // An additive field won't break the harness (unknown fields tolerated there) but must be a conscious schema change
        // Extend `ScrollLogLine` and this list
        for key in obj.keys() {
            assert!(
                REQUIRED_KEYS.contains(&key.as_str())
                    || OPTIONAL_KEYS.contains(&key.as_str())
                    || CONFIG_ECHO_KEYS.contains(&key.as_str()),
                "{evt} record carries a key unknown to the harness schema: {key} in {record}"
            );
        }

        // Optional-group placement per evt (the harness relies on the config echo riding stream_start and dropped riding finalize)
        match evt {
            "stream_start" => {
                for key in CONFIG_ECHO_KEYS {
                    assert!(
                        obj.contains_key(*key),
                        "stream_start missing config-echo key {key}: {record}"
                    );
                }
                assert!(
                    !obj.contains_key("dropped"),
                    "dropped is finalize-only: {record}"
                );
            }
            "flush" => {
                for key in CONFIG_ECHO_KEYS {
                    assert!(
                        !obj.contains_key(*key),
                        "config echo is stream_start-only: {record}"
                    );
                }
                assert!(
                    !obj.contains_key("dropped"),
                    "dropped is finalize-only: {record}"
                );
            }
            "finalize" => {
                for key in CONFIG_ECHO_KEYS {
                    assert!(
                        !obj.contains_key(*key),
                        "config echo is stream_start-only: {record}"
                    );
                }
                assert!(
                    obj.contains_key("dropped"),
                    "flood finalize must carry dropped: {record}"
                );
            }
            other => panic!("unknown evt value {other:?} in {record}"),
        }
    }
}

/// `/debug log` runtime toggle: enable builds a recorder (lazy-open, no file yet) targeting the default timestamped path; disable drops it.
#[test]
fn toggle_scroll_log_round_trips_without_env() {
    // new_at never reads the env, so the recorder starts absent.
    let mut state = MouseScrollState::new_at(Instant::now());
    assert!(!state.scroll_log_active());

    let path = state
        .toggle_scroll_log()
        .expect("enabling must return the log path");
    assert!(state.scroll_log_active());
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("jsonl"));

    assert!(
        state.toggle_scroll_log().is_none(),
        "disabling must return None"
    );
    assert!(!state.scroll_log_active());
}

/// Replay of a glide whose finalize re-priced Unknown accel-weighted and burst after the fingers stopped.
/// Must deliver the mid-stream total with no finalize drop; later motion decelerates and sums to at most one cap.
#[test]
fn real_session_glide_ends_without_finalize_burst_or_drop() {
    let config = make_config(3, ScrollInputMode::Auto).with_viewport_height(41);
    assert_eq!(config.flush_cap(), 20, "fixture: the session's cap echo");
    let base = Instant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scroll-log.jsonl");
    let mut state = MouseScrollState::new_at(base);
    state.recorder = Some(ScrollLogRecorder::new(path.clone(), base));

    // Events-per-flush counts as captured (54 events over ~407ms): dense middle at up to 4 events per 16.6ms slot, decelerating 1-event tail
    // Within a slot the terminal batches events ~4ms apart (sub-6ms spacing stays out of the accel window, exactly like the real capture)
    const EVENTS_PER_SLOT: &[u64] = &[
        1, 1, 2, 2, 4, 1, 3, 4, 4, 4, 4, 4, 4, 4, 4, 1, 1, 1, 1, 1, 1, 1, 1,
    ];
    const SLOT_MS: f64 = 16.6;
    assert_eq!(EVENTS_PER_SLOT.iter().sum::<u64>(), 54);

    let mut delivered_during_input = 0i64;
    let mut last_event_at = base;
    let mut event_flushes: Vec<i32> = Vec::new();
    for (slot, &events) in EVENTS_PER_SLOT.iter().enumerate() {
        let slot_start = base + Duration::from_micros((slot as f64 * SLOT_MS * 1000.0) as u64);
        for event in 0..events {
            last_event_at = slot_start + Duration::from_millis(event * 4);
            let update = state.on_scroll_event_at(last_event_at, ScrollDirection::Down, config);
            delivered_during_input += update.lines as i64;
            if update.lines != 0 {
                event_flushes.push(update.lines);
            }
        }
    }
    assert!(
        event_flushes.iter().all(|&lines| (1..=4).contains(&lines)),
        "fixture: the glide must flush 1-4 lines per slot as captured, got \
         {event_flushes:?}"
    );

    // Drive the post-input phase by the state machine's own deadlines.
    let tail: Vec<i32> = drive_suggested_ticks(&mut state, last_event_at)
        .into_iter()
        .map(|(_, lines)| lines)
        .filter(|&lines| lines != 0)
        .collect();
    assert!(!state.has_active_stream(), "gesture must finalize");

    let tail_total = tail.iter().map(|&l| i64::from(l)).sum::<i64>();
    // Old behavior delivered 74 (54 glide + a 20-line finalize burst) and dropped 47; the fix delivers the mid-stream total exactly
    assert_eq!(
        delivered_during_input + tail_total,
        54,
        "the gesture must travel exactly its glide total — more means the \
         finalize burst is back, less is under-travel"
    );
    assert!(
        tail_total <= i64::from(config.flush_cap()),
        "post-input motion must fit one cap, got {tail:?}"
    );
    assert!(
        tail.windows(2).all(|w| matches!(w, [a, b] if a >= b)),
        "post-input flushes must decelerate (non-increasing), got {tail:?}"
    );

    // The recorder's finalize line must show no burst (flushed 0 after the drain) and nothing dropped
    let raw = std::fs::read_to_string(&path).expect("finalize flushed the log");
    let last: serde_json::Value =
        serde_json::from_str(raw.lines().last().expect("nonempty")).expect("parses");
    assert_eq!(last.get("evt").and_then(|v| v.as_str()), Some("finalize"));
    assert_eq!(
        last.get("dropped"),
        Some(&serde_json::json!(0)),
        "the 47-line drop class must be gone"
    );
    assert_eq!(
        last.get("flushed"),
        Some(&serde_json::json!(0)),
        "the finalize no longer bursts a catch-up flush"
    );
    assert_eq!(last.get("events_total"), Some(&serde_json::json!(54)));
}
