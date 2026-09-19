//! `/logout` removes the auth credentials and returns to the login screen.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct LogoutCommand;

impl SlashCommand for LogoutCommand {
    slash_meta! {
        name: "logout",
        description: "Log out and return to the login screen",
        usage: "/logout",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::Logout)
    }
}
