//! The child-view policy under the fullscreen takeover. Session, composer, and settings dispatchers resolve
//! the ROOT agent, while view-targeted actions resolve the child through `with_active_agent`; the allowlist
//! admits only the latter, and unknown actions are denied by default. The chords a child must never start
//! (three open a root-only modal locally) are listed here too, gated in the child's own key funnel.

use crate::actions::ActionId;
use crate::app::actions::Action;
use crate::app::agent_view::ViewSurface;
use crate::app::app_view::InputOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildActionGate {
    Allow,
    Deny,
}

/// Navigation, folding, copying, link opening, the child's own turn cancel, focus moves (the composer guard in
/// `set_active_pane` keeps `FocusPrompt` inert), and dashboard navigation from the takeover header. `OpenUrl`/`OpenLink`
/// open in the browser and never touch a session.
pub(crate) fn gate_child_action(action: &Action) -> ChildActionGate {
    match action {
        Action::ScrollUp(_)
        | Action::ScrollDown(_)
        | Action::PageUp
        | Action::PageDown
        | Action::HalfPageUp
        | Action::HalfPageDown
        | Action::GotoTop
        | Action::GotoBottom
        | Action::SelectNext
        | Action::SelectPrev
        | Action::NextTurn
        | Action::PrevTurn
        | Action::NextResponse
        | Action::PrevResponse
        | Action::Collapse
        | Action::Expand
        | Action::ToggleFold
        | Action::ToggleExpandAll
        | Action::ExpandAllThinking
        | Action::ToggleRaw
        | Action::FocusScrollback
        | Action::FocusPrompt
        | Action::CancelTurn
        | Action::CopyBlockContent
        | Action::CopyBlockMeta
        | Action::OpenBlockViewer
        | Action::OpenLink(_)
        | Action::OpenUrl(_)
        | Action::OpenNextLink
        | Action::OpenPrevLink
        | Action::OpenScrollbackSearch(_)
        | Action::ClearPrompt
        | Action::OpenHistorySearch
        | Action::Quit
        | Action::ToggleMouseCapture
        | Action::AcceptWordSelectTip
        | Action::ShowWordSelectTip
        // Dashboard navigation resolves no agent at all; the takeover header paints these controls for the parent
        | Action::OpenDashboard
        | Action::DashboardOverlayExit
        | Action::DashboardOverlayPrev
        | Action::DashboardOverlayNext => ChildActionGate::Allow,
        _ => ChildActionGate::Deny,
    }
}

/// A denied action becomes a silent redraw; `Unchanged`/`Changed` pass through untouched. Fails closed: a mixed
/// `ActionPair` is dropped whole, and a denied `ActionThenForward` drops its forward pass too. The caller hoists the
/// child's `pending_effects` BEFORE filtering, so an effect emitted beside a denied action still reaches `AppView`.
pub(crate) fn filter_child_outcome(outcome: InputOutcome) -> InputOutcome {
    let denied = match &outcome {
        InputOutcome::Action(action)
        | InputOutcome::ActionThenForward(action)
        | InputOutcome::ArmPending { action, .. } => {
            gate_child_action(action) == ChildActionGate::Deny
        }
        InputOutcome::ActionPair(first, second) => {
            gate_child_action(first) == ChildActionGate::Deny
                || gate_child_action(second) == ChildActionGate::Deny
        }
        InputOutcome::Changed | InputOutcome::Unchanged => false,
    };
    if denied {
        InputOutcome::Changed
    } else {
        outcome
    }
}

impl ViewSurface {
    /// Chords the child surface must not run: three open a root-only modal locally (ModelPicker, CommandPalette,
    /// OpenSessions); the rest emit an `Action` the filter denies but are listed so the child's handler never starts
    /// them. `ShortcutsHelp` stays: the read-only cheatsheet shows the child's own keys.
    pub(crate) fn hides_chord(self, id: ActionId) -> bool {
        self == Self::ChildTakeover
            && matches!(
                id,
                ActionId::ModelPicker
                    | ActionId::CommandPalette
                    | ActionId::OpenSessions
                    | ActionId::OpenSettings
                    | ActionId::OpenExtensions
                    | ActionId::ToggleYolo
                    | ActionId::SendToBackground
                    | ActionId::EditPromptExternal
                    | ActionId::CycleMode
            )
    }
}

#[cfg(test)]
#[path = "child_action_filter_tests.rs"]
mod tests;
