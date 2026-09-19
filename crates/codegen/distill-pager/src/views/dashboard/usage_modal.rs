//! Dashboard host for the `/usage` modal ([`DashboardState::usage_modal`]).
//!
//! Routing lives in `views::usage_modal` (shared with the agent view); this module only maps the outcome onto the dashboard's modal slot
//! and its toast surface. The modal owns keyboard and mouse until it closes, like the shortcuts cheatsheet.

use crossterm::event::{Event, KeyEventKind};

use super::state::DashboardState;
use crate::app::app_view::InputOutcome;
use crate::views::usage_modal::{
    UsageModalOutcome, route_usage_modal_key, route_usage_modal_mouse,
};

impl DashboardState {
    /// Caller has confirmed `usage_modal.is_some()` (the gate in [`DashboardState::handle_input`]).
    pub(super) fn handle_usage_modal_input(&mut self, ev: &Event) -> InputOutcome {
        let Some(modal) = self.usage_modal.as_mut() else {
            return InputOutcome::Unchanged;
        };
        let outcome = match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                route_usage_modal_key(modal, key)
            }
            Event::Mouse(mouse) => {
                route_usage_modal_mouse(modal, mouse.kind, mouse.column, mouse.row)
            }
            Event::Paste(_)
            | Event::Key(_)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Resize(_, _) => UsageModalOutcome::Unchanged,
        };
        match outcome {
            UsageModalOutcome::Close => {
                self.usage_modal = None;
                InputOutcome::Changed
            }
            UsageModalOutcome::CopySessionId => {
                if let Some(id) = self
                    .usage_modal
                    .as_ref()
                    .and_then(|m| m.ctx.session_id.clone())
                {
                    self.copy_usage_modal_text(&id);
                }
                InputOutcome::Changed
            }
            UsageModalOutcome::CopyText(text) => {
                self.copy_usage_modal_text(&text);
                InputOutcome::Changed
            }
            UsageModalOutcome::Changed => InputOutcome::Changed,
            UsageModalOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    /// The feedback slot is the dashboard's only toast surface.
    fn copy_usage_modal_text(&mut self, text: &str) {
        let delivery = crate::clipboard::copy_text_or_file(text);
        self.error_toast = Some(delivery.toast_message().into_owned());
    }
}

#[cfg(test)]
#[path = "usage_modal_tests.rs"]
mod tests;
