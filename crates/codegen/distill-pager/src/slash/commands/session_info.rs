//! `/session-info`: show current session info (instant, not queued).

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Show session info (session ID, cwd, model, context usage).
pub struct SessionInfoCommand;

impl SlashCommand for SessionInfoCommand {
    slash_meta! {
        name: "session-info",
        description: "Show session info",
        usage: "/session-info",
        session_scoped: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }

        CommandResult::Action(Action::ShowSessionInfo)
    }
}
