//! Clipboard-image tip trigger: while the terminal is focused and the active agent is image-eligible, hint that ctrl+v pastes an image.
//! The hint covers an image already sitting on the pasteboard, without waiting for a focus switch.
//!
//! Trigger model: opportunistic, focus-scoped polling.
//! The caller drives [`ClipboardFocusTipState::poll`] only from event-loop iterations that already run for another reason.
//! Those are input, FocusGained, resize, or an animation tick.
//! Nothing schedules a wakeup and the tip never forces animation, so an idle/hibernating/unfocused app polls zero times.
//! Each in-window poll is throttled to one cheap `changeCount` read per [`POLL_INTERVAL`].
//! The heavier type classification runs ONLY on a changeCount delta.
//! Frequency is further capped by a fire cooldown plus a changeCount dedup (the same copied content never re-fires), not a seen-count.
//! The tip is contextual and recurring by design.
//!
//! The state machine takes the clock and BOTH probe steps as inputs.
//! Every transition, including "classify skips an unchanged changeCount", is unit-testable with a fake clock and call-counting probes.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::input::key::KeyShortcut;
use crate::theme::Theme;

/// Ephemeral-tip dedup key for the clipboard-image hint.
pub const CLIPBOARD_IMAGE_TIP_KEY: &str = "clipboard_image_tip";

/// Throttle for the opportunistic pasteboard poll: at most one `changeCount` read per this interval, even when the event loop iterates at ~30fps.
/// The poll runs only on existing loop iterations (it never schedules a tick), so this only caps how often one touches the pasteboard.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Minimum spacing between fires, so copy-heavy workflows aren't nagged on every new image.
const FIRE_COOLDOWN: Duration = Duration::from_secs(30);

/// Paste chord for the tip copy: always `ctrl+v`.
/// Most macOS terminal emulators capture Cmd by default and don't forward Cmd+V to a raw-mode TUI, so Ctrl+V is the chord actually delivered.
/// Derived from the real binding (not a literal), Ctrl+V being one of the two chords [`crate::input::key::is_paste_key`] accepts, so it can't drift.
fn paste_label() -> String {
    KeyShortcut::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
        .display()
        .to_ascii_lowercase()
}

/// Build the "Image in clipboard · {chord} to paste" tip.
/// Nothing caps how many times the tip can show; the changeCount dedup and the cooldown already limit frequency.
pub fn clipboard_image_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    // The key chord is styled like the shortcuts bar (bold secondary on dim text)
    let chord = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip::new(
        CLIPBOARD_IMAGE_TIP_KEY,
        Line::from(vec![
            Span::styled("Image in clipboard · ", dim),
            Span::styled(paste_label(), chord),
            Span::styled(" to paste", dim),
        ]),
    )
}

/// Result of one pasteboard classification pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Pasteboard change count at probe time (`None` when unavailable).
    pub change_count: Option<u64>,
    /// Whether a *pasteable* image was advertised: raster types with no file-URL types alongside.
    /// File-manager copies (Finder) put a file-icon raster on the board next to the file URLs, but ctrl+v routes those through path handling.
    /// So they must not fire a tip promising an image paste (see `clipboard_image_snapshot`).
    pub has_image: bool,
}

/// The `classify` step: the heavier native probe in one pasteboard pass, the changeCount plus the advertised type
/// list (no bytes, no subprocess). The throttled poll reaches here ONLY on a changeCount delta. the cheap
/// changeCount-only read gates it.
pub fn run_clipboard_check() -> CheckOutcome {
    let (change_count, has_image) = crate::clipboard::clipboard_image_snapshot();
    CheckOutcome {
        change_count,
        has_image,
    }
}

/// Pure state machine for the focus-scoped, opportunistically-polled clipboard-image tip. It never schedules
/// itself: the caller drives [`Self::poll`] from event-loop iterations already running for some other reason.
#[derive(Debug, Default)]
pub struct ClipboardFocusTipState {
    /// When the last poll actually read the pasteboard (throttle anchor).
    last_poll_at: Option<Instant>,
    /// changeCount observed by the last cheap read; a differing value is what warrants paying for the type classification.
    last_seen_change_count: Option<u64>,
    /// When the tip last actually showed (cooldown anchor).
    last_fired_at: Option<Instant>,
    /// changeCount of the content that last fired; identical content never fires twice even across long gaps.
    last_fired_change_count: Option<u64>,
}

impl ClipboardFocusTipState {
    /// Throttle gate: at most one poll per [`POLL_INTERVAL`], so a ~30fps loop still reads the pasteboard at most ~once a second.
    /// Pure; does not mutate.
    pub fn due_to_poll(&self, now: Instant) -> bool {
        self.last_poll_at
            .is_none_or(|at| now.duration_since(at) >= POLL_INTERVAL)
    }

    /// Whether `change_count` differs from the one the last cheap read saw.
    /// That is the signal the pasteboard changed and a classification is worth paying for.
    /// A `None` (changeCount unavailable, e.g. AppKit failed to load) is treated as "nothing new" so the cheap path never escalates blindly.
    fn is_new_change_count(&self, change_count: Option<u64>) -> bool {
        change_count.is_some() && change_count != self.last_seen_change_count
    }

