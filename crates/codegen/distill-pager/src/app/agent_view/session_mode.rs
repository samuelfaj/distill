// Modified for Distill by Samuel Fajreldines, 2026.
//! Session modes an agent publishes, kept next to the plan flags every plan surface already reads.

use agent_client_protocol as acp;
use distill_tools::types::SessionMode;

use crate::app::agent_view::AgentView;

impl AgentView {
    /// Stores the agent's offered modes. A published list drops permission staged by the
    /// pre-session Grok ring, which has no arm here and would otherwise stick on the prompt row.
    pub(crate) fn apply_session_modes(&mut self, modes: Option<acp::SessionModeState>) -> bool {
        let Some(modes) = modes else {
            return false;
        };
        self.session_mode = SessionMode::from_id(&modes.current_mode_id.0);
        self.session_mode_pending = None;
        self.plan_mode_active = self.session_mode.is_plan();
        self.available_modes = modes.available_modes;
        self.session.yolo_mode = false;
        self.session.auto_mode = false;
        self.deferred_permission_mode = None;
        true
    }

    /// Optimistic pick, else the confirmed mode. Plan exits through the plan flags must not
    /// still read as plan here.
    pub(crate) fn effective_session_mode(&self) -> SessionMode {
        if self.plan_mode_pending.unwrap_or(self.plan_mode_active) {
            return SessionMode::Plan;
        }
        let picked = self
            .session_mode_pending
            .as_ref()
            .unwrap_or(&self.session_mode);
        match picked {
            SessionMode::Ask => SessionMode::Ask,
            SessionMode::Default | SessionMode::Plan => SessionMode::Default,
        }
    }

    /// Prompt-row / peek flag for a published non-plan mode. Agent shows none.
    pub(crate) fn published_mode_label(&self) -> Option<&'static str> {
        match self.effective_session_mode() {
            SessionMode::Ask => Some(SessionMode::Ask.as_id()),
            SessionMode::Default | SessionMode::Plan => None,
        }
    }

    /// Same flag the prompt row uses when it is not in commenting / plan-approval chrome.
    pub(crate) fn prompt_row_mode_label(&self) -> Option<&'static str> {
        if self.plan_mode_pending.unwrap_or(self.plan_mode_active) {
            Some("plan")
        } else {
            self.published_mode_label()
        }
    }

    /// Next offered mode in ring order, with the name the agent gave it. Unknown ids are skipped.
    pub(crate) fn next_published_mode(&self) -> Option<(SessionMode, &str)> {
        let known: Vec<(SessionMode, &str)> = self
            .available_modes
            .iter()
            .filter_map(|mode| mode.id.0.parse().ok().map(|id| (id, mode.name.as_str())))
            .collect();
        let current = self.effective_session_mode();
        let position = known.iter().position(|(mode, _)| *mode == current);
        // An empty ring yields `None` from `get`; the modulo is only reached when a mode matched.
        let next = position.map_or(0, |index| (index + 1) % known.len());
        known.get(next).cloned()
    }

    /// Optimistic Shift+Tab pick, pending the agent's `CurrentModeUpdate`.
    pub(crate) fn pick_published_mode(&mut self, mode: SessionMode) {
        self.plan_mode_pending = Some(mode.is_plan());
        self.session_mode_pending = Some(mode);
    }
}
