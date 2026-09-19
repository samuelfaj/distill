//! `/home`: exit the current session and return to the welcome screen.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct HomeCommand;

impl SlashCommand for HomeCommand {
    slash_meta! {
        name: "home",
        aliases: ["welcome"],
        description: "Return to the welcome screen",
        usage: "/home",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ExitSession)
    }
}
