//! `/cheap-model`: which model serves the cheap lane, and how to change it.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

use super::provider_status::{cheap_lane_status, openrouter_entries};

pub struct CheapModelCommand;

impl SlashCommand for CheapModelCommand {
    slash_meta! {
        name: "cheap-model",
        aliases: ["cheap"],
        description: "Show or set the model that serves the cheap lane",
        usage: "/cheap-model [<entry>|clear]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let entry = args.trim();
        if entry.is_empty() {
            return CommandResult::Message(cheap_lane_status());
        }
        if entry == "clear" {
            return CommandResult::Action(Action::SetCheapModel(String::new()));
        }
        // Only a configured OpenRouter entry is a valid answer: the cheap lane
        // routes through OpenRouter, so anything else would be a wrong pick.
        let entries = openrouter_entries();
        if !entries.iter().any(|(key, _)| key == entry) {
            return CommandResult::Error(format!(
                "`{entry}` is not a configured OpenRouter entry.\n\n{}",
                cheap_lane_status()
            ));
        }
        CommandResult::Action(Action::SetCheapModel(entry.to_owned()))
    }
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
        assert!(matches!(run(""), CommandResult::Message(text) if text.contains("Cheap lane model:")));
    }

    #[test]
    fn clear_asks_the_dispatcher_to_empty_the_pick() {
        assert!(matches!(
            run("clear"),
            CommandResult::Action(Action::SetCheapModel(entry)) if entry.is_empty()
        ));
    }

    /// A typo must not reach the config: an unknown entry is an error naming the
    /// candidates, never a write of the wrong model into the cheap lane.
    #[test]
    fn an_unknown_entry_is_refused_with_the_candidates() {
        match run("definitely-not-a-configured-entry") {
            CommandResult::Error(text) => {
                assert!(text.contains("is not a configured OpenRouter entry"), "{text}");
                assert!(text.contains("Cheap lane model:"), "{text}");
            }
            other => panic!("expected Error, got {other:?}"),
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
            CommandResult::Action(Action::SetCheapModel(entry)) if entry == key
        ));
    }
}
