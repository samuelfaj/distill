//! `/worker-model`: choose the same-family model used for routine work.

use super::model::{parse_tier_selection, tier_suggestions};
use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

pub struct WorkerModelCommand;

impl SlashCommand for WorkerModelCommand {
    slash_meta! {
        name: "worker-model",
        aliases: ["worker"],
        description: "Choose a same-family model for routine work (default effort: auto)",
        usage: "/worker-model <model> [effort] | clear",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model> [effort]",
    }

    fn suggest_args(&self, ctx: &AppCtx, query: &str) -> Option<Vec<ArgItem>> {
        Some(tier_suggestions(ctx.models, query, true))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if args.trim() == "clear" {
            return CommandResult::Action(Action::SetTierLight(String::new(), None));
        }
        match parse_tier_selection(ctx.models, args) {
            Ok((model, effort)) => CommandResult::Action(Action::SetTierLight(model, effort)),
            Err(error) => CommandResult::Error(error),
        }
    }
}
