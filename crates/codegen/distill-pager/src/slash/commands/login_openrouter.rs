//! Sign in to OpenRouter in your browser.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use crate::app::actions::{Action, LoginProvider};

pub struct LoginOpenrouterCommand;

impl SlashCommand for LoginOpenrouterCommand {
    slash_meta! {
        name: "login-openrouter",
        description: "Sign in to OpenRouter in your browser",
        usage: "/login-openrouter",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::LoginProvider(LoginProvider::OpenRouter))
    }
}
