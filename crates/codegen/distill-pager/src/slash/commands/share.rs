use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Share the current session via a public URL.
pub struct ShareCommand;

impl SlashCommand for ShareCommand {
    slash_meta! {
        name: "share",
        description: "Share this session via URL",
        usage: "/share",
        session_scoped: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let _ = ctx;
        CommandResult::Error("Session sharing is temporarily disabled".to_string())
    }
}
