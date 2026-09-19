//! `/utility-model`: which model serves utility work, and how to change it.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

use super::provider_status::{cheap_lane_status, openrouter_entries};

pub struct CheapModelCommand;

impl SlashCommand for CheapModelCommand {
    slash_meta! {
        name: "utility-model",
        aliases: ["utility", "cheap-model", "cheap"],
        description: "Show or set the utility model",
        usage: "/utility-model <model> [effort] | clear",
        takes_args: true,
        args_required: true,
        offered_when_session_less: true,
        arg_placeholder: "<model> [effort]",
    }

    fn suggest_args(&self, ctx: &AppCtx, query: &str) -> Option<Vec<ArgItem>> {
        Some(super::model::tier_suggestions(ctx.models, query, false))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let entry = args.trim();
        if entry.is_empty() {
            return CommandResult::Message(cheap_lane_status());
        }
        if entry == "clear" {
            return CommandResult::Action(Action::SetCheapModel(String::new(), None));
        }
        if let Ok((model, effort)) = super::model::parse_tier_selection(ctx.models, entry) {
            if openrouter_entries().iter().any(|(key, _)| key == &model) {
                return CommandResult::Action(Action::SetCheapModel(model, effort));
            }
            return CommandResult::Error("Choose an OpenRouter model for Utility.".into());
        }
        let entry = super::model::split_trailing_token(entry)
            .filter(|(_, effort)| effort.eq_ignore_ascii_case("auto"))
            .map_or(entry, |(model, _)| model);
        // Two shapes are valid, and they mean different transports: a configured
        // OpenRouter entry id (the entry's base URL and key), or one or more
        // OpenRouter model ids, comma-separated, on the shipped defaults.
        if standalone_chain(entry) || openrouter_entries().iter().any(|(key, _)| key == entry) {
            return CommandResult::Action(Action::SetCheapModel(entry.to_owned(), None));
        }
        if let Some((prefix, _)) = super::model::split_trailing_token(entry)
            && ctx.models.resolve_by_name_or_id(prefix).is_some()
            && let Err(error) = super::model::parse_tier_selection(ctx.models, entry)
        {
            return CommandResult::Error(error);
        }
        CommandResult::Error(format!(
            "`{entry}` is neither a configured OpenRouter entry nor a list of OpenRouter model \
             ids.\n\nA chain is `vendor/model,vendor/model:free,…`.\n\n{}",
            cheap_lane_status()
        ))
    }
}

/// Whether the value is one or more OpenRouter model ids, comma-separated.
///
/// Every id must be non-empty, free of whitespace, and namespaced (`vendor/model`),
/// which is what keeps a typo from being written into the utility model
/// that cannot exist.
pub(crate) fn standalone_chain(value: &str) -> bool {
    let mut seen = false;
    for id in value.split(',').map(str::trim) {
        if id.is_empty() || id.chars().any(char::is_whitespace) || !id.contains('/') {
            return false;
        }
        seen = true;
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn run(args: &str) -> CommandResult {
        let models = crate::acp::ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        CheapModelCommand.run(&mut ctx, args)
    }

    #[test]
    fn bare_reports_the_lane_instead_of_writing() {
        assert!(matches!(run(""), CommandResult::Message(text) if text.contains("Utility model:")));
    }

    #[test]
    fn clear_asks_the_dispatcher_to_empty_the_pick() {
        assert!(matches!(
            run("clear"),
            CommandResult::Action(Action::SetCheapModel(entry, _)) if entry.is_empty()
        ));
    }

    /// A typo must not reach the config: an unknown entry is an error naming the
    /// candidates, never a write of the wrong model into the utility model.
    #[test]
    fn an_unknown_entry_is_refused_with_the_candidates() {
        match run("definitely-not-a-configured-entry") {
            CommandResult::Error(text) => {
                assert!(
                    text.contains("neither a configured OpenRouter entry"),
                    "{text}"
                );
                assert!(text.contains("Utility model:"), "{text}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// The owner's chain: a comma list of OpenRouter ids, tried in order, written
    /// verbatim so the shell sends it as OpenRouter's `models` fallbacks.
    #[test]
    fn a_comma_separated_chain_is_accepted_in_order() {
        let chain = "inclusionai/ling-3.0-flash-vl:free,inclusionai/ling-3.0-flash-vl,\
                     qwen/qwen3.7-flash";
        assert!(matches!(
            run(chain),
            CommandResult::Action(Action::SetCheapModel(written, _)) if written == chain
        ));
    }

    /// A single namespaced id is a chain of one.
    #[test]
    fn a_single_model_id_is_accepted_on_its_own() {
        assert!(matches!(
            run("inclusionai/ling-3.0-flash-vl:free"),
            CommandResult::Action(Action::SetCheapModel(_, _))
        ));
    }

    /// A list with a hole in it is refused: `a,,c` would silently drop a rung.
    #[test]
    fn a_chain_with_an_empty_rung_is_refused() {
        for bad in ["a/b,,c/d", "a/b,", ",a/b", "no-slash", "two words/model"] {
            assert!(
                matches!(run(bad), CommandResult::Error(_)),
                "`{bad}` must not be written into the utility model"
            );
        }
    }

    /// A configured entry is accepted verbatim — that is the string `[jev.local] model` will hold.
    #[test]
    fn a_configured_entry_is_picked_verbatim() {
        let Some((key, _)) = openrouter_entries().into_iter().next() else {
            return; // no OpenRouter entry in this environment: nothing to pin
        };
        assert!(matches!(
            run(&key),
            CommandResult::Action(Action::SetCheapModel(entry, _)) if entry == key
        ));
    }
}
