// Modified for Distill by Samuel Fajreldines, 2026.
//! `/model` (alias `/m`): switch the model and optionally its reasoning effort.
//! Chained autocomplete: after picking a reasoning-supported model, the trailing space re-opens the dropdown into a `low|medium|high|xhigh` sub-menu.

use agent_client_protocol as acp;

use crate::acp::model_state::ModelState;
use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::commands::effort_levels::{build_effort_arg_items, effort_auto_arg_item};

/// Switch the active model (and optionally its reasoning effort).
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    slash_meta! {
        name: "model",
        aliases: ["m"],
        description: "Switch the active model",
        usage: "/model <name> [effort]",
        takes_args: true,
        args_required: true,
        session_scoped: true,
        // The dashboard offers `/model` to pick the model for the next spawned agent (intercepted in `dispatch_dashboard_dispatch_slash`).
        offered_when_session_less: true,
        arg_placeholder: "<model> [effort]",
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }

        // Effort phase if input is "<reasoning-model> ", else model phase.
        if let Some(model_id) = detect_effort_phase(ctx.models, args_query) {
            return Some(build_effort_items(ctx.models, &model_id));
        }
        Some(build_model_items(ctx.models))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /model <name> [effort]".into());
        }

        // Prefer an exact full-string catalog match first. Model display names often contain spaces ("Grok 4.5").
        // If we split on the last token first, a shorter catalog entry ("Grok") would steal the prefix and treat "4.5" as an effort level
        if let Some(id) = ctx.models.resolve_by_name_or_id(trimmed) {
            return CommandResult::Action(Action::SetDefaultModel(id));
        }

        // A trailing effort token on a reasoning model makes a session-scoped switch (not persisted as default)
        // Resolve via the shared gate so a rejected level (e.g. `none` on grok-4.5) reports the effort error with the model's offered ids.
        // Without it the fall-through reports "Unknown model: … none"
        if let Some((prefix, token)) = split_trailing_token(trimmed)
            && let Some(id) = resolve_model(ctx.models, prefix)
        {
            if token.eq_ignore_ascii_case("auto") {
                return CommandResult::Action(Action::SetDefaultModel(id));
            }
            return match ctx.models.resolve_effort_for_model(&id, token) {
                Ok(effort) => CommandResult::Action(Action::SwitchModel {
                    model_id: id,
                    effort: Some(effort),
                }),
                Err(err) => CommandResult::Error(err.message()),
            };
        }

        CommandResult::Error(format!("Unknown model: {trimmed}"))
    }
}

/// Look up a model by case-insensitive display name OR model id match.
fn resolve_model(models: &ModelState, name: &str) -> Option<acp::ModelId> {
    models.resolve_by_name_or_id(name)
}

/// Split `args` into `(prefix, last_token)` on the final whitespace run.
/// Returns `None` when there is no interior whitespace to split on.
/// The token is resolved to an effort against the picked model's options by the caller.
pub(super) fn split_trailing_token(args: &str) -> Option<(&str, &str)> {
    let (prefix, last) = args.rsplit_once(char::is_whitespace)?;
    let prefix = prefix.trim_end();
    if prefix.is_empty() || last.is_empty() {
        return None;
    }
    Some((prefix, last))
}

/// Returns the matched model id when `args_query` is `"<reasoning-model> ..."`.
/// Candidates are tried longest name first to disambiguate names that share a prefix.
pub(super) fn detect_effort_phase(models: &ModelState, args_query: &str) -> Option<acp::ModelId> {
    if let Some((prefix, _)) = args_query.rsplit_once(char::is_whitespace)
        && let Some(id) = models.resolve_by_name_or_id(prefix.trim_end())
    {
        return Some(id);
    }
    let mut candidates: Vec<(&acp::ModelId, &str)> = models
        .available
        .iter()
        .map(|(id, info)| (id, info.name.as_str()))
        .collect();
    candidates.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));

    for (id, name) in candidates {
        if args_query
            .get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
            && args_query
                .get(name.len()..)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        {
            return Some(id.clone());
        }
    }
    None
}

/// One row per logical model.
/// Reasoning models get a trailing space in `insert_text` so the prompt widget chains into the effort sub-menu.
pub(super) fn build_model_items(models: &ModelState) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let mut items: Vec<ArgItem> = Vec::with_capacity(models.available.len());
    for (id, info) in &models.available {
        let is_current = current_id == Some(id);

        let display = if is_current {
            format!("{} (current)", info.name)
        } else {
            info.name.clone()
        };

        // A trailing space on reasoning models signals "more input expected" to the prompt widget
        // Enter then advances to the effort phase instead of submitting
        let insert_text = format!("{} ", info.name);

        items.push(ArgItem {
            display,
            match_text: info.name.clone(),
            insert_text,
            description: info.description.clone().unwrap_or_default(),
        });
    }
    items
}

