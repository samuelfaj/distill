//! `/side`: start an ephemeral side chat forked from the active session.
//!
//! Unlike `/fork`, a side chat never leaves the parent session, is not persisted, and
//! always forks in the current cwd (never a worktree). Optional inline text becomes the
//! side chat's first prompt. The dispatch layer (`dispatch_side`) emits the fork effect
//! with `sessionKind: "side"`.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct SideCommand;

impl SlashCommand for SideCommand {
    slash_meta! {
        name: "side",
        description: "Start an ephemeral side chat without leaving this session",
        usage: "/side [text]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[text]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let text = args.trim();
        CommandResult::Action(Action::Side {
            directive: (!text.is_empty()).then(|| text.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::ScreenMode;
    use crate::app::actions::Action;
    use crate::app::bundle::BundleState;
    use crate::slash::command::CommandResult;

    fn run(args: &str) -> CommandResult {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: ScreenMode::Fullscreen,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        SideCommand.run(&mut ctx, args)
    }

    #[test]
    fn metadata_matches_design() {
        let cmd = SideCommand;
        assert_eq!(cmd.name(), "side");
        assert!(cmd.takes_args(), "/side accepts args");
        assert!(!cmd.args_required(), "/side allows no args");
        assert_eq!(cmd.arg_placeholder(), Some("[text]"));
    }

    #[test]
    fn empty_args_produce_no_directive() {
        match run("   ") {
            CommandResult::Action(Action::Side { directive }) => assert!(directive.is_none()),
            other => panic!("expected Side action, got {other:?}"),
        }
    }

    #[test]
    fn text_becomes_the_directive() {
        match run("  explain the parser  ") {
            CommandResult::Action(Action::Side { directive }) => {
                assert_eq!(directive.as_deref(), Some("explain the parser"))
            }
            other => panic!("expected Side action, got {other:?}"),
        }
    }
}
