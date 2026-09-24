//! `/reasoning-model`: choose the optional reasoning model the main model
//! consults for planning and review, or `clear` it so the main model works alone.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

pub struct ReasoningModelCommand;

impl SlashCommand for ReasoningModelCommand {
    slash_meta! {
        name: "reasoning-model",
        aliases: ["reasoning"],
        description: "Choose the optional reasoning model for planning and review",
        usage: "/reasoning-model <model>|clear",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model>|clear",
    }

    fn suggest_args(&self, ctx: &AppCtx, _query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }
        let mut items: Vec<ArgItem> = ctx
            .models
            .available
            .iter()
            .map(|(id, info)| ArgItem {
                display: if ctx.models.reasoning_model.as_ref() == Some(id) {
                    format!("{} (selected)", info.name)
                } else {
                    info.name.clone()
                },
                match_text: info.name.clone(),
                insert_text: info.name.clone(),
                description: info.description.clone().unwrap_or_default(),
            })
            .collect();
        items.push(ArgItem {
            display: "clear".into(),
            match_text: "clear".into(),
            insert_text: "clear".into(),
            description: "Remove the reasoning model; the main model works alone".into(),
        });
        Some(items)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /reasoning-model <model>|clear".into());
        }
        if trimmed.eq_ignore_ascii_case("clear") {
            return CommandResult::Action(Action::ClearReasoningModel);
        }
        match ctx.models.resolve_by_name_or_id(trimmed) {
            Some(id) => CommandResult::Action(Action::SetReasoningModel(id)),
            None => CommandResult::Error(format!("Unknown model: {trimmed}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::ModelState;

    /// The reasoning model is a separate, optional choice: picking it must not
    /// touch the main model, and `clear` must remove it rather than fail.
    #[test]
    fn reasoning_command_selects_or_clears_the_optional_model() {
        let mut models = ModelState::default();
        let id = agent_client_protocol::ModelId::new("chatgpt/gpt-6-sol");
        models.available.insert(
            id.clone(),
            agent_client_protocol::ModelInfo::new(id.clone(), "GPT-6-Sol"),
        );
        let bundle = crate::app::bundle::BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        assert!(matches!(
            ReasoningModelCommand.run(&mut ctx, "GPT-6-Sol"),
            CommandResult::Action(Action::SetReasoningModel(selected)) if selected == id
        ));
        assert!(matches!(
            ReasoningModelCommand.run(&mut ctx, "clear"),
            CommandResult::Action(Action::ClearReasoningModel)
        ));
    }
}
