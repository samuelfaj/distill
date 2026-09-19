// Modified for Distill by Samuel Fajreldines, 2026.
//! `/effort`: set reasoning effort on the current model without re-picking it.
//!
//! Thin wrapper over `Action::SwitchModel` with the session's current model id and the chosen effort (same wire path as `/model <name> <effort>`).

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::commands::effort_levels::{EFFORT_AUTO_ID, build_effort_arg_items};

/// Set reasoning effort for the active model.
pub struct EffortCommand;

impl SlashCommand for EffortCommand {
    slash_meta! {
        name: "effort",
        description: "Set reasoning effort for the current model",
        // Levels are model-specific; empty-args and UnknownToken errors list the active model's offered option ids instead of a hardcoded set.
        usage: "/effort <level>",
        takes_args: true,
        args_required: true,
        session_scoped: true,
        arg_placeholder: "<level>",
    }

    fn suggest_args(&self, ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let options = ctx.models.reasoning_effort_options();
        if ctx.models.current.is_none() {
            return None;
        }
        let mut items = vec![crate::slash::commands::effort_levels::effort_auto_arg_item(
            ctx.models.effort_auto,
        )];
        items.extend(build_effort_arg_items(
            &options,
            ctx.models.reasoning_effort,
            !ctx.models.effort_auto,
            |option| option.id.clone(),
        ));
        Some(items)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };

        // `auto` is a mode, not a level: it asks the harness to let the decision
        // layer choose the effort for each model call. It is always available,
        // even when the model sends no effort menu of its own.
        if trimmed.eq_ignore_ascii_case(EFFORT_AUTO_ID) {
            return CommandResult::Action(Action::SetEffortAuto { model_id });
        }

        if trimmed.is_empty() {
            let offered: Vec<String> = ctx
                .models
                .reasoning_effort_options_for(&model_id)
                .into_iter()
                .map(|opt| opt.id)
                .collect();
            let current = ctx
                .models
                .reasoning_effort
                .map(|e| format!(" (current: {e})"))
                .unwrap_or_default();
            let levels = if offered.is_empty() {
                "<level>".to_string()
            } else {
                offered.join("|")
            };
            return CommandResult::Error(format!(
                "Usage: /effort <{EFFORT_AUTO_ID}|{levels}>{current}"
            ));
        }

