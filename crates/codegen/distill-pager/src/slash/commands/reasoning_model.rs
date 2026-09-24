//! `/reasoning-model`: choose the secondary model for deep reasoning and review.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

pub struct ReasoningModelCommand;

impl SlashCommand for ReasoningModelCommand {
    slash_meta! {
        name: "reasoning-model",
        aliases: ["reasoning"],
        description: "Choose the secondary model for planning and review",
        usage: "/reasoning-model <model>",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model>",
    }

    fn suggest_args(&self, ctx: &AppCtx, _query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }
        Some(
            ctx.models
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
                .collect(),
        )
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /reasoning-model <model>".into());
        }
        match ctx.models.resolve_by_name_or_id(trimmed) {
            Some(id) => CommandResult::Action(Action::SetDefaultModel(id)),
            None => CommandResult::Error(format!("Unknown model: {trimmed}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::ModelState;

    #[test]
    fn reasoning_command_selects_secondary_model() {
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
            CommandResult::Action(Action::SetDefaultModel(selected)) if selected == id
        ));
    }
}
