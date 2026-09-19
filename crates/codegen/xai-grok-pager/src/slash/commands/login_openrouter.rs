//! `/login-openrouter`: the OpenRouter key's state and the cheap lane it feeds.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use super::provider_status::openrouter_status;

pub struct LoginOpenrouterCommand;

impl SlashCommand for LoginOpenrouterCommand {
    slash_meta! {
        name: "login-openrouter",
        description: "Show the OpenRouter key state and the cheap lane",
        usage: "/login-openrouter",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Message(openrouter_status())
    }
}
