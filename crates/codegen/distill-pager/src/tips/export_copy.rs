use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::theme::Theme;

pub(crate) const EXPORT_COPY_TIP_KEY: &str = "export_copy_tip";

pub(crate) const EXPORT_COPY_TIP_SEEN_KEY: &str = "export_copy_tip_shown_count";

const EXPORT_COPY_TIP_SEEN_CAP: u32 = 3;

pub(crate) const EXPORT_COPY_TIP_TICKS: u16 = 600;

const WINDOW: Duration = Duration::from_secs(15);
const MIN_COPIES: usize = 3;
const UPGRADE_DEDUPE: Duration = Duration::from_millis(800);
const NEAR_ENTRIES: u64 = 16;

/// Drag-copy cluster; `note_drag_copy` reports armed, not show-now.
#[derive(Debug, Clone, Default)]
pub struct ExportCopyDetector {
    copies: VecDeque<(Instant, u64)>,
    slash_used: bool,
    pending_tip_ticks: u16,
}

impl ExportCopyDetector {
    /// `true` when the cluster is armed (tip pending after the Copied! debounce).
    /// `toast_ticks` is the duration of the toast this copy just showed.
    pub fn note_drag_copy(
        &mut self,
        now: Instant,
        entry_key: u64,
        tip_already_showing: bool,
        toast_ticks: u16,
    ) -> bool {
        if self.slash_used || tip_already_showing {
            self.pending_tip_ticks = 0;
            return false;
        }
        self.prune(now);
        if let Some((t, key)) = self.copies.back_mut()
            && *key == entry_key
            && now.saturating_duration_since(*t) < UPGRADE_DEDUPE
        {
            *t = now;
        } else {
            // Far from the previous copy: this drag starts a new cluster.
            if let Some(&(_, prev_key)) = self.copies.back()
                && entry_key.abs_diff(prev_key) > NEAR_ENTRIES
            {
                self.copies.clear();
            }
            self.copies.push_back((now, entry_key));
        }
        self.pending_tip_ticks = if self.copies.len() >= MIN_COPIES {
            toast_ticks
        } else {
            0
        };
        self.pending_tip_ticks > 0
    }

    pub fn note_slash_used(&mut self) {
        self.slash_used = true;
        self.pending_tip_ticks = 0;
    }

    /// Decrement the Copied! debounce. `true` when the tip should show.
    pub fn tick(&mut self, tip_already_showing: bool) -> bool {
        if self.slash_used || tip_already_showing || self.pending_tip_ticks == 0 {
            self.pending_tip_ticks = 0;
            return false;
        }
        self.pending_tip_ticks -= 1;
        self.pending_tip_ticks == 0
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.copies.front() {
            if now.saturating_duration_since(t) > WINDOW {
                self.copies.pop_front();
            } else {
                break;
            }
        }
    }
}