    /// `cheap` reads ONLY the pasteboard changeCount (one Obj-C message). A fireable image is deferred to
    /// [`Self::note_fired`] (called only on a landed show). So an image found but not shown re-classifies on the next
    /// poll.
    pub fn poll(
        &mut self,
        now: Instant,
        cheap: impl FnOnce() -> Option<u64>,
        classify: impl FnOnce() -> CheckOutcome,
    ) -> Option<CheckOutcome> {
        if !self.due_to_poll(now) {
            return None;
        }
        self.last_poll_at = Some(now);
        let change_count = cheap();
        if !self.is_new_change_count(change_count) {
            return None;
        }
        let outcome = classify();
        // Commit the classify-dedup now only for non-image content (nothing to show, so it's fully handled)
        // A fireable image waits for `note_fired` so a refused show stays retryable
        if !outcome.has_image {
            self.last_seen_change_count = change_count;
        }
        Some(outcome)
    }

    /// Whether `outcome` warrants showing the tip right now.
    /// Pure check: the caller commits via [`Self::note_fired`] only after the show actually lands, so refused shows never burn the cooldown or dedup.
    pub fn should_fire(&self, outcome: &CheckOutcome, now: Instant) -> bool {
        outcome.has_image
            && !self.in_cooldown(now)
            && (outcome.change_count.is_none()
                || outcome.change_count != self.last_fired_change_count)
    }

    /// Commit a successful (landed) show: anchors the cooldown, records the fired changeCount, and commits the
    /// classify-dedup too. So the same image isn't re-scanned once the cooldown elapses. A refused show (which never
    /// calls this) leaves `last_seen` stale and stays retryable.
    pub fn note_fired(&mut self, outcome: &CheckOutcome, now: Instant) {
        self.last_fired_at = Some(now);
        if outcome.change_count.is_some() {
            self.last_fired_change_count = outcome.change_count;
            self.last_seen_change_count = outcome.change_count;
        }
    }

