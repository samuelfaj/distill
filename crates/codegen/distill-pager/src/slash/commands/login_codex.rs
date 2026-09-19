//! Sign in to ChatGPT in your browser.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use crate::app::actions::{Action, LoginProvider};

pub struct LoginCodexCommand;

impl SlashCommand for LoginCodexCommand {
    slash_meta! {
        name: "login-codex",
        description: "Sign in to ChatGPT in your browser",
        usage: "/login-codex",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::LoginProvider(LoginProvider::ChatGpt))
    }
}