/// One row per effort level for the `/model` chained effort phase.
/// `insert_text` is `"ModelName high"` so selecting a row completes both tokens.
pub(super) fn build_effort_items(models: &ModelState, model_id: &acp::ModelId) -> Vec<ArgItem> {
    let info = match models.available.get(model_id) {
        Some(info) => info,
        None => return Vec::new(),
    };
    let model_name = info.name.clone();
    let is_current_model = models.current.as_ref() == Some(model_id);
    let options = models.reasoning_effort_options_for(model_id);
    let mut auto = effort_auto_arg_item(false);
    auto.display = "Auto Effort (default)".into();
    auto.insert_text = format!("{model_name} auto");
    auto.match_text = format!("0 {}", auto.insert_text);
    let mut items = vec![auto];
    items.extend(build_effort_arg_items(
        &options,
        models.reasoning_effort,
        is_current_model && !models.effort_auto,
        |option| format!("{model_name} {}", option.id),
    ));
    for item in &mut items {
        let token = item
            .insert_text
            .rsplit_once(' ')
            .map(|(_, token)| token)
            .unwrap_or("auto");
        item.match_text
            .push_str(&format!(" {} {token}", model_id.0));
    }
    items
}

/// Auxiliary tiers reuse the model/effort picker, restricted to their transport.
pub(super) fn tier_suggestions(models: &ModelState, query: &str, worker: bool) -> Vec<ArgItem> {
    let mut candidates = models.clone();
    let eligible = if worker {
        models
            .current_model_id_str()
            .map(distill_shell::jev::worker_candidates)
            .unwrap_or_default()
    } else {
        super::provider_status::openrouter_entries()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    };
    candidates
        .available
        .retain(|id, _| eligible.iter().any(|candidate| candidate == id.0.as_ref()));
    candidates.current = None;
    candidates.reasoning_effort = None;
    candidates.effort_auto = true;
    if let Some(id) = detect_effort_phase(&candidates, query) {
        return build_effort_items(&candidates, &id);
    }
    let mut items = build_model_items(&candidates);
    items.push(ArgItem {
        display: "clear".into(),
        match_text: "clear".into(),
        insert_text: "clear".into(),
        description: if worker {
            "Remove the worker model"
        } else {
            "Restore the default utility models"
        }
        .into(),
    });
    items
}

pub(super) fn parse_tier_selection(
    models: &ModelState,
    args: &str,
) -> Result<
    (
        String,
        Option<distill_shell::sampling::types::ReasoningEffort>,
    ),
    String,
