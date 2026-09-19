// Modified for Distill by Samuel Fajreldines, 2026.
//! The feedback form (Write / Drafts) hosted in minimal's live band.
//!
//! Reuses the full-TUI [`FeedbackModalState::render`] over the whole band, which `crate::overlay::compute_target` sizes like a centered app-modal.
//! The form owns every key while open, so a band too small for it paints a one-row Esc hint: an invisible owner is a frozen UI.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use distill_pager::terminal;
use distill_pager::terminal::overlay::PostFlush;
use distill_pager::theme::Theme;
use distill_pager::views::feedback_modal::FeedbackModalState;

/// Shown when the band is too small for the form (`render` returns `None` below 6 rows or 20 columns).
const FEEDBACK_BAND_TOO_SMALL_HINT: &str =
    "Feedback form needs a bigger terminal - press Esc to close";

/// Paint the form over `area` and hand back the Write caret plus the post-flush escapes.
/// The minimal prompt paints inline image previews, so a frame without its own escapes must still clear a stale placement or it sits over the form.
pub(super) fn render(
    buf: &mut Buffer,
    area: Rect,
    modal: &mut FeedbackModalState,
    theme: &Theme,
    compact: bool,
) -> (Option<(u16, u16)>, Option<PostFlush>) {
    match modal.render(buf, area, theme, compact) {
        Some(frame) => (
            frame.cursor,
            frame
                .post_flush
                .or_else(|| terminal::overlay::clear().map(PostFlush::from)),
        ),
        None => {
            let row = Rect {
                height: 1u16.min(area.height),
                ..area
            };
            super::live::render_warning_hint(buf, row, theme, FEEDBACK_BAND_TOO_SMALL_HINT);
            (None, terminal::overlay::clear().map(PostFlush::from))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_pager::views::feedback_modal::OpenFeedbackModal;
    use distill_pager::views::modal_window;

    /// Restores the process-global embedded flag even when an assertion panics.
    struct EmbedReset;
    impl Drop for EmbedReset {
        fn drop(&mut self) {
            modal_window::set_embedded(false);
        }
    }

    /// Non-embedded chrome centers a smaller popup, so the band height would fall under the renderer's 6-row floor; minimal sets embedded at startup.
    #[test]
    #[serial_test::serial]
    fn feedback_modal_paints_tabs_and_places_the_caret() {
        let _reset = EmbedReset;
        modal_window::set_embedded(true);
        let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
        let theme = Theme::terminal_default();
        let area = Rect::new(0, 0, 80, crate::overlay::MINIMAL_APP_MODAL_ROWS);
        let mut buf = Buffer::empty(area);
        let (cursor, _) = render(&mut buf, area, &mut modal, &theme, false);

        let text = crate::buffer_text(&buf);
        for needle in ["Feedback", "Write", "Drafts"] {
            assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
        }
        assert!(
            cursor.is_some(),
            "the Write composer caret must be the hardware cursor"
        );
    }

    #[test]
    #[serial_test::serial]
    fn feedback_modal_too_small_band_paints_the_esc_hint() {
        let _reset = EmbedReset;
        modal_window::set_embedded(true);
        let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
        let theme = Theme::terminal_default();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        let (cursor, _) = render(&mut buf, area, &mut modal, &theme, false);

        let text = crate::buffer_text(&buf);
        assert!(
            text.contains(FEEDBACK_BAND_TOO_SMALL_HINT),
            "the key owner must stay visible on a too-small band:\n{text}"
        );
        assert!(cursor.is_none());
    }
}
