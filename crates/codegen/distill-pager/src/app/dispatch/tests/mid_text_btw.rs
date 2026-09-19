//! A mid-message `/btw` token hoists the whole submission into the side question.

use super::*;
use crate::views::btw_overlay::BtwOverlayState;
use pretty_assertions::assert_eq;

const TYPED: &str = "explain the controller. /btw what is a WBC";
const QUESTION: &str = "explain the controller. what is a WBC";

fn assert_fullscreen_side_question(effects: &[Effect], question: &str) {
    assert!(
        matches!(
            effects,
            [Effect::SendBtw { question: sent, minimal_request_id: None, .. }] if sent == question
        ),
        "expected exactly one fullscreen /btw send of {question:?}, got {effects:?}"
    );
}

#[test]
fn mid_text_btw_sends_whole_message_as_side_question() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt(TYPED.to_owned()), &mut app);

    assert_fullscreen_side_question(&effects, QUESTION);
    let agent = test_agent(&app, id);
    assert!(
        matches!(
            &agent.btw_state,
            Some(BtwOverlayState::Loading { question }) if question == QUESTION
        ),
        "{:?}",
        agent.btw_state
    );
    assert_eq!("", agent.prompt.text());
}

#[test]
fn mid_text_btw_while_turn_running_bypasses_queue() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(Action::SendPrompt(TYPED.to_owned()), &mut app);

    assert_fullscreen_side_question(&effects, QUESTION);
    assert!(test_agent(&app, id).session.pending_prompts.is_empty());
}

#[test]
fn mid_text_btw_history_keeps_typed_text() {
    let mut app = test_app_with_agent();

    dispatch(Action::SendPrompt(TYPED.to_owned()), &mut app);

    assert_eq!(
        vec![TYPED.to_owned()],
        test_agent(&app, AgentId(0)).session.prompt_history
    );
}

#[test]
fn mid_text_btw_sends_composer_images() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("look /btw q ");
        let end = agent.prompt.text().len();
        agent.prompt.set_cursor(end);
        agent
            .prompt
            .insert_image(crate::app::agent_view::test_fixtures::test_pasted_image())
            .expect("chip");
    }
    let composed = test_agent(&app, id).prompt.text().to_owned();

    let effects = dispatch(Action::SendPrompt(composed), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendBtw { question, images, minimal_request_id: None, .. }]
                if question == "[Image] look q" && images.len() == 1
        ),
        "{effects:?}"
    );
    let agent = test_agent(&app, id);
    assert!(agent.prompt.images.is_empty());
    assert_eq!("", agent.prompt.text());
}

/// Paste first, then type `/btw q`: the raw text starts with the chip, not `/`, so only the hoist
/// can make it a side question.
#[test]
fn leading_image_chip_then_btw_is_a_side_question() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .prompt
            .insert_image(crate::app::agent_view::test_fixtures::test_pasted_image())
            .expect("chip");
        agent.prompt.textarea.insert_str("/btw q");
    }
    let composed = test_agent(&app, id).prompt.text().to_owned();
    assert_eq!("[Image #1] /btw q", composed);

    let effects = dispatch(Action::SendPrompt(composed), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendBtw { question, images, minimal_request_id: None, .. }]
                if question == "[Image] q" && images.len() == 1
        ),
        "{effects:?}"
    );
    let agent = test_agent(&app, id);
    assert!(agent.prompt.images.is_empty());
    assert_eq!("", agent.prompt.text());
}

/// The stripped text starts with `/nope`; the args must come from the hoisted line, not from it.
#[test]
fn leading_image_chip_then_unknown_command_then_btw_hoists_whole_line() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .prompt
            .insert_image(crate::app::agent_view::test_fixtures::test_pasted_image())
            .expect("chip");
        agent.prompt.textarea.insert_str("/nope hi /btw q");
    }
    let composed = test_agent(&app, id).prompt.text().to_owned();
    assert_eq!("[Image #1] /nope hi /btw q", composed);

    let effects = dispatch(Action::SendPrompt(composed), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendBtw { question, images, minimal_request_id: None, .. }]
                if question == "[Image] /nope hi q" && images.len() == 1
        ),
        "{effects:?}"
    );
}

#[test]
fn leading_unknown_command_with_btw_is_not_hoisted() {
    let mut app = test_app_with_agent();

    let effects = dispatch(Action::SendPrompt("/nope hi /btw q".to_owned()), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == "/nope hi /btw q"
        ),
        "{effects:?}"
    );
}

#[test]
fn literal_follow_up_with_btw_is_not_hoisted() {
    let mut app = test_app_with_agent();

    let effects = dispatch(Action::SubmitFollowUp("prose /btw q".to_owned()), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == "prose /btw q"
        ),
        "{effects:?}"
    );
}

#[test]
fn mid_text_other_builtin_still_passes_through() {
    let mut app = test_app_with_agent();

    let effects = dispatch(Action::SendPrompt("great /compact go".to_owned()), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == "great /compact go"
        ),
        "{effects:?}"
    );
}
