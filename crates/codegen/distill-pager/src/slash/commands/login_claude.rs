//! Sign in to Claude (Pro/Max subscription) in your browser.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use crate::app::actions::{Action, LoginProvider};

pub struct LoginClaudeCommand;

impl SlashCommand for LoginClaudeCommand {
    slash_meta! {
        name: "login-claude",
        description: "Sign in to Claude in your browser",
        usage: "/login-claude",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::LoginProvider(LoginProvider::Claude))
    }
}
