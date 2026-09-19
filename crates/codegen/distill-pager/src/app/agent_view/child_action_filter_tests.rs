use super::{ChildActionGate, filter_child_outcome, gate_child_action};
use crate::actions::{ActionId, ActionRegistry};
use crate::app::actions::Action;
use crate::app::agent_view::test_fixtures::{make_agent, parent_with_child};
use crate::app::agent_view::{ViewSurface, resolve_action};
use crate::app::app_view::InputOutcome;
use std::collections::BTreeSet;

/// The whole allow policy by variant name; anything else a child can reach must be denied.
const ALLOWED: &[&str] = &[
    "ScrollUp",
    "ScrollDown",
    "PageUp",
    "PageDown",
    "HalfPageUp",
    "HalfPageDown",
    "GotoTop",
    "GotoBottom",
    "SelectNext",
    "SelectPrev",
    "NextTurn",
    "PrevTurn",
    "NextResponse",
    "PrevResponse",
    "Collapse",
    "Expand",
    "ToggleFold",
    "ToggleExpandAll",
    "ExpandAllThinking",
    "ToggleRaw",
    "FocusScrollback",
    "FocusPrompt",
    "CancelTurn",
    "CopyBlockContent",
    "CopyBlockMeta",
    "OpenBlockViewer",
    "OpenLink",
    "OpenUrl",
    "OpenNextLink",
    "OpenPrevLink",
    "OpenScrollbackSearch",
    "ClearPrompt",
    "OpenHistorySearch",
    "Quit",
    "ToggleMouseCapture",
    "AcceptWordSelectTip",
    "ShowWordSelectTip",
    "OpenDashboard",
    "DashboardOverlayExit",
    "DashboardOverlayPrev",
    "DashboardOverlayNext",
];

/// Root-session actions a child reaches only through pane, queue, or mouse handlers, so the registry walk never
/// produces them; each would be dispatched against the ROOT.
fn unreached_denied_samples() -> Vec<Action> {
    vec![
        Action::SendPromptNow {
            text: String::from("hi"),
            images: Vec::new(),
        },
        Action::Interject {
            text: String::from("hi"),
            images: Vec::new(),
        },
        Action::SubmitFollowUp(String::from("f")),
        Action::RevisePlan(String::from("revise the plan")),
        Action::SendSlashCommandPreservingDraft(String::from("/help")),
        Action::QueueRemoveShared {
            id: String::from("p1"),
            expected_version: 1,
        },
        Action::QueueInterjectShared {
            id: String::from("p1"),
            expected_version: 1,
            new_text: None,
        },
        Action::DrainQueue,
        Action::Rewind,
        Action::RewindShowPicker,
        Action::KillBgTask(String::from("t1")),
    ]
}

fn actions_of(outcome: &InputOutcome) -> Vec<&Action> {
    match outcome {
        InputOutcome::Action(a)
        | InputOutcome::ActionThenForward(a)
        | InputOutcome::ArmPending { action: a, .. } => vec![a],
        InputOutcome::ActionPair(a, b) => vec![a, b],
        InputOutcome::Changed | InputOutcome::Unchanged => vec![],
    }
}

/// Every `Action` reachable through the registry (`resolve_action` and the agent-level handler) is allowed exactly
/// when its name is in `ALLOWED`; a newly reachable action is therefore denied until it is listed on both sides.
/// The unreached samples cover the root-session actions only pane and mouse handlers emit.
#[test]
fn registry_reachable_actions_match_allowlist() {
    let allowed: BTreeSet<&str> = ALLOWED.iter().copied().collect();
    let registry = ActionRegistry::defaults();
    let mut agent = make_agent();
    let mut reached = BTreeSet::new();
    for def in registry.all() {
        // A fresh modal slot per id so one arm's opened modal can't shadow the next
        agent.active_modal = None;
        let outcomes = [
            resolve_action(Some(def.id)),
            Some(agent.handle_agent_action_with_registry(def.id, &registry)),
        ];
        for outcome in outcomes.into_iter().flatten() {
            for action in actions_of(&outcome) {
                let name = action.as_ref();
                let expected = if allowed.contains(name) {
                    ChildActionGate::Allow
                } else {
                    ChildActionGate::Deny
                };
                assert_eq!(expected, gate_child_action(action), "{name}");
                reached.insert(name.to_owned());
            }
        }
    }
    assert!(reached.contains("SelectNext") && reached.contains("SetYoloMode"));
    for action in unreached_denied_samples() {
        let name = action.as_ref();
        assert!(
            !reached.contains(name),
            "{name} is registry-reachable; drop it from the samples"
        );
        assert_eq!(ChildActionGate::Deny, gate_child_action(&action), "{name}");
    }
}

