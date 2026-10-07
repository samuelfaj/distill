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
        usage: "/worker-model <model> [effort|auto] [variant] | clear",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model> [effort|auto] [variant]",
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
        if let Some(items) = super::model::build_variant_items(&worker, query) {
            return Some(items);
        }
        if let Some(id) = super::model::detect_effort_phase(&worker, query) {
            return Some(super::model::build_effort_items_chained(&worker, &id));
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
            return CommandResult::Error(
                "Usage: /worker-model <model> [effort|auto] [variant] | clear".into(),
            );
        }
        if trimmed.eq_ignore_ascii_case("clear") {
            return CommandResult::Action(Action::ClearWorkerModel);
        }
        match super::model::parse_selection(ctx.models, trimmed) {
            Ok((id, effort, variant)) => CommandResult::Action(super::model::with_variant(
                Action::SetWorkerModel(id, effort),
                "worker_variant",
                variant,
            )),
            Err(None) => CommandResult::Error(format!(
                "Unknown model: {trimmed}. Choose a model from the list; effort defaults to auto."
            )),
            Err(Some(error)) => CommandResult::Error(error),
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

    /// The worker command persists the variant to the worker role, not the main one.
    #[test]
    fn worker_command_persists_variant_to_worker_role() {
        let mut models = ModelState::default();
        let id = agent_client_protocol::ModelId::new("or-x");
        let meta = serde_json::json!({
            "supportsReasoningEffort": true,
            "openrouter": true,
            "reasoningEfforts": [{"id": "low", "value": "low", "label": "Low"}],
        });
        models.available.insert(
            id.clone(),
            agent_client_protocol::ModelInfo::new(id.clone(), "DeepSeek V4.1 Flash")
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
        match WorkerModelCommand.run(&mut ctx, "DeepSeek V4.1 Flash low exacto") {
            CommandResult::Action(Action::WithOpenrouterVariant { key, value, then }) => {
                assert_eq!(key, "worker_variant");
                assert_eq!(value, "exacto");
                assert!(matches!(*then, Action::SetWorkerModel(m, Some(_)) if m == id));
            }
            other => panic!("expected variant action, got {other:?}"),
        }
        assert!(matches!(
            WorkerModelCommand.run(&mut ctx, "DeepSeek V4.1 Flash low bogus"),
            CommandResult::Error(_)
        ));
    }
}
