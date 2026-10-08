// Modified for Distill by Samuel Fajreldines, 2026.
//! `/ultracode [on|off]`: toggle Ultracode, which orchestrates each request across parallel subagents.
//! Independent of reasoning effort.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct UltracodeCommand;

impl SlashCommand for UltracodeCommand {
    slash_meta! {
        name: "ultracode",
        description: "Toggle Ultracode: orchestrate work across parallel subagents",
        usage: "/ultracode [on|off]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[on|off]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim().to_ascii_lowercase().as_str() {
            "" => CommandResult::Action(Action::SetUltracode(None)),
            "on" => CommandResult::Action(Action::SetUltracode(Some(true))),
            "off" => CommandResult::Action(Action::SetUltracode(Some(false))),
            other => CommandResult::Error(format!(
                "unknown argument '{other}'; usage: /ultracode [on|off]"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::{builtin_commands, tests::make_ctx};
    use crate::slash::registry::CommandRegistry;

    fn run(args: &str) -> CommandResult {
        let reg = CommandRegistry::new(builtin_commands());
        let cmd = reg.get("ultracode").expect("/ultracode is registered").clone();
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        cmd.run(&mut ctx, args)
    }

    #[test]
    fn ultracode_no_args_toggles() {
        assert!(matches!(run(""), CommandResult::Action(Action::SetUltracode(None))));
    }

    #[test]
    fn ultracode_on_off_set_explicitly() {
        assert!(matches!(run("on"), CommandResult::Action(Action::SetUltracode(Some(true)))));
        assert!(matches!(run(" OFF "), CommandResult::Action(Action::SetUltracode(Some(false)))));
    }

    #[test]
    fn ultracode_rejects_unknown_argument() {
        assert!(matches!(run("bogus"), CommandResult::Error(_)));
    }
}