pub fn export_copy_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    let key_style = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip {
        ticks_remaining: EXPORT_COPY_TIP_TICKS,
        ..EphemeralTip::new(
            EXPORT_COPY_TIP_KEY,
            Line::from(vec![
                Span::styled("Copying a lot? ", dim),
                Span::styled("/copy", key_style),
                Span::styled(" last reply · ", dim),
                Span::styled("/export", key_style),
                Span::styled(" full transcript", dim),
            ]),
        )
        .with_session_seen_cap(EXPORT_COPY_TIP_SEEN_KEY, EXPORT_COPY_TIP_SEEN_CAP)
        .ambient()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_copy_tip_builder_applies_seen_gating() {
        assert_eq!(
            export_copy_tip().session_seen.map(|(key, _cap)| key),
            Some(EXPORT_COPY_TIP_SEEN_KEY)
        );
        assert_eq!(
            export_copy_tip().session_seen.map(|(_, cap)| cap),
            Some(EXPORT_COPY_TIP_SEEN_CAP)
        );
    }

    #[test]
    fn export_copy_tip_has_long_ambient_window() {
        let tip = export_copy_tip();
        assert_eq!(tip.ticks_remaining, EXPORT_COPY_TIP_TICKS);
        assert!(
            tip.ticks_remaining > super::super::DEFAULT_TIP_TICKS,
            "CTA tip must outlive the glanceable default"
        );
        assert!(tip.ambient, "occlusion must pause, not burn, the window");
    }

    #[test]
    fn export_copy_tip_names_both_commands() {
        let tip = export_copy_tip();
        let text: String = tip.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("/copy") && text.contains("/export") && text.contains("Copying a lot?"),
            "expected both commands in copy, got {text:?}"
        );
        assert_eq!(
            text,
            "Copying a lot? /copy last reply · /export full transcript"
        );
    }

    // Matches AgentView::show_toast default; production uses CopyDelivery::toast_ticks().
    const COPIED_TOAST_TICKS: u16 = 90;

    fn detector() -> ExportCopyDetector {
        ExportCopyDetector::default()
    }

    fn drag(p: &mut ExportCopyDetector, now: Instant, key: u64, showing: bool) -> bool {
        p.note_drag_copy(now, key, showing, COPIED_TOAST_TICKS)
    }

    fn arm_three(p: &mut ExportCopyDetector, t0: Instant) {
        assert!(!drag(p, t0, 1, false));
        assert!(!drag(p, t0 + Duration::from_secs(1), 2, false));
        assert!(drag(p, t0 + Duration::from_secs(2), 3, false));
    }

    #[test]
    fn two_drags_do_not_arm() {
        let mut p = detector();
        let t0 = Instant::now();
        assert!(!drag(&mut p, t0, 1, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(1), 2, false));
    }

    #[test]
    fn three_near_in_15s_arms() {
        let mut p = detector();
        arm_three(&mut p, Instant::now());
        assert!(!p.tick(false));
    }

    #[test]
    fn three_far_in_place_resets_cluster() {
        let mut p = detector();
        let t0 = Instant::now();
        assert!(!drag(&mut p, t0, 1, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(1), 2, false));
        let far = 2 + NEAR_ENTRIES + 1;
        assert!(!drag(&mut p, t0 + Duration::from_secs(2), far, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(3), far + 1, false));
        assert!(drag(&mut p, t0 + Duration::from_secs(4), far + 2, false));
    }

    #[test]
    fn sixteen_second_gap_prunes() {
        let mut p = detector();
        let t0 = Instant::now();
        assert!(!drag(&mut p, t0, 1, false));
        assert!(!drag(&mut p, t0 + Duration::from_millis(1), 2, false));
        let later = t0 + Duration::from_secs(16);
        assert!(!drag(&mut p, later, 3, false));
        assert!(!drag(&mut p, later + Duration::from_secs(1), 4, false));
        assert!(drag(&mut p, later + Duration::from_secs(2), 5, false));
    }

    #[test]
    fn same_entry_within_800ms_counts_as_one() {
        let mut p = detector();
        let t0 = Instant::now();
        assert!(!drag(&mut p, t0, 7, false));
        assert!(!drag(&mut p, t0 + Duration::from_millis(500), 7, false));
        assert!(!drag(&mut p, t0 + Duration::from_millis(700), 7, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(1), 8, false));
        assert!(drag(&mut p, t0 + Duration::from_secs(2), 9, false));
    }

    #[test]
    fn slash_used_never_arms() {
        let mut p = detector();
        let t0 = Instant::now();
        p.note_slash_used();
        assert!(!drag(&mut p, t0, 1, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(1), 2, false));
        assert!(!drag(&mut p, t0 + Duration::from_secs(2), 3, false));
        assert!(!p.tick(false));
    }

    #[test]
    fn tick_90_then_due() {
        let mut p = detector();
        arm_three(&mut p, Instant::now());
        for _ in 0..(COPIED_TOAST_TICKS - 1) {
            assert!(!p.tick(false));
        }
        assert!(p.tick(false));
        assert!(!p.tick(false));
    }

    #[test]
    fn tick_resets_on_new_qualifying_drag() {
        let mut p = detector();
        let t0 = Instant::now();
        arm_three(&mut p, t0);
        for _ in 0..40 {
            assert!(!p.tick(false));
        }
        assert!(drag(&mut p, t0 + Duration::from_secs(3), 4, false));
        for _ in 0..(COPIED_TOAST_TICKS - 1) {
            assert!(!p.tick(false));
        }
        assert!(p.tick(false));
    }

    #[test]
    fn already_showing_does_not_re_due() {
        let mut p = detector();
        let t0 = Instant::now();
        arm_three(&mut p, t0);
        assert!(!drag(&mut p, t0 + Duration::from_secs(3), 4, true));
        for _ in 0..COPIED_TOAST_TICKS {
            assert!(!p.tick(true));
        }
        assert!(!p.tick(false));
    }
}
