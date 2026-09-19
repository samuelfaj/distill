//! `/import-claude` opens the interactive Claude settings import modal.
//!
//! This is the in-session entry point: the slash command dispatches the shared `Action::ImportClaudeSettings` action.
//! The dispatch handler scans `.claude/settings*.json`, `~/.claude.json`, and `.mcp.json` and populates the modal state.
//! The agent view overlays the modal on top of the active session, the same selection UI as the welcome screen's Ctrl-I shortcut.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Open the interactive Claude settings import modal in the active session.
pub struct ImportClaudeCommand;

impl SlashCommand for ImportClaudeCommand {
    slash_meta! {
        name: "import-claude",
        description: "Open the Claude settings import modal",
        usage: "/import-claude",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if !trimmed.is_empty() {
            tracing::warn!("/import-claude does not accept arguments; ignoring");
        }
        CommandResult::Action(Action::ImportClaudeSettings)
    }
}
