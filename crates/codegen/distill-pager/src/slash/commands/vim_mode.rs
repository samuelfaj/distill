//! `/vim-mode`: toggle vim-style scrollback keybindings.
//!
//! When off (the default), bare-letter and Shift+letter keys in the scrollback jump focus to the prompt so the letter is typed into the textarea.
//! That covers j/k, h/l, g/G, y/Y, o/O, r, x, e/E, L/H, and the `i` insert alternative.
//! Arrow/Tab/Esc/Space/PgUp/PgDn and all Ctrl+letter bindings stay active in both modes.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct VimModeCommand;

impl SlashCommand for VimModeCommand {
    slash_meta! {
        name: "vim-mode",
        description: "Toggle vim-style scrollback keybindings (j/k, h/l, g/G, y/Y, …)",
        usage: "/vim-mode",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ToggleVimMode)
    }
}