    /// Whether the fire cooldown is still in effect.
    /// Part of the caller's in-window gate, so during the cooldown the poll touches the pasteboard zero times.
    pub fn in_cooldown(&self, now: Instant) -> bool {
        self.last_fired_at
            .is_some_and(|at| now.duration_since(at) < FIRE_COOLDOWN)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn outcome(change_count: Option<u64>, has_image: bool) -> CheckOutcome {
        CheckOutcome {
            change_count,
            has_image,
        }
    }

    /// Drive a full successful fire through the poll path: a changeCount delta, then classify, should_fire, note_fired.
    fn fire_via_poll(state: &mut ClipboardFocusTipState, now: Instant, change_count: u64) {
        let got = state
            .poll(
                now,
                || Some(change_count),
                || outcome(Some(change_count), true),
            )
            .expect("a changeCount delta should classify");
        assert!(state.should_fire(&got, now));
        state.note_fired(&got, now);
    }

    #[test]
    fn paste_chord_is_ctrl_v() {
        // Always ctrl+v (the chord terminals actually deliver), derived from the real binding so the label can't drift
        assert_eq!(paste_label(), "ctrl+v");
        assert!(crate::input::key::is_paste_key(
            &crossterm::event::KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn clipboard_image_tip_is_not_seen_capped() {
        // Frequency is capped by the changeCount dedup and the cooldown, never a seen-count, so the builder must not opt into the seen gate
        assert!(clipboard_image_tip().session_seen.is_none());
    }

    #[test]
    fn throttle_limits_reads_to_one_per_interval() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let cheap_reads = Cell::new(0u32);

        // First poll reads the cheap changeCount.
        let _ = state.poll(
            t0,
            || {
                cheap_reads.set(cheap_reads.get() + 1);
                Some(1)
            },
            || outcome(Some(1), false),
        );
        assert_eq!(cheap_reads.get(), 1);

        // A second poll within the interval is throttled; no cheap read at all
        let _ = state.poll(
            t0 + Duration::from_millis(500),
            || {
                cheap_reads.set(cheap_reads.get() + 1);
                Some(1)
            },
            || outcome(Some(1), false),
        );
        assert_eq!(cheap_reads.get(), 1, "two polls <1s apart → one read");

        // Once the interval elapses the next poll reads again.
        let _ = state.poll(
            t0 + POLL_INTERVAL,
            || {
                cheap_reads.set(cheap_reads.get() + 1);
                Some(2)
            },
            || outcome(Some(2), false),
        );
        assert_eq!(cheap_reads.get(), 2, "poll resumes after the interval");
    }

    #[test]
    fn unchanged_change_count_skips_classify_and_does_not_fire() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let classify_calls = Cell::new(0u32);

        // First poll: changeCount 5 is new, so classify runs once
        let first = state.poll(
            t0,
            || Some(5),
            || {
                classify_calls.set(classify_calls.get() + 1);
                outcome(Some(5), false)
            },
        );
        assert_eq!(first, Some(outcome(Some(5), false)));
        assert_eq!(classify_calls.get(), 1);

        // Next interval, SAME changeCount: the cheap path returns; the call-counter proves the classify probe was NOT invoked
        let next = state.poll(
            t0 + POLL_INTERVAL,
            || Some(5),
            || {
                classify_calls.set(classify_calls.get() + 1);
                outcome(Some(5), false)
            },
        );
        assert_eq!(next, None, "unchanged changeCount → no outcome");
        assert_eq!(
            classify_calls.get(),
            1,
            "classify must not run on an unchanged changeCount"
        );
    }

    #[test]
    fn change_to_image_classifies_and_fires_once() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let classify_calls = Cell::new(0u32);

        let got = state
            .poll(
                t0,
                || Some(3),
                || {
                    classify_calls.set(classify_calls.get() + 1);
                    outcome(Some(3), true)
                },
            )
            .expect("a changeCount delta classifies");
        assert_eq!(classify_calls.get(), 1);
        assert!(state.should_fire(&got, t0));
        state.note_fired(&got, t0);

        // Same content, past the cooldown, changeCount unchanged: the cheap path short-circuits
        // The classify closure panics if reached, proving the same image never re-classifies or re-fires
        let later = t0 + FIRE_COOLDOWN + Duration::from_secs(1);
        let again = state.poll(
            later,
            || Some(3),
            || panic!("classify must not run for unchanged (deduped) content"),
        );
        assert_eq!(again, None, "same image never re-fires");
    }

    #[test]
    fn refused_show_keeps_retrying_then_dedups_once_landed() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let classify_calls = Cell::new(0u32);

        // Image copied: classify runs and returns a fireable image.
        let got = state
            .poll(
                t0,
                || Some(7),
                || {
                    classify_calls.set(classify_calls.get() + 1);
                    outcome(Some(7), true)
                },
            )
            .expect("a changeCount delta classifies");
        assert_eq!(classify_calls.get(), 1);
        assert!(state.should_fire(&got, t0));

        // Show REFUSED: the caller did NOT call note_fired
        // `poll` must not have advanced `last_seen` for a fireable image, so the same changeCount RE-classifies on the next poll (the retry)
        let t1 = t0 + POLL_INTERVAL;
        let retry = state.poll(
            t1,
            || Some(7),
            || {
                classify_calls.set(classify_calls.get() + 1);
                outcome(Some(7), true)
            },
        );
        assert_eq!(
            retry,
            Some(outcome(Some(7), true)),
            "a refused image must re-classify, not be skipped as 'seen'"
        );
        assert_eq!(
            classify_calls.get(),
            2,
            "classify ran again for the un-shown image"
        );

        // Now the show LANDS: note_fired commits the classify-dedup too, so the same content past the cooldown does NOT re-classify
        let landed = retry.unwrap();
        state.note_fired(&landed, t1);
        let t2 = t1 + FIRE_COOLDOWN + Duration::from_secs(1);
        let after = state.poll(
            t2,
            || Some(7),
            || panic!("a shown image must not re-classify"),
        );
        assert_eq!(
            after, None,
            "a successfully shown image is deduped post-cooldown"
        );
    }

    #[test]
    fn cooldown_blocks_fire_until_elapsed() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        fire_via_poll(&mut state, t0, 1);

        // A different copy mid-cooldown classifies (changeCount changed) but should_fire refuses while the cooldown holds
        let during = t0 + Duration::from_secs(5);
        let o2 = state
            .poll(during, || Some(2), || outcome(Some(2), true))
            .expect("a new changeCount classifies");
        assert!(!state.should_fire(&o2, during), "inside the cooldown");

        // After the cooldown a fresh copy fires again.
        let after = t0 + FIRE_COOLDOWN + Duration::from_secs(1);
        let o3 = state
            .poll(after, || Some(3), || outcome(Some(3), true))
            .expect("a new changeCount classifies");
        assert!(state.should_fire(&o3, after), "cooldown over");
    }

    #[test]
    fn no_image_never_fires() {
        let state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        assert!(!state.should_fire(&outcome(Some(3), false), t0));
    }

    #[test]
    fn refused_show_burns_nothing() {
        let state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let got = outcome(Some(4), true);
        assert!(state.should_fire(&got, t0));
        // Caller could not paint (e.g. modal raced in) and did NOT commit: the same outcome stays fireable and no cooldown started.
        assert!(state.should_fire(&got, t0 + Duration::from_secs(1)));
        assert!(!state.in_cooldown(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn missing_change_count_still_fires_under_cooldown_cap() {
        let mut state = ClipboardFocusTipState::default();
        let t0 = Instant::now();
        let got = outcome(None, true);
        assert!(state.should_fire(&got, t0));
        state.note_fired(&got, t0);
        assert!(
            !state.should_fire(&got, t0 + Duration::from_secs(1)),
            "cooldown still caps when dedup is unavailable"
        );
    }
}
