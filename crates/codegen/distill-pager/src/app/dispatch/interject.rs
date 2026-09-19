//! Mid-turn interjection dispatch: optimistic local echo, the `x.ai/interject` effect, and prompt-history recording.

use super::ctx::NO_SESSION_NOTICE;
use super::voice::voice_stop_on_submit;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;

/// Send a mid-turn interjection.
/// Pushes a standard user prompt block locally for instant feedback, records the text in prompt history, and clears the prompt.
/// The id is recorded in `self_interjection_ids` so `handle_interjection` drops the echo instead of rendering a duplicate.
pub(super) fn dispatch_interject(
    app: &mut AppView,
    text: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    dispatch_interject_on(app, id, text, images)
}

/// Same as [`dispatch_interject`], but targets `agent_id` instead of the focused pane.
/// Used when a `/btw` answer lands on a non-active session.
pub(super) fn dispatch_interject_on(
    app: &mut AppView,
    id: AgentId,
    text: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    dispatch_interject_on_inner(app, id, text, images, /* user_submit */ true)
}

/// Wait-start / Enter flush: same wire send, without submit side effects (voice stop, history, toast).
pub(super) fn dispatch_interject_from_held_queue(
    app: &mut AppView,
    id: AgentId,
    text: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    dispatch_interject_on_inner(app, id, text, images, /* user_submit */ false)
}

fn dispatch_interject_on_inner(
    app: &mut AppView,
    id: AgentId,
    text: String,
    images: Vec<crate::prompt_images::PastedImage>,
    user_submit: bool,
) -> Vec<Effect> {
    // Voice is app-wide and bound to the focused composer
    // A /btw answer on another session must not commit interim text or kill dictation on the pane the user is actually talking into
    if user_submit && matches!(app.active_view, ActiveView::Agent(active) if active == id) {
        // Hard-reset only; `text` may not be from the composer
        let _ = voice_stop_on_submit(app);
    }
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    if user_submit {
        agent.ephemeral_tip.clear_on_submit();
        agent.release_hook_block_hold();
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast(NO_SESSION_NOTICE);
        return vec![];
    };

    if user_submit {
        agent.record_prompt_in_history(&text);
    }

    // Push a standard user prompt block locally for instant feedback
    // Record its id so the broadcast echo (`x.ai/session/interjection`) is deduped instead of rendering a second copy on this pane
    let interjection_id = uuid::Uuid::new_v4().to_string();
    agent.self_interjection_ids.insert(interjection_id.clone());
    let entry_id = agent
        .scrollback
        .push_block(RenderBlock::interjection_prompt(&text));
    agent
        .interjection_painted_blocks
        .insert(interjection_id.clone(), entry_id);
    agent
        .interjection_retry_images
        .insert(interjection_id.clone(), images.clone());

    // The composer is NOT touched here: the producer that consumed composer text (the InterjectPrompt registry arm) clears it at the call site
    // Every other producer (Send now, edit-interject, plan review comments) carries non-composer text and must keep the user's draft/stash
    if user_submit {
        agent.show_toast("Interjection sent");
    }

    // Image-bearing interjection: build text and image content blocks via the same helper as the queued-prompt drain path
    // The helper covers orphan-placeholder recovery, the allowlist, and the size cap. Text-only stays on the legacy wire.
    let blocks = if images.is_empty() {
        None
    } else {
        Some(crate::prompt_images::build_content_blocks_with_workspace(
            text.clone(),
            images,
            Some(std::path::Path::new(&agent.session.cwd)),
        ))
    };

    vec![Effect::SendInterject {
        agent_id: id,
        session_id,
        text,
        interjection_id,
        blocks,
    }]
}

