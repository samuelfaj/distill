//! `/tiers`: the three models a call can run on, and how to set each one.
//!
//! Reasoning is the session's own model, worker is its sibling for steps that do not
//! need it, utility is the OpenRouter lane. Jev picks between reasoning and worker (and
//! the effort) for each single model call while auto effort is on.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct TiersCommand;

impl SlashCommand for TiersCommand {
    slash_meta! {
        name: "tiers",
        description: "Show or set the reasoning, worker and utility models",
        usage: "/tiers [reasoning <name>|worker <id>|utility <ids>]",
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
            "reasoning" | "hard" => super::model::ModelCommand.run(ctx, rest),
            "worker" | "light" => super::worker_model::WorkerModelCommand.run(ctx, rest),
            "utility" | "cheap" => super::cheap_model::CheapModelCommand.run(ctx, rest),
            other => CommandResult::Error(format!(
                "Unknown tier `{other}`. Usage: /tiers [reasoning <name>|worker <id>|utility <ids>]"
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

    /// Bare `/tiers` opens the editable tier screen.
    #[test]
    fn bare_opens_the_editor() {
        assert!(matches!(
            run(""),
            CommandResult::Action(Action::ShowTierEditor)
        ));
    }

    #[test]
    fn light_writes_the_configured_sibling_and_clear_removes_it() {
        assert!(matches!(
            run("worker codex-luna"),
            CommandResult::Action(Action::SetTierLight(id, _)) if id == "codex-luna"
        ));
        assert!(matches!(
            run("worker clear"),
            CommandResult::Action(Action::SetTierLight(id, _)) if id.is_empty()
        ));
    }

    /// A value that cannot be a catalog key never reaches the config.
    #[test]
    fn light_refuses_a_value_that_is_not_a_model_id() {
        assert!(matches!(run("worker two words"), CommandResult::Error(_)));
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
    fn hard_without_a_name_explains_itself() {
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