#[test]
fn child_gate_denies_post_turn_plan_actions() {
    for action in [
        Action::ExecutePlan {
            plan_file_content: String::from("# Build it\n"),
            plan_file_uri: None,
        },
        Action::RevisePlan(String::from("revise the plan")),
        Action::SetPlanMode(crate::app::actions::PlanModeKind::Off),
    ] {
        assert_eq!(
            ChildActionGate::Deny,
            gate_child_action(&action),
            "{action:?}"
        );
        assert!(
            matches!(
                filter_child_outcome(InputOutcome::Action(action)),
                InputOutcome::Changed
            ),
            "denied plan actions must not dispatch"
        );
    }
}

#[test]
fn child_send_family_is_denied() {
    for outcome in [
        InputOutcome::Action(Action::SendPrompt(String::from("hi"))),
        InputOutcome::ActionThenForward(Action::SendPromptNow {
            text: String::from("hi"),
            images: Vec::new(),
        }),
        InputOutcome::ActionPair(
            Action::CopyBlockContent,
            Action::Interject {
                text: String::from("hi"),
                images: Vec::new(),
            },
        ),
        InputOutcome::ArmPending {
            action: Action::Rewind,
            shortcut: crate::key!(Esc),
            label: None,
            ttl: std::time::Duration::from_secs(1),
        },
    ] {
        assert!(matches!(
            filter_child_outcome(outcome),
            InputOutcome::Changed
        ));
    }
    assert!(matches!(
        filter_child_outcome(InputOutcome::Unchanged),
        InputOutcome::Unchanged
    ));
}

#[test]
fn child_navigation_and_link_actions_pass_filter() {
    for action in [
        Action::ScrollUp(1),
        Action::GotoBottom,
        Action::ToggleFold,
        Action::FocusPrompt,
        Action::FocusScrollback,
        Action::CancelTurn,
        Action::OpenUrl(String::from("https://x.ai")),
        Action::OpenNextLink,
        Action::OpenBlockViewer,
        Action::OpenScrollbackSearch(None),
        Action::Quit,
        // The takeover header's `[Dashboard]` and `‹`/`›` act for the parent
        Action::OpenDashboard,
        Action::DashboardOverlayExit,
        Action::DashboardOverlayPrev,
        Action::DashboardOverlayNext,
    ] {
        let name = action.as_ref().to_owned();
        let outcome = filter_child_outcome(InputOutcome::Action(action));
        assert!(
            matches!(outcome, InputOutcome::Action(ref a) if a.as_ref() == name),
            "{name} must pass, got {outcome:?}"
        );
    }
}

/// Every bound hidden chord dies in the child's own funnel (no modal, no `Action`) while the root still runs it; the
/// chords the child keeps (cheatsheet, cancel, tasks) are not hidden on either surface.
#[test]
fn child_funnel_blocks_hidden_chords_and_root_runs_them() {
    let registry = ActionRegistry::defaults();
    let mut parent = parent_with_child("child");
    let child = parent.subagent_view_mut("child").expect("child view");
    let hidden: Vec<ActionId> = registry
        .all()
        .iter()
        .map(|def| def.id)
        .filter(|id| ViewSurface::ChildTakeover.hides_chord(*id))
        .collect();
    assert!(hidden.contains(&ActionId::OpenSessions) && hidden.contains(&ActionId::CommandPalette));
    for id in hidden {
        assert!(!ViewSurface::Root.hides_chord(id), "{id:?}");
        child.active_modal = None;
        let outcome = child.handle_agent_action_with_registry(id, &registry);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "{id:?}: {outcome:?}"
        );
        assert!(
            child.active_modal.is_none(),
            "{id:?} opened a modal on the child"
        );
    }
    for id in [
        ActionId::ShortcutsHelp,
        ActionId::CancelTurn,
        ActionId::ToggleTasks,
    ] {
        assert!(!ViewSurface::ChildTakeover.hides_chord(id), "{id:?}");
    }
    let mut root = make_agent();
    for id in [ActionId::CommandPalette, ActionId::OpenSessions] {
        root.active_modal = None;
        root.handle_agent_action_with_registry(id, &registry);
        assert!(
            root.active_modal.is_some(),
            "{id:?} must still open on the root"
        );
    }
}