> {
    let args = args.trim();
    if let Some(id) = models.resolve_by_name_or_id(args) {
        return Ok((id.0.to_string(), None));
    }
    if let Some((prefix, token)) = split_trailing_token(args)
        && let Some(id) = models.resolve_by_name_or_id(prefix)
    {
        let effort = if token.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(
                models
                    .resolve_effort_for_model(&id, token)
                    .map_err(|error| error.message())?,
            )
        };
        return Ok((id.0.to_string(), effort));
    }
    Err(format!(
        "Unknown model: {args}. Choose a model from the list; effort defaults to auto."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn split_trailing_token_splits_on_final_whitespace() {
        assert_eq!(
            split_trailing_token("Reasoning X high"),
            Some(("Reasoning X", "high"))
        );
        assert_eq!(
            split_trailing_token("reasoning-x  xhigh"),
            Some(("reasoning-x", "xhigh"))
        );
        // No interior whitespace, so nothing to split off
        assert!(split_trailing_token("reasoning-x-pro").is_none());
    }

    #[test]
    fn empty_query_returns_one_row_per_logical_model() {
        let mut state = ModelState::default();
        let (rid, rinfo) = model_with_reasoning("reasoning-x", "Reasoning X");
        let (pid, pinfo) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(rid, rinfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
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
        assert_eq!(items.len(), 2, "model phase: one row per logical model");

        // A reasoning model has a trailing space in insert_text
        // The prompt widget reads it to keep the dropdown open after Enter so the effort sub-menu can render
        let reasoning = items
            .iter()
            .find(|i| i.match_text == "Reasoning X")
            .unwrap();
        assert_eq!(reasoning.insert_text, "Reasoning X ");

        // Every model offers auto, including models without fixed effort levels.
        let plain = items.iter().find(|i| i.match_text == "Grok 4.5").unwrap();
        assert_eq!(plain.insert_text, "Grok 4.5 ");
    }

    #[test]
    fn trailing_space_after_reasoning_model_enters_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
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
        // The args query has a trailing space, so this is the effort phase
        // Items come out ordered xhigh to low (strongest first) per EFFORT_LEVELS
        let items = cmd.suggest_args(&ctx, "Reasoning X ").unwrap();
        assert_eq!(items.len(), 5);
        let [auto, a, b, c, d] = items.as_slice() else {
            panic!("expected auto plus 4 levels: {items:?}");
        };
        assert_eq!(auto.insert_text, "Reasoning X auto");
        assert_eq!(a.insert_text, "Reasoning X xhigh");
        assert_eq!(b.insert_text, "Reasoning X high");
        assert_eq!(c.insert_text, "Reasoning X medium");
        assert_eq!(d.insert_text, "Reasoning X low");
        // Display is just the level so the user sees a clean column.
        assert_eq!(a.display, "xhigh");
        // match_text carries the sort-key prefix that forces the matcher's alphabetical tiebreak to render rows in EFFORT_LEVELS order
        assert!(a.match_text.starts_with("a "));
        assert!(d.match_text.starts_with("d "));
    }

    #[test]
    fn partial_effort_query_still_in_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
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
        // Still in effort phase; the matcher upstream narrows to high and xhigh
        let items = cmd.suggest_args(&ctx, "Reasoning X h").unwrap();
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn partial_model_query_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
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
        // No trailing space: the user is still typing the model name
        let items = cmd.suggest_args(&ctx, "Reason").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items.first().map(|item| item.insert_text.as_str()),
            Some("Reasoning X ")
        );
    }

    #[test]
    fn run_parses_model_plus_effort_when_supported() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X xhigh");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id.0.as_ref(), "reasoning-x");
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_unoffered_effort_with_effort_error_not_unknown_model() {
        // Regression: previously `resolve_effort_token_for` returned None and the handler fell through to `Unknown model: Reasoning X none`
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X none");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("unknown effort level 'none'"),
                    "expected effort error, got {msg}"
                );
                assert!(
                    msg.contains("use one of:"),
                    "expected offered levels in message, got {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("unknown model"),
                    "must not misreport as unknown model: {msg}"
                );
                let offered = msg.split_once("; ").map(|(_, r)| r).unwrap_or("");
                assert!(
                    !offered.contains("none"),
                    "must not list none as offered: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_prefers_full_multi_word_model_name_over_prefix_plus_effort() {
        // The catalog has both "Grok" (reasoning) and "Grok 4.5"
        // `/model Grok 4.5` must select the full name, not treat "4.5" as an effort on "Grok"
        let mut state = ModelState::default();
        let (short_id, short_info) = model_with_reasoning("grok", "Grok");
        let (long_id, long_info) = model_with_reasoning("grok-4.5", "Grok 4.5");
        state.available.insert(short_id, short_info);
        state.available.insert(long_id.clone(), long_info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, long_id);
            }
            other => panic!("expected SetDefaultModel(Grok 4.5), got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_effort_for_non_reasoning_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 high");
        // Falls through to "is the whole string a model name?", which it isn't, so we get an Unknown error
        assert!(matches!(result, CommandResult::Error(_)));
    }

    /// The bare `/model <name>` form dispatches `Action::SetDefaultModel(<ModelId>)` instead of the legacy `Action::SwitchModel { effort: None }`.
    /// The dispatcher routes it through both `Effect::SwitchModel` (session mutation) and `Effect::PersistSetting` (next-session default).
    /// The payload is the typed `acp::ModelId` (resolved at the slash boundary), not a String.
    #[test]
    fn run_bare_model_name_dispatches_set_default_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    /// Case-insensitive matching against the catalog: `/model grok 4.5` resolves to the same `ModelId` as `/model Grok 4.5`.
    #[test]
    fn run_set_default_model_resolves_case_insensitively() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }
    #[test]
    fn auto_is_offered_for_plain_models_and_accepted_by_name_or_id() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("plain", "Plain Model");
        state.available.insert(id.clone(), info);
        let items = build_effort_items(&state, &id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "Plain Model auto");
        for input in ["Plain Model auto", "plain AUTO", "plain"] {
            assert!(
                matches!(ModelCommand.run(&mut dummy_exec_ctx(&state), input), CommandResult::Action(Action::SetDefaultModel(selected)) if selected == id)
            );
            assert_eq!(
                parse_tier_selection(&state, input).unwrap(),
                ("plain".into(), None)
            );
        }
        assert_eq!(detect_effort_phase(&state, "plain "), Some(id));
    }

    #[test]
    fn worker_selection_resolves_display_names_and_validates_effort() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("luna", "Luna (ChatGPT)");
        state.available.insert(id, info);
        let cmd = super::super::worker_model::WorkerModelCommand;
        assert!(
            matches!(cmd.run(&mut dummy_exec_ctx(&state), "Luna (ChatGPT) high"), CommandResult::Action(Action::SetTierLight(model, Some(ReasoningEffort::High))) if model == "luna")
        );
        assert!(matches!(
            cmd.run(&mut dummy_exec_ctx(&state), "luna"),
            CommandResult::Action(Action::SetTierLight(_, None))
        ));
        assert!(matches!(
            cmd.run(&mut dummy_exec_ctx(&state), "luna turbo"),
            CommandResult::Error(_)
        ));
    }
}