        // Same gate-first policy as the CLI (`--effort`) and headless.
        match ctx.models.resolve_effort_for_model(&model_id, trimmed) {
            Ok(effort) => CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: Some(effort),
            }),
            Err(err) => CommandResult::Error(err.message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::effort_levels::EFFORT_LEVELS;
    use agent_client_protocol as acp;
    use distill_shell::sampling::types::ReasoningEffort;
    use std::sync::Arc;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn plain_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
    }

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    /// Command context for suggestion tests (the palette path).
    fn app_ctx(models: &ModelState) -> AppCtx<'_> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: false,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Inline,
            current_title: None,
        }
    }

    #[test]
    fn empty_args_errors_with_usage() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        state.reasoning_effort = Some(ReasoningEffort::Medium);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Usage: /effort"));
                // Legacy menu option ids only, not none/minimal
                assert!(msg.contains("xhigh|high|medium|low"), "msg={msg}");
                assert!(msg.contains("current: medium"));
                assert!(!msg.contains("none"));
                assert!(!msg.contains("minimal"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_level_errors() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "turbo");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("unknown effort level 'turbo'"), "msg={msg}");
                assert!(msg.contains("use one of:"), "msg={msg}");
                assert!(msg.contains("xhigh"), "msg={msg}");
                assert!(!msg.contains("none"), "msg={msg}");
                assert!(!msg.contains("minimal"), "msg={msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn valid_level_dispatches_switch_model_on_current() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::High));
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    /// `/effort auto` is a mode, not a level: it must dispatch the auto action
    /// even for a model whose menu offers nothing, and it must show up in the
    /// palette marked active once it is on.
    #[test]
    fn auto_dispatches_the_auto_action_and_marks_the_palette() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("plain"));
        state
            .available
            .insert(id.clone(), plain_model("plain", "Plain").1);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        match EffortCommand.run(&mut ctx, "auto") {
            CommandResult::Action(Action::SetEffortAuto { model_id }) => {
                assert_eq!(model_id, id, "auto applies to the current model")
            }
            other => panic!("expected SetEffortAuto, got {other:?}"),
        }
        match EffortCommand.run(&mut ctx, "AUTO") {
            CommandResult::Action(Action::SetEffortAuto { .. }) => {}
            other => panic!("auto is case-insensitive, got {other:?}"),
        }

        // The palette always offers the auto row, first, and marks it active.
        let (reasoning_id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(reasoning_id.clone(), info);
        state.current = Some(reasoning_id);
        let items = EffortCommand
            .suggest_args(&app_ctx(&state), "")
            .expect("the menu is offered");
        assert_eq!(items[0].insert_text, "auto", "auto leads the menu");
        assert!(
            !items[0].display.contains("(active)"),
            "auto is not active until it is chosen: {:?}",
            items[0].display
        );
        state.effort_auto = true;
        let items = EffortCommand
            .suggest_args(&app_ctx(&state), "")
            .expect("menu");
        assert!(
            items[0].display.contains("(active)"),
            "auto reads active once chosen: {:?}",
            items[0].display
        );
        assert!(
            items[1].display.contains("xhigh"),
            "the model's own levels follow the auto row: {:?}",
            items[1].display
        );
    }

    #[test]
    fn none_and_minimal_rejected_when_model_menu_omits_them() {
        // The legacy fallback menu is low..xhigh; `none`/`minimal` used to pass through and 400 on grok-4.5, so reject at the TUI instead
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        for token in ["none", "minimal"] {
            let result = EffortCommand.run(&mut ctx, token);
            match result {
                CommandResult::Error(ref msg) => {
                    assert!(
                        msg.contains(&format!("unknown effort level '{token}'")),
                        "expected Error for {token}, got {msg}"
                    );
                    // The error must not re-advertise the rejected token as a valid choice (aside from quoting it in "unknown effort level '…'")
                    let after_prefix = msg
                        .split_once("; ")
                        .map(|(_, rest)| rest)
                        .unwrap_or(msg.as_str());
                    assert!(
                        !after_prefix.contains(token),
                        "error must not list {token} as offered: {msg}"
                    );
                    assert!(!msg.contains("unset"), "msg={msg}");
                }
                other => panic!("expected Error for {token}, got {other:?}"),
            }
        }
    }

    #[test]
    fn none_accepted_when_model_menu_offers_it() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("voice-dual"));
        let info = acp::ModelInfo::new(id.clone(), "Voice Dual".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [
                    { "value": "none", "label": "None", "default": true },
                    { "value": "high", "label": "High" },
                ],
            })
            .as_object()
            .cloned(),
        );
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "none");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::None));
            }
            other => panic!("expected SwitchModel with none, got {other:?}"),
        }
    }

    #[test]
    fn remap_id_dispatches_mapped_canonical_effort() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("reasoning-x"));
        let info = acp::ModelInfo::new(id.clone(), "Reasoning X".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [{ "id": "deep", "value": "xhigh", "label": "Deep" }],
            })
            .as_object()
            .cloned(),
        );
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        // The rendered row inserts the id; `/effort deep` must send `xhigh`.
        match EffortCommand.run(&mut ctx, "deep") {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with remapped effort, got {other:?}"),
        }
    }

    #[test]
    fn non_reasoning_model_errors() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        assert!(matches!(
            result,
            CommandResult::Error(msg) if msg.contains("does not support reasoning effort")
        ));
    }

    #[test]
    fn no_current_model_errors() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        assert!(matches!(result, CommandResult::Error(msg) if msg.contains("No active model")));
    }

    #[test]
    fn suggest_args_none_without_current_or_support() {
        let cmd = EffortCommand;
        let empty = ModelState::default();
        let ctx = AppCtx {
            models: &empty,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        assert!(cmd.suggest_args(&ctx, "").is_none());

        let mut plain = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        plain.available.insert(id.clone(), info);
        plain.current = Some(id);
        let ctx = AppCtx {
            models: &plain,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        assert_eq!(cmd.suggest_args(&ctx, "").unwrap()[0].insert_text, "auto");
    }

    #[test]
    fn suggest_args_lists_levels_with_active_marker() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        state.reasoning_effort = Some(ReasoningEffort::High);

        let cmd = EffortCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        // The auto row leads the menu; the model's own levels follow it.
        assert_eq!(items.len(), EFFORT_LEVELS.len() + 1);
        assert_eq!(items[0].insert_text, "auto");
        assert_eq!(items[1].insert_text, "xhigh");
        assert_eq!(items[2].insert_text, "high");
        assert_eq!(items[2].display, "high (active)");
        assert_eq!(items[3].insert_text, "medium");
        assert_eq!(items[4].insert_text, "low");
        assert!(items[1].match_text.starts_with("a "));
        assert!(items[4].match_text.starts_with("d "));
        assert!(
            items[0].match_text.starts_with("0 "),
            "the auto row leads the tiebreak too"
        );
    }
}
