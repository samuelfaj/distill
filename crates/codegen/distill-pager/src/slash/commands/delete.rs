//! `/delete`: delete this session's history (welcome, or dashboard when attached).

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct DeleteCommand;

impl SlashCommand for DeleteCommand {
    slash_meta! {
        name: "delete",
        description: "Delete this session",
        usage: "/delete",
        session_scoped: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session to delete".into());
        }
        CommandResult::Action(Action::DeleteCurrentSession)
    }
}
