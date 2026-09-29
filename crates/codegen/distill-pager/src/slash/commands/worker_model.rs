//! `/worker-model`: choose the optional worker model that runs the work the
//! main model delegates, and its effort, or `clear` it so the main model does
//! all the work.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

pub struct WorkerModelCommand;

impl SlashCommand for WorkerModelCommand {
    slash_meta! {
        name: "worker-model",
        aliases: ["worker"],
        description: "Choose the optional worker model and its effort for delegated work",
        usage: "/worker-model <model> [effort|auto] | clear",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model> [effort|auto]",
    }

    fn suggest_args(&self, ctx: &AppCtx, query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }
        // The same model-then-effort picker as `/model`, marked against the
        // worker's own saved model and effort.
        let mut worker = ctx.models.clone();
        worker.current = ctx.models.worker_model.clone();
        worker.reasoning_effort = ctx.models.worker_effort;
        worker.effort_auto = ctx.models.worker_effort.is_none();
        if let Some(id) = super::model::detect_effort_phase(&worker, query) {
            return Some(super::model::build_effort_items(&worker, &id));
        }
        let mut items = super::model::build_model_items(&worker);
        items.push(ArgItem {
            display: "clear".into(),
            match_text: "clear".into(),
            insert_text: "clear".into(),
            description: "Remove the worker model; the main model does all the work".into(),
        });
        Some(items)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /worker-model <model> [effort|auto] | clear".into());
        }
        if trimmed.eq_ignore_ascii_case("clear") {
            return CommandResult::Action(Action::ClearWorkerModel);
        }
        match super::model::parse_tier_selection(ctx.models, trimmed) {
            Ok((model, effort)) => CommandResult::Action(Action::SetWorkerModel(
                agent_client_protocol::ModelId::new(model),
                effort,
            )),
            Err(error) => CommandResult::Error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::ModelState;

    /// The worker model is a separate, optional choice: picking it must not
    /// touch the main model, a trailing effort pins the worker's effort, `auto`
    /// or no effort leaves it to Jev, and `clear` removes the worker.
    #[test]
    fn worker_command_selects_a_model_and_effort_or_clears_it() {
        let mut models = ModelState::default();
        let id = agent_client_protocol::ModelId::new("chatgpt/gpt-6-luna");
        let meta = serde_json::json!({
            "supportsReasoningEffort": true,
            "reasoningEfforts": [
                {"id": "low", "value": "low", "label": "Low"},
                {"id": "medium", "value": "medium", "label": "Medium"},
            ],
        });
        models.available.insert(
            id.clone(),
            agent_client_protocol::ModelInfo::new(id.clone(), "GPT-6-Luna")
                .meta(meta.as_object().cloned()),
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
            WorkerModelCommand.run(&mut ctx, "GPT-6-Luna"),
            CommandResult::Action(Action::SetWorkerModel(selected, None)) if selected == id
        ));
        assert!(matches!(
            WorkerModelCommand.run(&mut ctx, "GPT-6-Luna auto"),
            CommandResult::Action(Action::SetWorkerModel(selected, None)) if selected == id
        ));
        assert!(matches!(
            WorkerModelCommand.run(&mut ctx, "GPT-6-Luna medium"),
            CommandResult::Action(Action::SetWorkerModel(
                selected,
                Some(distill_shell::sampling::types::ReasoningEffort::Medium)
            )) if selected == id
        ));
        assert!(
            matches!(WorkerModelCommand.run(&mut ctx, "GPT-6-Luna max"), CommandResult::Error(_)),
            "a level the model does not offer is refused"
        );
        assert!(matches!(
            WorkerModelCommand.run(&mut ctx, "clear"),
            CommandResult::Action(Action::ClearWorkerModel)
        ));
    }
}
