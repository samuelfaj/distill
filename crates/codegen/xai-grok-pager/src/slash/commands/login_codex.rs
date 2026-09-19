//! `/login-codex`: the Codex subscription's sign-in state and the step that fixes it.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use super::provider_status::codex_status;

pub struct LoginCodexCommand;

impl SlashCommand for LoginCodexCommand {
    slash_meta! {
        name: "login-codex",
        description: "Show the Codex subscription sign-in state",
        usage: "/login-codex",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Message(codex_status())
    }
}
