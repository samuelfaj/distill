//! Ephemeral tip: empty Enter after a mid-turn queue force-sends the top follow-up.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::EphemeralTip;
use crate::theme::Theme;

pub(crate) const SEND_NOW_TIP_KEY: &str = "send_now_tip";
pub(crate) const SEND_NOW_TIP_SEEN_KEY: &str = "send_now_tip_shown_count";
const SEND_NOW_TIP_SEEN_CAP: u32 = 3;

pub fn send_now_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    let key_style = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    EphemeralTip::new(
        SEND_NOW_TIP_KEY,
        Line::from(vec![
            Span::styled("Queued · ", dim),
            Span::styled("Enter", key_style),
            Span::styled(" to send now", dim),
        ]),
    )
    .with_session_seen_cap(SEND_NOW_TIP_SEEN_KEY, SEND_NOW_TIP_SEEN_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_now_tip_builder_applies_seen_gating() {
        assert_eq!(
            send_now_tip().session_seen.map(|(key, _cap)| key),
            Some(SEND_NOW_TIP_SEEN_KEY)
        );
        assert_eq!(
            send_now_tip().session_seen.map(|(_, cap)| cap),
            Some(SEND_NOW_TIP_SEEN_CAP)
        );
    }

    #[test]
    fn send_now_tip_advertises_enter() {
        let tip = send_now_tip();
        let text: String = tip.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("Enter") && text.contains("send now") && text.contains("Queued"),
            "expected queued/send-now copy with Enter, got {text:?}"
        );
    }
}