/// Cancel-and-send: send `text` (and images) as a fresh `sendNow` prompt so the shell cancels the running turn and runs it next.
/// The user block paints at dispatch (the arm hides the queue echo; the adoption reuses the block).
pub(super) fn dispatch_send_prompt_now(
    app: &mut AppView,
    text: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    // Hard-reset only; `text` may be a queue row, not the composer
    let _ = voice_stop_on_submit(app);
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let reconnect_pending = app.reconnect_pending;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    agent.release_hook_block_hold();
    // Composer / queue-row / paste Send now all land here, not in dispatch_send_prompt_inner. Drop the credit-limit stash so Try Again cannot resubmit the blocked prompt after the user has moved on.
    // dispatch_send_prompt_inner. Drop the credit-limit stash so Try Again
    // cannot resubmit the blocked prompt after the user has moved on.
    agent.credit_limit_stashed_prompt = None;

    // Mid-outage guard (mirrors the plain prompt path): the producers already consumed the payload (composer text / queue row)
    // Requeue it locally instead of firing into a dead channel and losing the message
    if reconnect_pending {
        let queue_id = agent.session.next_queue_id;
        agent.session.next_queue_id += 1;
        agent
            .session
            .pending_prompts
            .push_front(crate::app::agent::QueuedPrompt {
                images,
                ..crate::app::agent::QueuedPrompt::plain(
                    queue_id,
                    &text,
                    crate::app::agent::QueueEntryKind::Prompt,
                )
            });
        agent.show_toast("Reconnecting, please wait...");
        return vec![];
    }

    // Submitting retires any edit-contextual ephemeral tip.
    agent.ephemeral_tip.clear_on_submit();

    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast(NO_SESSION_NOTICE);
        return vec![];
    };

    agent.record_prompt_in_history(&text);

    let prompt_id = uuid::Uuid::new_v4().to_string();
    // Self-originated: the ACP gate must treat this prompt's deltas as ours.
    agent.note_self_originated_prompt(&prompt_id);
    // Expect the shell's send-now cancel so the turn-end handling suppresses its marker
    super::queue::arm_send_now_and_paint_dispatched(agent, &prompt_id, &text);

    let blocks = crate::prompt_images::build_content_blocks_with_workspace(
        text.clone(),
        images,
        Some(std::path::Path::new(&agent.session.cwd)),
    );

    // Optimistic queue-pane echo, reconciled by the shell's queue broadcast.
    let sid_str = session_id.0.to_string();
    super::queue::push_server_queue_echo(app, id, &sid_str, &prompt_id, &text, "prompt");
    crate::unified_log::info(
        "prompt.send_now",
        Some(&sid_str),
        Some(serde_json::json!({ "len": text.len(), "prompt_id": prompt_id })),
    );

    vec![Effect::SendPromptNow {
        agent_id: id,
        session_id,
        blocks,
        prompt_id,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::actions::Action;
    use crate::app::agent::AgentId;
    use crate::app::dispatch::router::dispatch;
    use crate::app::dispatch::tests::test_app_with_agent;
    use agent_client_protocol as acp;

    /// Composer-clear ownership: dispatch NEVER touches the composer.
    /// The only composer-text producer (the InterjectPrompt registry arm) clears it at the call site.
    /// Every other producer (Send now, edit-interject, plan review comments) carries non-composer text whose draft/stash must survive dispatch.
    #[test]
    fn interject_dispatch_never_touches_the_composer() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Unrelated draft survives a plain interject.
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .set_text("stashed draft");
        let effects = dispatch(
            Action::Interject {
                text: "edited body".into(),
                images: vec![],
            },
            &mut app,
        );
        assert!(matches!(effects.as_slice(), [Effect::SendInterject { .. }]));
        assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "stashed draft");

        // Edited-queued interject: fire-and-forget, composer untouched.
        let effects = dispatch(
            Action::QueueInterjectShared {
                id: "p1".into(),
                expected_version: 1,
                new_text: Some("edited body".into()),
            },
            &mut app,
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::QueueInterject { .. }]
        ));
        assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "stashed draft");

        // Even a composer that equals the interjected text is preserved; the InterjectPrompt arm already cleared it for the composer path
        app.agents.get_mut(&id).unwrap().prompt.set_text("send me");
        let _ = dispatch(
            Action::Interject {
                text: "send me".into(),
                images: vec![],
            },
            &mut app,
        );
        assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "send me");
    }

    /// Interjecting is a submit: it retires the active ephemeral tip.
    #[test]
    fn interject_clears_active_ephemeral_tip() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        let agent = app.agents.get_mut(&id).unwrap();
        let _ = agent.ephemeral_tip.show(
            crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint")),
            &mut std::collections::HashMap::new(),
        );
        assert!(agent.ephemeral_tip.is_active());

        let _ = dispatch(
            Action::Interject {
                text: "mid-turn note".into(),
                images: vec![],
            },
            &mut app,
        );
        assert!(
            !app.agents.get(&id).unwrap().ephemeral_tip.is_active(),
            "interject submit must clear the tip"
        );
    }

    /// A no-session interject still retires the tip: the clear runs before the "No active session" early return, matching the other submit paths.
    #[test]
    fn interject_without_session_still_clears_ephemeral_tip() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.session_id = None;
        let _ = agent.ephemeral_tip.show(
            crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint")),
            &mut std::collections::HashMap::new(),
        );
        assert!(agent.ephemeral_tip.is_active());

        let effects = dispatch(
            Action::Interject {
                text: "mid-turn note".into(),
                images: vec![],
            },
            &mut app,
        );

        let agent = app.agents.get(&id).unwrap();
        assert!(
            !agent.ephemeral_tip.is_active(),
            "no-session interject must still clear the tip"
        );
        assert!(
            effects.is_empty(),
            "no-session interject dispatches no effects"
        );
        assert_eq!(
            agent.toast.as_ref().map(|(m, _)| m.as_str()),
            Some("No active session"),
            "no-session interject takes the 'No active session' path"
        );
    }

    /// Image-bearing interject builds structured blocks (Text first with the placeholder intact, then one Image block).
    /// No-image stays legacy (`blocks: None`) so the wire shape is byte-identical.
    #[test]
    fn interject_with_images_builds_blocks_text_first() {
        let mut app = test_app_with_agent();

        let mut img = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
            data: vec![1, 2, 3],
            mime_type: "image/png".into(),
        });
        img.display_number = 1;

        let effects = dispatch(
            Action::Interject {
                text: "look at [Image #1] please".into(),
                images: vec![img],
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::SendInterject {
                    text,
                    blocks: Some(blocks),
                    ..
                },
            ] => {
                assert_eq!(text, "look at [Image #1] please");
                assert_eq!(blocks.len(), 2);
                match &blocks.first().unwrap_or_else(|| panic!("missing index")) {
                    acp::ContentBlock::Text(tb) => {
                        assert!(tb.text.contains("[Image #1]"), "got {:?}", tb.text)
                    }
                    other => panic!("expected Text first, got {other:?}"),
                }
                assert!(matches!(
                    &blocks.get(1).unwrap_or_else(|| panic!("missing index")),
                    acp::ContentBlock::Image(_)
                ));
            }
            other => panic!("expected SendInterject with blocks, got {other:?}"),
        }

        let effects = dispatch(
            Action::Interject {
                text: "plain".into(),
                images: vec![],
            },
            &mut app,
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::SendInterject { blocks: None, .. }]
        ));
    }
}
