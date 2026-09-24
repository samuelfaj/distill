//! `/tiers`: the three models a call can run on, and how to set each one.
//!
//! The main model runs every step; the optional reasoning model plans and
//! reviews steps the main model cannot do alone.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct TiersCommand;

impl SlashCommand for TiersCommand {
    slash_meta! {
        name: "tiers",
        description: "Show or set the main, reasoning and utility models",
        usage: "/tiers [main <name>|reasoning <name>|utility <ids>]",
        takes_args: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::Action(Action::ShowTierEditor);
        }
        let (what, rest) = match args.split_once(char::is_whitespace) {
            Some((what, rest)) => (what, rest.trim()),
            None => (args, ""),
        };
        match what {
            "main" => super::model::ModelCommand.run(ctx, rest),
            "reasoning" => super::reasoning_model::ReasoningModelCommand.run(ctx, rest),
            "utility" | "cheap" => super::cheap_model::CheapModelCommand.run(ctx, rest),
            other => CommandResult::Error(format!(
                "Unknown tier `{other}`. Usage: /tiers [main <name>|reasoning <name>|utility <ids>]"
            )),
        }
    }
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
        let mut models = ModelState::default();
        let id = agent_client_protocol::ModelId::new("codex-luna");
        models.available.insert(
            id.clone(),
            agent_client_protocol::ModelInfo::new(id, "Luna"),
        );
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

    /// Bare `/tiers` opens the editable tier screen.
    #[test]
    fn bare_opens_the_editor() {
        assert!(matches!(
            run(""),
            CommandResult::Action(Action::ShowTierEditor)
        ));
    }

    /// `main` sets the required main model; `reasoning` sets or clears the
    /// optional one. Each tier goes through its own command, so they agree.
    #[test]
    fn main_and_reasoning_tiers_route_to_their_commands() {
        assert!(matches!(
            run("main codex-luna"),
            CommandResult::Action(Action::SetDefaultModel(id)) if id.0.as_ref() == "codex-luna"
        ));
        assert!(matches!(
            run("reasoning codex-luna"),
            CommandResult::Action(Action::SetReasoningModel(id)) if id.0.as_ref() == "codex-luna"
        ));
        assert!(matches!(
            run("reasoning clear"),
            CommandResult::Action(Action::ClearReasoningModel)
        ));
    }

    /// A value that cannot be a catalog key never reaches the config.
    #[test]
    fn main_refuses_a_value_that_is_not_a_model_id() {
        assert!(matches!(run("main two words"), CommandResult::Error(_)));
    }

    /// The utility tier is the same door as `/utility-model`, including the chain.
    #[test]
    fn cheap_accepts_a_chain_and_clear() {
        let chain = "inclusionai/ling-3.0-flash-vl:free,qwen/qwen3.7-flash";
        assert!(matches!(
            run(&format!("utility {chain}")),
            CommandResult::Action(Action::SetCheapModel(written, _)) if written == chain
        ));
        assert!(matches!(
            run("utility clear"),
            CommandResult::Action(Action::SetCheapModel(written, _)) if written.is_empty()
        ));
        assert!(matches!(run("utility no-slash"), CommandResult::Error(_)));
    }

    #[test]
    fn reasoning_without_a_name_explains_itself() {
        assert!(matches!(run("reasoning"), CommandResult::Error(_)));
    }

    #[test]
    fn an_unknown_tier_is_refused_with_the_usage() {
        match run("medium grok-4.6") {
            CommandResult::Error(text) => assert!(text.contains("Unknown tier"), "{text}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
