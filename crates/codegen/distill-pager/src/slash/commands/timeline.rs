//! `/timeline`: toggle the timeline sidebar (per-turn tick rail).
//!
//! Computes the new value itself and dispatches the typed `Action::SetTimeline(bool)`, mirroring `/timestamps`.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use crate::slash::{ModeSupport, Remedy};

pub struct TimelineCommand;

impl SlashCommand for TimelineCommand {
    slash_meta! {
        name: "timeline",
        description: "Toggle the timeline sidebar",
        usage: "/timeline",
        mode_support: ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "the timeline rail needs the interactive scrollback pane",
        }),
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let new = !crate::appearance::cache::load_show_timeline();
        CommandResult::Action(Action::SetTimeline(new))
    }
}
