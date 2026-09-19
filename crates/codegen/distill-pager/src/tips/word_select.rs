//! The tip shown after a double-click on scrollback while text-selection mode is fold/nav; it advertises Settings → Text selection → Word select.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::theme::Theme;

/// Ephemeral-tip dedup key for the word-select settings hint.
pub(crate) const WORD_SELECT_TIP_KEY: &str = "word_select_tip";

/// Key into the in-memory map that counts how many times this tip was shown this session.
pub(crate) const WORD_SELECT_TIP_SEEN_KEY: &str = "word_select_tip_shown_count";

/// Stop showing after this many shows within a single session.
const WORD_SELECT_TIP_SEEN_CAP: u32 = 3;

/// Tip lifetime: ~20s at the 30fps animation cadence (vs the ~3s default).
/// This tip is a call to action (read it, decide, press the chord), so it gets a much longer window than a glanceable notice.
/// The window stays bounded: ambient pauses it while occluded, and any prompt edit retires it the moment the user starts doing something else.
pub(crate) const WORD_SELECT_TIP_TICKS: u16 = 600;

/// Pressing this chord while the tip is on screen flips `keep_text_selection` to `word_select` (see `Action::AcceptWordSelectTip`).
/// Outside the tip's TTL the chord keeps its normal meaning (prompt yank).
/// Any prompt edit retires the tip, so the long TTL cannot shadow a kill-then-yank sequence.
pub(crate) const WORD_SELECT_ACCEPT_CHORD: &str = "Ctrl+Y";

/// Build "Want double-click to select? Shows are capped at [`WORD_SELECT_TIP_SEEN_CAP`] per session, counted in
/// memory only.
pub fn word_select_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    let key_style = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip {
        ticks_remaining: WORD_SELECT_TIP_TICKS,
        ..EphemeralTip::new(
            WORD_SELECT_TIP_KEY,
            Line::from(vec![
                Span::styled("Want double-click to select? ", dim),
                Span::styled("/settings", key_style),
                Span::styled(" → Text selection · ", dim),
                Span::styled(WORD_SELECT_ACCEPT_CHORD, key_style),
                Span::styled(": enable now", dim),
            ]),
        )
        .with_session_seen_cap(WORD_SELECT_TIP_SEEN_KEY, WORD_SELECT_TIP_SEEN_CAP)
        // Ambient: the tip is not about the draft being edited, so an unrelated submit keeps it
        // Occlusion (permission ask, modal) pauses the TTL instead of burning the decision window off-screen
        .ambient()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_select_tip_builder_applies_seen_gating() {
        assert_eq!(
            word_select_tip().session_seen.map(|(key, _cap)| key),
            Some(WORD_SELECT_TIP_SEEN_KEY)
        );
        assert_eq!(
            word_select_tip().session_seen.map(|(_, cap)| cap),
            Some(WORD_SELECT_TIP_SEEN_CAP)
        );
    }

    /// The call-to-action window: a long TTL and ambient (pauses while occluded, survives an unrelated submit).
    /// Editing the prompt retires the tip, bounding the window; see the `PromptEvent::Edited` hook in `agent_view/prompt.rs`.
    #[test]
    fn word_select_tip_has_long_ambient_window() {
        let tip = word_select_tip();
        assert_eq!(tip.ticks_remaining, WORD_SELECT_TIP_TICKS);
        assert!(
            tip.ticks_remaining > super::super::DEFAULT_TIP_TICKS,
            "CTA tip must outlive the glanceable default"
        );
        assert!(tip.ambient, "occlusion must pause, not burn, the window");
    }

    #[test]
    fn word_select_tip_advertises_settings_and_accept_chord() {
        let tip = word_select_tip();
        let text: String = tip.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("double-click to select")
                && text.contains("/settings")
                && text.contains("Text selection")
                && text.contains(WORD_SELECT_ACCEPT_CHORD),
            "expected settings path + accept chord copy, got {text:?}"
        );
    }
}
