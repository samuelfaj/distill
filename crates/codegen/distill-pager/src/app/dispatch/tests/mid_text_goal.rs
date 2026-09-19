//! Mid-message `/goal` toasts and does not start a turn.

use super::*;
use crate::slash::mid_text_hoist::MID_TEXT_GOAL_NOTICE;
use pretty_assertions::assert_eq;

const TESLA: &str = "here are the crashes\n\n/goal figure out why these sessions crash";

fn register_goal(app: &mut AppView, id: AgentId) {
    let agent = app.agents.get_mut(&id).unwrap();
    let models = agent.session.models.clone();
    agent.prompt.sync_acp_commands(
        &[acp::AvailableCommand::new(
            "goal",
            "Set, manage, or check an autonomous goal",
        )],
        None,
        &models,
    );
}

fn toast_text(app: &AppView, id: AgentId) -> Option<&str> {
    test_agent(app, id)
        .toast
        .as_ref()
        .map(|(message, _)| message.as_str())
}

fn assert_mid_text_goal_refused(app: &AppView, id: AgentId, effects: &[Effect], kept: &str) {
    assert!(
        effects.is_empty(),
        "mid-text /goal must not start a turn, got {effects:?}"
    );
    assert_eq!(Some(MID_TEXT_GOAL_NOTICE), toast_text(app, id));
    let agent = test_agent(app, id);
    assert_eq!(kept, agent.prompt.text());
    assert!(agent.scrollback.is_empty());
    assert!(agent.btw_state.is_none());
    assert!(agent.session.prompt_history.is_empty());
}

#[test]
fn mid_text_goal_toasts_and_does_not_send() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_goal(&mut app, id);
    test_agent_mut(&mut app, id).prompt.set_text(TESLA);

    let effects = dispatch(Action::SendPrompt(TESLA.to_owned()), &mut app);

    assert_mid_text_goal_refused(&app, id, &effects, TESLA);
}

/// `/btw` hoist must not run first: `context /goal … /btw …` would become
/// `/btw context /goal …` and skip the toast.
#[test]
fn mid_text_goal_with_btw_toasts_and_does_not_hoist() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_goal(&mut app, id);
    const TYPED: &str = "context /goal investigate /btw why";
    test_agent_mut(&mut app, id).prompt.set_text(TYPED);

    let effects = dispatch(Action::SendPrompt(TYPED.to_owned()), &mut app);

    assert_mid_text_goal_refused(&app, id, &effects, TYPED);
}

#[test]
fn mid_text_btw_then_goal_toasts_and_does_not_hoist() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_goal(&mut app, id);
    const TYPED: &str = "context /btw why /goal investigate";
    test_agent_mut(&mut app, id).prompt.set_text(TYPED);

    let effects = dispatch(Action::SendPrompt(TYPED.to_owned()), &mut app);

    assert_mid_text_goal_refused(&app, id, &effects, TYPED);
}

#[test]
fn leading_goal_still_sends() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_goal(&mut app, id);

    let effects = dispatch(
        Action::SendPrompt("/goal figure out why these sessions crash".into()),
        &mut app,
    );

    assert!(toast_text(&app, id).is_none());
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }]
                if text == "/goal figure out why these sessions crash"
        ),
        "leading /goal must still pass through, got {effects:?}"
    );
}

#[test]
fn mid_text_goal_without_acp_command_stays_a_prompt() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt(TESLA.to_owned()), &mut app);

    assert!(toast_text(&app, id).is_none());
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == TESLA
        ),
        "unknown /goal is ordinary chat, got {effects:?}"
    );
}

#[test]
fn literal_follow_up_with_goal_is_not_toasted() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_goal(&mut app, id);

    let effects = dispatch(Action::SubmitFollowUp(TESLA.to_owned()), &mut app);

    assert!(toast_text(&app, id).is_none());
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == TESLA
        ),
        "{effects:?}"
    );
}

#[test]
fn mid_text_goal_in_minimal_is_a_system_line() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(0);
    register_goal(&mut app, id);

    let effects = dispatch(Action::SendPrompt(TESLA.to_owned()), &mut app);

    assert!(effects.is_empty(), "{effects:?}");
    assert!(toast_text(&app, id).is_none());
    assert_eq!(MID_TEXT_GOAL_NOTICE, last_system_text(&app, id));
}
