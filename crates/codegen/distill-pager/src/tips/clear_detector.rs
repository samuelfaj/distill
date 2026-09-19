//! Undo-tip trigger: detects a user-initiated wipe of a substantial prompt draft so the pager can hint that the undo chord brings it back.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::input::key::KeyShortcut;
use crate::theme::Theme;

/// Ephemeral-tip dedup key for the undo hint.
pub(crate) const UNDO_TIP_KEY: &str = "undo_tip";

/// Key for the undo tip in the per-session in-memory seen-count map (`AppView::tip_seen_counts`). The count never reaches disk.
pub(crate) const UNDO_TIP_SEEN_KEY: &str = "undo_tip_shown_count";

/// The tip stops showing after this many shows within a single session.
const UNDO_TIP_SEEN_CAP: u32 = 3;

/// A draft must have reached this many chars for its wipe to matter.
const FIRE_PEAK_LEN: usize = 20;

/// Text left after the wipe counts as "cleared" at or below this many chars.
const FIRE_RESIDUE_LEN: usize = 5;

/// Undo chord for the tip copy: always `ctrl+z`.
/// Most macOS terminals capture Cmd by default and don't forward Cmd+Z to a raw-mode TUI, so Ctrl+Z is the chord delivered on every platform.
/// The label is still derived from the real binding (not a literal): [`crate::input::key::is_undo_key`] accepts it, so it can't drift.
fn undo_chord_label() -> String {
    KeyShortcut::new(KeyCode::Char('z'), KeyModifiers::CONTROL)
        .display()
        .to_ascii_lowercase()
}

/// Build the "Input cleared · {chord} to undo" tip; it stops showing after [`UNDO_TIP_SEEN_CAP`] shows in a session (counted in memory).
pub fn undo_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    // The key chord uses the shortcuts bar's styling: bold secondary on dim text
    let chord = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip::new(
        UNDO_TIP_KEY,
        Line::from(vec![
            Span::styled("Input cleared · ", dim),
            Span::styled(undo_chord_label(), chord),
            Span::styled(" to undo", dim),
        ]),
    )
    .with_session_seen_cap(UNDO_TIP_SEEN_KEY, UNDO_TIP_SEEN_CAP)
}

/// The detector must be fed only user-initiated edits. Programmatic mutations (submit clears, queue restores, slash
/// completions) must not be observed. The `last_len` resync absorbs any that slip through, so the next user edit
/// does not fire on a peak the user did not build down from.
#[derive(Debug, Default)]
pub struct ClearDetector {
    /// High-water mark of the draft since the last fire or resync.
    peak_len: usize,
    /// Text length after the last observed user edit; a mismatch at the next edit means programmatic changes happened in between.
    last_len: usize,
}

impl ClearDetector {
    /// Observe one user-initiated edit as (length before, length after).
    /// Returns true when a substantial draft was just wiped.
    pub fn observe_user_edit(&mut self, before: usize, after: usize) -> bool {
        if before != self.last_len {
            // Programmatic mutation since the last user edit: adopt the current draft as the baseline
            // Firing here would use a peak the user did not build down from
            self.peak_len = before;
        }
        let fired = self.peak_len >= FIRE_PEAK_LEN && after <= FIRE_RESIDUE_LEN;
        // Reset after a fire so re-typing must build a fresh peak.
        self.peak_len = if fired {
            after
        } else {
            self.peak_len.max(after)
        };
        self.last_len = after;
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay a typing session as successive (before, after) user edits.
    fn type_to(detector: &mut ClearDetector, from: usize, to: usize) {
        let mut len = from;
        while len != to {
            let next = if to > len { len + 1 } else { len - 1 };
            assert!(
                !detector.observe_user_edit(len, next),
                "no fire expected while moving {len} -> {next}"
            );
            len = next;
        }
    }

    #[test]
    fn gradual_delete_fires_at_residue_threshold() {
        let mut d = ClearDetector::default();
        type_to(&mut d, 0, 30);
        type_to(&mut d, 30, 6); // still above the residue threshold
        assert!(d.observe_user_edit(6, 5), "crossing into residue fires");
        // Peak was reset: continuing to delete must not re-fire.
        assert!(!d.observe_user_edit(5, 0));
    }

    #[test]
    fn one_shot_clear_fires() {
        let mut d = ClearDetector::default();
        type_to(&mut d, 0, 25);
        assert!(d.observe_user_edit(25, 0), "ctrl+c style 25 -> 0 wipe");
    }

    #[test]
    fn short_draft_never_fires() {
        let mut d = ClearDetector::default();
        type_to(&mut d, 0, 19); // one below FIRE_PEAK_LEN
        assert!(!d.observe_user_edit(19, 0));
    }

    #[test]
    fn programmatic_clear_resyncs_without_firing() {
        let mut d = ClearDetector::default();
        type_to(&mut d, 0, 30);
        // Submit wiped the draft outside the detector (unobserved); the next user edit starts from 0 and must not fire on the stale peak
        assert!(!d.observe_user_edit(0, 1));
        assert!(!d.observe_user_edit(1, 0), "tiny draft, no fire");
    }

    #[test]
    fn wiping_a_programmatically_restored_draft_fires() {
        let mut d = ClearDetector::default();
        // A queue edit restored an 80-char draft (unobserved), then the user wipes it: the resync adopts 80 as the peak and the wipe fires
        assert!(d.observe_user_edit(80, 0));
    }

    #[test]
    fn refire_requires_building_a_new_peak() {
        let mut d = ClearDetector::default();
        type_to(&mut d, 0, 25);
        assert!(d.observe_user_edit(25, 0));
        type_to(&mut d, 0, 25);
        assert!(d.observe_user_edit(25, 0), "fresh peak fires again");
    }

    #[test]
    fn undo_tip_builder_applies_seen_gating() {
        // Asserting the exact key and cap would restate the constants; what matters is that the builder opts into the per-session seen gate at all
        assert_eq!(
            undo_tip().session_seen.map(|(key, _cap)| key),
            Some(UNDO_TIP_SEEN_KEY)
        );
    }

    #[test]
    fn undo_tip_chord_is_ctrl_z() {
        // The label pins ctrl+z, the chord terminals actually deliver on every platform
        assert_eq!(undo_chord_label(), "ctrl+z");
        // The advertised chord is one is_undo_key accepts, so the label can't drift from the binding it documents
        assert!(crate::input::key::is_undo_key(
            &crossterm::event::KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL,)
        ));
    }
}
