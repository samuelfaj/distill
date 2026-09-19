//! Wait overlay after opening grok.com/connectors from the MCP tab.
//!
//! Covers the list until the user refreshes (R) or dismisses (Esc). Shared by Needs Auth and
//! Ctrl+O / URL click. Owns the overlay's paint and its key/mouse routing; the extensions modal
//! only decides when the overlay is active and applies the returned outcome.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::clipboard::ClipboardDelivery;
use crate::theme::Theme;
use crate::views::mcps_modal::{MCP_SERVERS_REFRESH_KEY, managed_connectors_url};
use crate::views::modal_window::{fill_overlay_content, word_wrap};

#[cfg(test)]
#[path = "managed_connectors_wait_tests.rs"]
mod tests;

/// Footer shortcut id: dismiss this overlay (`esc back`). 98 cycles tabs; 99 closes the modal.
pub(crate) const WAIT_BACK_SHORTCUT_ID: usize = 97;

/// Overlay state for one wait: the URL it was opened with plus per-frame hover/hit rects.
#[derive(Debug, Clone)]
pub struct ManagedConnectorsWaitState {
    /// Fixed for the life of the wait; shared with OSC8 link spans without re-formatting per frame.
    pub url: Arc<str>,
    pub copy_hovered: bool,
    pub copy_rect: Option<Rect>,
    pub url_rects: Vec<Rect>,
    pub url_copied: bool,
}

/// What the extensions modal should do after the wait overlay consumed an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedConnectorsWaitOutcome {
    /// Nothing changed; no redraw needed.
    Ignored,
    /// Overlay-local state changed (hover, copied badge) or the event was absorbed; redraw.
    Changed,
    /// Refresh the MCP list, which also ends the wait.
    Refresh,
    /// Leave the overlay and return to the list without refreshing.
    Dismiss,
    /// Open grok.com/connectors again; the overlay stays up.
    OpenConnectors,
}

impl ManagedConnectorsWaitState {
    pub fn new(team_id: Option<&str>) -> Self {
        Self {
            url: managed_connectors_url(team_id).into(),
            copy_hovered: false,
            copy_rect: None,
            url_rects: Vec::new(),
            url_copied: false,
        }
    }

    pub fn clear_hit_rects(&mut self) {
        self.copy_rect = None;
        self.url_rects.clear();
    }

    /// Refresh and `Esc` leave the overlay; Ctrl+O reopens the URL and keeps it. Every other key,
    /// including Tab, is swallowed on purpose: a tab switch clears the wait, and one stray
    /// keystroke should not discard it silently. Keyboard users press Esc first, then Tab.
    pub fn handle_key(&self, key: &KeyEvent) -> ManagedConnectorsWaitOutcome {
        match key.code {
            KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
                ManagedConnectorsWaitOutcome::OpenConnectors
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&MCP_SERVERS_REFRESH_KEY) => {
                ManagedConnectorsWaitOutcome::Refresh
            }
            KeyCode::Esc => ManagedConnectorsWaitOutcome::Dismiss,
            _ => ManagedConnectorsWaitOutcome::Ignored,
        }
    }

    /// Pointer events are absorbed so picker hit-rects underneath cannot re-fire OAuth; only the
    /// copy button and the painted URL react. `copy` is injected so tests can observe the copied
    /// flag without a system clipboard.
    pub fn handle_mouse(
        &mut self,
        mouse: &MouseEvent,
        copy: impl FnOnce(&str) -> ClipboardDelivery,
    ) -> ManagedConnectorsWaitOutcome {
        let at = Position::new(mouse.column, mouse.row);
        let in_copy = self.copy_rect.is_some_and(|rect| rect.contains(at));
        let in_url = self.url_rects.iter().any(|rect| rect.contains(at));
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if self.copy_hovered == in_copy {
                    ManagedConnectorsWaitOutcome::Ignored
                } else {
                    self.copy_hovered = in_copy;
                    ManagedConnectorsWaitOutcome::Changed
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_copy => {
                self.url_copied = copy(&self.url).reported_success();
                ManagedConnectorsWaitOutcome::Changed
            }
            MouseEventKind::Down(MouseButton::Left) if in_url => {
                ManagedConnectorsWaitOutcome::OpenConnectors
            }
            MouseEventKind::Down(_) => ManagedConnectorsWaitOutcome::Changed,
            _ => ManagedConnectorsWaitOutcome::Ignored,
        }
    }
}

/// Paint the overlay into `msg_area` and record this frame's copy-button and URL hit rects. The
/// caller has already cleared last frame's rects via [`ManagedConnectorsWaitState::clear_hit_rects`].
pub(crate) fn render_managed_connectors_wait(
    buf: &mut Buffer,
    msg_area: Rect,
    wait: &mut ManagedConnectorsWaitState,
    theme: &Theme,
) {
    fill_overlay_content(buf, msg_area, theme);

    let btn = if wait.url_copied {
        "[copied]"
    } else {
        "[copy the url]"
    };
    // Chunks borrow a local handle so the paint loop below can still record rects on `wait`.
    let url = Arc::clone(&wait.url);
    let url_chunks = word_wrap(&url, msg_area.width as usize);

    #[derive(Clone, Copy)]
    enum Line<'a> {
        Text(&'a str),
        Spacer,
        Btn(&'a str),
        Url(&'a str),
    }
    let mut lines = vec![
        Line::Text("Finish in the browser."),
        Line::Spacer,
        Line::Text("Refresh when you're done."),
        Line::Spacer,
        Line::Btn(btn),
    ];
    lines.extend(url_chunks.iter().copied().map(Line::Url));
    // Truncation cuts from the end, where the copy button and URL sit. Give up the spacers first
    // so a short overlay still shows both targets.
    if lines.len() > msg_area.height as usize {
        lines.retain(|line| !matches!(line, Line::Spacer));
    }
    let wrap_url = url_chunks.len() > 1;
    let msg_height = lines.len().min(msg_area.height as usize);
    let msg_y = msg_area.y + (msg_area.height.saturating_sub(msg_height as u16)) / 2;
    let text_style = Style::reset().fg(theme.accent_tool).bg(theme.bg_base);
    let width_of = |s: &str| UnicodeWidthStr::width(s) as u16;
    let centered = |w: u16| msg_area.x + msg_area.width.saturating_sub(w) / 2;
    for (i, line) in lines.iter().take(msg_height).enumerate() {
        let y = msg_y + i as u16;
        match *line {
            // The fill above already cleared the row.
            Line::Spacer => {}
            Line::Text(s) => {
                buf.set_string(centered(width_of(s)), y, s, text_style);
            }
            Line::Btn(s) => {
                let w = width_of(s);
                let x = centered(w);
                let mut btn_style = Style::reset().fg(theme.accent_tool).bg(theme.bg_base);
                if wait.copy_hovered {
                    btn_style = btn_style
                        .bg(theme.bg_highlight)
                        .add_modifier(Modifier::BOLD);
                }
                buf.set_string(x, y, s, btn_style);
                wait.copy_rect = Some(Rect::new(x, y, w.max(1), 1));
            }
            Line::Url(s) => {
                let w = width_of(s);
                let x = if wrap_url { msg_area.x } else { centered(w) };
                let url_style = Style::reset()
                    .fg(theme.link_fg)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::UNDERLINED);
                buf.set_string(x, y, s, url_style);
                wait.url_rects.push(Rect::new(x, y, w.max(1), 1));
            }
        }
    }
}
