// Modified for Distill by Samuel Fajreldines, 2026.
use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct McpsCommand;

impl SlashCommand for McpsCommand {
    slash_meta! {
        name: "mcps",
        description: "Show MCP server status",
        usage: "/mcps",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: crate::views::extensions_modal::ExtensionsTab::McpServers,
            trigger: distill_telemetry::events::ExtensionsModalTrigger::SlashCommand,
        })
    }
}
