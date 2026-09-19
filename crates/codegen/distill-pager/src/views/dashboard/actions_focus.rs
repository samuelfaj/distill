//! The keyboard cursor on the dashboard's actions row (`+ New Agent`, `Open Previous /resume`, `Worktree Ctrl+w`):
//! which item can hold it, their visual order, and how `←`/`→` walk that order over the items the last frame painted.
//! Owns the one item-to-hit-area mapping that navigation and the renderer's focus fallback both use.

use crate::app::agent_view::HitArea;
use crate::views::dashboard::state::DashboardState;

/// A keyboard cursor target on the actions row.
/// `←`/`→` walk [`ActionsFocus::VISUAL_ORDER`] over the items the last frame painted; `Enter` acts like a click on the focused item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionsFocus {
    /// `+ New Agent`: Enter creates a session (or dispatches a typed draft).
    NewAgent,
    /// `Open Previous /resume` (v2 workspace dashboard only): Enter opens the session picker.
    OpenPrevious,
    /// `Worktree Ctrl+w`: Enter toggles worktree mode.
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Left,
    Right,
}

impl ActionsFocus {
    /// Left-to-right order on the row, which is also the order the walk follows.
    pub(crate) const VISUAL_ORDER: [ActionsFocus; 3] = [
        ActionsFocus::NewAgent,
        ActionsFocus::OpenPrevious,
        ActionsFocus::Worktree,
    ];

    /// The click target the last frame registered for this item.
    pub(crate) fn hit(self, state: &DashboardState) -> &HitArea {
        match self {
            ActionsFocus::NewAgent => &state.new_agent_button_hit,
            ActionsFocus::OpenPrevious => &state.open_session_button_hit,
            ActionsFocus::Worktree => &state.worktree_toggle_hit,
        }
    }

    /// Whether the last frame painted this item; unpainted items (dropped for width, or `Open Previous` outside the workspace
    /// dashboard) are skipped by the walk and vacate the cursor.
    pub(crate) fn is_painted(self, state: &DashboardState) -> bool {
        self.hit(state).rect.is_some()
    }

    /// The painted item one `step` away from `self` in visual order; `None` at either end (no wrap) or when `self` itself is unpainted.
    pub(crate) fn neighbour(self, state: &DashboardState, step: Step) -> Option<ActionsFocus> {
        let painted = |item: &ActionsFocus| item.is_painted(state);
        match step {
            Step::Right => next_after(Self::VISUAL_ORDER.into_iter().filter(painted), self),
            Step::Left => next_after(Self::VISUAL_ORDER.into_iter().rev().filter(painted), self),
        }
    }
}

/// The element following `current` in `iter`, or `None` when `current` is absent or last.
fn next_after(
    mut iter: impl Iterator<Item = ActionsFocus>,
    current: ActionsFocus,
) -> Option<ActionsFocus> {
    iter.find(|item| *item == current)?;
    iter.next()
}

#[cfg(test)]
#[path = "actions_focus_tests.rs"]
mod tests;
