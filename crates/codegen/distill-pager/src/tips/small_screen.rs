//! Small-screen tip: on smallish terminals, advertise that `/compact-mode` reclaims the padding and sticky-header rows.
//!
//! Shown once per run, at the first stable agent-view draw only, never on a later resize.
//! Below the band auto-compact already trims those rows; above it the default layout is roomy enough that the hint is noise.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::theme::Theme;
use crate::views::agent::AUTO_COMPACT_MAX_ROWS;

/// Ephemeral-tip dedup key for the small-screen `/compact-mode` hint.
pub(crate) const SMALL_SCREEN_TIP_KEY: &str = "small_screen_tip";

/// Key into the map that counts, in memory and per session, how many times this tip was shown.
pub(crate) const SMALL_SCREEN_TIP_SEEN_KEY: &str = "small_screen_tip_shown_count";

/// Stop showing after this many shows within a single session.
const SMALL_SCREEN_TIP_SEEN_CAP: u32 = 1;

/// Tallest terminal (rows) that still counts as "tight on space".
const SMALL_SCREEN_TIP_MAX_ROWS: u16 = 28;

/// Whether `rows` falls in the band the tip targets.
/// The band sits above the auto-compact threshold, where compacting is the user's call, and ends at [`SMALL_SCREEN_TIP_MAX_ROWS`].
pub fn small_screen_band_contains(rows: u16) -> bool {
    (AUTO_COMPACT_MAX_ROWS + 1..=SMALL_SCREEN_TIP_MAX_ROWS).contains(&rows)
}

/// Build "Tight on space? Try /compact-mode", shown at most [`SMALL_SCREEN_TIP_SEEN_CAP`] time per session (counted in memory).
/// Ambient because the tip is not about the draft: submitting a prompt right after it appears must not retire it.
/// While the tip is occluded its TTL stops ticking; otherwise a quick Enter into a multi-second turn reduces the tip to a sub-second blink.
pub fn small_screen_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    // The command token gets the same styling the other tips give their chord and key tokens
    let command = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip::new(
        SMALL_SCREEN_TIP_KEY,
        Line::from(vec![
            Span::styled("Tight on space? Try ", dim),
            Span::styled("/compact-mode", command),
        ]),
    )
    .with_session_seen_cap(SMALL_SCREEN_TIP_SEEN_KEY, SMALL_SCREEN_TIP_SEEN_CAP)
    .ambient()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_starts_one_row_above_the_auto_compact_threshold() {
        // At the threshold auto-compact engages instead; one above is the first height the tip targets
        assert!(!small_screen_band_contains(AUTO_COMPACT_MAX_ROWS));
        assert!(small_screen_band_contains(AUTO_COMPACT_MAX_ROWS + 1));
    }

    #[test]
    fn band_ends_at_the_max_row_boundary() {
        assert!(small_screen_band_contains(SMALL_SCREEN_TIP_MAX_ROWS));
        assert!(!small_screen_band_contains(SMALL_SCREEN_TIP_MAX_ROWS + 1));
    }

    #[test]
    fn band_rejects_degenerate_heights() {
        assert!(!small_screen_band_contains(0));
        assert!(!small_screen_band_contains(1));
    }

    #[test]
    fn small_screen_tip_builder_applies_seen_gating() {
        assert_eq!(
            small_screen_tip().session_seen.map(|(key, _cap)| key),
            Some(SMALL_SCREEN_TIP_SEEN_KEY)
        );
        assert_eq!(
            small_screen_tip().session_seen.map(|(_, cap)| cap),
            Some(SMALL_SCREEN_TIP_SEEN_CAP)
        );
    }

    #[test]
    fn small_screen_tip_advertises_compact_mode() {
        let tip = small_screen_tip();
        let text: String = tip.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Tight on space? Try /compact-mode");
    }

    #[test]
    fn small_screen_tip_is_ambient() {
        // The tip must survive prompt submission and pause its TTL under occlusion; otherwise a quick Enter into a slow turn blinks it away
        assert!(small_screen_tip().ambient);
    }
}
