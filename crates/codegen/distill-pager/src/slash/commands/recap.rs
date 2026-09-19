//! `/recap` (alias `/summarize`): summarize the session so far ("where was I").
//!
//! Returns `CommandResult::Action(Action::SendRecap { auto: false })`.
//! The dispatch layer fires it as the ACP ext method `x.ai/recap`, which bypasses the prompt queue.
//! The recap arrives asynchronously as a scrollback line and is never added to the model conversation.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct RecapCommand;

impl SlashCommand for RecapCommand {
    slash_meta! {
        name: "recap",
        aliases: ["summarize"],
        description: "Summarize the session so far",
        usage: "/recap",
        session_scoped: true,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::SendRecap { auto: false })
    }
}
