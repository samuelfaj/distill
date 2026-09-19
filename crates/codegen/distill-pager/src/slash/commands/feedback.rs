//! `/feedback`: send a report inline or open the feedback modal.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Bare `/feedback` opens the feedback modal in every screen mode; `/feedback <text>` sends immediately,
/// without a model turn and without waiting on the prompt queue.
pub struct FeedbackCommand;

impl SlashCommand for FeedbackCommand {
    slash_meta! {
        name: "feedback",
        description: "Send feedback about the current session",
        usage: "/feedback [text]",
        takes_args: true,
        arg_placeholder: "[feedback text]",
    }

    fn submission_refusal(&self, args: &str, voice_owns_prompt: bool) -> Option<&'static str> {
        if args.trim().is_empty() && voice_owns_prompt {
            Some("Stop voice input before opening the feedback form")
        } else {
            None
        }
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        // Prompt dispatch removes live image elements before passing these arguments and
        // attaches the composer images to the action it gets back.
        let user_text = args.trim();
        let (action, result) = if user_text.is_empty() {
            ("open_empty", Action::OpenFeedbackModal(Default::default()))
        } else {
            (
                "send_inline",
                Action::SendFeedback {
                    text: user_text.to_owned(),
                    images: Default::default(),
                    trace: None,
                },
            )
        };
        crate::unified_log::info(
            "feedback.command",
            ctx.session_id.map(|s| s.0.as_ref()),
            Some(serde_json::json!({
                "screen_mode": ctx.screen_mode.meta_label(),
                "arg_chars": user_text.chars().count(),
                "action": action,
            })),
        );
        CommandResult::Action(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    fn make_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        let bundle = Box::leak(Box::new(crate::app::bundle::BundleState::default()));
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    /// The whitespace case matters: the composer keeps a trailing space while the user is still typing the command.
    #[test]
    fn bare_and_whitespace_open_the_empty_modal() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = FeedbackCommand;

        for args in ["", "   ", "\t"] {
            match cmd.run(&mut ctx, args) {
                CommandResult::Action(Action::OpenFeedbackModal(open)) => {
                    assert_eq!(open.text, None, "{args:?} should open empty");
                    assert!(
                        open.images.is_empty(),
                        "images attach at dispatch, not here"
                    );
                }
                other => panic!("{args:?} should open the modal, got {other:?}"),
            }
        }
    }

    #[test]
    fn inline_text_sends_immediately() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);

        match FeedbackCommand.run(&mut ctx, "  todo is chopped  ") {
            CommandResult::Action(Action::SendFeedback {
                text,
                images,
                trace,
            }) => {
                assert_eq!("todo is chopped", text);
                assert!(images.is_empty(), "images attach at dispatch, not here");
                assert_eq!(None, trace);
            }
            other => panic!("inline text must send immediately, got {other:?}"),
        }
    }
}
