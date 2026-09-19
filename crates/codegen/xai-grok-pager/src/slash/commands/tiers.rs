//! `/tiers`: the three models a call can run on, and how to set each one.
//!
//! Hard is the session's own model, light is its sibling for steps that do not
//! need it, cheap is the OpenRouter lane. Jev picks between hard and light (and
//! the effort) for each single model call while auto effort is on.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use super::cheap_model::standalone_chain;
use super::provider_status::{cheap_lane_status, openrouter_entries, tier_status};

pub struct TiersCommand;

impl SlashCommand for TiersCommand {
    slash_meta! {
        name: "tiers",
        description: "Show or set the hard, light and cheap models",
        usage: "/tiers [hard <name>|light <id>|cheap <ids>]",
        takes_args: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::Message(tier_status(
                ctx.models.current_model_name().as_deref(),
            ));
        }
        let (what, rest) = match args.split_once(char::is_whitespace) {
            Some((what, rest)) => (what, rest.trim()),
            None => (args, ""),
        };
        match what {
            "hard" => set_hard(ctx, rest),
            "light" => set_light(rest),
            "cheap" => set_cheap(rest),
            other => CommandResult::Error(format!(
                "Unknown tier `{other}`. Usage: /tiers [hard <name>|light <id>|cheap <ids>]"
            )),
        }
    }
}

/// The hard tier is the session's model: setting it is the model switch, so
/// there is one home for it rather than a second copy in the config.
fn set_hard(ctx: &mut CommandExecCtx, name: &str) -> CommandResult {
    if name.is_empty() {
        return CommandResult::Error(
            "Usage: /tiers hard <name> — the model a session runs on (same as /model)".to_owned(),
        );
    }
    match ctx.models.resolve_by_name_or_id(name) {
        Some(id) => CommandResult::Action(Action::SetDefaultModel(id)),
        None => CommandResult::Error(format!(
            "`{name}` is not in the catalog. `/model` lists what is."
        )),
    }
}

fn set_light(id: &str) -> CommandResult {
    if id.is_empty() {
        return CommandResult::Error(
            "Usage: /tiers light <model-id>|clear — the session model's lighter sibling".to_owned(),
        );
    }
    if id == "clear" {
        return CommandResult::Action(Action::SetTierLight(String::new()));
    }
    // The sibling has to be a catalog entry: the harness needs its endpoint,
    // backend and window to check it is the same family.
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c))
    {
        return CommandResult::Error(format!(
            "`{id}` cannot be a model id. Use the `[model.<id>]` key from the config, or \\
             `clear` to remove the tier."
        ));
    }
    CommandResult::Action(Action::SetTierLight(id.to_owned()))
}

fn set_cheap(ids: &str) -> CommandResult {
    if ids.is_empty() {
        return CommandResult::Message(cheap_lane_status());
    }
    if ids == "clear" {
        return CommandResult::Action(Action::SetCheapModel(String::new()));
    }
    if standalone_chain(ids) || openrouter_entries().iter().any(|(key, _)| key == ids) {
        return CommandResult::Action(Action::SetCheapModel(ids.to_owned()));
    }
    CommandResult::Error(format!(
        "`{ids}` is neither a configured OpenRouter entry nor a list of OpenRouter model ids.\\n\\n{}",
        cheap_lane_status()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::ModelState;

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
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        TiersCommand.run(&mut ctx, args)
    }

    /// Bare `/tiers` reports the three, in the order a call falls through them.
    #[test]
    fn bare_reports_the_three_tiers() {
        match run("") {
            CommandResult::Message(text) => {
                let hard = text.find("Hard model").expect("hard");
                let light = text.find("Light model").expect("light");
                let cheap = text.find("Cheap lane model:").expect("cheap");
                assert!(hard < light && light < cheap, "{text}");
            }
            other => panic!("expected the tier report, got {other:?}"),
        }
    }

    #[test]
    fn light_writes_the_configured_sibling_and_clear_removes_it() {
        assert!(matches!(
            run("light codex-luna"),
            CommandResult::Action(Action::SetTierLight(id)) if id == "codex-luna"
        ));
        assert!(matches!(
            run("light clear"),
            CommandResult::Action(Action::SetTierLight(id)) if id.is_empty()
        ));
    }

    /// A value that cannot be a catalog key never reaches the config.
    #[test]
    fn light_refuses_a_value_that_is_not_a_model_id() {
        assert!(matches!(run("light two words"), CommandResult::Error(_)));
    }

    /// The cheap tier is the same door as `/cheap-model`, including the chain.
    #[test]
    fn cheap_accepts_a_chain_and_clear() {
        let chain = "inclusionai/ling-3.0-flash-vl:free,qwen/qwen3.7-flash";
        assert!(matches!(
            run(&format!("cheap {chain}")),
            CommandResult::Action(Action::SetCheapModel(written)) if written == chain
        ));
        assert!(matches!(
            run("cheap clear"),
            CommandResult::Action(Action::SetCheapModel(written)) if written.is_empty()
        ));
        assert!(matches!(run("cheap no-slash"), CommandResult::Error(_)));
    }

    #[test]
    fn hard_without_a_name_explains_itself() {
        assert!(matches!(run("hard"), CommandResult::Error(_)));
    }

    #[test]
    fn an_unknown_tier_is_refused_with_the_usage() {
        match run("medium grok-4.6") {
            CommandResult::Error(text) => assert!(text.contains("Unknown tier"), "{text}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
